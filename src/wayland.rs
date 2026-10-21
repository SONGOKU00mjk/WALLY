//! The Wayland frontend: wlr-layer-shell surfaces, shm buffers and the
//! event loop.
//!
//! Ownership in one sentence: each [`SourceState`] owns the decoded pixels
//! for one (path, animate) pair; every [`Surface`] points at one source and
//! repaints when either the source's serial moves or the surface itself
//! went dirty (configure, resize, scale change). Dirty surfaces that show
//! the same source with the same look and geometry are painted as one
//! group — the first composes the frame straight into its buffer, the
//! rest get a plain copy — so the expensive compose runs once per group
//! and no second copy of the wallpaper ever exists in memory.

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use wayland_client::backend::{ReadEventsGuard, WaylandError};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{
    Layer, ZwlrLayerShellV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1,
};

use crate::cli::{Color, Config, Mode};
use crate::render;
use crate::shm::ShmFile;
use crate::source::Source;

/// Everything an output needs to know about its look, resolved once at
/// surface creation instead of on every frame.
#[derive(Clone, Copy)]
struct Look {
    mode: Mode,
    background: Color,
    color: Option<Color>,
}

/// How long to wait before retrying a failed decode of a static image.
const ERROR_BACKOFF: Duration = Duration::from_secs(1);

/// One decoded image, shared by every surface pointed at the same
/// (path, animate) pair.
struct SourceState {
    key: (PathBuf, bool),
    source: Source,
    /// Last failed paint attempt; retries are throttled so a broken file
    /// can neither spam the log nor spin the decoder every tick.
    last_error: Option<Instant>,
    /// The surface hosting this source's GIF canvas, if any. Sticky: the
    /// canvas bytes live in that surface's buffer, so the host must not
    /// flip between outputs that merely look identical.
    host: Option<wl_surface::WlSurface>,
}

struct OutputEntry {
    registry_name: u32,
    output: wl_output::WlOutput,
    name: Option<String>,
    scale: u32,
    /// A layer surface was (or deliberately was not) created for it.
    surfaced: bool,
}

struct Surface {
    output: wl_output::WlOutput,
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    /// Configure dimensions, in logical pixels.
    width: u32,
    height: u32,
    scale: u32,
    configured: bool,
    /// Needs a repaint even when the source serial is unchanged.
    dirty: bool,
    painted_serial: u64,
    /// Index into `App::sources`; `None` for solid-color outputs.
    source: Option<usize>,
    /// The resolved look for this output (mode, colors).
    look: Look,

    // Buffer plumbing. The mmap lives as long as the surface; on a size
    // change the old buffer is parked until the compositor releases it —
    // destroying it early is a protocol violation that blanks the
    // wallpaper on strict compositors (niri, Smithay-based).
    shm: Option<ShmFile>,
    pool: Option<wl_shm_pool::WlShmPool>,
    buffer: Option<wl_buffer::WlBuffer>,
    pending_release: Vec<wl_buffer::WlBuffer>,
}

pub struct App {
    config: Config,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<ZwlrLayerShellV1>,
    outputs: Vec<OutputEntry>,
    surfaces: Vec<Surface>,
    sources: Vec<SourceState>,
    /// Set once a surface existed and all of them are gone (swaybg exits
    /// there too).
    quit: bool,
    ever_had_surface: bool,
}

