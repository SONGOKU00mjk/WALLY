//! Image loading and animation stepping.
//!
//! One [`Source`] per distinct (path, animate) pair. Static images are
//! decoded once, painted everywhere, then released. Animated sources step
//! exactly once per tick — so N monitors sharing a wallpaper share one
//! timeline — and keep exactly one canvas: the frame on screen. A GIF
//! whose native size matches an output's buffer hands even that canvas
//! over: its frames are disposed and blended directly in the output's
//! scanout buffer (hosted mode), leaving no private copy in this process.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, Frame, Frames, ImageFormat, RgbaImage};

use crate::cli::Color;
use crate::render;

/// How long a frame is shown. GIF delays are centiseconds and 0–1 cs means
/// "as fast as possible"; browsers clamp that to ~100 ms. WebP delays are
/// milliseconds and can be 0 in broken files. The same policy applies to
/// both, with a 20 ms floor so no stream can busy-loop the compositor.
fn normalize_delay(d: Duration) -> Duration {
    if d <= Duration::from_millis(10) {
        Duration::from_millis(100)
    } else {
        d.max(Duration::from_millis(20))
    }
}

/// `image::Delay` in wall-clock terms. `numer_denom_ms` is exact for the
/// millisecond ratios both the GIF and WebP decoders produce.
fn delay_duration(delay: image::Delay) -> Duration {
    let (num, den) = delay.numer_denom_ms();
    if den == 0 {
        Duration::from_millis(0)
    } else {
        Duration::from_millis(num as u64) / den
    }
}

/// What to draw.
pub enum Source {
    /// A still image. `decoded` is dropped after every output painted it
    /// and lazily re-decoded from disk if a repaint is ever needed again.
    Static { path: PathBuf, decoded: Option<RgbaImage> },
    /// A GIF or animated WebP running on its own clock. Boxed: the enum
    /// lives in a Vec and the animation state is the heavyweight member.
    Animated(Box<Anim>),
}

pub enum Step {
    /// New pixels are available (`serial` was bumped).
    Advanced,
    /// The stream broke repeatedly; the animation froze on its last good
    /// frame and will not be scheduled again.
    Frozen,
}

enum AnimFormat {
    Gif,
    WebP,
}

/// The per-format decode machinery behind an [`Anim`].
///
/// Both variants stream from disk; no encoded bytes are ever held in RAM.
/// The steady-state footprint of either variant is exactly one canvas —
/// the frame on screen — plus small decoder internals. With a
/// screen-sized GIF that canvas is the output's scanout buffer itself
/// (hosted mode), so nothing at all is held in process memory.
enum AnimStream {
    /// GIF decoded by the `gif` crate, composited in place onto the
    /// display canvas by our own disposal state machine (see
    /// [`Pending`]). The `image` crate's iterator would need a second
    /// full-canvas buffer for that.
    Gif(Box<GifStream>),
    /// Animated WebP via the `image` crate's iterator, which already
    /// decodes straight into a caller-sized buffer and keeps no extra
    /// canvas of our own. The frames come out full-canvas, so the buffer
    /// we hold is a disposable snapshot of what is on screen — it is
    /// dropped once every output painted it and re-pulled on demand.
    WebP(Frames<'static>),
}

pub struct Anim {
    path: PathBuf,
    stream: AnimStream,
    /// Native pixel dimensions of the animation. A canvas of exactly this
    /// size can be hosted in an output's scanout buffer.
    dims: (u32, u32),
    /// Composited frame currently on screen. For an owned GIF this is the
    /// compositing canvas each new frame blends onto; for WebP it is a
    /// disposable snapshot (the decoder keeps the true canvas); `None`
    /// while a GIF is hosted or between paints in hosted WebP mode.
    current: Option<RgbaImage>,
    /// Hosted mode: the compositing canvas lives in an output's scanout
    /// buffer and `host_bg` is the color it was filled with.
    hosted: bool,
    host_bg: Color,
    /// WebP only: the second frame pulled during the open-time peek that
    /// distinguishes animations from still images. Consumed by the first
    /// step and never refilled — no steady-state prefetch.
    pending_frame: Option<Frame>,
    delay: Duration,
    /// Consecutive failures; 3 freezes the animation.
    errors: u32,
    frozen: bool,
    /// Bumped whenever new pixels appear (in the canvas or the host
    /// buffer).
    serial: u64,
    /// When the current frame should be replaced.
    next_step: Option<Instant>,
}

/// One frame's metadata, copied out of the `gif` crate before the borrow
/// on the decoder ends.
#[derive(Clone, Copy)]
struct GifFrameInfo {
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    dispose: gif::DisposalMethod,
    /// Centiseconds, exactly as stored in the file.
    delay_cs: u16,
}

/// The disposal of the last-blended frame, applied at the start of the
/// next blend. This deferral is what lets GIFs composite with ONE canvas:
/// the current frame keeps its drawn pixels on screen, while the previous
/// frame's "restore to background/previous" only takes effect when the
/// next frame actually samples those pixels.
///
/// Semantics (mirroring the `image` crate's iterator):
/// - `Keep`/`Any`: nothing pending — canvas state simply persists.
/// - `Background`: the frame's rect is cleared to transparent before the
///   next blend; until then its drawn pixels remain visible.
/// - `Previous`: the frame's rect is restored to its pre-frame state.
///   (Consecutive `Previous` frames with overlapping rects restore to the
///   most recent pre-frame state rather than the very first one; that
///   pathological case is underspecified and every viewer differs.)
enum Pending {
    Background { left: u32, top: u32, width: u32, height: u32 },
    /// `pixels` is row-major over the *clipped* rect, `width` pixels wide.
    Previous { left: u32, top: u32, width: u32, pixels: Vec<u8> },
}

struct GifStream {
    decoder: gif::Decoder<BufReader<File>>,
    pending: Option<Pending>,
}

/// The GIF compositing canvas.
///
/// Two layouts share the disposal machinery below:
/// - **Owned**: the standalone RGBA canvas used when the animation must
///   be resampled before it reaches the screen. Transparent pixels are
///   cleared to zero and the background is applied at compose time.
/// - **Hosted**: the output's scanout buffer itself, XRGB8888, premul-
///   tiplied with the letterbox color. This is possible exactly when the
///   animation's native size equals the buffer's pixel size: every
///   canvas pixel maps onto the same screen pixel, all three disposal
///   rules are rect operations, and "clear to transparent + compose over
///   bg" becomes "fill with bg". The frame on screen IS the canvas, so
///   no second copy of the wallpaper exists anywhere.
struct Canvas<'a> {
    bytes: &'a mut [u8],
    w: u32,
    h: u32,
    /// `Some(bg)` = hosted XRGB canvas; `None` = owned RGBA canvas.
    hosted: Option<Color>,
}

