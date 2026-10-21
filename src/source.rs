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
use image::{ImageFormat, RgbaImage};
use image_webp::WebPDecoder;

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

/// What to draw.
pub enum Source {
    /// A still image. `decoded` is dropped after every output painted it
    /// and lazily re-decoded from disk if a repaint is ever needed again.
    Static {
        path: PathBuf,
        decoded: Option<RgbaImage>,
    },
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
    /// Animated WebP decoded straight through `image-webp`: every frame
    /// is read into the display canvas itself (`read_frame` composites
    /// and fills the whole canvas), so a steady-state step allocates
    /// nothing and no second full-canvas buffer exists.
    WebP(Box<WebpStream>),
}

pub struct Anim {
    path: PathBuf,
    stream: AnimStream,
    /// Native pixel dimensions of the animation. A canvas of exactly this
    /// size can be hosted in an output's scanout buffer.
    dims: (u32, u32),
    /// Composited frame currently on screen. For a GIF this is the
    /// compositing canvas each new frame blends onto; for WebP it doubles
    /// as the decoder's decode target (frames are read into it in
    /// place). `None` while a GIF is hosted.
    current: Option<RgbaImage>,
    /// Hosted mode: the compositing canvas lives in an output's scanout
    /// buffer and `host_bg` is the color it was filled with.
    hosted: bool,
    host_bg: Color,
    delay: Duration,
    /// Consecutive failures; 3 freezes the animation.
    errors: u32,
    frozen: bool,
    /// Bumped whenever new pixels appear (in the canvas or the host
    /// buffer).
    serial: u64,
    /// Whether the most recent step changed any on-screen pixel. GIFs in
    /// the wild often repeat frames to pace playback; a step that wrote
    /// no new bytes lets the paint path skip compose, copy and commit.
    /// WebP frames are decoded wholesale, so they always report true.
    frame_changed: bool,
    /// The canvas region the most recent step touched (source pixels),
    /// when `frame_changed` and known. GIF frames are usually sub-rects;
    /// the paint pipeline resamples, copies and damages only that region
    /// instead of the whole wallpaper. `None` = whole canvas (restarts,
    /// WebP's full-canvas frames) or "no change".
    dirty_rect: Option<render::Rect>,
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
    /// Index that means "keep the canvas as it is" for this pixel.
    transparent: Option<u8>,
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
    Background {
        left: u32,
        top: u32,
        width: u32,
        height: u32,
    },
    /// `pixels` is row-major over the *clipped* rect, `width` pixels wide.
    Previous {
        left: u32,
        top: u32,
        width: u32,
        pixels: Vec<u8>,
    },
}

struct GifStream {
    decoder: gif::Decoder<BufReader<File>>,
    /// The file's global palette (RGB triplets), copied out at open time
    /// so a frame without a local palette can be mapped while the frame
    /// itself still borrows the decoder.
    global_palette: Vec<u8>,
    pending: Option<Pending>,
}

/// The animated-WebP decode machinery.
///
/// Drives `image-webp` directly instead of through the `image` crate's
/// frame iterator, which allocates a fresh full canvas per frame — plus a
/// second RGB→RGBA conversion copy for opaque files. Here one frame is
/// decoded at a time into the display canvas itself; looping is a seek
/// ([`WebPDecoder::reset_animation`]), not a file reopen.
struct WebpStream {
    decoder: WebPDecoder<BufReader<File>>,
    /// Composited canvas size; every frame fills it whole.
    dims: (u32, u32),
    /// Whether the file carries alpha. Alpha files decode straight into
    /// the RGBA canvas; opaque files need a 3-byte-per-pixel decode
    /// target, which is this reusable buffer.
    rgba: bool,
    rgb: Vec<u8>,
}

impl WebpStream {
    /// Decode the next frame into `canvas` in place. No allocation: the
    /// same canvas is the decode target on every steady-state step.
    /// Returns the frame's delay.
    fn read_into(&mut self, canvas: &mut RgbaImage) -> Result<Duration, image_webp::DecodingError> {
        debug_assert_eq!(canvas.dimensions(), self.dims);
        if self.rgba {
            let ms = self.decoder.read_frame(canvas.as_mut())?;
            return Ok(Duration::from_millis(u64::from(ms)));
        }
        let ms = self.decoder.read_frame(&mut self.rgb)?;
        for (d, s) in canvas
            .as_mut()
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(self.rgb.as_chunks::<3>().0)
        {
            *d = [s[0], s[1], s[2], 255];
        }
        Ok(Duration::from_millis(u64::from(ms)))
    }

    /// Decode the first frame into a fresh canvas (open and reopen path).
    fn read_first(&mut self) -> Result<(RgbaImage, Duration), image_webp::DecodingError> {
        let (w, h) = self.dims;
        let mut canvas = RgbaImage::new(w, h);
        let delay = self.read_into(&mut canvas)?;
        Ok((canvas, delay))
    }
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
    /// Set when any mutation actually altered a byte. GIFs from the wild
    /// often repeat frames (or re-draw an unchanged sub-rect) to pace
    /// playback; detecting a no-op step lets the paint path skip the
    /// compose, the sibling copies and the commit entirely.
    changed: bool,
    /// Union of the rects this canvas's mutations touched, clipped to
    /// the canvas. Reset per step (a fresh `Canvas` is built for every
    /// step); drives the dirty-rect paint pipeline.
    dirty: Option<render::Rect>,
}