/// Distinct (path, animate) pairs across every config group.
fn source_keys(config: &Config) -> Vec<(PathBuf, bool)> {
    let mut keys: Vec<(PathBuf, bool)> = Vec::new();
    let groups = std::iter::once(&config.default).chain(config.targets.iter().map(|(_, c)| c));
    for g in groups {
        if let Some(p) = &g.image {
            let key = (p.clone(), g.animate);
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cli::OutputCfg;

    #[test]
    fn source_keys_are_deduplicated_per_animate_flag() {
        let config = Config {
            default: OutputCfg {
                image: Some(PathBuf::from("a.png")),
                animate: true,
                ..OutputCfg::default()
            },
            targets: vec![
                (
                    "DP-1".into(),
                    OutputCfg {
                        image: Some(PathBuf::from("a.png")),
                        ..OutputCfg::default()
                    },
                ),
                (
                    "eDP-1".into(),
                    OutputCfg {
                        image: Some(PathBuf::from("b.png")),
                        animate: false,
                        ..OutputCfg::default()
                    },
                ),
            ],
        };
        let keys = source_keys(&config);
        // Same path+animate collapses; the no-animate variant is a second source.
        assert_eq!(
            keys,
            vec![
                (PathBuf::from("a.png"), true),
                (PathBuf::from("b.png"), false)
            ]
        );
    }

    #[test]
    fn compose_frame_is_deterministic() {
        use std::path::Path;

        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/photo.png");
        let source = Source::open(&path, false).unwrap();
        let mut st = SourceState {
            key: (path, false),
            source,
            last_error: None,
            host: None,
        };
        let look = Look {
            mode: Mode::Fill,
            background: [0, 0, 0],
            color: None,
        };

        let mut a = vec![0u8; 8 * 8 * 4];
        let mut b = vec![0xAAu8; 8 * 8 * 4];
        assert!(compose_frame(
            std::slice::from_mut(&mut st),
            0,
            &mut a,
            8,
            8,
            look
        ));
        assert!(compose_frame(
            std::slice::from_mut(&mut st),
            0,
            &mut b,
            8,
            8,
            look
        ));
        assert_eq!(a, b, "two composes of the same frame are identical");
    }

    /// Footprint probe for a 1080p static wallpaper, mirroring the real
    /// paint path: decode -> compose into the shm buffer -> idle_release
    /// -> relax. Run with:
    ///
    /// `cargo test --release steady_state_footprint -- --ignored --nocapture`
    #[test]
    #[ignore = "footprint probe: cargo test --release steady_state_footprint -- --ignored --nocapture"]
    fn steady_state_footprint() {
        // Same allocator tuning the daemon does in main(); without it the
        // harness's arena behavior skews the freed-buffer reports.
        crate::tune_allocator();

        fn rss_kb() -> i64 {
            let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
            let pages: i64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
            pages * 4096 / 1024
        }
        fn report(label: &str, base: i64) {
            let kb = rss_kb();
            eprintln!("{label:<38} rss {kb:>6} kB  ({:+>6} kB)", kb - base);
        }

        // A 1920x1080 photo-like PNG on disk, decoded like run() does.
        let (w, h) = (1920u32, 1080u32);
        let tmp = std::env::temp_dir().join("wally-footprint-probe.png");
        image::RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x % 255) as u8, (y % 255) as u8, ((x + y) % 255) as u8, 255])
        })
        .save_with_format(&tmp, image::ImageFormat::Png)
        .unwrap();
        let base = rss_kb();
        report("baseline", base);

        let source = Source::open(&tmp, false).unwrap();
        let mut st = SourceState {
            key: (tmp.clone(), false),
            source,
            last_error: None,
            host: None,
        };
        report("after decode (image in RAM)", base);

        let mut shm = ShmFile::new(w as usize * h as usize * 4).unwrap();
        let look = Look {
            mode: Mode::Fill,
            background: [0, 0, 0],
            color: None,
        };
        compose_frame(
            std::slice::from_mut(&mut st),
            0,
            shm.slice_mut(),
            w,
            h,
            look,
        );
        report("after compose into shm buffer", base);

        st.source.idle_release(); // what release_idle does once painted
        report("after idle_release (decoded freed)", base);

        shm.relax(); // what paint_dirty does after every commit
        report("after relax (framebuffer pages out)", base);
        std::thread::sleep(std::time::Duration::from_millis(200));
        report("steady state (200 ms later)", base);

        // CPU cost of the paint kernels (the thing opt-level tunes),
        // measured last so the probe allocations above stay out of the
        // RSS report.
        let runs = 20;
        let t0 = std::time::Instant::now();
        for _ in 0..runs {
            compose_frame(
                std::slice::from_mut(&mut st),
                0,
                shm.slice_mut(),
                w,
                h,
                look,
            );
        }
        eprintln!(
            "kernel copy    (1080p wallpaper -> 1080p buffer): {:>8.2?}/compose",
            t0.elapsed() / runs
        );

        // The animation hot path: a 720p animation canvas resampled onto
        // a 1080p buffer every frame.
        let src_small = image::RgbaImage::from_fn(1280, 720, |x, y| {
            image::Rgba([(x % 255) as u8, (y % 255) as u8, ((x + y) % 255) as u8, 255])
        });
        let t0 = std::time::Instant::now();
        for _ in 0..runs {
            render::compose_into(&src_small, Mode::Fill, w, h, [0, 0, 0], shm.slice_mut());
        }
        eprintln!(
            "kernel upscale (720p canvas -> 1080p buffer):    {:>8.2?}/compose",
            t0.elapsed() / runs
        );

        std::fs::remove_file(&tmp).ok();
    }
}