impl Canvas<'_> {
    fn owned(img: &mut RgbaImage) -> Canvas<'_> {
        let (w, h) = img.dimensions();
        Canvas { bytes: &mut *img, w, h, hosted: None }
    }

    /// `bytes` is `w * h * 4` XRGB8888 bytes.
    fn hosted(bytes: &mut [u8], w: u32, h: u32, bg: Color) -> Canvas<'_> {
        debug_assert_eq!(bytes.len(), w as usize * h as usize * 4);
        Canvas { bytes, w, h, hosted: Some(bg) }
    }

    /// The previous frame's disposal takes effect now (it was deferred so
    /// its drawn pixels could stay on screen until this point).
    fn apply_pending(&mut self, pending: Pending) {
        match pending {
            Pending::Background { left, top, width, height } => {
                self.clear_rect(left, top, width, height)
            }
            Pending::Previous { left, top, width, pixels } => {
                self.restore_rect(left, top, width, &pixels)
            }
        }
    }

    /// `Background` disposal: clear the rect to transparent (owned) or
    /// to the background color (hosted) — both show the letterbox color.
    fn clear_rect(&mut self, left: u32, top: u32, width: u32, height: u32) {
        let (cw, ch) = (self.w as usize, self.h as usize);
        let (left, top) = (left as usize, top as usize);
        let width = (width as usize).min(cw.saturating_sub(left));
        let height = (height as usize).min(ch.saturating_sub(top));
        let px: [u8; 4] = match self.hosted {
            None => [0, 0, 0, 0],
            Some([r, g, b]) => [b, g, r, 0],
        };
        for y in top..top + height {
            let row = (y * cw + left) * 4;
            for p in self.bytes[row..row + width * 4].as_chunks_mut::<4>().0 {
                *p = px;
            }
        }
    }

    /// Copy a rect's pixels out of the canvas (pre-blend state for the
    /// `Previous` disposal). Both layouts are plain 4-byte pixels, so
    /// this is row memcpy for either.
    fn save_rect(&self, left: u32, top: u32, width: u32, height: u32) -> Vec<u8> {
        let width = width.min(self.w.saturating_sub(left));
        let height = height.min(self.h.saturating_sub(top));
        if width == 0 || height == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        let stride = self.w as usize;
        for y in top..top + height {
            let row = ((y as usize * stride) + left as usize) * 4;
            out.extend_from_slice(&self.bytes[row..row + (width * 4) as usize]);
        }
        out
    }

    /// Restore a rect saved by [`Canvas::save_rect`].
    fn restore_rect(&mut self, left: u32, top: u32, width: u32, pixels: &[u8]) {
        if width == 0 {
            return;
        }
        let stride = self.w as usize;
        let mut src = pixels;
        for y in top as usize..self.h as usize {
            if src.is_empty() {
                break;
            }
            let row_len = (width as usize * 4).min(src.len());
            let (row, rest) = src.split_at(row_len);
            let d = (y * stride + left as usize) * 4;
            let n = row.len().min(self.bytes.len().saturating_sub(d));
            self.bytes[d..d + n].copy_from_slice(&row[..n]);
            src = rest;
        }
    }

    /// Port of the `image` crate's GIF blend rule: opaque frame pixels
    /// replace the canvas, transparent ones let the canvas (the
    /// "previous" state) show through. GIF transparency is binary, so
    /// this is a plain replace.
    fn blend_frame(&mut self, info: &GifFrameInfo, frame: &[u8]) {
        let (cw, ch) = (self.w as usize, self.h as usize);
        let (fw, fh) = (info.width as usize, info.height as usize);
        if fw == 0 || fh == 0 {
            return;
        }
        let hosted = self.hosted.is_some();
        let bytes: &mut [u8] = &mut *self.bytes;
        for row in 0..fh {
            let y = info.top as usize + row;
            if y >= ch {
                break; // frame extends below the canvas; rest of the rows too
            }
            for col in 0..fw {
                let x = info.left as usize + col;
                if x >= cw {
                    break;
                }
                let s = (row * fw + col) * 4;
                if frame[s + 3] != 0 {
                    let d = (y * cw + x) * 4;
                    if hosted {
                        // RGBA frame -> XRGB canvas.
                        bytes[d..d + 4].copy_from_slice(&[frame[s + 2], frame[s + 1], frame[s], 0]);
                    } else {
                        bytes[d..d + 4].copy_from_slice(&frame[s..s + 4]);
                    }
                }
            }
        }
    }
}

impl Source {
    /// Open a file as a wallpaper source, sniffing the format first so
    /// AVIF/JXL can be routed to their dedicated decoders and GIF/WebP to
    /// the animation path.
    pub fn open(path: &Path, animate: bool) -> Result<Source> {
        match detect(path)? {
            Detected::Jxl => {
                #[cfg(feature = "jxl")]
                {
                    let img = decode_jxl(path)?;
                    Ok(Source::Static { path: path.to_owned(), decoded: Some(img) })
                }
                #[cfg(not(feature = "jxl"))]
                {
                    bail!(
                        "`{}` is a JPEG XL file, but this build has no JXL support \
                         (rebuild with `--features jxl`)",
                        path.display()
                    );
                }
            }
            Detected::Avif => {
                #[cfg(feature = "avif")]
                {
                    let img = decode_avif(path)?;
                    Ok(Source::Static { path: path.to_owned(), decoded: Some(img) })
                }
                #[cfg(not(feature = "avif"))]
                {
                    bail!(
                        "`{}` looks like an AVIF/HEIF container, but this build has no AVIF \
                         support (rebuild with `--features avif`)",
                        path.display()
                    );
                }
            }
            Detected::Common(format) => match (format, animate) {
                (ImageFormat::Gif, true) => anim_or_static(path, AnimFormat::Gif),
                (ImageFormat::WebP, true) => anim_or_static(path, AnimFormat::WebP),
                _ => {
                    let img = decode_any(path)?;
                    Ok(Source::Static { path: path.to_owned(), decoded: Some(img) })
                }
            },
        }
    }

    #[cfg(test)]
    pub fn is_animated(&self) -> bool {
        matches!(self, Source::Animated(_))
    }

    /// Monotonic counter bumped every time new pixels appear.
    pub fn serial(&self) -> u64 {
        match self {
            Source::Animated(a) => a.serial,
            _ => 1,
        }
    }

    /// When an animated source wants its next step, if it is still alive.
    pub fn next_step(&self) -> Option<Instant> {
        match self {
            Source::Animated(a) => a.next_step,
            _ => None,
        }
    }

    /// True after repeated decode failures froze an animation. Frozen is
    /// also observable as `next_step() == None` on an animated source.
    #[cfg(test)]
    pub fn is_frozen(&self) -> bool {
        matches!(self, Source::Animated(a) if a.frozen)
    }

    /// Advance an animated source by one frame (no-op for static/color).
    pub fn step(&mut self) -> Step {
        let anim = match self {
            Source::Animated(a) if !a.frozen => a,
            _ => return Step::Advanced,
        };
        if anim.hosted && anim.current.is_none() {
            // Hosted GIFs are stepped through `step_hosted`, which blends
            // into the output buffer; there is no in-process canvas here.
            return Step::Advanced;
        }

        // Show one frame, decoded on demand; end-of-stream reopens the
        // file, so the animation loops until it is changed. On repeated
        // failures it freezes on its last good frame.
        let shown = anim.advance();
        if shown {
            anim.schedule();
        }
        // When nothing was shown (broken reopen), next_step stays in the
        // past so the retry happens on the very next loop iteration — the
        // same behavior the prefetch-based code had.
        if anim.frozen {
            Step::Frozen
        } else {
            Step::Advanced
        }
    }

