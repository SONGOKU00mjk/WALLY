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

use image::RgbaImage;

use crate::cli::{Color, Mode};

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
        Self { raw: img.as_raw(), w: img.width(), h: img.height(), xrgb: false }
    }

    /// `raw` is `w * h * 4` XRGB8888 bytes (B, G, R, X per pixel).
    pub fn xrgb(raw: &'a [u8], w: u32, h: u32) -> Self {
        debug_assert_eq!(raw.len(), w as usize * h as usize * 4);
        Self { raw, w, h, xrgb: true }
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
            [over(rgba[2], bg[2]), over(rgba[1], bg[1]), over(rgba[0], bg[0]), 0]
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
fn resample_into(
    src: &Src,
    from: Region,
    dst: &mut [u8],
    at: Place,
    tw: u32,
    th: u32,
    bg: Color,
) {
    let iw = src.w as usize;
    let (w, h, tw, th) = (from.w as usize, from.h as usize, tw as usize, th as usize);
    let raw = src.raw;
    let xs = axis_taps(w, tw);
    let ys = axis_taps(h, th);

    // Pass 1: resample every source row horizontally (h rows of tw px).
    let mut tmp = vec![0u8; h * tw * 4];
    for (sy, drow) in tmp.chunks_exact_mut(tw * 4).enumerate() {
        let srow =
            raw[((from.y0 as usize + sy) * iw + from.x0 as usize) * 4..][..w * 4].as_chunks::<4>().0;
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
    let mut acc = vec![0f32; tw * 4];
    for dy in 0..th {
        let d = (at.oy + dy) * at.stride * 4 + at.ox * 4;
        let drow = &mut dst[d..d + tw * 4];
        acc.fill(0.0);
        let (off, count) = ys.spans[dy];
        for &(ri, wt) in &ys.taps[off as usize..off as usize + count as usize] {
            let rrow = &tmp[ri as usize * tw * 4..][..tw * 4];
            for (a, p) in acc.as_chunks_mut::<4>().0.iter_mut().zip(rrow.as_chunks::<4>().0) {
                for (a, s) in a.iter_mut().zip(p.iter().copied()) {
                    *a += wt * f32::from(s);
                }
            }
        }
        for (dp, a) in drow.as_chunks_mut::<4>().0.iter_mut().zip(acc.as_chunks::<4>().0) {
            let mut px = [0u8; 4];
            for (p, v) in px.iter_mut().zip(a) {
                *p = (v + 0.5) as u8;
            }
            *dp = pack_src(src, &px, bg);
        }
    }
}

// --------------------------------------------------------------- stages

/// Bilinear (= Triangle) upscale of the `w x h` source region at
/// `(x0, y0)` to `tw x th`, packed straight into XRGB8888 `dst` at `at`.
///
/// Integer Q8 inner loops: for every output pixel the two source taps
/// blend as `(a * (256 - t) + b * t + 128) >> 8` per channel. Same
/// center mapping as [`resample_into`], so the parity tests pin this
/// pass to `image`'s Triangle upscale within ±1 LSB too.
fn bilinear_into(
    src: &Src,
    from: Region,
    dst: &mut [u8],
    at: Place,
    tw: u32,
    th: u32,
    bg: Color,
) {
    let iw = src.w as usize;
    let (w, h, tw, th) = (from.w as usize, from.h as usize, tw as usize, th as usize);
    let raw = src.raw;

    // Per output sample: the two source taps and the blend fraction in
    // Q8 (0..=255). Centers outside the source collapse onto the edge
    // pixel, matching the general pass's edge handling.
    let map = |src_len: usize, dst_len: usize| -> Vec<(u32, u32, u16)> {
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
                    (f as u32, (f + 1).min(last) as u32, ((s - s.floor()) * 256.0) as u16)
                }
            })
            .collect()
    };
    let xs = map(w, tw);
    let ys = map(h, th);

    let blend = |a: u8, b: u8, t: u16| -> u8 {
        ((u32::from(a) * (256 - u32::from(t)) + u32::from(b) * u32::from(t) + 128) >> 8) as u8
    };

    // Pass 1: stretch every source row horizontally (h rows of tw px).
    let mut tmp = vec![0u8; h * tw * 4];
    for (sy, drow) in tmp.chunks_exact_mut(tw * 4).enumerate() {
        let srow =
            raw[((from.y0 as usize + sy) * iw + from.x0 as usize) * 4..][..w * 4].as_chunks::<4>().0;
        for (dp, &(i0, i1, t)) in drow.as_chunks_mut::<4>().0.iter_mut().zip(&xs) {
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
        for (dp, (p0, p1)) in drow.as_chunks_mut::<4>().0.iter_mut().zip(r0.iter().zip(r1)) {
            let mut px = [0u8; 4];
            for (p, (a, b)) in px.iter_mut().zip(p0.iter().zip(p1)) {
                *p = blend(*a, *b, t);
            }
            *dp = pack_src(src, &px, bg);
        }
    }
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
        convert(dst, at, src, Region { x0: from.x0, y0: from.y0, w: tw, h: th }, bg);
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
            place(src, Region { x0, y0, w, h }, tw, th, bg, dst, Place {
                stride: pw as usize,
                ox: 0,
                oy: 0,
            });
        }
        // Fit/Center letterbox: background first, image centered.
        Mode::Fit | Mode::Center => {
            fill(dst, bg);
            place(src, Region { x0, y0, w, h }, tw, th, bg, dst, Place {
                stride: pw as usize,
                ox: ((pw - tw) / 2) as usize,
                oy: ((ph - th) / 2) as usize,
            });
        }
        // Tile: one native-size tile in scratch, then repeat it.
        Mode::Tile => {
            let mut tile = vec![0u8; iw as usize * ih as usize * 4];
            convert(
                &mut tile,
                Place { stride: iw as usize, ox: 0, oy: 0 },
                src,
                Region { x0: 0, y0: 0, w: iw, h: ih },
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
        let dst = render(1, 1, Mode::Stretch, BLACK, &solid(1, 1, [255, 255, 255, 128]));
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
                assert_eq!(px(&dst, 3, x as u32, y as u32), [7, y * 85, x * 85, 0], "at {x},{y}");
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
            Rgba([(x * 7 % 256) as u8, (y * 11 % 256) as u8, (x * y % 256) as u8, 255])
        });

        for (tw, th) in [(111u32, 69u32), (20, 12), (74, 46), (300, 23), (60, 15), (37, 8)] {
            let reference = imageops::resize(&img, tw, th, FilterType::Triangle);
            let mut theirs = vec![0u8; tw as usize * th as usize * 4];
            convert(
                &mut theirs,
                Place { stride: tw as usize, ox: 0, oy: 0 },
                &Src::rgba(&reference),
                Region { x0: 0, y0: 0, w: tw, h: th },
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
            Rgba([(x * 13 % 256) as u8, (y * 5 % 256) as u8, (x * y % 256) as u8, 255])
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
            *dp = [over(sp[2], bg[2]), over(sp[1], bg[1]), over(sp[0], bg[0]), 0];
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
}