/// Entry point: connect, validate sources, surface every output, loop.
pub fn run(config: Config) -> Result<()> {
    // Fail fast on unreadable/broken images before touching the
    // compositor — distinct images decode in parallel.
    let keys = source_keys(&config);
    let sources: Vec<SourceState> = std::thread::scope(|scope| -> Result<Vec<SourceState>> {
        let opened: Vec<_> = keys
            .iter()
            .map(|k| scope.spawn(|| Source::open(&k.0, k.1)))
            .collect();
        let mut out = Vec::with_capacity(keys.len());
        let mut first_error = None;
        for (handle, key) in opened.into_iter().zip(&keys) {
            let source = match handle.join() {
                Ok(r) => r,
                Err(_) => bail!("image decoder thread panicked"),
            };
            match source {
                Ok(source) => out.push(SourceState {
                    key: key.clone(),
                    source,
                    last_error: None,
                    host: None,
                }),
                Err(e) if first_error.is_none() => {
                    first_error = Some(e.context(format!("could not load `{}`", key.0.display())));
                }
                Err(_) => {}
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(out),
        }
    })?;

    let conn = Connection::connect_to_env()
        .context("could not connect to a Wayland compositor — is WAYLAND_DISPLAY set?")?;
    let mut queue = conn.new_event_queue::<App>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut app = App {
        config,
        compositor: None,
        shm: None,
        shell: None,
        outputs: Vec::new(),
        surfaces: Vec::new(),
        sources,
        quit: false,
        ever_had_surface: false,
    };

    queue
        .roundtrip(&mut app)
        .context("initial Wayland roundtrip failed")?;

    if app.compositor.is_none() {
        bail!("wl_compositor is not available");
    }
    if app.shm.is_none() {
        bail!("wl_shm is not available");
    }
    if app.shell.is_none() {
        bail!(
            "this compositor does not support wlr-layer-shell.\n\
             \n\
             wally needs a wlroots- or Smithay-based compositor:\n\
             Sway, Hyprland, river, niri, Wayfire, labwc, KDE, …\n\
             \n\
             GNOME requires a Shell extension and is out of scope."
        );
    }

    // Outputs whose Done fired before the shell was bound get their
    // surfaces now (and on every later roundtrip / Done event).
    app.ensure_all_surfaces(&qh);
    queue.roundtrip(&mut app).ok();
    app.ensure_all_surfaces(&qh);

    event_loop(&conn, &mut queue, &mut app, &qh)
}

fn event_loop(
    conn: &Connection,
    queue: &mut EventQueue<App>,
    app: &mut App,
    qh: &QueueHandle<App>,
) -> Result<()> {
    loop {
        if app.quit {
            return Ok(());
        }

        queue
            .dispatch_pending(app)
            .context("Wayland dispatch failed")?;

        app.tick(qh);

        // Sleep until the next animation deadline or the next Wayland
        // event, whichever comes first.
        let timeout_ms = app.next_timeout_ms();
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        flush(conn, &guard).context("Wayland flush failed")?;

        let mut fds = [libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };
        match n {
            -1 if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {
                continue;
            }
            -1 => {
                return Err(std::io::Error::last_os_error()).context("poll on Wayland fd failed");
            }
            _ => {}
        }
        if fds[0].revents & libc::POLLIN != 0 {
            guard.read().context("Wayland read failed")?;
        }
    }
}

/// Send everything queued; wait for socket writability on short writes.
fn flush(conn: &Connection, guard: &ReadEventsGuard) -> Result<()> {
    loop {
        match conn.flush() {
            Ok(()) => return Ok(()),
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let mut fds = [libc::pollfd {
                    fd: guard.connection_fd().as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                }];
                unsafe { libc::poll(fds.as_mut_ptr(), 1, 1_000) };
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Physical pixel size of a surface: configure size times buffer scale.
fn physical_size(s: &Surface) -> (u32, u32) {
    let scale = s.scale.max(1);
    (
        s.width.max(1).saturating_mul(scale).max(1),
        s.height.max(1).saturating_mul(scale).max(1),
    )
}

/// WALLY_DEBUG verbosity: unset = off, `2` = per-event trace, anything
/// else = the default one-line reports.
fn debug_level() -> u32 {
    match std::env::var("WALLY_DEBUG") {
        Ok(v) if v.trim() == "2" => 2,
        Ok(_) => 1,
        Err(_) => 0,
    }
}

/// Cumulative user+system CPU time in milliseconds, read from
/// /proc/self/stat.
fn cpu_time_ms() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields 14 (utime) and 15 (stime), in clock ticks. Field 2 is the
    // comm name "(wally)" — no spaces — so plain splitting is safe.
    let mut fields = stat.split_whitespace();
    let utime: u64 = fields.nth(13)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    let clk = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    Some((utime + stime) * 1000 / clk)
}

/// Resident-set and CPU-time report for `WALLY_DEBUG`, printed only when
/// the RSS changes or the CPU total grows by 100 ms — so the footprint
/// can be watched from the outside without a debugger and without
/// spamming the log.
fn debug_rss() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_KB: AtomicU64 = AtomicU64::new(0);
    static LAST_CPU_MS: AtomicU64 = AtomicU64::new(0);

    if debug_level() == 0 {
        return;
    }
    if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
        if let Some(pages) = statm
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse::<u64>().ok())
        {
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
            let kb = pages.saturating_mul(page) / 1024;
            if LAST_KB.swap(kb, Ordering::Relaxed) != kb {
                eprintln!("wally: rss {kb} kB");
            }
        }
    }
    if let Some(ms) = cpu_time_ms() {
        let last = LAST_CPU_MS.load(Ordering::Relaxed);
        if ms >= last + 100 {
            LAST_CPU_MS.store(ms, Ordering::Relaxed);
            eprintln!("wally: cpu {ms} ms");
        }
    }
}