    /// Pixels to paint right now, decoding a released static from disk or
    /// re-pulling a dropped WebP snapshot.
    pub fn pixels(&mut self) -> Result<&RgbaImage> {
        match self {
            Source::Static { path, decoded } => {
                if decoded.is_none() {
                    *decoded = Some(decode_any(path)?);
                }
                Ok(decoded.as_ref().expect("decoded above"))
            }
            Source::Animated(a) => {
                if a.current.is_none() {
                    if matches!(a.stream, AnimStream::Gif(_)) {
                        // A hosted GIF's canvas lives in the output's
                        // buffer; painting must go through the host
                        // path, never through here.
                        bail!("hosted GIF canvas is not addressable from pixels()");
                    }
                    if a.frozen {
                        bail!("animation is frozen on its last good frame");
                    }
                    // Hosted WebP between paints: pull the next frame.
                    if !a.advance() {
                        bail!("could not decode the next animation frame");
                    }
                    a.schedule();
                }
                Ok(a.current.as_ref().expect("shown above"))
            }
        }
    }

    /// Free memory that only serves a repaint of the *current* frame.
    /// Called when every surface has painted it: statics re-decode from
    /// disk, animated WebPs re-pull the next frame on demand. An owned
    /// GIF canvas is compositing *state*, never freed; a hosted GIF has
    /// nothing in process memory to begin with.
    pub fn idle_release(&mut self) {
        match self {
            Source::Static { decoded, .. } => {
                *decoded = None;
            }
            Source::Animated(a) => {
                if matches!(a.stream, AnimStream::WebP(_)) {
                    a.current = None;
                }
            }
        }
    }

    /// Native pixel size of an animation.
    pub fn anim_dims(&self) -> Option<(u32, u32)> {
        match self {
            Source::Animated(a) if !a.frozen => Some(a.dims),
            _ => None,
        }
    }

    /// A GIF of this exact size can host its canvas in an output's
    /// scanout buffer (zero private canvas). `None` for anything else.
    pub fn host_dims(&self) -> Option<(u32, u32)> {
        match self {
            Source::Animated(a)
                if !a.frozen && !a.hosted && matches!(a.stream, AnimStream::Gif(_)) =>
            {
                Some(a.dims)
            }
            _ => None,
        }
    }

    /// True while the compositing canvas lives in an output's buffer.
    pub fn is_hosted(&self) -> bool {
        matches!(self, Source::Animated(a) if a.hosted)
    }

    /// Hand the canvas to `dst` (`w * h * 4` XRGB8888 bytes): fill it
    /// with `bg`, restart the stream and show frame 1 in place. The
    /// visible pixels are the frame already on screen, so the serial
    /// stays — nothing is repainted for the handover itself. Returns
    /// false when the source cannot host (not a GIF, frozen, or the
    /// buffer does not match); the animation continues the ordinary way.
    pub fn begin_host(&mut self, bg: Color, dst: &mut [u8]) -> bool {
        let (path, dims, is_gif) = match self {
            Source::Animated(a) if !a.frozen && !a.hosted => (
                a.path.clone(),
                a.dims,
                matches!(a.stream, AnimStream::Gif(_)),
            ),
            _ => return false,
        };
        if !is_gif {
            return false; // only GIFs host; WebP canvases live in the decoder
        }
        if dst.len() != dims.0 as usize * dims.1 as usize * 4 {
            return false;
        }
        match open_gif_hosted(&path, dst, dims.0, dims.1, bg) {
            Ok((gs, delay_cs)) => {
                if let Source::Animated(a) = self {
                    a.stream = AnimStream::Gif(gs);
                    a.current = None;
                    a.hosted = true;
                    a.host_bg = bg;
                    a.delay = normalize_delay(gif_delay(delay_cs));
                    a.schedule();
                }
                true
            }
            // Not an error worth counting: hosting is an optimization;
            // the animation continues the ordinary way.
            Err(e) => {
                eprintln!(
                    "wally: not hosting `{}` in the output buffer: {e:#}",
                    path.display()
                );
                false
            }
        }
    }

    /// Blend the next GIF frame straight into the host buffer.
    pub fn step_hosted(&mut self, dst: &mut [u8]) -> Step {
        let anim = match self {
            Source::Animated(a) if !a.frozen => a,
            _ => return Step::Advanced,
        };
        if !anim.hosted {
            // Defensive: fall back to the ordinary path.
            return self.step();
        }
        anim.step_hosted(dst);
        if anim.frozen {
            Step::Frozen
        } else {
            Step::Advanced
        }
    }

    /// Take the canvas back into process memory. Restarts the GIF: the
    /// on-screen canvas bytes lived only in the buffer being given up.
    /// Happens when the hosting output changed geometry or went away.
    pub fn ensure_owned(&mut self) {
        if let Source::Animated(a) = self {
            if a.hosted {
                a.reopen_gif_any(None);
            }
        }
    }
}

impl Anim {
    /// Show one frame. Frames are decoded on demand at each step —
    /// nothing is prefetched, so between ticks only the on-screen canvas
    /// is resident. At the end of the stream (or on a decode error) the
    /// file starts over from the top — this is what makes GIFs and
    /// animated WebPs loop forever. Returns true when a frame ended up on
    /// screen (including the first frame of a restarted stream); when
    /// false and the source is not frozen, `next_step` stays in the past
    /// so the retry happens on the very next tick.
    fn advance(&mut self) -> bool {
        match &mut self.stream {
            AnimStream::Gif(gs) => {
                let Some(canvas) = self.current.as_mut() else {
                    // Hosted GIFs step through `step_hosted`; a stale
                    // caller must not spin here.
                    return false;
                };
                match pull_gif(gs, &mut Canvas::owned(canvas)) {
                    Ok(Some(info)) => {
                        self.delay = normalize_delay(gif_delay(info.delay_cs));
                        self.serial += 1;
                        self.errors = 0;
                        true
                    }
                    Ok(None) | Err(_) => self.reopen_gif_any(None),
                }
            }
            AnimStream::WebP(frames) => {
                let frame = match self.pending_frame.take() {
                    Some(f) => Some(f), // the open-time peek, shown exactly once
                    None => match pull(frames) {
                        Ok(Some(f)) => Some(f),
                        Ok(None) | Err(_) => None,
                    },
                };
                match frame {
                    Some(f) => {
                        self.show_webp(f);
                        true
                    }
                    None => self.reopen_webp(),
                }
            }
        }
    }