impl Canvas<'_> {
    fn owned(img: &mut RgbaImage) -> Canvas<'_> {
        let (w, h) = img.dimensions();
        Canvas {
            bytes: &mut *img,
            w,
            h,
            hosted: None,
            changed: false,
            dirty: None,
        }
    }

    /// `bytes` is `w * h * 4` XRGB8888 bytes.
    fn hosted(bytes: &mut [u8], w: u32, h: u32, bg: Color) -> Canvas<'_> {
        debug_assert_eq!(bytes.len(), w as usize * h as usize * 4);
        Canvas {
            bytes,
            w,
            h,
            hosted: Some(bg),
            changed: false,
            dirty: None,
        }
    }

    /// Whether any mutation in this canvas's lifetime altered a byte.
    fn changed(&self) -> bool {
        self.changed
    }

    /// Record a rect (source pixels) this step may have touched.
    fn note_dirty(&mut self, x: u32, y: u32, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let x = x.min(self.w);
        let y = y.min(self.h);
        let r = render::Rect {
            x,
            y,
            w: w.min(self.w - x),
            h: h.min(self.h - y),
        };
        if r.is_empty() {
            return;
        }
        self.dirty = Some(match self.dirty {
            Some(d) => d.union(r),
            None => r,
        });
    }

    /// The previous frame's disposal takes effect now (it was deferred so
    /// its drawn pixels could stay on screen until this point).
    fn apply_pending(&mut self, pending: Pending) {
        match pending {
            Pending::Background {
                left,
                top,
                width,
                height,
            } => self.clear_rect(left, top, width, height),
            Pending::Previous {
                left,
                top,
                width,
                pixels,
            } => self.restore_rect(left, top, width, &pixels),
        }
    }

    /// `Background` disposal: clear the rect to transparent (owned) or
    /// to the background color (hosted) — both show the letterbox color.
    fn clear_rect(&mut self, left: u32, top: u32, width: u32, height: u32) {
        self.note_dirty(left, top, width, height);
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
                if *p != px {
                    *p = px;
                    self.changed = true;
                }
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
        self.note_dirty(left, top, width, self.h.saturating_sub(top));
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
            let dst = &mut self.bytes[d..d + n];
            if dst != &row[..n] {
                dst.copy_from_slice(&row[..n]);
                self.changed = true;
            }
            src = rest;
        }
    }

    /// Blend an indexed frame onto the canvas through a per-frame LUT.
    ///
    /// Port of the `image` crate's GIF blend rule: opaque frame pixels
    /// replace the canvas, transparent ones let the canvas (the
    /// "previous" state) show through. GIF transparency is binary, so
    /// this is a plain replace. The decoder hands over one byte per pixel
    /// (the palette index); `lut` maps each index to four canvas bytes in
    /// the canvas's own layout — XRGB for hosted, RGBA for owned — so the
    /// blend is a single indexed pass with no intermediate full-frame
    /// RGBA copy anywhere.
    fn blend_indexed(
        &mut self,
        info: &GifFrameInfo,
        indexes: &[u8],
        lut: &[u8],
        transparent: Option<u8>,
    ) {
        let (cw, ch) = (self.w as usize, self.h as usize);
        let (fw, fh) = (info.width as usize, info.height as usize);
        if fw == 0 || fh == 0 {
            return;
        }
        self.note_dirty(info.left, info.top, info.width, info.height);
        let bytes: &mut [u8] = &mut *self.bytes;
        for (row, idx_row) in indexes.chunks_exact(fw).enumerate().take(fh) {
            let y = info.top as usize + row;
            if y >= ch {
                break; // frame extends below the canvas; rest of the rows too
            }
            let drow = (y * cw + info.left as usize) * 4;
            for (col, &i) in idx_row.iter().enumerate() {
                if info.left as usize + col >= cw {
                    break; // frame extends past the right edge
                }
                if Some(i) == transparent {
                    continue; // canvas (previous state) shows through
                }
                let d = drow + col * 4;
                let px = &lut[i as usize * 4..i as usize * 4 + 4];
                if bytes[d..d + 4] != *px {
                    bytes[d..d + 4].copy_from_slice(px);
                    self.changed = true;
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
                    Ok(Source::Static {
                        path: path.to_owned(),
                        decoded: Some(img),
                    })
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
                    Ok(Source::Static {
                        path: path.to_owned(),
                        decoded: Some(img),
                    })
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
                    Ok(Source::Static {
                        path: path.to_owned(),
                        decoded: Some(img),
                    })
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

    /// Whether the most recent animation step changed any pixel. Static
    /// sources always report true. A `false` lets the paint path skip the
    /// compose, the sibling copies and the commit — every surface that
    /// showed the previous frame is already showing exactly these pixels.
    pub fn frame_changed(&self) -> bool {
        match self {
            Source::Animated(a) => a.frame_changed,
            _ => true,
        }
    }

    /// Whether this source animates. Animated surfaces keep their
    /// framebuffer out of `relax()` (see `paint_dirty`): the next tick
    /// rewrites it, so dropping the pages would only buy a re-fault of
    /// the whole buffer every frame.
    pub fn is_animation(&self) -> bool {
        matches!(self, Source::Animated(_))
    }

    /// The canvas region (source pixels) the most recent step touched,
    /// when known and changed. `None` = full compose (statics, restarts,
    /// full-canvas frames) or an unchanged frame (nothing to paint).
    pub fn frame_dirty(&self) -> Option<render::Rect> {
        match self {
            Source::Animated(a) if a.frame_changed && !a.frozen => a.dirty_rect,
            _ => None,
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

    /// Pixels to paint right now. Owned animations always hold their
    /// canvas (it is decode/compositing state); a released static is
    /// decoded from disk on demand.
    pub fn pixels(&mut self) -> Result<&RgbaImage> {
        match self {
            Source::Static { path, decoded } => {
                if decoded.is_none() {
                    *decoded = Some(decode_any(path)?);
                }
                Ok(decoded.as_ref().expect("decoded above"))
            }
            Source::Animated(a) => {
                if a.hosted {
                    // A hosted GIF's canvas lives in the output's buffer;
                    // painting must go through the host path.
                    bail!("hosted GIF canvas is not addressable from pixels()");
                }
                Ok(a.current
                    .as_ref()
                    .expect("owned animation holds its canvas"))
            }
        }
    }

    /// Free memory that only serves a repaint of the *current* frame.
    /// Called when every surface has painted it: statics re-decode from
    /// disk on demand. Both animation canvases are decode/compositing
    /// state — the GIF blends onto its canvas, the WebP decoder decodes
    /// into it — so they are reused in place every step and never freed;
    /// dropping them would just re-allocate the same bytes next tick.
    pub fn idle_release(&mut self) {
        match self {
            Source::Static { decoded, .. } => {
                *decoded = None;
            }
            Source::Animated(_) => {}
        }
    }

    /// Native pixel size of an animation.
    pub fn anim_dims(&self) -> Option<(u32, u32)> {
        match self {
            Source::Animated(a) if !a.frozen => Some(a.dims),
            _ => None,
        }
    }

    /// What `--dry-run` reports about one image file: format, decoded
    /// dimensions, animation. Opening goes through [`Source::open`], so a
    /// file this build cannot display fails here with the same error a
    /// real run would produce.
    pub fn describe(path: &Path, animate: bool) -> Result<FileInfo> {
        let mut source = Source::open(path, animate)?;
        let animated = matches!(source, Source::Animated(_));
        let (width, height) = if animated {
            source.anim_dims().unwrap_or((0, 0))
        } else {
            source
                .pixels()
                .map(|img| img.dimensions())
                .unwrap_or((0, 0))
        };
        Ok(FileInfo {
            format: format_name(path)?,
            width,
            height,
            animated,
        })
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
                    a.frame_changed = true; // state transition: repaint freely
                    a.dirty_rect = None; // first paint after handover is full
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
                let (pulled, changed, dirty) = {
                    let Some(canvas) = self.current.as_mut() else {
                        // Hosted GIFs step through `step_hosted`; a stale
                        // caller must not spin here.
                        return false;
                    };
                    let mut cv = Canvas::owned(canvas);
                    let res = pull_gif(gs, &mut cv);
                    let changed = cv.changed();
                    let dirty = cv.dirty;
                    (res, changed, dirty)
                };
                match pulled {
                    Ok(Some(info)) => {
                        self.delay = normalize_delay(gif_delay(info.delay_cs));
                        self.serial += 1;
                        self.errors = 0;
                        self.frame_changed = changed;
                        self.dirty_rect = if changed { dirty } else { None };
                        true
                    }
                    Ok(None) | Err(_) => self.reopen_gif_any(None),
                }
            }
            AnimStream::WebP(ws) => {
                let dims = ws.dims;
                // Decode one frame into the on-screen canvas when it
                // matches (the steady state — no allocation), or into a
                // fresh one after open/reopen/resize. Field-split
                // borrows: `ws` owns `self.stream`, the canvas work
                // touches `self.current` only, so both live side by side.
                let mut decode = |current: &mut Option<RgbaImage>,
                                  rewind: bool|
                 -> Result<
                    (Option<RgbaImage>, Duration),
                    image_webp::DecodingError,
                > {
                    if rewind {
                        ws.decoder.reset_animation();
                    }
                    match current.as_mut() {
                        Some(canvas) if canvas.dimensions() == dims => {
                            ws.read_into(canvas).map(|delay| (None, delay))
                        }
                        _ => ws.read_first().map(|(img, delay)| (Some(img), delay)),
                    }
                };
                let current = &mut self.current;
                let outcome = match decode(current, false) {
                    Ok((fresh, delay)) => Ok((fresh, delay)),
                    Err(image_webp::DecodingError::NoMoreFrames) => {
                        // End of stream: rewind with a seek (cheaper than
                        // reopening the file) and show frame 1 again —
                        // the infinite loop.
                        decode(current, true)
                    }
                    Err(e) => Err(e),
                };
                match outcome {
                    Ok((fresh, delay)) => {
                        if let Some(img) = fresh {
                            self.current = Some(img);
                        }
                        self.delay = normalize_delay(delay);
                        self.serial += 1;
                        self.errors = 0;
                        self.frame_changed = true;
                        // Frames arrive as full canvases from the decoder.
                        self.dirty_rect = None;
                        true
                    }
                    Err(_) => self.reopen_webp(),
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
        let (w, h) = (
            u32::from(gs.decoder.width()),
            u32::from(gs.decoder.height()),
        );
        if w == 0 || h == 0 {
            self.bump_error(
                "GIF",
                anyhow::anyhow!(
                    "GIF `{}` has a zero-sized logical screen",
                    self.path.display()
                ),
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
                    self.frame_changed = true; // restart: repaint regardless
                    self.dirty_rect = None;
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
                    self.frame_changed = true; // restart: repaint regardless
                    self.dirty_rect = None;
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
        match make_webp(&self.path) {
            Ok(mut ws) => match ws.read_first() {
                Ok((img, delay)) => {
                    let dims = ws.dims;
                    self.stream = AnimStream::WebP(Box::new(ws));
                    self.dims = dims;
                    self.current = Some(img);
                    self.hosted = false; // WebP never hosts
                    self.delay = normalize_delay(delay);
                    self.serial += 1;
                    self.errors = 0;
                    self.frame_changed = true; // restart: repaint regardless
                    self.dirty_rect = None;
                    self.schedule();
                    true
                }
                Err(e) => {
                    self.bump_error("WebP", webp_err(e));
                    false
                }
            },
            Err(e) => {
                self.bump_error("WebP", e);
                false
            }
        }
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
        let (pulled, changed, dirty) = {
            let mut canvas = Canvas::hosted(dst, w, h, self.host_bg);
            let res = pull_gif(gs, &mut canvas);
            let changed = canvas.changed();
            let dirty = canvas.dirty;
            (res, changed, dirty)
        };
        match pulled {
            Ok(Some(info)) => {
                self.delay = normalize_delay(gif_delay(info.delay_cs));
                self.serial += 1;
                self.errors = 0;
                self.frame_changed = changed;
                self.dirty_rect = if changed { dirty } else { None };
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
                delay: first_delay,
                errors: 0,
                frozen: false,
                serial: 1,
                frame_changed: true,
                dirty_rect: None,
                next_step: Some(Instant::now() + first_delay),
            })))
        }
        // Single-frame GIF: a still wallpaper in an animation container.
        Some(false) => Ok(Source::Static {
            path: path.to_owned(),
            decoded: Some(canvas),
        }),
        None => {
            eprintln!(
                "wally: second frame of `{}` failed to decode; showing only the first",
                path.display()
            );
            Ok(Source::Static {
                path: path.to_owned(),
                decoded: Some(canvas),
            })
        }
    }
}

/// Open the GIF decoder, blend the first frame onto a fresh (transparent)
/// canvas and return the stream, the canvas and the first frame's delay.
fn open_gif_stream(path: &Path) -> Result<(Box<GifStream>, RgbaImage, u16)> {
    let mut gs = make_gif(path)?;
    let (w, h) = (
        u32::from(gs.decoder.width()),
        u32::from(gs.decoder.height()),
    );
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
    let (dw, dh) = (
        u32::from(gs.decoder.width()),
        u32::from(gs.decoder.height()),
    );
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
    // Indexed output: frames arrive as one palette index per pixel
    // (w * h bytes). Mapping them onto the canvas through a per-frame LUT
    // during the blend is exactly the RGBA conversion the decoder would
    // otherwise do — minus the full-frame RGBA buffer it would keep
    // resident (8.3 MB at 1080p becomes 2.1 MB of indexes) and minus one
    // full-frame pass per animation frame. Same decode memory limit as
    // before (the default).
    options.set_color_output(gif::ColorOutput::Indexed);
    let decoder = options
        .read_info(BufReader::new(file))
        .with_context(|| format!("not a valid GIF: `{}`", path.display()))?;
    let global_palette = decoder.global_palette().unwrap_or(&[]).to_vec();
    Ok(Box::new(GifStream {
        decoder,
        global_palette,
        pending: None,
    }))
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
        transparent: frame.transparent,
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

    // Effective palette: a frame-local table overrides the global one.
    // Both are RGB triplets; entries past the table's end are black.
    let palette: &[u8] = match frame.palette.as_deref() {
        Some(p) if p.len() >= 3 => p,
        _ => &gs.global_palette,
    };
    // 256-entry LUT in the canvas's own byte layout — built on the stack
    // while the frame still borrows the decoder.
    let hosted = canvas.hosted.is_some();
    let mut lut = [0u8; 256 * 4];
    for (i, px) in lut.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let j = i * 3;
        let (r, g, b) = if j + 2 < palette.len() {
            (palette[j], palette[j + 1], palette[j + 2])
        } else {
            (0, 0, 0)
        };
        *px = if hosted { [b, g, r, 0] } else { [r, g, b, 255] };
    }

    canvas.blend_indexed(&info, &frame.buffer, &lut, info.transparent);
    Ok(Some(info))
}

/// `gif` frame delays are centiseconds; `image` maps them to
/// `delay * 10 ms / 1`, so feed the policy pipeline the same value.
fn gif_delay(delay_cs: u16) -> Duration {
    Duration::from_millis(u64::from(delay_cs) * 10)
}

fn open_webp(path: &Path) -> Result<Source> {
    let mut ws = make_webp(path)?;

    // A single-frame animation container is a still wallpaper: decode the
    // one frame and take the static path. (`num_frames` is read from the
    // header — no pixel decoding, and no open-time peek of frame 2 like
    // the iterator-based code needed.)
    if ws.decoder.num_frames() <= 1 {
        let (img, _) = ws.read_first().map_err(webp_err)?;
        return Ok(Source::Static {
            path: path.to_owned(),
            decoded: Some(img),
        });
    }

    // Show frame 1 immediately; the stream sits on frame 2.
    let (canvas, delay) = ws.read_first().map_err(webp_err)?;
    let first_delay = normalize_delay(delay);
    let dims = ws.dims;
    Ok(Source::Animated(Box::new(Anim {
        path: path.to_owned(),
        stream: AnimStream::WebP(Box::new(ws)),
        dims,
        current: Some(canvas),
        hosted: false,
        host_bg: [0, 0, 0],
        delay: first_delay,
        errors: 0,
        frozen: false,
        serial: 1,
        frame_changed: true,
        dirty_rect: None,
        next_step: Some(Instant::now() + first_delay),
    })))
}

/// Open an animated WebP stream. Still images bail here (the caller
/// falls back to the static decoder), so only genuine animations get
/// through.
fn make_webp(path: &Path) -> Result<WebpStream> {
    let file = File::open(path).with_context(|| format!("could not open `{}`", path.display()))?;
    let decoder = WebPDecoder::new(BufReader::new(file))
        .with_context(|| format!("not a valid WebP file: `{}`", path.display()))?;
    if !decoder.is_animated() {
        bail!("`{}` is a still WebP image", path.display());
    }
    let dims = decoder.dimensions();
    if dims.0 == 0 || dims.1 == 0 {
        bail!("WebP `{}` has a zero-sized canvas", path.display());
    }
    let rgba = decoder.has_alpha();
    // Opaque files decode as RGB (3 B/px) into this reusable buffer;
    // alpha files decode straight into the RGBA canvas.
    let rgb = if rgba {
        Vec::new()
    } else {
        vec![0u8; dims.0 as usize * dims.1 as usize * 3]
    };
    Ok(WebpStream {
        decoder,
        dims,
        rgba,
        rgb,
    })
}

/// Map a decoder error to a readable anyhow error.
fn webp_err(e: image_webp::DecodingError) -> anyhow::Error {
    anyhow::Error::new(e).context("frame decode failed")
}

/// Try the animation path first; still images inside animation containers
/// (single-frame WebP has no ANMF chunks at all) and plain stills fall
/// back to the static decoder. On total failure the original animation
/// error is reported, since it is the more specific one.
fn anim_or_static(path: &Path, format: AnimFormat) -> Result<Source> {
    match open_anim(path, format) {
        Ok(src) => Ok(src),
        Err(anim_err) => match decode_any(path) {
            Ok(img) => Ok(Source::Static {
                path: path.to_owned(),
                decoded: Some(img),
            }),
            Err(_) => Err(anim_err),
        },
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

/// One image file's placement-relevant facts, as `--dry-run` prints them.
pub struct FileInfo {
    pub format: &'static str,
    pub width: u32,
    pub height: u32,
    pub animated: bool,
}

/// Human name of the file's real format (content-sniffed, falling back to
/// the extension hint), matching what `Source::open` would have routed to.
pub(crate) fn format_name(path: &Path) -> Result<&'static str> {
    Ok(match detect(path)? {
        Detected::Common(ImageFormat::Png) => "png",
        Detected::Common(ImageFormat::Jpeg) => "jpeg",
        Detected::Common(ImageFormat::Gif) => "gif",
        Detected::Common(ImageFormat::WebP) => "webp",
        Detected::Common(ImageFormat::Bmp) => "bmp",
        Detected::Common(ImageFormat::Tiff) => "tiff",
        Detected::Common(ImageFormat::Tga) => "tga",
        Detected::Common(ImageFormat::Pnm) => "pnm",
        Detected::Common(ImageFormat::Qoi) => "qoi",
        Detected::Common(ImageFormat::Hdr) => "hdr",
        Detected::Common(ImageFormat::Dds) => "dds",
        Detected::Common(ImageFormat::Farbfeld) => "farbfeld",
        Detected::Common(ImageFormat::Ico) => "ico",
        Detected::Common(_) => "image",
        Detected::Jxl => "jpeg xl",
        Detected::Avif => "avif",
    })
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
        if matches!(
            &head[8..12],
            b"avif" | b"avis" | b"av01" | b"mif1" | b"miaf"
        ) {
            return Ok(Detected::Avif);
        }
    }

    // Extension hint. Content sniffing above covers real files; this
    // catches the rest (truncated downloads, exotic container brands,
    // mis-sniffing containers) so the error names the actual problem —
    // "support is not compiled in; rebuild with …" — instead of a generic
    // decoder error. With the feature enabled the decoder still verifies
    // the content, so a renamed file fails honestly there.
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("jxl") => return Ok(Detected::Jxl),
        Some(e) if e.eq_ignore_ascii_case("avif") => return Ok(Detected::Avif),
        _ => {}
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

    let file = File::open(path).with_context(|| format!("could not open `{}`", path.display()))?;
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
                image::Rgba([f32_to_u8(r), f32_to_u8(g), f32_to_u8(b), f32_to_u8(a)]),
            );
        }
    }
    Ok(out)
}

#[cfg(feature = "avif")]
fn decode_avif(path: &Path) -> Result<RgbaImage> {
    use avif_decode::{Decoder, Image};

    let file = File::open(path).with_context(|| format!("could not open `{}`", path.display()))?;
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
        Image::Rgba8(v) => from_samples(v.width(), v.height(), v.buf(), |p| [p.r, p.g, p.b, p.a]),
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

    // The parity references below drive the `image` crate's own decoders.
    use image::AnimationDecoder;

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

    /// A still wallpaper must never schedule a wake-up: `next_step() ==
    /// None` is what lets the event loop block in poll with no timeout
    /// and sit at 0% CPU. If this ever regresses, static wallpapers
    /// start polling again.
    #[test]
    fn static_sources_never_schedule_a_wake() {
        for name in ["photo.png", "anim.gif"] {
            let src = Source::open(&fixture(name), false).unwrap();
            assert!(
                !src.is_animated(),
                "{name} opened with animate=false must be static"
            );
            assert!(src.next_step().is_none(), "{name} must not schedule a wake");
        }
    }

    /// Per-frame cost of the hosted GIF path at 1080p: indexed decode +
    /// LUT blend straight into an XRGB buffer, worst case (every frame a
    /// full replace, no sub-rect shortcuts). Run with:
    ///
    /// `cargo test --release gif_frame_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "cost probe: cargo test --release gif_frame_cost -- --ignored --nocapture"]
    fn gif_frame_cost() {
        let dir = std::env::temp_dir().join(format!("wally-cost-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cost.gif");
        let (w, h) = (1920u16, 1080u16);
        let mut palette = Vec::with_capacity(768);
        for i in 0..256u16 {
            palette.extend_from_slice(&[i as u8, (i * 3) as u8, (i * 7) as u8]);
        }
        let file = File::create(&path).unwrap();
        let mut enc = gif::Encoder::new(file, w, h, &palette).unwrap();
        enc.set_repeat(gif::Repeat::Infinite).unwrap();
        let mut idx = vec![0u8; w as usize * h as usize];
        for f in 0..90u8 {
            for (i, p) in idx.iter_mut().enumerate() {
                *p = ((i as u32 % u32::from(w)) / 8) as u8 + f;
            }
            let frame = gif::Frame {
                width: w,
                height: h,
                delay: 4,
                buffer: std::borrow::Cow::Owned(idx.clone()),
                ..Default::default()
            };
            enc.write_frame(&frame).unwrap();
        }
        drop(enc);

        let mut src = Source::open(&path, true).unwrap();
        let dims = src.anim_dims().unwrap();
        let mut host = vec![0u8; dims.0 as usize * dims.1 as usize * 4];
        assert!(src.begin_host([0, 0, 0], &mut host));
        src.step_hosted(&mut host); // warm-up
        let runs = 60;
        let t0 = std::time::Instant::now();
        for _ in 0..runs {
            src.step_hosted(&mut host);
        }
        eprintln!(
            "hosted 1080p full-frame GIF step (decode + blend): {:?}/frame",
            t0.elapsed() / runs
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn canvas_reports_only_real_changes() {
        // A frame blended onto a matching canvas is a no-op; the same
        // frame onto a different canvas is a change.
        let mut img = RgbaImage::new(4, 2);
        let info = GifFrameInfo {
            left: 0,
            top: 0,
            width: 4,
            height: 2,
            dispose: gif::DisposalMethod::Keep,
            delay_cs: 10,
            transparent: None,
        };
        // Palette indexes over a table where index i maps to
        // [i*30, 10, 20, 255]; pixels cycle 0,1,2,0...
        let indexes: Vec<u8> = (0..8u8).map(|i| i % 3).collect();
        let mut lut = [0u8; 256 * 4];
        for (i, px) in lut.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *px = [(i * 30) as u8, 10, 20, 255];
        }

        let mut cv = Canvas::owned(&mut img);
        cv.blend_indexed(&info, &indexes, &lut, None);
        assert!(cv.changed());

        // Blend the identical frame onto the already-filled canvas.
        let mut cv2 = Canvas::owned(&mut img);
        cv2.blend_indexed(&info, &indexes, &lut, None);
        assert!(!cv2.changed());

        // One different pixel flips the flag again.
        let mut changed_indexes = indexes.clone();
        changed_indexes[1] = 200;
        let mut cv3 = Canvas::owned(&mut img);
        cv3.blend_indexed(&info, &changed_indexes, &lut, None);
        assert!(cv3.changed());
    }

    #[test]
    fn gif_repeated_frame_reports_no_change() {
        // anim-repeat.gif shows red, red (an exact repeat), green, blue.
        // Optimized GIFs repeat frames to pace playback; a repeated frame
        // must be detected so the paint path can skip the commit.
        let mut src = Source::open(&fixture("anim-repeat.gif"), true).unwrap();
        assert!(src.is_animated());

        // The canvas starts transparent; frame 1 (opaque red) is a change.
        assert!(src.frame_changed());
        src.step();
        assert!(
            !src.frame_changed(),
            "the repeated red frame changed nothing"
        );
        src.step();
        assert!(src.frame_changed(), "green is a change");
        src.step();
        assert!(src.frame_changed(), "blue is a change");
    }

    #[test]
    fn hosted_gif_repeated_frame_reports_no_change() {
        let path = fixture("anim-repeat.gif");
        let mut src = Source::open(&path, true).unwrap();
        let dims = src.anim_dims().unwrap();
        let mut buf = vec![0u8; dims.0 as usize * dims.1 as usize * 4];
        let bg: Color = [1, 2, 3];
        assert!(src.begin_host(bg, &mut buf));

        // begin_host restarts into frame 1 (red over the bg fill) — a change.
        assert!(src.frame_changed());
        // Frame 2 repeats frame 1: the blend writes nothing new.
        src.step_hosted(&mut buf);
        assert!(
            !src.frame_changed(),
            "the repeated red frame changed nothing"
        );
        // Frame 3 (green) is a change.
        src.step_hosted(&mut buf);
        assert!(src.frame_changed());
    }

    #[test]
    fn delay_policy() {
        let d = Duration::from_millis;
        assert_eq!(normalize_delay(d(0)), d(100));
        assert_eq!(normalize_delay(d(10)), d(100)); // 1 cs, browser rule
        assert_eq!(normalize_delay(d(15)), d(20)); // floor
        assert_eq!(normalize_delay(d(100)), d(100)); // honored as-is
    }

    /// Table-driven clamp policy: every centisecond value a GIF can carry
    /// (0..=65535 covers the wire format; the interesting band is 0..8 cs)
    /// and the millisecond values animated WebP carries. 0-1 cs is "as
    /// fast as possible" and gets the browser's 100 ms; everything below
    /// the 20 ms floor is raised to it; real delays pass through.
    #[test]
    fn delay_clamp_table() {
        let d = Duration::from_millis;
        let cases: &[(u64, u64)] = &[
            // (input ms, expected ms)
            (0, 100),         // 0 cs
            (1, 100),         // broken sub-1cs
            (5, 100),         // webp 0.5 cs
            (10, 100),        // 1 cs — the browser rule
            (11, 20),         // just above the fast-as-possible band
            (15, 20),         // floor
            (19, 20),         // floor
            (20, 20),         // exactly the floor
            (21, 21),         // pass-through begins
            (30, 30),         // 3 cs, pass-through
            (100, 100),       // 10 cs, pass-through
            (1000, 1000),     // a deliberate 1 s frame stays 1 s
            (65_000, 65_000), // beyond any sane frame; still honored
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_delay(d(*input)), d(*expected), "input {input} ms");
        }
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
        assert!(
            cycle.windows(2).any(|w| w[0] != w[1]),
            "fixture must animate"
        );
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
    fn webp_canvas_is_decode_state() {
        // The WebP canvas doubles as the decoder's decode target (frames
        // are read into it in place), so idle_release keeps it — same
        // policy as the GIF compositing canvas. Paints keep working.
        let mut src = Source::open(&fixture("anim.webp"), true).unwrap();
        src.step();
        assert!(src.pixels().is_ok());
        src.idle_release();
        match &src {
            Source::Animated(a) => {
                assert!(a.current.is_some(), "canvas is decode state, kept");
            }
            _ => panic!("must stay animated"),
        }
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

    // ------------------------------------------- adversarial containers
    //
    // Broken and hostile inputs must degrade the way the docs promise:
    // clean error at open, or a frozen animation after repeated step
    // failures — never a panic, never a busy loop.

    /// Slice a fixture to a fraction of its bytes.
    fn truncated(name: &str, fraction: usize) -> PathBuf {
        let bytes = std::fs::read(fixture(name)).unwrap();
        let cut = bytes.len() * fraction / 100;
        let dir = std::env::temp_dir().join(format!("wally-adv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("trunc-{}-{}", fraction, name));
        std::fs::write(&p, &bytes[..cut]).unwrap();
        p
    }

    /// A single-frame animated WebP: rebuild the RIFF container from
    /// anim.webp keeping only VP8X + ANIM + the first ANMF chunk.
    fn single_frame_anim_webp() -> PathBuf {
        let bytes = std::fs::read(fixture("anim.webp")).unwrap();
        // RIFF header: "RIFF" + LE32 size + "WEBP", then chunks of
        // (fourcc, LE32 size, payload, pad-to-even).
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WEBP");
        let mut keep: Vec<Vec<u8>> = Vec::new();
        let mut off = 12usize;
        while off + 8 <= bytes.len() {
            let fourcc = &bytes[off..off + 4];
            let size = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
            let end = (off + 8 + size).min(bytes.len());
            let mut chunk = bytes[off..end].to_vec();
            if size % 2 == 1 && end < bytes.len() {
                chunk.push(0); // keep the padding byte so offsets stay even
            }
            match fourcc {
                b"VP8X" | b"ANIM" | b"ANMF" => {
                    keep.push(chunk);
                    if fourcc == b"ANMF" {
                        break; // first frame only
                    }
                }
                _ => {}
            }
            off = off + 8 + size + (size % 2);
        }
        assert!(
            keep.iter().any(|c| &c[0..4] == b"ANIM"),
            "fixture has an ANIM chunk"
        );
        assert_eq!(keep.iter().filter(|c| &c[0..4] == b"ANMF").count(), 1);

        let payload_len: usize = keep.iter().map(|c| c.len()).sum();
        let mut out = Vec::with_capacity(12 + payload_len);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(4 + payload_len as u32).to_le_bytes());
        out.extend_from_slice(b"WEBP");
        for c in keep {
            out.extend_from_slice(&c);
        }
        let dir = std::env::temp_dir().join(format!("wally-adv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("single-frame-anim.webp");
        std::fs::write(&p, &out).unwrap();
        p
    }

    fn adv_dir() -> PathBuf {
        std::env::temp_dir().join(format!("wally-adv-{}", std::process::id()))
    }

    #[test]
    fn truncated_gif_fails_clean_or_freezes_never_panics() {
        for fraction in [10usize, 40, 70, 95] {
            let p = truncated("anim.gif", fraction);
            match Source::open(&p, true) {
                Err(e) => assert!(!e.to_string().is_empty(), "error text exists"),
                Ok(mut src) => {
                    // Opening succeeded on the truncated bytes: stepping
                    // must fail a bounded number of times and freeze, or
                    // keep looping (a decoder that tolerates the cut) —
                    // but never panic and never hang.
                    for _ in 0..8 {
                        let _ = src.step();
                    }
                    assert!(src.is_frozen() || src.next_step().is_some());
                }
            }
        }
        let _ = std::fs::remove_dir_all(adv_dir());
    }

    #[test]
    fn truncated_webp_fails_clean_or_freezes_never_panics() {
        for fraction in [10usize, 40, 70, 95] {
            let p = truncated("anim-lossy.webp", fraction);
            match Source::open(&p, true) {
                Err(e) => assert!(!e.to_string().is_empty()),
                Ok(mut src) => {
                    for _ in 0..8 {
                        let _ = src.step();
                    }
                    assert!(src.is_frozen() || src.next_step().is_some());
                }
            }
        }
        let _ = std::fs::remove_dir_all(adv_dir());
    }

    #[test]
    fn single_frame_anim_webp_is_a_still_by_design() {
        let p = single_frame_anim_webp();
        let mut src = Source::open(&p, true).expect("single-frame ANIM container opens");
        // Deliberate policy: the header says num_frames() == 1, so there
        // is nothing to animate — the file takes the static path, no
        // animation machinery, no loop timer. The adversarial point is
        // that an ANIM chunk must not buy a panic or a busy loop either
        // way.
        assert!(!src.is_animation(), "one ANMF frame opens as a still");
        assert_eq!(src.pixels().expect("still decodes").dimensions(), (64, 48));
        let _ = std::fs::remove_dir_all(adv_dir());
    }

    #[test]
    fn content_wins_over_extension() {
        // anim.gif bytes named .png: sniffing routes by content, so this
        // is a GIF animation no matter the name.
        let dir = adv_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("looks-like.png");
        std::fs::copy(fixture("anim.gif"), &p).unwrap();
        let src = Source::open(&p, true).expect("content sniffing ignores the extension");
        assert!(src.is_animation());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tiny_and_extreme_gifs_open_and_step() {
        let dir = adv_dir();
        std::fs::create_dir_all(&dir).unwrap();
        for (w, h) in [(1u16, 1u16), (1, 9), (9, 1), (3, 3)] {
            let p = dir.join(format!("t{}x{}.gif", w, h));
            write_test_gif(&p, w, h, &[solid(w, h, 0), solid(w, h, 1)]);
            let mut src =
                Source::open(&p, true).unwrap_or_else(|e| panic!("{}x{} opens: {e}", w, h));
            for _ in 0..3 {
                let _ = src.step();
            }
            assert!(!src.is_frozen(), "{}x{} steps cleanly", w, h);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_special_formats() {
        assert!(matches!(
            detect(&fixture("anim.gif")).unwrap(),
            Detected::Common(ImageFormat::Gif)
        ));
        assert!(matches!(
            detect(&fixture("anim.webp")).unwrap(),
            Detected::Common(ImageFormat::WebP)
        ));
        assert!(matches!(
            detect(&fixture("photo.png")).unwrap(),
            Detected::Common(ImageFormat::Png)
        ));
    }

    /// A `.jxl`/`.avif` file whose content does not sniff cleanly must
    /// still produce the "support is not compiled in" hint instead of a
    /// generic decoder error — that is task 2.3's contract.
    #[test]
    #[cfg(not(feature = "jxl"))]
    fn jxl_extension_without_the_feature_prints_the_rebuild_hint() {
        let dir = std::env::temp_dir().join(format!("wally-hint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("broken.jxl");
        std::fs::write(&p, b"neither a codestream nor a container").unwrap();

        let err = match Source::open(&p, true) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("must fail"),
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            err.contains("no JXL support") && err.contains("rebuild with"),
            "got: {err}"
        );
    }

    #[test]
    #[cfg(not(feature = "avif"))]
    fn avif_extension_without_the_feature_prints_the_rebuild_hint() {
        let dir = std::env::temp_dir().join(format!("wally-hint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("broken.avif");
        std::fs::write(&p, b"not an isobmff box").unwrap();

        let err = match Source::open(&p, true) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("must fail"),
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            err.contains("no AVIF support") && err.contains("rebuild with"),
            "got: {err}"
        );
    }

    /// With the feature compiled in, the extension hint must not lie: the
    /// decoder still rejects garbage content (just with a different,
    /// honest message).
    #[test]
    #[cfg(feature = "jxl")]
    fn jxl_extension_with_the_feature_still_verifies_content() {
        let dir = std::env::temp_dir().join(format!("wally-hint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("broken.jxl");
        std::fs::write(&p, b"neither a codestream nor a container").unwrap();

        let err = match Source::open(&p, true) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("must fail"),
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.contains("not a valid JPEG XL"), "got: {err}");
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
        assert_eq!(
            px(&src, 0, 0),
            image::Rgba([0, 255, 0, 255]),
            "outside survives"
        );
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
        assert_eq!(
            px(&src, 1, 1),
            image::Rgba([0, 255, 0, 255]),
            "restore to previous"
        );
        assert_eq!(px(&src, 0, 0), image::Rgba([0, 255, 0, 255]));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gif_single_frame_still_composites_opaque() {
        // A still GIF opened through the animation path must keep its
        // pixels: frame 1 blended onto a transparent canvas.
        let src = Source::open(&fixture("still.gif"), true).unwrap();
        let Source::Static {
            decoded: Some(img), ..
        } = &src
        else {
            panic!("still.gif is static")
        };
        assert!(
            img.as_raw().iter().any(|&b| b != 0),
            "frame pixels survived"
        );
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

    /// Every pixel that actually changed between two frames must lie
    /// inside the rect the step reported — that contract is what makes
    /// the dirty-rect compose, sibling copies and partial damage safe.
    #[test]
    fn gif_dirty_rect_covers_every_changed_pixel() {
        let mut src = Source::open(&fixture("anim.gif"), true).unwrap();
        let (w, h) = src.anim_dims().unwrap();
        assert!(src.is_animation(), "is_animation is the relax-policy hook");
        // 4 frames, stepped across EOF once (full-repaint restarts are
        // allowed to report None — they repaint everything anyway).
        for _ in 0..8 {
            let before = shown(&src).clone();
            src.step();
            if !src.frame_changed() {
                assert_eq!(src.frame_dirty(), None, "unchanged = no rect");
                continue;
            }
            let after = shown(&src).clone();
            let Some(r) = src.frame_dirty() else {
                continue; // restart frame: full repaint, trivially covered
            };
            assert!(
                r.x + r.w <= w && r.y + r.h <= h,
                "rect {r:?} inside {w}x{h}"
            );
            for y in 0..h {
                for x in 0..w {
                    if before.get_pixel(x, y) != after.get_pixel(x, y) {
                        assert!(
                            x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h,
                            "changed pixel ({x},{y}) outside dirty rect {r:?}"
                        );
                    }
                }
            }
        }
    }

    /// WebP frames arrive as full canvases from the decoder: full repaint.
    #[test]
    fn webp_dirty_rect_is_full_canvas() {
        let mut src = Source::open(&fixture("anim.webp"), true).unwrap();
        src.step();
        assert!(src.frame_changed());
        assert_eq!(src.frame_dirty(), None, "WebP frames are full-canvas");
    }

    /// The hosted path reports the blended frame rect in buffer
    /// coordinates (canvas == buffer for a host), so the paint path can
    /// damage exactly those pixels.
    #[test]
    fn hosted_dirty_rect_is_buffer_local() {
        let mut src = Source::open(&fixture("anim.gif"), true).unwrap();
        let dims = src.anim_dims().unwrap();
        let mut host = vec![0u8; dims.0 as usize * dims.1 as usize * 4];
        assert!(src.begin_host([0, 0, 0], &mut host));
        assert_eq!(src.frame_dirty(), None, "handover repaints fully");
        src.step_hosted(&mut host);
        if src.frame_changed() {
            let r = src.frame_dirty().expect("changed blend reports its rect");
            assert!(r.x + r.w <= dims.0 && r.y + r.h <= dims.1);
        }
    }
}