/// Compose the current frame of `sources[si]` straight into `dst` —
/// resample, placement and letterbox fill in one pass, with no
/// intermediate screen-sized buffer. Returns false when the frame could
/// not be produced; the previous frame stays on screen and retries are
/// throttled.
fn compose_frame(
    sources: &mut [SourceState],
    si: usize,
    dst: &mut [u8],
    pw: u32,
    ph: u32,
    look: Look,
) -> bool {
    let st = &mut sources[si];
    if st.last_error.is_some_and(|at| at.elapsed() < ERROR_BACKOFF) {
        return false;
    }
    let SourceState {
        key: path_key,
        source,
        last_error,
        ..
    } = st;
    match source.pixels() {
        Ok(img) => {
            render::compose_into(img, look.mode, pw, ph, look.background, dst);
            *last_error = None;
            true
        }
        Err(e) => {
            *last_error = Some(Instant::now());
            eprintln!("wally: could not paint `{}`: {e:#}", path_key.0.display());
            false
        }
    }
}

/// A surface eligible to host a GIF canvas: configured, painted once, and
/// a buffer of exactly the animation's pixel size (so compositing and
/// display are the same write — no resample in between).
fn is_host_surface(s: &Surface, dims: Option<(u32, u32)>) -> bool {
    let Some((w, h)) = dims else {
        return false;
    };
    s.source.is_some()
        && s.configured
        && s.painted_serial > 0
        && physical_size(s) == (w, h)
        && s.shm
            .as_ref()
            .is_some_and(|m| m.len() == w as usize * h as usize * 4)
}

/// First surface that can host `source`'s GIF canvas.
fn host_candidate(surfaces: &[Surface], source: &Source) -> Option<(usize, Color)> {
    let dims = source.host_dims()?;
    surfaces
        .iter()
        .position(|s| s.source.is_some() && is_host_surface(s, Some(dims)))
        .map(|idx| (idx, surfaces[idx].look.background))
}

/// Attach, damage and commit a finished buffer, then record the serial
/// that is on screen.
fn commit_surface(s: &mut Surface, pw: u32, ph: u32, serial: u64) {
    if let Some(buffer) = &s.buffer {
        s.surface.attach(Some(buffer), 0, 0);
        s.surface.damage_buffer(0, 0, pw as i32, ph as i32);
        s.surface.commit();
    }
    s.painted_serial = serial;
    s.dirty = false;
}