    /// Reopen the GIF from disk and show its first frame — into the host
    /// buffer when `dst` is given and the file still has the expected
    /// size, into a fresh owned canvas otherwise (which also adapts to a
    /// file that changed size on disk). On failure count the error
    /// (3 strikes freeze the animation).
    fn reopen_gif_any(&mut self, dst: Option<&mut [u8]>) -> bool {
        let mut gs = match make_gif(&self.path) {
            Ok(gs) => gs,
            Err(e) => {
                self.bump_error("GIF", e);
                return false;
            }
        };
        let (w, h) = (u32::from(gs.decoder.width()), u32::from(gs.decoder.height()));
        if w == 0 || h == 0 {
            self.bump_error(
                "GIF",
                anyhow::anyhow!("GIF `{}` has a zero-sized logical screen", self.path.display()),
            );
            return false;
        }

        let stay_hosted = match &dst {
            Some(buf) => (w, h) == self.dims && buf.len() == w as usize * h as usize * 4,
            None => false,
        };
        if stay_hosted {
            let buf = dst.expect("checked above");
            let bg = self.host_bg;
            render::fill(buf, bg);
            match pull_gif(&mut gs, &mut Canvas::hosted(buf, w, h, bg)) {
                Ok(Some(info)) => {
                    self.stream = AnimStream::Gif(gs);
                    self.delay = normalize_delay(gif_delay(info.delay_cs));
                    self.serial += 1;
                    self.errors = 0;
                    self.schedule();
                    true
                }
                Ok(None) | Err(_) => {
                    // Corrupt mid-file: the canvas was wiped; retry from
                    // the top of the file, in the host buffer.
                    self.bump_error("GIF", anyhow::anyhow!("frame decode failed"));
                    false
                }
            }
        } else {
            // Size changed on disk (or no host buffer): adapt, own, and
            // let the host scan re-evaluate on the next tick.
            self.dims = (w, h);
            let mut canvas = RgbaImage::new(w, h);
            match pull_gif(&mut gs, &mut Canvas::owned(&mut canvas)) {
                Ok(Some(info)) => {
                    self.stream = AnimStream::Gif(gs);
                    self.current = Some(canvas);
                    self.hosted = false;
                    self.delay = normalize_delay(gif_delay(info.delay_cs));
                    self.serial += 1;
                    self.errors = 0;
                    self.schedule();
                    true
                }
                Ok(None) | Err(_) => {
                    self.bump_error("GIF", anyhow::anyhow!("frame decode failed"));
                    false
                }
            }
        }
    }

    /// Same for animated WebP: fresh stream, show the first frame.
    fn reopen_webp(&mut self) -> bool {
        match make_frames(&self.path) {
            Ok(mut frames) => match pull(&mut frames) {
                Ok(Some(frame)) => {
                    self.stream = AnimStream::WebP(frames);
                    self.show_webp(frame);
                    true
                }
                Ok(None) => {
                    self.bump_error("WebP", anyhow::anyhow!("animation has no frames"));
                    false
                }
                Err(e) => {
                    self.bump_error("WebP", e);
                    false
                }
            },
            Err(e) => {
                self.bump_error("WebP", e);
                false
            }
        }
    }

    fn show_webp(&mut self, frame: Frame) {
        let (left, top, delay) = (frame.left(), frame.top(), frame.delay());
        let buffer = frame.into_buffer();
        if left == 0 && top == 0 && buffer.dimensions() == self.dims {
            self.current = Some(buffer); // the common case: a full-canvas frame
        } else if let Some(canvas) = self.current.as_mut() {
            // Sub-rect frame: blend onto the snapshot. image-webp emits
            // full-canvas frames (verified 0.2.4), so this is defensive.
            image::imageops::overlay(canvas, &buffer, left as i64, top as i64);
        } else {
            // Nothing to blend onto (snapshot was dropped): show as-is.
            self.current = Some(buffer);
        }
        self.delay = normalize_delay(delay_duration(delay));
        self.serial += 1;
        self.errors = 0;
    }

    /// Hosted step: pull the next frame and blend it straight into the
    /// output's buffer. End of stream (or a decode error) refills the
    /// buffer with the background and restarts the file — same loop
    /// semantics as the owned path, minus the private canvas.
    fn step_hosted(&mut self, dst: &mut [u8]) {
        let (w, h) = self.dims;
        if dst.len() != w as usize * h as usize * 4 {
            // The host surface changed size underneath us; take the
            // canvas back (restarts the GIF) and continue owned.
            self.reopen_gif_any(None);
            return;
        }
        let AnimStream::Gif(gs) = &mut self.stream else {
            return; // not a GIF; nothing to blend
        };
        match pull_gif(gs, &mut Canvas::hosted(dst, w, h, self.host_bg)) {
            Ok(Some(info)) => {
                self.delay = normalize_delay(gif_delay(info.delay_cs));
                self.serial += 1;
                self.errors = 0;
                self.schedule();
            }
            Ok(None) | Err(_) => {
                self.reopen_gif_any(Some(dst));
            }
        }
    }

    fn schedule(&mut self) {
        if !self.frozen {
            self.next_step = Some(Instant::now() + self.delay);
        }
    }

    fn bump_error(&mut self, what: &str, e: anyhow::Error) {
        self.errors += 1;
        eprintln!("wally: {what} error ({}/3): {e:#}", self.errors);
        if self.errors >= 3 {
            self.frozen = true;
            self.next_step = None;
            eprintln!("wally: {what} froze on its last good frame");
        }
    }
}

fn open_anim(path: &Path, format: AnimFormat) -> Result<Source> {
    match format {
        AnimFormat::Gif => open_gif(path),
        AnimFormat::WebP => open_webp(path),
    }
}

/// Open an animated GIF: blend the first frame onto a fresh canvas, then
/// peek (without decoding pixels) whether a second frame exists.
fn open_gif(path: &Path) -> Result<Source> {
    let (gs, canvas, first_delay_cs) = open_gif_stream(path)?;
    let first_delay = normalize_delay(gif_delay(first_delay_cs));

    match peek_gif_second_frame(path) {
        Some(true) => {
            let (iw, ih) = canvas.dimensions();
            Ok(Source::Animated(Box::new(Anim {
                path: path.to_owned(),
                stream: AnimStream::Gif(gs),
                dims: (iw, ih),
                current: Some(canvas),
                hosted: false,
                host_bg: [0, 0, 0],
                pending_frame: None,
                delay: first_delay,
                errors: 0,
                frozen: false,
                serial: 1,
                next_step: Some(Instant::now() + first_delay),
            })))
        }
        // Single-frame GIF: a still wallpaper in an animation container.
        Some(false) => Ok(Source::Static { path: path.to_owned(), decoded: Some(canvas) }),
        None => {
            eprintln!(
                "wally: second frame of `{}` failed to decode; showing only the first",
                path.display()
            );
            Ok(Source::Static { path: path.to_owned(), decoded: Some(canvas) })
        }
    }
}

/// Open the GIF decoder, blend the first frame onto a fresh (transparent)
/// canvas and return the stream, the canvas and the first frame's delay.
fn open_gif_stream(path: &Path) -> Result<(Box<GifStream>, RgbaImage, u16)> {
    let mut gs = make_gif(path)?;
    let (w, h) = (u32::from(gs.decoder.width()), u32::from(gs.decoder.height()));
    if w == 0 || h == 0 {
        bail!("GIF `{}` has a zero-sized logical screen", path.display());
    }
    let mut canvas = RgbaImage::new(w, h);
    let Some(info) = pull_gif(&mut gs, &mut Canvas::owned(&mut canvas))? else {
        bail!("file contains no frames");
    };
    Ok((gs, canvas, info.delay_cs))
}

/// Open the GIF decoder and blend its first frame straight into `dst`
/// (an XRGB8888 buffer of exactly `w * h * 4` bytes, prefilled with the
/// background). Same as [`open_gif_stream`], minus the private canvas.
fn open_gif_hosted(
    path: &Path,
    dst: &mut [u8],
    w: u32,
    h: u32,
    bg: Color,
) -> Result<(Box<GifStream>, u16)> {
    let mut gs = make_gif(path)?;
    let (dw, dh) = (u32::from(gs.decoder.width()), u32::from(gs.decoder.height()));
    if (dw, dh) != (w, h) {
        bail!("GIF `{}` changed its logical size on disk", path.display());
    }
    render::fill(dst, bg);
    let Some(info) = pull_gif(&mut gs, &mut Canvas::hosted(dst, w, h, bg))? else {
        bail!("file contains no frames");
    };
    Ok((gs, info.delay_cs))
}

