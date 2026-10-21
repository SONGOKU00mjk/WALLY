//! Composing a decoded RGBA image into an output's XRGB8888 shm buffer.
//!
//! One entry point: [`compose_into`] resamples, places and letterbox-fills
//! the image straight into the output's buffer — there is no intermediate
//! screen-sized copy, so a wallpaper lives in exactly one place in memory
//! (the buffer the compositor scans out). That is possible because the
//! resamplers below write each row at its final position inside the
//! destination (stride + offset) instead of into a packed side buffer.
//!
//! Everything here is pure pixel math: no Wayland, no file I/O, easy to
//! unit-test. Byte order is little-endian XRGB8888: blue, green, red,
//! unused.

use std::cell::RefCell;

use image::RgbaImage;

use crate::cli::{Color, Mode};

// Reusable resample scratch. Animation recomposes the canvas every tick,
// and a fresh `vec!` per frame costs an mmap + munmap round-trip plus a
// page fault on every row touched — measurable on 1080p canvases. The
// buffer is grow-only (never shrinks, never re-zeroed: every row is
// fully written before it is read) and capped, so oversized jobs fall
// back to per-call allocation and the cache cannot balloon RSS.
thread_local! {
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

const SCRATCH_CAP: usize = 4 * 1024 * 1024;

/// Run `f` over `len` scratch bytes, reusing a thread-local buffer when
/// the job fits the cap.
fn with_scratch<R>(len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    if len > SCRATCH_CAP {
        return f(&mut vec![0u8; len]);
    }
    SCRATCH.with(|cell| {
        let mut buf = cell.borrow_mut();
        if buf.len() < len {
            buf.resize(len, 0);
        }
        f(&mut buf[..len])
    })
}

thread_local! {
    static SCRATCH_F32: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}

/// The same, for the vertical-pass accumulator (`resample_into` used to
/// allocate a fresh `vec![0f32; tw * 4]` on every animation frame).
fn with_acc<R>(len: usize, f: impl FnOnce(&mut [f32]) -> R) -> R {
    if len > SCRATCH_CAP / 4 {
        return f(&mut vec![0f32; len]);
    }
    SCRATCH_F32.with(|cell| {
        let mut buf = cell.borrow_mut();
        if buf.len() < len {
            buf.resize(len, 0.0);
        }
        let (buf, _) = buf.split_at_mut(len);
        f(buf)
    })
}

/// A pixel rectangle (in canvas, buffer or source coordinates).
///
/// This is the currency of the animation fast path: a GIF frame usually
/// redraws only a sub-rect of the canvas, and that rect can be carried
/// through the whole paint pipeline — resample, sibling copy, compositor
/// damage — instead of recomposing the entire wallpaper every tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// Smallest rectangle covering both (clipping overflow to `u32::MAX`
    /// is harmless: callers intersect with their buffer bounds anyway).
    pub fn union(self, other: Rect) -> Rect {
        let x1 = self
            .x
            .saturating_add(self.w)
            .max(other.x.saturating_add(other.w));
        let y1 = self
            .y
            .saturating_add(self.h)
            .max(other.y.saturating_add(other.h));
        Rect {
            x: self.x.min(other.x),
            y: self.y.min(other.y),
            w: x1.saturating_sub(self.x.min(other.x)),
            h: y1.saturating_sub(self.y.min(other.y)),
        }
    }

    pub fn intersect(self, other: Rect) -> Rect {
        let x1 = self
            .x
            .saturating_add(self.w)
            .min(other.x.saturating_add(other.w));
        let y1 = self
            .y
            .saturating_add(self.h)
            .min(other.y.saturating_add(other.h));
        Rect {
            x: self.x.max(other.x),
            y: self.y.max(other.y),
            w: x1.saturating_sub(self.x.max(other.x)),
            h: y1.saturating_sub(self.y.max(other.y)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

// ------------------------------------------------------- filter caches
//
// `axis_taps` and the Q8 map are pure functions of (src_len, dst_len),
// but an animation resamples with the same geometry every frame —
// rebuilding the weight tables (and allocating their Vecs) per frame is
// pure waste. They are cached keyed by their inputs; the cache is small
// and replacement is round-robin. `Arc` so both axes can be held at once
// without holding a RefCell borrow across the inner call.

thread_local! {
    static TAPS_F32: RefCell<TapsCache> = const { RefCell::new(Vec::new()) };
    static MAP_Q8: RefCell<Q8Cache> = const { RefCell::new(Vec::new()) };
}

/// Weight tables for one geometry, shared behind an `Arc` so both axes
/// can be held at once without holding a RefCell borrow.
type TapsCache = Vec<((usize, usize), std::sync::Arc<AxisTaps>)>;
type Q8Map = Vec<(u32, u32, u16)>;
type Q8Cache = Vec<((usize, usize), std::sync::Arc<Q8Map>)>;

const TAPS_CACHE_SLOTS: usize = 8;

fn cached_arc<K: PartialEq + Copy, V>(
    cell: &RefCell<Vec<(K, std::sync::Arc<V>)>>,
    key: K,
    build: impl FnOnce() -> V,
    slots: usize,
) -> std::sync::Arc<V> {
    if let Some((_, v)) = cell.borrow().iter().find(|(k, _)| *k == key) {
        return v.clone();
    }
    let v = std::sync::Arc::new(build());
    let mut cache = cell.borrow_mut();
    if cache.len() >= slots {
        cache.remove(0); // round-robin: animations cycle few geometries
    }
    cache.push((key, v.clone()));
    v
}

/// Cached [`axis_taps`].
fn axis_taps_cached(src_len: usize, dst_len: usize) -> std::sync::Arc<AxisTaps> {
    TAPS_F32.with(|cell| {
        cached_arc(
            cell,
            (src_len, dst_len),
            || axis_taps(src_len, dst_len),
            TAPS_CACHE_SLOTS,
        )
    })
}

/// Cached [`bilinear_into`]'s Q8 two-tap map for one axis.
fn q8_map_cached(src_len: usize, dst_len: usize) -> std::sync::Arc<Q8Map> {
    MAP_Q8.with(|cell| {
        cached_arc(
            cell,
            (src_len, dst_len),
            || q8_map(src_len, dst_len),
            TAPS_CACHE_SLOTS,
        )
    })
}

/// Fill the whole buffer with a solid color.
pub fn fill(dst: &mut [u8], color: Color) {
    let [r, g, b] = color;
    for px in dst.as_chunks_mut::<4>().0 {
        *px = [b, g, r, 0];
    }
}

/// A source pixel buffer for composition.
///
/// Two layouts share the resamplers below because Triangle blending is
/// channel-order agnostic — only the final pack interprets bytes:
/// - `Rgba`: a decoded image, alpha meaningful, packed over `bg`.
/// - `Xrgb`: an already-XRGB8888 canvas (the hosted GIF canvas living in
///   an output's scanout buffer). Its bytes are copied verbatim; the
///   alpha slot is don't-care, so no background blending can occur.
///   This is what lets a second output with different geometry resample
///   the hosted canvas without a pixel-sized intermediate copy.
pub struct Src<'a> {
    raw: &'a [u8],
    w: u32,
    h: u32,
    xrgb: bool,
}

impl<'a> Src<'a> {
    pub fn rgba(img: &'a RgbaImage) -> Self {
        Self {
            raw: img.as_raw(),
            w: img.width(),
            h: img.height(),
            xrgb: false,
        }
    }

    /// `raw` is `w * h * 4` XRGB8888 bytes (B, G, R, X per pixel).
    pub fn xrgb(raw: &'a [u8], w: u32, h: u32) -> Self {
        debug_assert_eq!(raw.len(), w as usize * h as usize * 4);
        Self {
            raw,
            w,
            h,
            xrgb: true,
        }
    }

    fn dims(&self) -> (u32, u32) {
        (self.w, self.h)
    }
}

/// Pack one source pixel (already in source layout) as XRGB8888 bytes.
#[inline]
fn pack_src(src: &Src, px: &[u8; 4], bg: Color) -> [u8; 4] {
    if src.xrgb {
        // Same byte order as the destination; the X slot is ignored by
        // the compositor.
        [px[0], px[1], px[2], 0]
    } else {
        pack(px, bg)
    }
}

/// Blend one RGBA pixel over `bg` and pack it as XRGB8888 bytes.
///
/// The fully-opaque and fully-transparent cases skip the blend math —
/// they are the overwhelming majority in real wallpapers (JPEG, GIF and
/// most PNGs are opaque everywhere).
#[inline]
fn pack(rgba: &[u8; 4], bg: Color) -> [u8; 4] {
    match rgba[3] {
        255 => [rgba[2], rgba[1], rgba[0], 0],
        0 => [bg[2], bg[1], bg[0], 0],
        a => {
            let a = u16::from(a);
            let over = |s: u8, b: u8| -> u8 {
                ((u16::from(s) * a + u16::from(b) * (255 - a)) / 255) as u8
            };
            [
                over(rgba[2], bg[2]),
                over(rgba[1], bg[1]),
                over(rgba[0], bg[0]),
                0,
            ]
        }
    }
}

/// A source-pixel rectangle to sample from.
struct Region {
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
}

/// Where output rows land inside a destination buffer: `stride` pixels
/// per row, with the region's top-left corner at `(ox, oy)`.
#[derive(Clone, Copy)]
struct Place {
    stride: usize,
    ox: usize,
    oy: usize,
}

/// Convert a source region into packed XRGB8888 bytes placed at `at`.
/// XRGB sources are row-copied verbatim; RGBA sources go through `pack`.
fn convert(dst: &mut [u8], at: Place, src: &Src, from: Region, bg: Color) {
    let stride = src.w as usize;
    let raw = src.raw;
    for row in 0..from.h as usize {
        let s = ((from.y0 as usize + row) * stride + from.x0 as usize) * 4;
        let d = (at.oy + row) * at.stride * 4 + at.ox * 4;
        if src.xrgb {
            dst[d..d + from.w as usize * 4].copy_from_slice(&raw[s..s + from.w as usize * 4]);
        } else {
            let src_row = raw[s..s + from.w as usize * 4].as_chunks::<4>().0;
            let dst_row = dst[d..d + from.w as usize * 4].as_chunks_mut::<4>().0;
            for (dp, sp) in dst_row.iter_mut().zip(src_row) {
                *dp = pack(sp, bg);
            }
        }
    }
}

// ------------------------------------------------------------- resample

/// Precomputed Triangle weights for one axis: for each output sample, a
/// normalized span of `(source index, weight)` taps.
struct AxisTaps {
    taps: Vec<(u32, f32)>,
    /// `(offset into taps, tap count)` per output sample.
    spans: Vec<(u32, u16)>,
}

/// Triangle filter kernel; support 1.0.
fn triangle(x: f32) -> f32 {
    (1.0 - x.abs()).max(0.0)
}

/// Build the per-axis weight table mapping `src_len` samples onto
/// `dst_len` samples, mirroring `image`'s Triangle mapping: the output
/// pixel center `(d + 0.5) * ratio - 0.5` picks up taps within the
/// kernel support, which widens by the scale factor when shrinking.
fn axis_taps(src_len: usize, dst_len: usize) -> AxisTaps {
    let ratio = src_len as f32 / dst_len as f32;
    let sratio = ratio.max(1.0);
    let mut taps = Vec::new();
    let mut spans = Vec::with_capacity(dst_len);

    for d in 0..dst_len {
        let center = (d as f32 + 0.5) * ratio - 0.5;
        let left = (center - sratio).floor() as i64;
        let left = left.clamp(0, src_len as i64 - 1);
        let right = (center + sratio).ceil() as i64;
        let right = right.clamp(left + 1, src_len as i64);

        let start = taps.len() as u32;
        let mut sum = 0f32;
        for i in left..right {
            let wt = triangle((i as f32 - center) / sratio);
            if wt > 0.0 {
                taps.push((i as u32, wt));
                sum += wt;
            }
        }
        if sum > 0.0 {
            for (_, wt) in &mut taps[start as usize..] {
                *wt /= sum;
            }
        }
        spans.push((start, (taps.len() as u32 - start) as u16));
    }
    AxisTaps { taps, spans }
}

/// Per output sample: the two source taps and the blend fraction in Q8
/// (0..=255), for [`bilinear_into`]. Centers outside the source collapse
/// onto the edge pixel, matching the general pass's edge handling.
fn q8_map(src_len: usize, dst_len: usize) -> Vec<(u32, u32, u16)> {
    (0..dst_len)
        .map(|d| {
            let s = (d as f64 + 0.5) * (src_len as f64 / dst_len as f64) - 0.5;
            let last = src_len - 1;
            if s <= 0.0 {
                (0, 0, 0)
            } else if s >= last as f64 {
                (last as u32, last as u32, 0)
            } else {
                let f = s.floor() as usize;
                (
                    f as u32,
                    (f + 1).min(last) as u32,
                    ((s - s.floor()) * 256.0) as u16,
                )
            }
        })
        .collect()
}

/// Resample the `w x h` source region at `(x0, y0)` to `tw x th` and
/// pack it straight into XRGB8888 `dst` at `at` (`tw * th * 4` bytes
/// worth of rows, `at.stride` pixels per row).
///
/// This is a hand-rolled separable Triangle filter — the same kernel and
/// center mapping as `image`'s resizer (the unit tests pin the output to
/// within ±1 LSB) — but it works through one transient byte buffer
/// instead of `image`'s f32 intermediates, which need ~265 MB of scratch
/// for a 4K frame and make a single 1080p → 4K resize take about a
/// second. Here the same frame is tens of milliseconds.
fn resample_into(src: &Src, from: Region, dst: &mut [u8], at: Place, tw: u32, th: u32, bg: Color) {
    let iw = src.w as usize;
    let (w, h, tw, th) = (from.w as usize, from.h as usize, tw as usize, th as usize);
    let raw = src.raw;
    let xs = axis_taps_cached(w, tw);
    let ys = axis_taps_cached(h, th);

    // Pass 1: resample every source row horizontally (h rows of tw px).
    with_scratch(h * tw * 4, |tmp| {
        for (sy, drow) in tmp.chunks_exact_mut(tw * 4).enumerate() {
            let srow = raw[((from.y0 as usize + sy) * iw + from.x0 as usize) * 4..][..w * 4]
                .as_chunks::<4>()
                .0;
            for (dp, &(off, count)) in drow.as_chunks_mut::<4>().0.iter_mut().zip(&xs.spans) {
                let mut acc = [0f32; 4];
                for &(si, wt) in &xs.taps[off as usize..off as usize + count as usize] {
                    let p = srow[si as usize];
                    for (a, s) in acc.iter_mut().zip(p.iter().copied()) {
                        *a += wt * f32::from(s);
                    }
                }
                for (dp, a) in dp.iter_mut().zip(acc) {
                    *dp = (a + 0.5) as u8;
                }
            }
        }

        // Pass 2: resample vertically and pack over the background, straight
        // into the rows' final position.
        with_acc(tw * 4, |acc| {
            for dy in 0..th {
                let d = (at.oy + dy) * at.stride * 4 + at.ox * 4;
                let drow = &mut dst[d..d + tw * 4];
                acc.fill(0.0);
                let (off, count) = ys.spans[dy];
                for &(ri, wt) in &ys.taps[off as usize..off as usize + count as usize] {
                    let rrow = &tmp[ri as usize * tw * 4..][..tw * 4];
                    for (a, p) in acc
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(rrow.as_chunks::<4>().0)
                    {
                        for (a, s) in a.iter_mut().zip(p.iter().copied()) {
                            *a += wt * f32::from(s);
                        }
                    }
                }
                for (dp, a) in drow
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(acc.as_chunks::<4>().0)
                {
                    let mut px = [0u8; 4];
                    for (p, v) in px.iter_mut().zip(a) {
                        *p = (v + 0.5) as u8;
                    }
                    *dp = pack_src(src, &px, bg);
                }
            }
        })
    })
}

// --------------------------------------------------------------- stages

/// Bilinear (= Triangle) upscale of the `w x h` source region at
/// `(x0, y0)` to `tw x th`, packed straight into XRGB8888 `dst` at `at`.
///
/// Integer Q8 inner loops: for every output pixel the two source taps
/// blend as `(a * (256 - t) + b * t + 128) >> 8` per channel. Same
/// center mapping as [`resample_into`], so the parity tests pin this
/// pass to `image`'s Triangle upscale within ±1 LSB too.
fn bilinear_into(src: &Src, from: Region, dst: &mut [u8], at: Place, tw: u32, th: u32, bg: Color) {
    let iw = src.w as usize;
    let (w, h, tw, th) = (from.w as usize, from.h as usize, tw as usize, th as usize);
    let raw = src.raw;

    let xs = q8_map_cached(w, tw);
    let ys = q8_map_cached(h, th);
    let (xs, ys) = (&xs[..], &ys[..]);

    let blend = |a: u8, b: u8, t: u16| -> u8 {
        ((u32::from(a) * (256 - u32::from(t)) + u32::from(b) * u32::from(t) + 128) >> 8) as u8
    };

    // Pass 1: stretch every source row horizontally (h rows of tw px).
    with_scratch(h * tw * 4, |tmp| {
        for (sy, drow) in tmp.chunks_exact_mut(tw * 4).enumerate() {
            let srow = raw[((from.y0 as usize + sy) * iw + from.x0 as usize) * 4..][..w * 4]
                .as_chunks::<4>()
                .0;
            for (dp, &(i0, i1, t)) in drow.as_chunks_mut::<4>().0.iter_mut().zip(xs) {
                let (a, b) = (srow[i0 as usize], srow[i1 as usize]);
                *dp = [
                    blend(a[0], b[0], t),
                    blend(a[1], b[1], t),
                    blend(a[2], b[2], t),
                    blend(a[3], b[3], t),
                ];
            }
        }

        // Pass 2: stretch vertically and pack over the background, straight
        // into the rows' final position.
        for (dy, &(j0, j1, t)) in ys.iter().enumerate() {
            let d = (at.oy + dy) * at.stride * 4 + at.ox * 4;
            let drow = &mut dst[d..d + tw * 4];
            let (r0, r1) = (
                tmp[j0 as usize * tw * 4..][..tw * 4].as_chunks::<4>().0,
                tmp[j1 as usize * tw * 4..][..tw * 4].as_chunks::<4>().0,
            );
            for (dp, (p0, p1)) in drow
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(r0.iter().zip(r1))
            {
                let mut px = [0u8; 4];
                for (p, (a, b)) in px.iter_mut().zip(p0.iter().zip(p1)) {
                    *p = blend(*a, *b, t);
                }
                *dp = pack_src(src, &px, bg);
            }
        }
    })
}

/// Per mode: which source region (x0, y0, w, h) maps to a target of
/// (tw, th). Placement and letterboxing happen in [`compose_src_into`].
fn geometry(iw: u32, ih: u32, mode: Mode, pw: u32, ph: u32) -> (u32, u32, u32, u32, u32, u32) {
    match mode {
        Mode::Stretch => (0, 0, iw, ih, pw, ph),
        Mode::Fill => {
            // Crop the source to the output's aspect ratio (centered),
            // then scale to exactly pw x ph — same result as cover+crop,
            // but the resample only touches pixels that end up on screen.
            let target = pw as f64 / ph as f64;
            let (cw, ch) = if iw as f64 / ih as f64 > target {
                ((ih as f64 * target).round() as u32, ih)
            } else {
                (iw, (iw as f64 / target).round() as u32)
            };
            let (cw, ch) = (cw.clamp(1, iw), ch.clamp(1, ih));
            ((iw - cw) / 2, (ih - ch) / 2, cw, ch, pw, ph)
        }
        Mode::Fit => {
            let s = (pw as f64 / iw as f64).min(ph as f64 / ih as f64);
            let fw = ((iw as f64 * s).round() as u32).clamp(1, pw);
            let fh = ((ih as f64 * s).round() as u32).clamp(1, ph);
            (0, 0, iw, ih, fw, fh)
        }
        Mode::Center => {
            // 1:1, centered, cropped to the output if larger.
            let (cw, ch) = (iw.min(pw), ih.min(ph));
            ((iw - cw) / 2, (ih - ch) / 2, cw, ch, cw, ch)
        }
        Mode::Tile => (0, 0, iw, ih, iw, ih),
    }
}

/// Run the right resampler for `from` → `tw x th` and write the rows at
/// their final position `at` inside `dst`.
fn place(src: &Src, from: Region, tw: u32, th: u32, bg: Color, dst: &mut [u8], at: Place) {
    if (from.w, from.h) == (tw, th) {
        // Exact size (copy, center): rows pass straight through.
        convert(
            dst,
            at,
            src,
            Region {
                x0: from.x0,
                y0: from.y0,
                w: tw,
                h: th,
            },
            bg,
        );
    } else if tw >= from.w && th >= from.h {
        // Pure enlargement — the animation hot path (a small GIF/WebP
        // blown up to the panel). Integer bilinear, much faster than the
        // general f32 pass and pinned to it by the parity tests.
        bilinear_into(src, from, dst, at, tw, th, bg);
    } else {
        // Shrinking (or one axis each way): general Triangle pass.
        resample_into(src, from, dst, at, tw, th, bg);
    }
}

/// Map a changed source-pixel rect to the destination rect its pixels
/// land in, under `mode`, for an output of `pw x ph`. Returns `None`
/// when the restricted path does not pay off (Tile repeats everything,
/// the rect is degenerate, or it covers most of the image) — callers
/// fall back to a full compose. The result is conservative: every
/// destination pixel any source pixel of `dirty` can influence under
/// the resamplers' kernels lies inside it, and letterbox pixels (which
/// never change) are excluded.
pub fn dirty_dest(src_dims: (u32, u32), mode: Mode, pw: u32, ph: u32, dirty: Rect) -> Option<Rect> {
    if pw == 0 || ph == 0 || src_dims.0 == 0 || src_dims.1 == 0 {
        return None;
    }
    let (iw, ih) = src_dims;
    let (x0, y0, w, h, tw, th) = geometry(iw, ih, mode, pw, ph);
    if tw == 0 || th == 0 || w == 0 || h == 0 {
        return None;
    }
    let (ox, oy) = match mode {
        Mode::Stretch | Mode::Fill => (0, 0),
        Mode::Fit | Mode::Center => ((pw - tw) / 2, (ph - th) / 2),
        Mode::Tile => return None, // repeating tiles touch everything
    };

    // Clip to the source crop that reaches the screen.
    let d = dirty.intersect(Rect { x: x0, y: y0, w, h });
    if d.is_empty() {
        return None;
    }

    // Round outward, then widen by the filter support (a destination
    // sample picks up `ratio` source pixels per axis; ceil + 2 covers
    // both directions and the rounding).
    let ex = (tw / w).max(1) + 2;
    let ey = (th / h).max(1) + 2;
    let fx = f64::from(tw) / f64::from(w);
    let fy = f64::from(th) / f64::from(h);
    let dx0 = ((f64::from(d.x - x0) * fx).floor() as i64 - i64::from(ex) + i64::from(ox))
        .clamp(0, i64::from(pw)) as u32;
    let dy0 = ((f64::from(d.y - y0) * fy).floor() as i64 - i64::from(ey) + i64::from(oy))
        .clamp(0, i64::from(ph)) as u32;
    let dx1 = ((f64::from(d.x + d.w - x0) * fx).ceil() as i64 + i64::from(ex) + i64::from(ox))
        .clamp(i64::from(dx0), i64::from(pw)) as u32;
    let dy1 = ((f64::from(d.y + d.h - y0) * fy).ceil() as i64 + i64::from(ey) + i64::from(oy))
        .clamp(i64::from(dy0), i64::from(ph)) as u32;

    // Letterbox pixels never change; keep only the image's on-screen area.
    let rect = Rect {
        x: dx0,
        y: dy0,
        w: dx1 - dx0,
        h: dy1 - dy0,
    }
    .intersect(Rect {
        x: ox,
        y: oy,
        w: tw,
        h: th,
    });
    if rect.is_empty() {
        return None;
    }
    // Not worth the restricted path when it is (almost) the whole image.
    if rect.w * 8 >= tw * 7 && rect.h * 8 >= th * 7 {
        return None;
    }
    Some(rect)
}

/// Restricted [`place`]: run the resampler for the full `from` → `tw x th`
/// mapping but write only the destination rows/columns of `rect`, in the
/// image's destination coordinates (0..tw × 0..th). Byte-identical to
/// [`place`] restricted to those rows/columns — the resamplers are
/// per-output-sample independent, so the restriction just narrows the
/// loops (and, for the vertical pass, the source-row band that feeds it).
#[allow(clippy::too_many_arguments)] // `rect` rides along the full-path args
fn place_rect(
    src: &Src,
    from: Region,
    tw: u32,
    th: u32,
    bg: Color,
    dst: &mut [u8],
    at: Place,
    rect: Rect,
) {
    debug_assert!(rect.x + rect.w <= tw && rect.y + rect.h <= th);
    if (from.w, from.h) == (tw, th) {
        // 1:1: the rect maps straight back to source pixels.
        convert(
            dst,
            Place {
                stride: at.stride,
                ox: at.ox + rect.x as usize,
                oy: at.oy + rect.y as usize,
            },
            src,
            Region {
                x0: from.x0 + rect.x,
                y0: from.y0 + rect.y,
                w: rect.w,
                h: rect.h,
            },
            bg,
        );
    } else if tw >= from.w && th >= from.h {
        bilinear_into_rect(src, from, dst, at, tw, th, bg, rect);
    } else {
        resample_into_rect(src, from, dst, at, tw, th, bg, rect);
    }
}

/// [`bilinear_into`], restricted to `rect` (image-local destination
/// coordinates). Same math, narrowed loops: pass 1 resamples only the
/// source rows that feed the rect's destination rows, and only the rect's
/// destination columns; pass 2 writes only the rect.
#[allow(clippy::too_many_arguments)] // `rect` rides along the full-path args
fn bilinear_into_rect(
    src: &Src,
    from: Region,
    dst: &mut [u8],
    at: Place,
    tw: u32,
    th: u32,
    bg: Color,
    rect: Rect,
) {
    let iw = src.w as usize;
    let (w, h, tw, th) = (from.w as usize, from.h as usize, tw as usize, th as usize);
    let raw = src.raw;
    let xs = &q8_map_cached(w, tw)[..];
    let ys = &q8_map_cached(h, th)[..];
    let blend = |a: u8, b: u8, t: u16| -> u8 {
        ((u32::from(a) * (256 - u32::from(t)) + u32::from(b) * u32::from(t) + 128) >> 8) as u8
    };

    let (dx0, dx1) = (rect.x as usize, (rect.x + rect.w) as usize);
    let (dy0, dy1) = (rect.y as usize, (rect.y + rect.h) as usize);
    let rw = dx1 - dx0;
    // Source rows feeding the destination window: ys maps dest row →
    // (i0, i1, t); the band runs from the first row's first tap to the
    // last row's second tap. Scratch rows are renumbered into the band.
    let band_lo = ys[dy0].0 as usize;
    let band_hi = ys[dy1 - 1].1 as usize;
    let band = band_hi - band_lo + 1;

    with_scratch(band * rw * 4, |tmp| {
        for (sy, drow) in tmp.chunks_exact_mut(rw * 4).enumerate() {
            let srow = raw[((from.y0 as usize + band_lo + sy) * iw + from.x0 as usize) * 4..]
                [..w * 4]
                .as_chunks::<4>()
                .0;
            for (dp, &(i0, i1, t)) in drow.as_chunks_mut::<4>().0.iter_mut().zip(&xs[dx0..dx1]) {
                let (a, b) = (srow[i0 as usize], srow[i1 as usize]);
                *dp = [
                    blend(a[0], b[0], t),
                    blend(a[1], b[1], t),
                    blend(a[2], b[2], t),
                    blend(a[3], b[3], t),
                ];
            }
        }

        for (dy, &(j0, j1, t)) in ys.iter().enumerate().take(dy1).skip(dy0) {
            let d = (at.oy + dy) * at.stride * 4 + (at.ox + dx0) * 4;
            let drow = &mut dst[d..d + rw * 4];
            let (r0, r1) = (
                &tmp[(j0 as usize - band_lo) * rw * 4..][..rw * 4],
                &tmp[(j1 as usize - band_lo) * rw * 4..][..rw * 4],
            );
            for (dp, (p0, p1)) in drow
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(r0.as_chunks::<4>().0.iter().zip(r1.as_chunks::<4>().0))
            {
                let mut px = [0u8; 4];
                for (p, (a, b)) in px.iter_mut().zip(p0.iter().zip(p1)) {
                    *p = blend(*a, *b, t);
                }
                *dp = pack_src(src, &px, bg);
            }
        }
    })
}

/// [`resample_into`], restricted like [`bilinear_into_rect`].
#[allow(clippy::too_many_arguments)] // `rect` rides along the full-path args
fn resample_into_rect(
    src: &Src,
    from: Region,
    dst: &mut [u8],
    at: Place,
    tw: u32,
    th: u32,
    bg: Color,
    rect: Rect,
) {
    let iw = src.w as usize;
    let (w, h, tw, th) = (from.w as usize, from.h as usize, tw as usize, th as usize);
    let raw = src.raw;
    let xs = axis_taps_cached(w, tw);
    let ys = axis_taps_cached(h, th);

    let (dx0, dx1) = (rect.x as usize, (rect.x + rect.w) as usize);
    let (dy0, dy1) = (rect.y as usize, (rect.y + rect.h) as usize);
    let rw = dx1 - dx0;
    // Source rows feeding the destination window: spans index the taps
    // array; each tap's `.0` is the absolute source row it samples. The
    // band runs from the first dest row's first tap to the last dest
    // row's last tap (source rows rise monotonically with dest rows).
    let band_lo = ys.taps[ys.spans[dy0].0 as usize].0 as usize;
    let band_hi = {
        let (off, count) = ys.spans[dy1 - 1];
        ys.taps[off as usize + count as usize - 1].0 as usize
    };
    let band = band_hi - band_lo + 1;

    with_scratch(band * rw * 4, |tmp| {
        for (sy, drow) in tmp.chunks_exact_mut(rw * 4).enumerate() {
            let srow = raw[((from.y0 as usize + band_lo + sy) * iw + from.x0 as usize) * 4..]
                [..w * 4]
                .as_chunks::<4>()
                .0;
            for (dp, &(off, count)) in drow
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(&xs.spans[dx0..dx1])
            {
                let mut acc = [0f32; 4];
                for &(si, wt) in &xs.taps[off as usize..off as usize + count as usize] {
                    let p = srow[si as usize];
                    for (a, s) in acc.iter_mut().zip(p.iter().copied()) {
                        *a += wt * f32::from(s);
                    }
                }
                for (dp, a) in dp.iter_mut().zip(acc) {
                    *dp = (a + 0.5) as u8;
                }
            }
        }

        with_acc(rw * 4, |acc| {
            for dy in dy0..dy1 {
                let d = (at.oy + dy) * at.stride * 4 + (at.ox + dx0) * 4;
                let drow = &mut dst[d..d + rw * 4];
                acc.fill(0.0);
                let (off, count) = ys.spans[dy];
                for &(ri, wt) in &ys.taps[off as usize..off as usize + count as usize] {
                    let rrow = &tmp[(ri as usize - band_lo) * rw * 4..][..rw * 4];
                    for (a, p) in acc
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(rrow.as_chunks::<4>().0)
                    {
                        for (a, s) in a.iter_mut().zip(p.iter().copied()) {
                            *a += wt * f32::from(s);
                        }
                    }
                }
                for (dp, a) in drow
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(acc.as_chunks::<4>().0)
                {
                    let mut px = [0u8; 4];
                    for (p, v) in px.iter_mut().zip(a) {
                        *p = (v + 0.5) as u8;
                    }
                    *dp = pack_src(src, &px, bg);
                }
            }
        })
    })
}

/// Compose `img` per `mode` into `dst` (`pw * ph * 4` XRGB8888 bytes,
/// stride `pw`) — the complete wallpaper for one output in a single
/// pass, with no intermediate screen-sized buffer.
///
/// A source that already has the requested size is copied, never
/// resampled — the common "wallpaper matches my panel" case pays nothing.
pub fn compose_into(img: &RgbaImage, mode: Mode, pw: u32, ph: u32, bg: Color, dst: &mut [u8]) {
    compose_src_into(&Src::rgba(img), mode, pw, ph, bg, dst);
}

/// Compose an already-packed XRGB8888 source (e.g. the hosted GIF canvas
/// living in another output's scanout buffer). Alpha handling is skipped
/// entirely: the canvas is opaque by construction.
pub fn compose_xrgb_into(
    raw: &[u8],
    dims: (u32, u32),
    mode: Mode,
    pw: u32,
    ph: u32,
    bg: Color,
    dst: &mut [u8],
) {
    compose_src_into(&Src::xrgb(raw, dims.0, dims.1), mode, pw, ph, bg, dst);
}

/// [`compose_into`], restricted to `rect` (buffer coordinates): only the
/// rect's pixels are recomputed, byte-identical to a full compose there.
/// Pass a rect from [`dirty_dest`] (or the full image area); the
/// image's on-screen area is intersected out of it, letterbox pixels are
/// never touched (they cannot have changed). Tile falls back to full.
pub fn compose_into_rect(
    img: &RgbaImage,
    mode: Mode,
    pw: u32,
    ph: u32,
    bg: Color,
    dst: &mut [u8],
    rect: Rect,
) {
    compose_src_into_rect(&Src::rgba(img), mode, pw, ph, bg, dst, rect);
}

/// [`compose_xrgb_into`], restricted like [`compose_into_rect`].
#[allow(clippy::too_many_arguments)] // `rect` rides along the full-path args
pub fn compose_xrgb_into_rect(
    raw: &[u8],
    dims: (u32, u32),
    mode: Mode,
    pw: u32,
    ph: u32,
    bg: Color,
    dst: &mut [u8],
    rect: Rect,
) {
    compose_src_into_rect(&Src::xrgb(raw, dims.0, dims.1), mode, pw, ph, bg, dst, rect);
}

fn compose_src_into_rect(
    src: &Src,
    mode: Mode,
    pw: u32,
    ph: u32,
    bg: Color,
    dst: &mut [u8],
    rect: Rect,
) {
    debug_assert_eq!(dst.len(), pw as usize * ph as usize * 4);
    let (iw, ih) = src.dims();
    if iw == 0 || ih == 0 || pw == 0 || ph == 0 {
        return; // degenerate: callers use the full path for these
    }
    let (x0, y0, w, h, tw, th) = geometry(iw, ih, mode, pw, ph);
    let (ox, oy) = match mode {
        Mode::Stretch | Mode::Fill => (0usize, 0usize),
        Mode::Fit | Mode::Center => (((pw - tw) / 2) as usize, ((ph - th) / 2) as usize),
        Mode::Tile => return, // unsupported here; callers fall back
    };
    // To image-local coordinates, clipped to the image's destination area.
    let local = Rect {
        x: rect.x.saturating_sub(ox as u32),
        y: rect.y.saturating_sub(oy as u32),
        w: rect.w,
        h: rect.h,
    }
    .intersect(Rect {
        x: 0,
        y: 0,
        w: tw,
        h: th,
    });
    if local.is_empty() {
        return;
    }
    place_rect(
        src,
        Region { x0, y0, w, h },
        tw,
        th,
        bg,
        dst,
        Place {
            stride: pw as usize,
            ox,
            oy,
        },
        local,
    );
}

fn compose_src_into(src: &Src, mode: Mode, pw: u32, ph: u32, bg: Color, dst: &mut [u8]) {
    debug_assert_eq!(dst.len(), pw as usize * ph as usize * 4);
    let (iw, ih) = src.dims();
    if iw == 0 || ih == 0 || pw == 0 || ph == 0 {
        fill(dst, bg);
        return;
    }

    let (x0, y0, w, h, tw, th) = geometry(iw, ih, mode, pw, ph);
    match mode {
        // Stretch/Fill cover the output exactly.
        Mode::Stretch | Mode::Fill => {
            place(
                src,
                Region { x0, y0, w, h },
                tw,
                th,
                bg,
                dst,
                Place {
                    stride: pw as usize,
                    ox: 0,
                    oy: 0,
                },
            );
        }
        // Fit/Center letterbox: background first, image centered.
        Mode::Fit | Mode::Center => {
            fill(dst, bg);
            place(
                src,
                Region { x0, y0, w, h },
                tw,
                th,
                bg,
                dst,
                Place {
                    stride: pw as usize,
                    ox: ((pw - tw) / 2) as usize,
                    oy: ((ph - th) / 2) as usize,
                },
            );
        }
        // Tile: one native-size tile in scratch, then repeat it.
        Mode::Tile => {
            let mut tile = vec![0u8; iw as usize * ih as usize * 4];
            convert(
                &mut tile,
                Place {
                    stride: iw as usize,
                    ox: 0,
                    oy: 0,
                },
                src,
                Region {
                    x0: 0,
                    y0: 0,
                    w: iw,
                    h: ih,
                },
                bg,
            );
            tile_rows(dst, pw, ph, &tile, iw, ih);
        }
    }
}

/// Repeat a `tw x th` tile over a `pw x ph` buffer.
fn tile_rows(dst: &mut [u8], pw: u32, ph: u32, tile: &[u8], tw: u32, th: u32) {
    let (tw, th) = (tw as usize, th as usize);
    for dy in 0..ph as usize {
        let s = (dy % th) * tw * 4;
        let d = dy * pw as usize * 4;
        let mut dx = 0;
        while dx < pw as usize {
            let n = (pw as usize - dx).min(tw) * 4;
            dst[d + dx * 4..d + dx * 4 + n].copy_from_slice(&tile[s..s + n]);
            dx += tw;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::imageops::{self, FilterType};
    use image::Rgba;
    use std::time::Instant;

    fn render(pw: u32, ph: u32, mode: Mode, bg: Color, img: &RgbaImage) -> Vec<u8> {
        let mut dst = vec![0u8; pw as usize * ph as usize * 4];
        compose_into(img, mode, pw, ph, bg, &mut dst);
        dst
    }

    fn buf(pw: u32, ph: u32) -> Vec<u8> {
        vec![0xAA; pw as usize * ph as usize * 4]
    }

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> RgbaImage {
        RgbaImage::from_fn(w, h, |_, _| Rgba(rgba))
    }

    fn px(dst: &[u8], pw: u32, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * pw as usize + x as usize) * 4;
        [dst[i], dst[i + 1], dst[i + 2], dst[i + 3]]
    }

    const BLACK: Color = [0, 0, 0];

    #[test]
    fn fill_uses_xrgb_byte_order() {
        let mut dst = buf(2, 2);
        fill(&mut dst, [255, 0, 0]); // pure red
        assert_eq!(px(&dst, 2, 0, 0), [0, 0, 255, 0]);
    }

    #[test]
    fn stretch_fills_exactly() {
        let dst = render(4, 4, Mode::Stretch, BLACK, &solid(2, 2, [255, 0, 0, 255]));
        assert_eq!(px(&dst, 4, 0, 0), [0, 0, 255, 0]);
        assert_eq!(px(&dst, 4, 3, 3), [0, 0, 255, 0]);
    }

    #[test]
    fn fit_letterboxes_with_background() {
        // A 1x2 image into 4x4: scales to 2x4, letterboxed left/right.
        let dst = render(4, 4, Mode::Fit, BLACK, &solid(1, 2, [255, 0, 0, 255]));
        assert_eq!(px(&dst, 4, 0, 0), [0, 0, 0, 0]);
        assert_eq!(px(&dst, 4, 3, 3), [0, 0, 0, 0]);
        assert_eq!(px(&dst, 4, 1, 0), [0, 0, 255, 0]);
        assert_eq!(px(&dst, 4, 2, 3), [0, 0, 255, 0]);
    }

    #[test]
    fn center_is_one_to_one() {
        // A 2x2 image into a 4x2 output: centered horizontally.
        let dst = render(4, 2, Mode::Center, BLACK, &solid(2, 2, [0, 255, 0, 255]));
        assert_eq!(px(&dst, 4, 1, 0), [0, 255, 0, 0]);
        assert_eq!(px(&dst, 4, 2, 1), [0, 255, 0, 0]);
        assert_eq!(px(&dst, 4, 0, 0), [0, 0, 0, 0]);
        assert_eq!(px(&dst, 4, 3, 1), [0, 0, 0, 0]);
    }

    #[test]
    fn tile_repeats() {
        let dst = render(4, 4, Mode::Tile, BLACK, &solid(2, 2, [0, 0, 255, 255]));
        for (x, y) in [(0, 0), (2, 0), (1, 3), (3, 2)] {
            assert_eq!(px(&dst, 4, x, y), [255, 0, 0, 0], "at {x},{y}");
        }
    }

    #[test]
    fn fill_crops_to_aspect() {
        // 4x2 source into a 2x2 output: horizontal crop, no distortion.
        let img = RgbaImage::from_fn(4, 2, |x, _| Rgba([(x * 60) as u8, 0, 0, 255]));
        let dst = render(2, 2, Mode::Fill, BLACK, &img);
        // Center crop keeps source columns 1 and 2; XRGB byte order means
        // the red channel is the third byte.
        assert_eq!(px(&dst, 2, 0, 0), [0, 0, 60, 0]);
        assert_eq!(px(&dst, 2, 1, 1), [0, 0, 120, 0]);
    }

    #[test]
    fn alpha_blends_over_background() {
        // 50% white over black background = ~128 gray.
        let dst = render(
            1,
            1,
            Mode::Stretch,
            BLACK,
            &solid(1, 1, [255, 255, 255, 128]),
        );
        let [b, g, r, _] = px(&dst, 1, 0, 0);
        assert_eq!((r, g, b), (128, 128, 128));
    }

    #[test]
    fn exact_size_source_is_copied_verbatim() {
        // When the source already matches the output, no resample may run:
        // every pixel must survive the round trip untouched.
        let img = RgbaImage::from_fn(3, 3, |x, y| Rgba([x as u8 * 85, y as u8 * 85, 7, 255]));
        let dst = render(3, 3, Mode::Stretch, BLACK, &img);
        for y in 0..3u8 {
            for x in 0..3u8 {
                assert_eq!(
                    px(&dst, 3, x as u32, y as u32),
                    [7, y * 85, x * 85, 0],
                    "at {x},{y}"
                );
            }
        }
    }

    #[test]
    fn fit_upscale_of_tiny_image_is_exact() {
        // 1x2 -> 2x4 is an integer scale; columns must stay pure red.
        let dst = render(4, 4, Mode::Fit, BLACK, &solid(1, 2, [255, 0, 0, 255]));
        for y in 0..4 {
            assert_eq!(px(&dst, 4, 1, y), [0, 0, 255, 0]);
            assert_eq!(px(&dst, 4, 2, y), [0, 0, 255, 0]);
        }
    }

    #[test]
    fn degenerate_inputs_fill_background() {
        for (iw, ih) in [(0u32, 2u32), (2, 0)] {
            let img = RgbaImage::new(iw, ih);
            let dst = render(2, 2, Mode::Stretch, [9, 8, 7], &img);
            assert_eq!(px(&dst, 2, 0, 0), [7, 8, 9, 0]);
            assert_eq!(px(&dst, 2, 1, 1), [7, 8, 9, 0]);
        }
    }

    /// The hand-rolled resampler must track `image`'s Triangle filter
    /// within ±1 LSB per channel, up and down.
    #[test]
    fn resample_matches_image_triangle() {
        let img = RgbaImage::from_fn(37, 23, |x, y| {
            Rgba([
                (x * 7 % 256) as u8,
                (y * 11 % 256) as u8,
                (x * y % 256) as u8,
                255,
            ])
        });

        for (tw, th) in [
            (111u32, 69u32),
            (20, 12),
            (74, 46),
            (300, 23),
            (60, 15),
            (37, 8),
        ] {
            let reference = imageops::resize(&img, tw, th, FilterType::Triangle);
            let mut theirs = vec![0u8; tw as usize * th as usize * 4];
            convert(
                &mut theirs,
                Place {
                    stride: tw as usize,
                    ox: 0,
                    oy: 0,
                },
                &Src::rgba(&reference),
                Region {
                    x0: 0,
                    y0: 0,
                    w: tw,
                    h: th,
                },
                BLACK,
            );

            let mut mine = vec![0u8; tw as usize * th as usize * 4];
            compose_into(&img, Mode::Stretch, tw, th, BLACK, &mut mine);

            for (i, (a, b)) in mine.iter().zip(&theirs).enumerate() {
                let d = i32::from(*a) - i32::from(*b);
                assert!(d.abs() <= 1, "{tw}x{th}: byte {i} differs by {d}");
            }
        }
    }

    /// Composing from an XRGB canvas must produce the same pixels as
    /// composing from the equivalent opaque RGBA image — the hosted-GIF
    /// path relies on this for siblings with different geometry. The X
    /// slot is don't-care (every writer in wally leaves it 0, the
    /// compositor ignores it), so the comparison masks it.
    #[test]
    fn xrgb_source_matches_opaque_rgba_source() {
        let img = RgbaImage::from_fn(23, 15, |x, y| {
            Rgba([
                (x * 13 % 256) as u8,
                (y * 5 % 256) as u8,
                (x * y % 256) as u8,
                255,
            ])
        });
        // RGBA -> XRGB bytes, with the X slot set to 0xAA garbage.
        let mut xrgb = Vec::with_capacity(img.as_raw().len());
        for p in img.as_raw().as_chunks::<4>().0 {
            xrgb.extend_from_slice(&[p[2], p[1], p[0], 0xAA]);
        }

        fn mask_x(buf: &mut [u8]) {
            for px in buf.as_chunks_mut::<4>().0 {
                px[3] = 0;
            }
        }

        for (mode, (pw, ph)) in [
            (Mode::Stretch, (46u32, 60u32)),
            (Mode::Fill, (23, 15)),
            (Mode::Fill, (11, 7)),
            (Mode::Fit, (40, 30)),
            (Mode::Center, (30, 30)),
            (Mode::Tile, (50, 32)),
        ] {
            let mut from_rgba = vec![0u8; pw as usize * ph as usize * 4];
            compose_into(&img, mode, pw, ph, BLACK, &mut from_rgba);
            let mut from_xrgb = vec![0u8; pw as usize * ph as usize * 4];
            compose_xrgb_into(&xrgb, (23, 15), mode, pw, ph, BLACK, &mut from_xrgb);
            mask_x(&mut from_rgba);
            mask_x(&mut from_xrgb);
            assert_eq!(from_rgba, from_xrgb, "{mode:?} {pw}x{ph}");
        }
    }

    // -------------------------------------------------------------- perf

    /// The old 0.5.x pipeline, for comparison: full-size Triangle resample
    /// (`image`, f32 intermediates) followed by a per-pixel blend loop.
    fn naive_compose(dst: &mut [u8], pw: u32, ph: u32, img: &RgbaImage) {
        let scaled = imageops::resize(img, pw, ph, FilterType::Triangle);
        let bg = [0, 0, 0];
        for (dp, sp) in dst
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(scaled.as_raw().as_chunks::<4>().0)
        {
            let a = u16::from(sp[3]);
            let over = |s: u8, b: u8| ((u16::from(s) * a + u16::from(b) * (255 - a)) / 255) as u8;
            *dp = [
                over(sp[2], bg[2]),
                over(sp[1], bg[1]),
                over(sp[0], bg[0]),
                0,
            ];
        }
    }

    fn time_ms<F: FnMut()>(mut f: F) -> f64 {
        f(); // warm-up: allocator, page faults, branch predictors
        let mut best = f64::INFINITY;
        for _ in 0..5 {
            let t = Instant::now();
            f();
            let ms = t.elapsed().as_secs_f64() * 1e3;
            if ms < best {
                best = ms;
            }
        }
        best
    }

    #[test]
    #[ignore = "perf: cargo test --release -- --ignored --nocapture perf"]
    fn perf_wallpaper_pipeline_4k() {
        let (pw, ph) = (3840u32, 2160u32);
        let src_1k = RgbaImage::from_fn(1920, 1080, |x, y| {
            Rgba([(x % 256) as u8, (y % 256) as u8, (x / 7 % 256) as u8, 255])
        });
        let src_4k = RgbaImage::from_fn(3840, 2160, |x, y| {
            Rgba([(x % 256) as u8, (y % 256) as u8, (x / 7 % 256) as u8, 255])
        });
        let mut dst = vec![0u8; pw as usize * ph as usize * 4];

        // Upscale 1080p -> 4K (typical animation source).
        let old_up = time_ms(|| {
            naive_compose(&mut dst, pw, ph, &src_1k);
            std::hint::black_box(&dst);
        });
        let new_up = time_ms(|| {
            compose_into(&src_1k, Mode::Fill, pw, ph, BLACK, &mut dst);
            std::hint::black_box(&dst);
        });

        // Already-4K source: the copy path, no resample at all.
        let old_1to1 = time_ms(|| {
            naive_compose(&mut dst, pw, ph, &src_4k);
            std::hint::black_box(&dst);
        });
        let new_1to1 = time_ms(|| {
            compose_into(&src_4k, Mode::Fill, pw, ph, BLACK, &mut dst);
            std::hint::black_box(&dst);
        });

        println!("4K fill, 1080p source:  old {old_up:8.2} | new {new_up:8.2} ms");
        println!("4K fill, 4K source:     old {old_1to1:8.2} | new {new_1to1:8.2} ms");
    }

    /// The dirty-rect fast path must be byte-identical to the full
    /// compose inside the rect, and must not touch a single pixel
    /// outside it. Covers the 1:1 copy, the Q8 bilinear upscale and the
    /// general Triangle shrink, with crop (Fill) and letterbox (Fit /
    /// Center) placements, over sources mixing opaque, translucent and
    /// transparent pixels.
    #[test]
    fn rect_compose_matches_full_and_spares_the_rest() {
        let (iw, ih) = (96u32, 64u32);
        let mut img = RgbaImage::new(iw, ih);
        let mut seed = 0x1234_5678u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            seed >> 16
        };
        for px in img.pixels_mut() {
            let a = match rnd() % 3 {
                0 => 0,
                1 => 128,
                _ => 255,
            };
            *px = Rgba([rnd() as u8, rnd() as u8, rnd() as u8, a]);
        }

        // (pw, ph, mode): 1:1, 2x upscale, 1/2 shrink, crop+shrink,
        // letterboxed upscale, 1:1 centered, letterboxed shrink.
        let cases = [
            (96, 64, Mode::Stretch),
            (192, 128, Mode::Stretch),
            (48, 32, Mode::Stretch),
            (160, 90, Mode::Fill),
            (200, 100, Mode::Fit),
            (120, 120, Mode::Center),
            (80, 80, Mode::Fit),
        ];
        let dirty = [
            Rect {
                x: 10,
                y: 5,
                w: 20,
                h: 9,
            },
            Rect {
                x: 0,
                y: 0,
                w: 96,
                h: 4,
            },
            Rect {
                x: 50,
                y: 30,
                w: 46,
                h: 34,
            },
            Rect {
                x: 47,
                y: 0,
                w: 4,
                h: 64,
            },
        ];

        for (pw, ph, mode) in cases {
            let full = render(pw, ph, mode, BLACK, &img);
            // A different wallpaper the restricted compose starts from:
            // anything it leaves alone must still show this.
            let other = solid(pw, ph, [1, 2, 3, 255]);
            let other_full = render(pw, ph, mode, BLACK, &other);
            for d in dirty {
                let Some(rect) = dirty_dest((iw, ih), mode, pw, ph, d) else {
                    continue; // full compose is the right call there
                };
                let mut dst = other_full.clone();
                compose_into_rect(&img, mode, pw, ph, BLACK, &mut dst, rect);

                let inside = |x: u32, y: u32| {
                    x >= rect.x && x < rect.x + rect.w && y >= rect.y && y < rect.y + rect.h
                };
                for y in 0..ph {
                    for x in 0..pw {
                        let i = (y as usize * pw as usize + x as usize) * 4;
                        let want = if inside(x, y) {
                            &full[i..i + 4]
                        } else {
                            &other_full[i..i + 4]
                        };
                        assert_eq!(
                            &dst[i..i + 4],
                            want,
                            "{mode:?} {pw}x{ph}, dirty {d:?}, rect {rect:?}, pixel ({x},{y})"
                        );
                    }
                }
            }
        }
    }

    /// dirty_dest's fallbacks: Tile can never restrict, a rect covering
    /// (almost) the whole image must fall back to the full path, and the
    /// returned rect is always inside the output and the image's area.
    #[test]
    fn dirty_dest_falls_back_when_restriction_does_not_pay() {
        assert_eq!(
            dirty_dest(
                (96, 64),
                Mode::Tile,
                96,
                64,
                Rect {
                    x: 0,
                    y: 0,
                    w: 5,
                    h: 5
                }
            ),
            None
        );
        // Whole image changed: full compose.
        assert_eq!(
            dirty_dest(
                (96, 64),
                Mode::Stretch,
                96,
                64,
                Rect {
                    x: 0,
                    y: 0,
                    w: 96,
                    h: 64
                }
            ),
            None
        );
        // Mapped rects always land inside the buffer.
        for r in [
            Rect {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
            },
            Rect {
                x: 90,
                y: 60,
                w: 6,
                h: 4,
            },
        ] {
            if let Some(rect) = dirty_dest((96, 64), Mode::Fit, 200, 100, r) {
                assert!(rect.x + rect.w <= 200 && rect.y + rect.h <= 100);
                // Letterboxed: the image area is (25..175, 0..100).
                assert!(rect.x >= 25 && rect.x + rect.w <= 175);
            }
        }
    }

    /// CPU savings of the restricted path on the owned-animation hot
    /// case: an 800x600 GIF (too big or too odd to host) blown up to a
    /// 1920x1080 panel with a typical 800x120 band update per frame.
    #[test]
    #[ignore = "perf probe: cargo test --release compose_rect_savings -- --ignored --nocapture"]
    fn compose_rect_savings() {
        let (iw, ih) = (800u32, 600u32);
        let mut img = RgbaImage::new(iw, ih);
        let mut seed = 0xdead_beefu32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            seed >> 16
        };
        for px in img.pixels_mut() {
            *px = Rgba([rnd() as u8, rnd() as u8, rnd() as u8, 255]);
        }

        let (pw, ph) = (1920u32, 1080u32);
        let mut dst = vec![0u8; pw as usize * ph as usize * 4];
        let dirty = Rect {
            x: 0,
            y: 240,
            w: 800,
            h: 120,
        };
        let rect = dirty_dest((iw, ih), Mode::Stretch, pw, ph, dirty).expect("restricts");

        fn time_ms(f: &mut dyn FnMut()) -> f64 {
            f(); // warm up (fills the caches)
            let n = 30;
            let t0 = Instant::now();
            for _ in 0..n {
                f();
            }
            t0.elapsed().as_secs_f64() * 1000.0 / n as f64
        }
        let full = time_ms(&mut || {
            compose_into(&img, Mode::Stretch, pw, ph, BLACK, &mut dst);
            std::hint::black_box(&dst);
        });
        let part = time_ms(&mut || {
            compose_into_rect(&img, Mode::Stretch, pw, ph, BLACK, &mut dst, rect);
            std::hint::black_box(&dst);
        });
        println!(
            "1080p frame: full {full:6.2} ms | rect {part:6.2} ms ({:.1}x) — rect {}x{}",
            full / part,
            rect.w,
            rect.h
        );
    }
}