/// Record a frame as shown without talking to the compositor: the step
/// produced exactly the pixels already on screen (GIFs in the wild often
/// repeat frames to pace playback). Same bookkeeping as
/// [`commit_surface`] minus the attach/damage/commit round-trip.
fn skip_commit(s: &mut Surface, serial: u64) {
    s.painted_serial = serial;
    s.dirty = false;
    if debug_level() >= 2 {
        eprintln!("wally: unchanged frame — commit skipped");
    }
}

/// Surfaces that display the same thing: (source index, mode, background,
/// physical width, height) and the surface indices that want it.
type PaintGroup = ((usize, Mode, Color, u32, u32), Vec<usize>);

impl App {
    // ------------------------------------------------------------- tick

    /// Animation + repaint bookkeeping. Called once per loop iteration.
    fn tick(&mut self, qh: &QueueHandle<Self>) {
        let now = Instant::now();

        // Step due animations, once per source — never per surface — so
        // N monitors sharing a wallpaper share one timeline. A
        // screen-sized GIF hosts its compositing canvas in one output's
        // scanout buffer: the frame on screen IS the canvas, and frames
        // are blended straight into it, leaving no private copy.
        let Self {
            sources, surfaces, ..
        } = self;
        for (i, st) in sources.iter_mut().enumerate() {
            if !surfaces.iter().any(|s| s.source == Some(i)) {
                continue; // don't decode frames nobody will see
            }
            let Some(at) = st.source.next_step() else {
                continue;
            };
            if now < at {
                continue;
            }
            if st.source.is_hosted() {
                // Keep stepping in the buffer the canvas lives in.
                let dims = st.source.anim_dims();
                let host = st
                    .host
                    .as_ref()
                    .and_then(|h| surfaces.iter().position(|s| &s.surface == h))
                    .filter(|&idx| is_host_surface(&surfaces[idx], dims));
                match host {
                    Some(idx) => {
                        let slice = surfaces[idx].shm.as_mut().expect("host shm").slice_mut();
                        st.source.step_hosted(slice);
                    }
                    None => {
                        // Host gone or geometry changed: take the canvas
                        // back (restarts the GIF) and re-evaluate later.
                        st.host = None;
                        st.source.ensure_owned();
                    }
                }
            } else if let Some((idx, bg)) = host_candidate(surfaces, &st.source) {
                // First step in an eligible buffer: hand the canvas over.
                // Same frame 1 lands in it, so nothing visible changes.
                let slice = surfaces[idx]
                    .shm
                    .as_mut()
                    .expect("candidate shm")
                    .slice_mut();
                if st.source.begin_host(bg, slice) {
                    st.host = Some(surfaces[idx].surface.clone());
                }
            } else {
                st.source.step();
            }
        }

        self.paint_dirty(qh);

        self.release_idle();

        debug_rss();
    }

    fn source_has_surface(&self, idx: usize) -> bool {
        self.surfaces.iter().any(|s| s.source == Some(idx))
    }

    fn surface_needs_paint(&self, i: usize) -> bool {
        let s = &self.surfaces[i];
        if !s.configured {
            return false;
        }
        if s.dirty {
            return true;
        }
        match s.source {
            None => false, // solid color paints once per configure
            Some(si) => self.sources[si].source.serial() > s.painted_serial,
        }
    }

    /// Static images are freed once every surface referencing them painted
    /// the current serial; a later repaint re-decodes from disk. Animated
    /// WebPs likewise drop their full-frame snapshot (the decoder keeps
    /// the true compositing canvas). The composed frame stays on screen in
    /// the output's own buffer either way.
    fn release_idle(&mut self) {
        for si in 0..self.sources.len() {
            let serial = self.sources[si].source.serial();
            let idle = self.surfaces.iter().all(|s| match s.source {
                Some(x) if x == si => !s.dirty && s.painted_serial >= serial,
                _ => true,
            });
            if idle {
                self.sources[si].source.idle_release();
            }
        }
    }

    /// Milliseconds to sleep before the next deadline, or `-1` to block
    /// until the compositor sends something.
    fn next_timeout_ms(&self) -> i32 {
        let now = Instant::now();
        let mut min: Option<Duration> = None;
        for (i, st) in self.sources.iter().enumerate() {
            if !self.source_has_surface(i) {
                continue;
            }
            if let Some(at) = st.source.next_step() {
                let wait = at.saturating_duration_since(now);
                min = Some(match min {
                    Some(m) if m <= wait => m,
                    _ => wait,
                });
            }
        }
        match min {
            Some(d) => d.as_millis().clamp(0, i32::MAX as u128) as i32,
            None => -1,
        }
    }