/// Does the GIF have a second frame? Checked with a throwaway decoder so
/// the real stream stays positioned right after frame 1, undecoded.
/// `None` = the check itself failed (truncated/corrupt file).
fn peek_gif_second_frame(path: &Path) -> Option<bool> {
    let mut gs = make_gif(path).ok()?;
    match gs.decoder.read_next_frame() {
        Ok(Some(_)) => {}
        _ => return None,
    }
    match gs.decoder.next_frame_info() {
        Ok(Some(_)) => Some(true),
        Ok(None) => Some(false),
        Err(_) => None,
    }
}

fn make_gif(path: &Path) -> Result<Box<GifStream>> {
    let file = File::open(path).with_context(|| format!("could not open `{}`", path.display()))?;
    let mut options = gif::DecodeOptions::new();
    // Identical to what the image crate's GifDecoder sets: RGBA output,
    // everything else default (including the 50 MB decode memory limit).
    options.set_color_output(gif::ColorOutput::RGBA);
    let decoder = options
        .read_info(BufReader::new(file))
        .with_context(|| format!("not a valid GIF: `{}`", path.display()))?;
    Ok(Box::new(GifStream { decoder, pending: None }))
}

/// Pull, composite and show the next GIF frame in place. Returns the
/// frame's metadata, or `None` at end of stream. This is the whole GIF
/// animation engine: decode the sub-rect, apply the previous frame's
/// deferred disposal, blend, then defer this frame's disposal.
fn pull_gif(gs: &mut GifStream, canvas: &mut Canvas) -> Result<Option<GifFrameInfo>> {
    let Some(frame) = gs
        .decoder
        .read_next_frame()
        .map_err(|e| anyhow::Error::new(e).context("frame decode failed"))?
    else {
        return Ok(None);
    };
    // Copy the metadata out before the borrow on the decoder ends.
    let info = GifFrameInfo {
        left: u32::from(frame.left),
        top: u32::from(frame.top),
        width: u32::from(frame.width),
        height: u32::from(frame.height),
        dispose: frame.dispose,
        delay_cs: frame.delay,
    };

    // The previous frame's disposal takes effect now (it was deferred so
    // its drawn pixels could stay on screen until this point).
    if let Some(pending) = gs.pending.take() {
        canvas.apply_pending(pending);
    }

    // Snapshot the pre-frame state first: a `Previous` disposal restores
    // exactly this.
    gs.pending = match info.dispose {
        gif::DisposalMethod::Background => Some(Pending::Background {
            left: info.left,
            top: info.top,
            width: info.width,
            height: info.height,
        }),
        gif::DisposalMethod::Previous => Some(Pending::Previous {
            left: info.left,
            top: info.top,
            width: info.width,
            pixels: canvas.save_rect(info.left, info.top, info.width, info.height),
        }),
        gif::DisposalMethod::Any | gif::DisposalMethod::Keep => None,
    };

    canvas.blend_frame(&info, &frame.buffer);
    Ok(Some(info))
}

/// `gif` frame delays are centiseconds; `image` maps them to
/// `delay * 10 ms / 1`, so feed the policy pipeline the same value.
fn gif_delay(delay_cs: u16) -> Duration {
    Duration::from_millis(u64::from(delay_cs) * 10)
}

fn open_webp(path: &Path) -> Result<Source> {
    let mut frames = make_frames(path)?;
    let first = pull(&mut frames)?.context("file contains no frames")?;
    let first_delay = normalize_delay(delay_duration(first.delay()));
    let first_image = first.buffer().clone();

    // Peek one frame ahead: a "still" image inside an animation container
    // (single-frame WebP has no ANMF chunks at all) is just a static
    // wallpaper. The peeked frame becomes the first step's payload.
    let second = match pull(&mut frames) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("wally: second frame of `{}` failed to decode ({e:#}); showing only the first", path.display());
            None
        }
    };

    match second {
        None => Ok(Source::Static { path: path.to_owned(), decoded: Some(first_image) }),
        Some(second) => {
            let (iw, ih) = first_image.dimensions();
            Ok(Source::Animated(Box::new(Anim {
                path: path.to_owned(),
                stream: AnimStream::WebP(frames),
                dims: (iw, ih),
                current: Some(first_image),
                hosted: false,
                host_bg: [0, 0, 0],
                pending_frame: Some(second),
                delay: first_delay,
                errors: 0,
                frozen: false,
                serial: 1,
                next_step: Some(Instant::now() + first_delay),
            })))
        }
    }
}

fn make_frames(path: &Path) -> Result<Frames<'static>> {
    let file =
        File::open(path).with_context(|| format!("could not open `{}`", path.display()))?;
    let reader = BufReader::new(file);
    Ok(WebPDecoder::new(reader)
        .with_context(|| format!("not a valid WebP file: `{}`", path.display()))?
        .into_frames())
}

/// Try the animation path first; still images inside animation containers
/// (single-frame WebP has no ANMF chunks at all) and plain stills fall
/// back to the static decoder. On total failure the original animation
/// error is reported, since it is the more specific one.
fn anim_or_static(path: &Path, format: AnimFormat) -> Result<Source> {
    match open_anim(path, format) {
        Ok(src) => Ok(src),
        Err(anim_err) => match decode_any(path) {
            Ok(img) => Ok(Source::Static { path: path.to_owned(), decoded: Some(img) }),
            Err(_) => Err(anim_err),
        },
    }
}

/// Pull the next frame, mapping decoder errors to a readable message.
fn pull(frames: &mut Frames) -> Result<Option<Frame>> {
    match frames.next() {
        None => Ok(None),
        Some(Ok(frame)) => Ok(Some(frame)),
        Some(Err(e)) => Err(anyhow::Error::new(e).context("frame decode failed")),
    }
}