    // ---------------------------------------------------------- outputs

    /// Create layer surfaces for every bound output that lacks one.
    fn ensure_all_surfaces(&mut self, qh: &QueueHandle<Self>) {
        let outputs: Vec<(wl_output::WlOutput, u32)> = self
            .outputs
            .iter()
            .filter(|o| !o.surfaced)
            .map(|o| (o.output.clone(), o.scale))
            .collect();
        for (output, scale) in outputs {
            self.create_surface(&output, scale, qh);
        }
    }

    fn create_surface(&mut self, output: &wl_output::WlOutput, scale: u32, qh: &QueueHandle<Self>) {
        let (Some(compositor), Some(shell)) = (&self.compositor, &self.shell) else {
            return; // globals not bound yet; ensure_all_surfaces runs again later
        };

        let name = self
            .outputs
            .iter()
            .find(|o| o.output == *output)
            .and_then(|o| o.name.clone());
        let cfg = self.config.for_output(name.as_deref());
        if cfg.image.is_none() && cfg.color.is_none() {
            eprintln!(
                "wally: no image or color configured for output `{}`; skipping it",
                name.as_deref().unwrap_or("<unnamed>")
            );
            if let Some(o) = self.outputs.iter_mut().find(|o| o.output == *output) {
                o.surfaced = true;
            }
            return;
        }
        let look = Look {
            mode: cfg.mode,
            background: cfg.background,
            color: cfg.color,
        };
        let source = cfg.image.as_ref().and_then(|path| {
            self.sources
                .iter()
                .position(|s| s.key.0 == *path && s.key.1 == cfg.animate)
        });

        let surface = compositor.create_surface(qh, ());
        let layer = shell.get_layer_surface(
            &surface,
            Some(output),
            Layer::Background,
            "wally".into(),
            qh,
            (),
        );
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_exclusive_zone(-1);
        layer.set_margin(0, 0, 0, 0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);

        // Never intercept pointer input; this is a background layer.
        let region = compositor.create_region(qh, ());
        surface.set_input_region(Some(&region));
        surface.commit();

        self.ever_had_surface = true;
        self.surfaces.push(Surface {
            output: output.clone(),
            surface,
            layer,
            width: 0,
            height: 0,
            scale: scale.max(1),
            configured: false,
            dirty: true,
            painted_serial: 0,
            source,
            look,
            shm: None,
            pool: None,
            buffer: None,
            pending_release: Vec::new(),
        });
        if let Some(o) = self.outputs.iter_mut().find(|o| o.output == *output) {
            o.surfaced = true;
        }
    }

    // ------------------------------------------------------------ paint

    /// Repaint every dirty, configured surface. Image surfaces that show
    /// the same source with the same look and geometry are painted as one
    /// group: the first one composes the frame straight into its buffer
    /// (no intermediate copy) and the rest receive a plain memcpy — the
    /// expensive compose runs once no matter how many monitors share it.
    fn paint_dirty(&mut self, qh: &QueueHandle<Self>) {
        let mut jobs: Vec<usize> = Vec::new();
        for i in 0..self.surfaces.len() {
            if !self.surface_needs_paint(i) {
                continue;
            }
            let (pw, ph) = physical_size(&self.surfaces[i]);

            // (Re)allocate the shm buffer when the geometry changed.
            let needed = pw as usize * ph as usize * 4;
            if self.surfaces[i]
                .shm
                .as_ref()
                .is_none_or(|m| m.len() != needed)
                && !self.realloc_buffer(i, pw, ph, qh)
            {
                continue;
            }

            // Map the physical-sized buffer onto the logical surface;
            // without this a scale-2 output shows only the top-left quarter.
            let scale = self.surfaces[i].scale.max(1);
            self.surfaces[i].surface.set_buffer_scale(scale as i32);
            jobs.push(i);
        }

        let Self {
            sources, surfaces, ..
        } = self;

        // Solid-color surfaces: fill and go, no grouping needed.
        for &i in &jobs {
            if surfaces[i].source.is_some() {
                continue;
            }
            let (pw, ph) = physical_size(&surfaces[i]);
            let s = &mut surfaces[i];
            let slice = s.shm.as_mut().expect("realloc above").slice_mut();
            render::fill(slice, s.look.color.unwrap_or(s.look.background));
            commit_surface(s, pw, ph, 1);
            // The color lives in the memfd now; drop our pages (below).
            if let Some(m) = s.shm.as_ref() {
                m.relax();
            }
        }

        // Image surfaces: group by what they display.
        let mut groups: Vec<PaintGroup> = Vec::new();
        for &i in &jobs {
            let s = &surfaces[i];
            if let Some(si) = s.source {
                let (pw, ph) = physical_size(s);
                let key = (si, s.look.mode, s.look.background, pw, ph);
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, members)) => members.push(i),
                    None => groups.push((key, vec![i])),
                }
            }
        }

        for ((si, _mode, _bg, pw, ph), members) in groups {
            let look = surfaces[members[0]].look;
            let pi = members[0];
            let serial = sources[si].source.serial();

            // A frame that changed no pixels needs no compose, no copy
            // and no commit — a surface already showing the previous
            // frame is already showing this one. Members that missed the
            // previous frame (fresh configure, resize, a newly added
            // output) still repaint in full.
            let unchanged = !sources[si].source.frame_changed();
            let skip_ok = |s: &Surface| unchanged && !s.dirty && s.painted_serial + 1 == serial;

            // A hosted GIF's canvas is its host output's buffer: the frame
            // was blended straight into it during the step. Groups with
            // other geometries compose from the host's bytes — there is
            // no in-process canvas to read.
            let host_idx = sources[si]
                .host
                .as_ref()
                .filter(|_| sources[si].source.is_hosted())
                .and_then(|h| surfaces.iter().position(|s| &s.surface == h));

            let mut painted = true;
            if skip_ok(&surfaces[pi]) {
                skip_commit(&mut surfaces[pi], serial);
            } else {
                painted = match host_idx {
                    // Pixels are already in this buffer.
                    Some(hi) if hi == pi => true,
                    Some(hi) => {
                        let Some((iw, ih)) = sources[si].source.anim_dims() else {
                            continue;
                        };
                        let (before, rest) = surfaces.split_at_mut(hi);
                        let (host_s, after) = rest.split_at_mut(1);
                        let host_raw = host_s[0].shm.as_ref().expect("host shm").slice();
                        let target = if pi < hi {
                            &mut before[pi]
                        } else {
                            &mut after[pi - hi - 1]
                        };
                        let dst = target.shm.as_mut().expect("realloc above").slice_mut();
                        render::compose_xrgb_into(
                            host_raw,
                            (iw, ih),
                            look.mode,
                            pw,
                            ph,
                            look.background,
                            dst,
                        );
                        true
                    }
                    // The ordinary path: compose from the source's pixels.
                    None => {
                        let slice = surfaces[pi]
                            .shm
                            .as_mut()
                            .expect("realloc above")
                            .slice_mut();
                        compose_frame(sources, si, slice, pw, ph, look)
                    }
                };
            }
            if !painted {
                // Keep the previous frame on screen; retry after backoff.
                continue;
            }

            // Identical siblings get the finished bytes verbatim — unless
            // they already show them (unchanged frame, nothing missed).
            let (before, rest) = surfaces.split_at_mut(pi);
            let (primary, after) = rest.split_at_mut(1);
            let src: &[u8] = primary[0].shm.as_ref().expect("realloc above").slice();
            for &i in members.iter().filter(|&&i| i != pi) {
                let s = if i < pi {
                    &mut before[i]
                } else {
                    &mut after[i - pi - 1]
                };
                if skip_ok(s) {
                    skip_commit(s, serial);
                } else {
                    s.shm
                        .as_mut()
                        .expect("realloc above")
                        .slice_mut()
                        .copy_from_slice(src);
                }
            }

            for &i in &members {
                let s = &mut surfaces[i];
                if !s.dirty && s.painted_serial == serial {
                    continue; // skip_commit already recorded this frame
                }
                commit_surface(s, pw, ph, serial);
            }

            // The pixels are committed and live in the memfd the
            // compositor reads — our pages of this buffer are dead weight
            // between repaints, so drop them from the RSS. Exception: a
            // hosted GIF's host buffer IS the compositing canvas, written
            // in place at every step; its pages must stay resident.
            for &i in &members {
                if Some(i) == host_idx {
                    continue;
                }
                if let Some(m) = surfaces[i].shm.as_ref() {
                    m.relax();
                }
            }
        }
    }

    fn realloc_buffer(&mut self, i: usize, pw: u32, ph: u32, qh: &QueueHandle<Self>) -> bool {
        let Some(shm) = &self.shm else {
            eprintln!("wally: wl_shm disappeared, cannot allocate a buffer");
            return false;
        };
        let size = pw as usize * ph as usize * 4;
        let Ok(file) = ShmFile::new(size) else {
            eprintln!("wally: could not create a {size}-byte shm file");
            return false;
        };

        {
            // Park the old buffer; dropping it while the compositor still
            // reads it is illegal (see `pending_release`).
            let s = &mut self.surfaces[i];
            if let Some(old) = s.buffer.take() {
                s.pending_release.push(old);
            }
            s.pool = None;
            s.shm = Some(file);
        }

        let fd = self.surfaces[i].shm.as_ref().expect("just set").fd();
        let pool = shm.create_pool(fd, size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            pw as i32,
            ph as i32,
            (pw * 4) as i32,
            wl_shm::Format::Xrgb8888,
            qh,
            (),
        );
        let s = &mut self.surfaces[i];
        s.pool = Some(pool);
        s.buffer = Some(buffer);
        true
    }

    fn find_surface_by_layer(&self, layer: &ZwlrLayerSurfaceV1) -> Option<usize> {
        self.surfaces.iter().position(|s| &s.layer == layer)
    }

    fn find_surface_by_output(&self, output: &wl_output::WlOutput) -> Option<usize> {
        self.surfaces.iter().position(|s| s.output == *output)
    }

    /// Drop a surface and everything attached to it.
    fn destroy_surface(&mut self, i: usize) {
        let s = self.surfaces.remove(i);
        s.layer.destroy();
        s.surface.destroy();
    }
}