/// Plain decode through the image crate, for every non-animated route.
fn decode_any(path: &Path) -> Result<RgbaImage> {
    let img = image::ImageReader::open(path)
        .with_context(|| format!("could not open `{}`", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("could not sniff the format of `{}`", path.display()))?
        .decode()
        .with_context(|| format!("could not decode `{}`", path.display()))?;
    Ok(img.into_rgba8())
}

enum Detected {
    /// Decodable by the image crate (includes GIF and WebP).
    Common(ImageFormat),
    Jxl,
    Avif,
}

/// Sniff enough bytes to route special formats; everything else goes to
/// the image crate's own format detection.
fn detect(path: &Path) -> Result<Detected> {
    let mut head = [0u8; 12];
    let n = {
        use std::io::Read;
        let mut file =
            File::open(path).with_context(|| format!("could not open `{}`", path.display()))?;
        file.read(&mut head).unwrap_or(0)
    };

    if n >= 2 && head[0] == 0xFF && head[1] == 0x0A {
        return Ok(Detected::Jxl);
    }
    if n >= 12 && &head[4..8] == b"JXL " {
        return Ok(Detected::Jxl);
    }
    if n >= 12 && &head[4..8] == b"ftyp" {
        // ISO-BMFF brand box; `mif1`/`miaf` are generic HEIF brands an
        // AVIF may legitimately use.
        if matches!(&head[8..12], b"avif" | b"avis" | b"av01" | b"mif1" | b"miaf") {
            return Ok(Detected::Avif);
        }
    }

    let format = image::ImageReader::open(path)
        .ok()
        .and_then(|r| r.with_guessed_format().ok())
        .and_then(|r| r.format());
    match format {
        Some(f) => Ok(Detected::Common(f)),
        None => Ok(Detected::Common(ImageFormat::Png)), // decode_any reports the real error
    }
}

#[cfg(feature = "jxl")]
fn decode_jxl(path: &Path) -> Result<RgbaImage> {
    use jxl_oxide::JxlImage;

    let file =
        File::open(path).with_context(|| format!("could not open `{}`", path.display()))?;
    let jxl = JxlImage::builder()
        .read(file)
        .map_err(|e| anyhow::anyhow!("not a valid JPEG XL file `{}`: {e}", path.display()))?;
    let render = jxl
        .render_frame(0)
        .map_err(|e| anyhow::anyhow!("could not decode JPEG XL `{}`: {e}", path.display()))?;

    let fb = render.image_all_channels();
    let (w, h, ch) = (fb.width(), fb.height(), fb.channels());
    let data = fb.buf();
    if w == 0 || h == 0 || ch == 0 || data.len() < w * h * ch {
        bail!(
            "JPEG XL `{}`: unexpected frame buffer ({}x{}, {ch} channels, {} samples)",
            path.display(),
            w,
            h,
            data.len()
        );
    }

    let f32_to_u8 = |v: f32| -> u8 { (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8 };
    let mut out = RgbaImage::new(w as u32, h as u32);
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * ch;
            let (r, g, b, a) = match ch {
                1 => (data[i], data[i], data[i], 1.0),
                2 => (data[i], data[i], data[i], data[i + 1]),
                3 => (data[i], data[i + 1], data[i + 2], 1.0),
                _ => (data[i], data[i + 1], data[i + 2], data[i + 3]),
            };
            out.put_pixel(
                x as u32,
                y as u32,
                image::Rgba([
                    f32_to_u8(r),
                    f32_to_u8(g),
                    f32_to_u8(b),
                    f32_to_u8(a),
                ]),
            );
        }
    }
    Ok(out)
}