// ------------------------------------------------------------ dispatch

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "zwlr_layer_shell_v1" => {
                    state.shell = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_output" => {
                    let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                    state.outputs.push(OutputEntry {
                        registry_name: name,
                        output,
                        name: None,
                        scale: 1,
                        surfaced: false,
                    });
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                let Some(pos) = state.outputs.iter().position(|o| o.registry_name == name) else {
                    return;
                };
                let output = state.outputs.remove(pos).output;
                if let Some(i) = state.find_surface_by_output(&output) {
                    state.destroy_surface(i);
                }
                if state.ever_had_surface && state.surfaces.is_empty() {
                    state.quit = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let Some(idx) = state.outputs.iter().position(|o| o.output == *proxy) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => state.outputs[idx].name = Some(name),
            wl_output::Event::Scale { factor } => {
                state.outputs[idx].scale = factor.max(1) as u32;
            }
            wl_output::Event::Done => {
                let scale = state.outputs[idx].scale;
                // First Done: create the surface. Later dones (mode/scale
                // changes): propagate the scale and repaint.
                if !state.outputs[idx].surfaced {
                    let output = state.outputs[idx].output.clone();
                    state.create_surface(&output, scale, qh);
                }
                if let Some(s) = state.find_surface_by_output(proxy) {
                    if state.surfaces[s].scale != scale.max(1) {
                        state.surfaces[s].scale = scale.max(1);
                        state.surfaces[s].dirty = true;
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(i) = state.find_surface_by_layer(proxy) else {
            return;
        };
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                proxy.ack_configure(serial);
                let s = &mut state.surfaces[i];
                s.width = width.max(1);
                s.height = height.max(1);
                s.configured = true;
                s.dirty = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.destroy_surface(i);
                if state.ever_had_surface && state.surfaces.is_empty() {
                    state.quit = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            // Compositor is done with it; dropping the proxy now sends
            // wl_buffer.destroy, which is finally legal.
            for s in &mut state.surfaces {
                s.pending_release.retain(|b| b != proxy);
            }
        }
    }
}

/// Interfaces whose events we don't care about.
macro_rules! noop_dispatch {
    ($($ty:ty),* $(,)?) => {
        $(
            impl Dispatch<$ty, ()> for App {
                fn event(
                    _: &mut Self,
                    _: &$ty,
                    _: <$ty as Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }
        )*
    };
}

noop_dispatch!(
    wl_compositor::WlCompositor,
    wl_surface::WlSurface,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_region::WlRegion,
    ZwlrLayerShellV1,
);