#[cfg(feature = "avif")]
fn decode_avif(path: &Path) -> Result<RgbaImage> {
    use avif_decode::{Decoder, Image};

    let file =
        File::open(path).with_context(|| format!("could not open `{}`", path.display()))?;
    let decoder = Decoder::from_reader(&mut BufReader::new(file))
        .map_err(|e| anyhow::anyhow!("not a valid AVIF file `{}`: {e}", path.display()))?;
    let img = decoder
        .to_image()
        .map_err(|e| anyhow::anyhow!("could not decode AVIF `{}`: {e}", path.display()))?;

    // 16-bit samples are commonly left-aligned in the container; >> 8 keeps
    // the significant bits either way.
    let h16 = |v: u16| -> u8 { (v >> 8) as u8 };
    fn from_samples<P: Copy>(
        w: usize,
        h: usize,
        src: &[P],
        px: impl Fn(&P) -> [u8; 4],
    ) -> RgbaImage {
        let mut data = Vec::with_capacity(w * h * 4);
        for p in src {
            data.extend_from_slice(&px(p));
        }
        RgbaImage::from_raw(w as u32, h as u32, data).expect("RGBA buffer size matches dims")
    }

    Ok(match img {
        Image::Rgb8(v) => from_samples(v.width(), v.height(), v.buf(), |p| [p.r, p.g, p.b, 255]),
        Image::Rgba8(v) => {
            from_samples(v.width(), v.height(), v.buf(), |p| [p.r, p.g, p.b, p.a])
        }
        Image::Gray8(v) => from_samples(v.width(), v.height(), v.buf(), |p| {
            let y = p.value();
            [y, y, y, 255]
        }),
        Image::Rgb16(v) => from_samples(v.width(), v.height(), v.buf(), |p| {
            [h16(p.r), h16(p.g), h16(p.b), 255]
        }),
        Image::Rgba16(v) => from_samples(v.width(), v.height(), v.buf(), |p| {
            [h16(p.r), h16(p.g), h16(p.b), h16(p.a)]
        }),
        Image::Gray16(v) => from_samples(v.width(), v.height(), v.buf(), |p| {
            let y = h16(p.value());
            [y, y, y, 255]
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    /// The owned canvas / last shown frame of an animated source.
    fn shown(src: &Source) -> &RgbaImage {
        match src {
            Source::Animated(a) => a.current.as_ref().expect("frame on hand"),
            _ => panic!("must be animated"),
        }
    }

    #[test]
    fn delay_policy() {
        let d = Duration::from_millis;
        assert_eq!(normalize_delay(d(0)), d(100));
        assert_eq!(normalize_delay(d(10)), d(100)); // 1 cs, browser rule
        assert_eq!(normalize_delay(d(15)), d(20)); // floor
        assert_eq!(normalize_delay(d(100)), d(100)); // honored as-is
    }

    #[test]
    fn animated_gif_steps_and_loops() {
        let mut src = Source::open(&fixture("anim.gif"), true).unwrap();
        assert!(src.is_animated());
        let frames_total = 4usize; // fixture has 4 frames

        // Step through every frame; the moving square must change pixels.
        let mut seen_colors = Vec::new();
        for _ in 0..frames_total {
            src.step();
            let px = shown(&src).get_pixel(4, 4);
            seen_colors.push([px[0], px[1], px[2]]);
        }
        // The square hops, so the corner pixel goes white -> color -> ...
        assert!(seen_colors.windows(2).any(|w| w[0] != w[1]));

        // Keep stepping: EOF must loop, not error or freeze.
        for _ in 0..frames_total * 3 {
            assert!(matches!(src.step(), Step::Advanced));
        }
        assert!(!src.is_frozen());
    }

    #[test]
    fn gif_sequence_repeats_after_eof() {
        // Regression: the stream used to restart into a one-frame queue
        // while the old iterator stayed exhausted, so the file played
        // once and then sat on its first frame forever.
        let frames_total = 4usize;
        let mut src = Source::open(&fixture("anim.gif"), true).unwrap();

        let color_at = |src: &Source| {
            let px = shown(src).get_pixel(4, 4);
            [px[0], px[1], px[2]]
        };

        // Record one full cycle, then require the next two to repeat it.
        let mut cycle = Vec::new();
        for _ in 0..frames_total {
            src.step();
            cycle.push(color_at(&src));
        }
        assert!(cycle.windows(2).any(|w| w[0] != w[1]), "fixture must animate");
        for _rep in 0..2 {
            for (f, expected) in cycle.iter().enumerate() {
                src.step();
                assert_eq!(color_at(&src), *expected, "loop diverged at frame {f}");
            }
        }
    }

    #[test]
    fn webp_sequence_repeats_after_eof() {
        // 40 steps is far past the fixture's end: the stream must wrap
        // repeatedly without errors, panics, or freezing.
        let mut src = Source::open(&fixture("anim.webp"), true).unwrap();
        for _ in 0..40 {
            assert!(matches!(src.step(), Step::Advanced));
        }
        assert!(!src.is_frozen());
    }

    #[test]
    fn gif_zero_delay_is_clamped() {
        // Frame 2 of the fixture has a 0 ms duration.
        let mut src = Source::open(&fixture("anim.gif"), true).unwrap();
        src.step();
        let Source::Animated(a) = &src else { panic!() };
        assert_eq!(a.delay, Duration::from_millis(100));
    }
    #[test]
    fn still_gif_is_static() {
        let src = Source::open(&fixture("still.gif"), true).unwrap();
        assert!(matches!(src, Source::Static { .. }));
        assert!(!src.is_animated());
    }

    #[test]
    fn animated_webp_steps() {
        let mut src = Source::open(&fixture("anim.webp"), true).unwrap();
        assert!(src.is_animated());
        for _ in 0..12 {
            assert!(matches!(src.step(), Step::Advanced));
        }
        assert!(!src.is_frozen());
    }

    #[test]
    fn animated_lossy_webp_steps() {
        let mut src = Source::open(&fixture("anim-lossy.webp"), true).unwrap();
        assert!(src.is_animated());
        for _ in 0..12 {
            assert!(matches!(src.step(), Step::Advanced));
        }
    }

    #[test]
    fn still_webp_is_static() {
        let src = Source::open(&fixture("still.webp"), true).unwrap();
        assert!(matches!(src, Source::Static { .. }));
    }

    #[test]
    fn png_decodes_with_alpha() {
        let mut src = Source::open(&fixture("photo.png"), true).unwrap();
        assert!(matches!(src, Source::Static { .. }));
        let img = src.pixels().unwrap();
        assert_eq!(img.dimensions(), (80, 60));
        assert!(img.get_pixel(0, 0)[3] < 255);
    }

    #[test]
    fn no_animate_shows_first_frame() {
        let src = Source::open(&fixture("anim.gif"), false).unwrap();
        assert!(matches!(src, Source::Static { .. }));
    }

    #[test]
    fn static_release_and_reload() {
        let mut src = Source::open(&fixture("photo.png"), true).unwrap();
        assert!(src.pixels().is_ok());
        src.idle_release();
        if let Source::Static { decoded, .. } = &src {
            assert!(decoded.is_none());
        }
        // Lazy reload on next access.
        assert!(src.pixels().is_ok());
    }

    #[test]
    fn webp_snapshot_drops_and_reloads() {
        // The WebP snapshot is disposable: idle_release frees it, and the
        // next paint re-pulls a frame from the decoder on demand.
        let mut src = Source::open(&fixture("anim.webp"), true).unwrap();
        src.step();
        assert!(src.pixels().is_ok());
        src.idle_release();
        match &src {
            Source::Animated(a) => assert!(a.current.is_none(), "snapshot dropped"),
            _ => panic!("must stay animated"),
        }
        // A repaint materializes the next frame without freezing.
        assert!(src.pixels().is_ok());
        assert!(!src.is_frozen());
    }

    #[test]
    fn corrupt_file_errors_cleanly() {
        let dir = std::env::temp_dir().join(format!("wally-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("corrupt.gif");
        std::fs::write(&p, b"GIF89a\x01\x00\x01\x00garbage").unwrap();
        // Opening may fail outright (corrupt first frame); if it succeeds,
        // stepping must not panic or hang either.
        if let Ok(mut src) = Source::open(&p, true) {
            let _ = src.step();
            let _ = src.step();
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_animation_file_freezes() {
        let dir = std::env::temp_dir().join(format!("wally-freeze-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("anim.gif");
        std::fs::copy(fixture("anim.gif"), &p).unwrap();
        let mut src = Source::open(&p, true).unwrap();
        assert!(src.is_animated());
        std::fs::remove_file(&p).unwrap();

        // Steps succeed until the stream ends, then the restart fails
        // three times in a row and the animation freezes.
        for _ in 0..12 {
            src.step();
        }
        assert!(src.is_frozen());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_special_formats() {
        assert!(matches!(detect(&fixture("anim.gif")).unwrap(), Detected::Common(ImageFormat::Gif)));
        assert!(matches!(detect(&fixture("anim.webp")).unwrap(), Detected::Common(ImageFormat::WebP)));
        assert!(matches!(detect(&fixture("photo.png")).unwrap(), Detected::Common(ImageFormat::Png)));
    }

    // ------------------------------------------------- in-place compositing
    //
    // The GIF path composites into the display canvas itself. These tests
    // pin it against the `image` crate's iterator (pixel parity on real
    // files) and against the spec's disposal rules (synthetic files built
    // with the `gif` crate's encoder).

    /// Pixel-exact parity with the `image` crate's GIF iterator on a real
    /// file: frame 1 at open, then one image frame per step.
    #[test]
    fn gif_compositing_matches_image_iterator() {
        let mut src = Source::open(&fixture("anim.gif"), true).unwrap();

        let file = File::open(fixture("anim.gif")).unwrap();
        let reference: Vec<RgbaImage> =
            image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file))
                .unwrap()
                .into_frames()
                .map(|f| f.unwrap().into_buffer())
                .collect();

        fn canvas_of(src: &Source) -> &RgbaImage {
            match src {
                Source::Animated(a) => a.current.as_ref().expect("owned canvas"),
                _ => panic!("must stay animated"),
            }
        }

        assert_eq!(canvas_of(&src), &reference[0], "frame 1 at open");
        for (i, want) in reference.iter().enumerate().skip(1) {
            src.step();
            assert_eq!(canvas_of(&src), want, "frame {}", i + 1);
        }
    }

    fn write_test_gif(path: &Path, w: u16, h: u16, frames: &[gif::Frame<'static>]) {
        let file = File::create(path).unwrap();
        // Global palette: index 0 = red, index 1 = green.
        let palette = [255u8, 0, 0, 0, 255, 0];
        let mut enc = gif::Encoder::new(file, w, h, &palette).unwrap();
        enc.set_repeat(gif::Repeat::Infinite).unwrap();
        for f in frames {
            enc.write_frame(f).unwrap();
        }
    }

    fn solid(w: u16, h: u16, index: u8) -> gif::Frame<'static> {
        gif::Frame {
            width: w,
            height: h,
            delay: 10,
            buffer: std::borrow::Cow::Owned(vec![index; w as usize * h as usize]),
            ..Default::default()
        }
    }

    #[test]
    fn gif_background_disposal_reveals_background() {
        let dir = std::env::temp_dir().join(format!("wally-bg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bg.gif");
        // f1: whole canvas green, keep. f2: red 2x2 at (1,1), restore to
        // background. f3: same rect, fully transparent.
        let mut f2 = solid(2, 2, 0);
        f2.left = 1;
        f2.top = 1;
        f2.dispose = gif::DisposalMethod::Background;
        let mut f3 = solid(2, 2, 0);
        f3.left = 1;
        f3.top = 1;
        f3.dispose = gif::DisposalMethod::Any;
        f3.transparent = Some(0);
        write_test_gif(&p, 4, 4, &[solid(4, 4, 1), f2, f3]);

        let mut src = Source::open(&p, true).unwrap();
        let px = |src: &Source, x: u32, y: u32| *shown(src).get_pixel(x, y);

        src.step(); // f2: red square on green field
        assert_eq!(px(&src, 0, 0), image::Rgba([0, 255, 0, 255]));
        assert_eq!(px(&src, 1, 1), image::Rgba([255, 0, 0, 255]));
        assert_eq!(px(&src, 3, 3), image::Rgba([0, 255, 0, 255]));

        src.step(); // f3: transparent square — background shows through
        assert_eq!(px(&src, 1, 1), image::Rgba([0, 0, 0, 0]), "rect must clear");
        assert_eq!(px(&src, 0, 0), image::Rgba([0, 255, 0, 255]), "outside survives");
        assert_eq!(px(&src, 3, 3), image::Rgba([0, 255, 0, 255]));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gif_previous_disposal_restores() {
        let dir = std::env::temp_dir().join(format!("wally-prev-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("prev.gif");
        // f1: green field, keep. f2: red square, restore to previous.
        // f3: transparent square — must reveal the pre-f2 (green) state.
        let mut f2 = solid(2, 2, 0);
        f2.left = 1;
        f2.top = 1;
        f2.dispose = gif::DisposalMethod::Previous;
        let mut f3 = solid(2, 2, 0);
        f3.left = 1;
        f3.top = 1;
        f3.dispose = gif::DisposalMethod::Any;
        f3.transparent = Some(0);
        write_test_gif(&p, 4, 4, &[solid(4, 4, 1), f2, f3]);

        let mut src = Source::open(&p, true).unwrap();
        let px = |src: &Source, x: u32, y: u32| *shown(src).get_pixel(x, y);

        src.step();
        assert_eq!(px(&src, 1, 1), image::Rgba([255, 0, 0, 255]));
        src.step();
        assert_eq!(px(&src, 1, 1), image::Rgba([0, 255, 0, 255]), "restore to previous");
        assert_eq!(px(&src, 0, 0), image::Rgba([0, 255, 0, 255]));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gif_single_frame_still_composites_opaque() {
        // A still GIF opened through the animation path must keep its
        // pixels: frame 1 blended onto a transparent canvas.
        let src = Source::open(&fixture("still.gif"), true).unwrap();
        let Source::Static { decoded: Some(img), .. } = &src else { panic!("still.gif is static") };
        assert!(img.as_raw().iter().any(|&b| b != 0), "frame pixels survived");
    }

    // -------------------------------------------------------- hosted canvas
    //
    // The hosted path composites straight into an output's scanout buffer
    // when the animation's native size equals the buffer's pixel size.
    // These tests drive it through a plain byte buffer and pin it against
    // composing the `image` crate's iterator frames over the background.

    #[test]
    fn hosted_gif_matches_composed_iterator() {
        let mut src = Source::open(&fixture("anim.gif"), true).unwrap();
        let dims = src.anim_dims().unwrap();
        let bg: Color = [10, 20, 30];
        let mut host = vec![0u8; dims.0 as usize * dims.1 as usize * 4];
        let mut want = host.clone();

        let file = File::open(fixture("anim.gif")).unwrap();
        let reference: Vec<RgbaImage> =
            image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file))
                .unwrap()
                .into_frames()
                .map(|f| f.unwrap().into_buffer())
                .collect();

        // The handover itself lands frame 1 in the buffer.
        assert!(src.begin_host(bg, &mut host));
        assert!(src.is_hosted());
        crate::render::compose_into(
            &reference[0],
            crate::cli::Mode::Stretch,
            dims.0,
            dims.1,
            bg,
            &mut want,
        );
        assert_eq!(host, want, "frame 1 in the host buffer");

        for (i, frame) in reference.iter().enumerate().skip(1) {
            src.step_hosted(&mut host);
            crate::render::compose_into(
                frame,
                crate::cli::Mode::Stretch,
                dims.0,
                dims.1,
                bg,
                &mut want,
            );
            assert_eq!(host, want, "frame {}", i + 1);
        }
    }

    #[test]
    fn hosted_gif_loops_after_eof() {
        let frames_total = 4usize;
        let mut src = Source::open(&fixture("anim.gif"), true).unwrap();
        let dims = src.anim_dims().unwrap();
        let bg: Color = [0, 0, 0];
        let mut host = vec![0u8; dims.0 as usize * dims.1 as usize * 4];

        // The handover blends frame 1 into the buffer; capture it, then
        // record the rest of one full cycle.
        assert!(src.begin_host(bg, &mut host));
        let first = host.clone();
        let mut cycle = Vec::new();
        for _ in 0..frames_total - 1 {
            src.step_hosted(&mut host);
            cycle.push(host.clone());
        }
        let full: Vec<Vec<u8>> = std::iter::once(first).chain(cycle).collect();

        // Two more full cycles past the end of the stream must repeat the
        // bytes exactly — same loop semantics as the owned path.
        for _ in 0..2 {
            for want in &full {
                src.step_hosted(&mut host);
                assert_eq!(&host, want);
            }
        }
        assert!(!src.is_frozen());
    }

    #[test]
    fn hosted_gif_freezes_when_file_vanishes() {
        let dir = std::env::temp_dir().join(format!("wally-host-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("anim.gif");
        std::fs::copy(fixture("anim.gif"), &p).unwrap();
        let mut src = Source::open(&p, true).unwrap();
        let dims = src.anim_dims().unwrap();
        let mut host = vec![0u8; dims.0 as usize * dims.1 as usize * 4];

        assert!(src.begin_host([0, 0, 0], &mut host));
        std::fs::remove_file(&p).unwrap();

        // Steps succeed until the stream ends, then the restart fails
        // three times in a row and the animation freezes with its last
        // good frame still in the buffer.
        for _ in 0..12 {
            src.step_hosted(&mut host);
        }
        assert!(src.is_frozen());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hosted_gif_returns_to_owned() {
        let mut src = Source::open(&fixture("anim.gif"), true).unwrap();
        let dims = src.anim_dims().unwrap();
        let mut host = vec![0u8; dims.0 as usize * dims.1 as usize * 4];
        assert!(src.begin_host([0, 0, 0], &mut host));
        assert!(src.is_hosted());

        src.ensure_owned();
        assert!(!src.is_hosted());
        // The canvas restarted (frame 1 on hand) and playback continues
        // through the ordinary path.
        assert!(src.pixels().is_ok());
        assert!(matches!(src.step(), Step::Advanced));
        assert!(!src.is_frozen());
    }

    #[test]
    fn begin_host_refuses_mismatched_buffer() {
        let mut src = Source::open(&fixture("anim.gif"), true).unwrap();
        assert!(!src.begin_host([0, 0, 0], &mut [0u8; 7]));
        assert!(!src.is_hosted());
        // Playback continues through the ordinary path.
        assert!(matches!(src.step(), Step::Advanced));
        assert!(!src.is_frozen());
    }

    #[test]
    fn webp_never_hosts() {
        // WebP compositing lives inside the image crate's decoder; only
        // GIFs hand their canvas to an output buffer.
        let mut src = Source::open(&fixture("anim.webp"), true).unwrap();
        let dims = src.anim_dims().unwrap();
        let mut host = vec![0u8; dims.0 as usize * dims.1 as usize * 4];
        assert!(!src.begin_host([0, 0, 0], &mut host));
        assert!(!src.is_hosted());
        assert!(matches!(src.step(), Step::Advanced));
    }
}
