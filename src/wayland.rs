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
//!
//! Playback is presentation-gated: every commit carries a
//! `wl_surface.frame` callback, and an animation whose last committed
//! frame was never presented (output off, session locked, layer
//! occluded) drops to a slow probe heartbeat instead of rendering for
//! nobody. A surface that has never produced callback evidence at all
//! keeps the old always-animate behavior, so a compositor without
//! functioning frame callbacks can never slow playback down.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use wayland_client::backend::{ReadEventsGuard, WaylandError};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool,
    wl_surface,
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

/// Animated surfaces keep their framebuffer resident unless the next
/// tick is at least this far away: a relax() before the next paint only
/// buys a full re-fault of the buffer every frame.
const ANIM_RELAX_IDLE: Duration = Duration::from_secs(2);

/// When a compositor stops presenting an output (screen off, lid closed,
/// session locked, layer fully occluded) the frame callback of the last
/// committed buffer never fires. A gated animation then steps at most
/// once per this interval: a heartbeat that can never wedge and keeps the
/// animation at most one interval behind when the output comes back,
/// while cutting the decode/blend/commit work for pixels nobody can see.
const ANIM_PROBE: Duration = Duration::from_secs(5);

/// How long after a frame callback a source still counts as visible.
/// Covers the gap between a commit and its own callback on slower-than-
/// animation displays: without it a 20 ms GIF on a 30 Hz panel would look
/// un-presented between every commit and drop to probe pacing. Real
/// disappearance (callbacks stop entirely) outlives the window; our own
/// commits do not refresh it, so an off output still parks after it.
const GATE_GRACE: Duration = Duration::from_millis(150);

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
    /// Next gated (probe) step, while no surface presenting this source
    /// is being composited. `None` when visible or not yet probed.
    probe_at: Option<Instant>,
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
    /// The connector name this surface was created for, when the
    /// compositor reports one; used for the state record.
    output_name: Option<String>,
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

    /// Outstanding `wl_surface.frame` callback of the most recent commit
    /// (one slot; a newer request replaces an older one — older probe
    /// callbacks simply match nothing when they finally fire).
    frame_cb: Option<wl_callback::WlCallback>,
    /// Whether the last committed frame was actually presented: its
    /// frame callback fired. False from the commit until the callback.
    presented: bool,
    /// Lifetime presentation evidence: `Some` once this surface ever got
    /// a frame callback. None surfaces are treated as visible — this is
    /// what makes the gate fail open for compositors that never send
    /// callbacks.
    frame_seen: Option<Instant>,
    /// The transition machinery ran at a different buffer format; when
    /// set, the next paint re-allocates the buffer in plain XRGB before
    /// composing.
    force_realloc: bool,
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
    /// --replace bookkeeping: old instances are asked to exit once the
    /// first frame round is on screen.
    replaced: bool,
    /// The state record is written once, after the first frame round.
    recorded: bool,
    /// `--transition` state: (kind, started, duration). `None` once the
    /// transition finished and the buffers went back to opaque XRGB.
    transition: Option<(crate::cli::TransitionKind, Instant, Duration)>,
    /// The `--daemon` control socket, bound once the first frame round is
    /// on screen. `None` for one-shot runs.
    daemon: Option<(std::os::unix::net::UnixListener, PathBuf)>,
    /// `--rotate` state: directory, interval, sorted candidate files,
    /// next index, due time. The re-entrancy guard lives on `App`
    /// (`rotating`) because apply_config ticks.
    rotate: Option<RotateState>,
    rotating: bool,
}

struct RotateState {
    dir: PathBuf,
    interval: Duration,
    files: Vec<PathBuf>,
    next_idx: usize,
    due: Instant,
}

/// Image files in a rotation directory, sorted for a deterministic cycle.
fn rotation_files(dir: &Path) -> Vec<PathBuf> {
    const EXTS: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "tif", "tga", "pnm", "qoi", "hdr",
        "dds", "ff", "ico", "avif", "jxl",
    ];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| EXTS.contains(&e.to_ascii_lowercase().as_str()))
                    .unwrap_or(false)
        })
        .collect();
    files.sort();
    files
}

/// Distinct (path, animate) pairs across every config group. Shared by the
/// compositor path and `--dry-run`.
pub(crate) fn source_keys(config: &Config) -> Vec<(PathBuf, bool)> {
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
    fn presentation_gate_fails_open_and_honors_grace() {
        let now = Instant::now();
        // No evidence at all: the compositor may not do frame callbacks —
        // keep the pre-gating always-animate behavior.
        assert!(gate_open(None, false, now));
        assert!(gate_open(None, true, now));
        // Stale evidence + unpresented commit: the output stopped being
        // composited — gate closed.
        assert!(!gate_open(Some(now - Duration::from_secs(5)), false, now));
        // Presented: open, no matter how old the evidence is.
        assert!(gate_open(Some(now - Duration::from_secs(5)), true, now));
        // Fresh callback but the newest commit is not presented yet: the
        // commit→callback gap of slower-than-animation displays — grace
        // keeps playback at full speed.
        assert!(gate_open(
            Some(now - Duration::from_millis(100)),
            false,
            now
        ));
    }

    #[test]
    fn replace_scan_excludes_our_own_pid() {
        use std::os::unix::ffi::OsStrExt as _;
        let pids = find_other_wallies();
        assert!(!pids.contains(&std::process::id()));
        // Every reported pid's cmdline really names a wally other than us.
        for pid in &pids {
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).expect("pid still exists");
            let argv0 = cmdline.split(|&b| b == 0).next().unwrap_or(&cmdline[..]);
            assert_eq!(
                std::path::Path::new(std::ffi::OsStr::from_bytes(argv0))
                    .file_name()
                    .and_then(|s| s.to_str()),
                Some("wally")
            );
        }
    }

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
            ..Config::default()
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
            probe_at: None,
        };
        let look = Look {
            mode: Mode::Fill,
            background: [0, 0, 0],
            color: None,
        };

        let mut a = vec![0u8; 8 * 8 * 4];
        let mut b = vec![0xAAu8; 8 * 8 * 4];
        let (ok_a, rect_a) = compose_frame(std::slice::from_mut(&mut st), 0, &mut a, 8, 8, look);
        let (ok_b, rect_b) = compose_frame(std::slice::from_mut(&mut st), 0, &mut b, 8, 8, look);
        assert!(ok_a && ok_b);
        assert_eq!(rect_a, rect_b, "rect tracking is deterministic too");
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
            probe_at: None,
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
/// Decode every distinct (path, animate) pair in parallel; the first
/// failure wins. Shared by the one-shot run, `--restore` and the
/// daemon's `set`.
fn open_sources(keys: &[(PathBuf, bool)]) -> Result<Vec<SourceState>> {
    std::thread::scope(|scope| -> Result<Vec<SourceState>> {
        let opened: Vec<_> = keys
            .iter()
            .map(|k| scope.spawn(|| Source::open(&k.0, k.1)))
            .collect();
        let mut out = Vec::with_capacity(keys.len());
        let mut first_error = None;
        for (handle, key) in opened.into_iter().zip(keys) {
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
                    probe_at: None,
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
    })
}

pub fn run(config: Config) -> Result<()> {
    // Fail fast on unreadable/broken images before touching the
    // compositor — distinct images decode in parallel.
    let keys = source_keys(&config);
    let sources = open_sources(&keys)?;

    let conn = Connection::connect_to_env()
        .context("could not connect to a Wayland compositor — is WAYLAND_DISPLAY set?")?;
    let mut queue = conn.new_event_queue::<App>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let transition = config
        .transition
        .map(|(kind, ms)| (kind, Instant::now(), Duration::from_millis(ms)));
    let rotate = config.rotate.as_ref().map(|(dir, secs)| RotateState {
        dir: dir.clone(),
        interval: Duration::from_secs(*secs),
        files: Vec::new(),
        next_idx: 0,
        due: Instant::now() + Duration::from_secs(*secs),
    });
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
        replaced: false,
        recorded: false,
        transition,
        daemon: None,
        rotate,
        rotating: false,
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

    let result = event_loop(&conn, &mut queue, &mut app, &qh);

    // Remove the control socket so a later daemon can bind cleanly.
    if let Some((_, path)) = app.daemon.take() {
        let _ = std::fs::remove_file(path);
    }
    result
}

// ------------------------------------------------------------- listing

/// One connected output, as `--list-outputs` and `--dry-run` report it.
pub struct OutputInfo {
    pub name: String,
    /// Current mode, physical pixels.
    pub width: u32,
    pub height: u32,
    /// Current mode refresh rate in millihertz (0 when unknown).
    pub refresh_mhz: u32,
    /// The output's scale factor as reported by `wl_output.scale`.
    pub scale: u32,
    /// Transform name: normal, 90, 180, 270, flipped, flipped-90, …
    pub transform: &'static str,
    /// True when the transform rotates by 90/270 degrees, so the mode's
    /// width/height swap between physical and logical axes.
    pub rotated: bool,
}

/// Which transforms rotate the image by 90/270 degrees (physical and
/// logical axes swap). Used by `--dry-run` to map a mode onto the
/// compositor's logical geometry.
pub(crate) fn transform_rotates(t: wl_output::Transform) -> bool {
    matches!(
        t,
        wl_output::Transform::_90
            | wl_output::Transform::_270
            | wl_output::Transform::Flipped90
            | wl_output::Transform::Flipped270
    )
}

fn transform_name(t: wl_output::Transform) -> &'static str {
    use wl_output::Transform as T;
    match t {
        T::Normal => "normal",
        T::_90 => "90",
        T::_180 => "180",
        T::_270 => "270",
        T::Flipped => "flipped",
        T::Flipped90 => "flipped-90",
        T::Flipped180 => "flipped-180",
        T::Flipped270 => "flipped-270",
        _ => "unknown",
    }
}

/// One connected output being collected by [`OutputLister`]. Fields fill
/// in as events arrive; the proxy is kept for event-to-entry matching.
struct ListedOutput {
    output: wl_output::WlOutput,
    name: Option<String>,
    scale: u32,
    mode: Option<(u32, u32, u32)>,
    transform: wl_output::Transform,
}

/// Minimal read-only client state: binds `wl_output` globals and collects
/// their properties. Nothing else is bound, no surface is ever created.
#[derive(Default)]
struct OutputLister {
    outputs: Vec<ListedOutput>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for OutputLister {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == wl_output::WlOutput::interface().name {
                let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                state.outputs.push(ListedOutput {
                    output,
                    name: None,
                    scale: 1,
                    mode: None,
                    transform: wl_output::Transform::Normal,
                });
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for OutputLister {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(idx) = state.outputs.iter().position(|o| o.output == *proxy) else {
            return;
        };
        let o = &mut state.outputs[idx];
        match event {
            wl_output::Event::Name { name } => o.name = Some(name),
            wl_output::Event::Scale { factor } => o.scale = factor.max(1) as u32,
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                let Ok(flags) = flags.into_result() else {
                    return;
                };
                if flags.contains(wl_output::Mode::Current) {
                    o.mode = Some((
                        width.max(0) as u32,
                        height.max(0) as u32,
                        refresh.max(0) as u32,
                    ));
                }
            }
            // The output's transform arrives with the geometry event.
            wl_output::Event::Geometry { transform, .. } => {
                if let Ok(t) = transform.into_result() {
                    o.transform = t;
                }
            }
            _ => {}
        }
    }
}

/// Enumerate connected outputs read-only: registry roundtrip to bind the
/// `wl_output` globals, a second roundtrip to collect their events. No
/// surface is created and nothing is drawn — this is what `--list-outputs`
/// and `--dry-run` run on.
pub fn list_outputs() -> Result<Vec<OutputInfo>> {
    let conn = Connection::connect_to_env()
        .context("could not connect to a Wayland compositor — is WAYLAND_DISPLAY set?")?;
    let mut queue = conn.new_event_queue::<OutputLister>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut lister = OutputLister::default();
    queue
        .roundtrip(&mut lister)
        .context("initial Wayland roundtrip failed")?;
    queue
        .roundtrip(&mut lister)
        .context("output enumeration roundtrip failed")?;

    let mut infos: Vec<OutputInfo> = lister
        .outputs
        .into_iter()
        .filter_map(|o| {
            let (width, height, refresh_mhz) = o.mode?;
            Some(OutputInfo {
                name: o.name.unwrap_or_else(|| "unknown".to_string()),
                width,
                height,
                refresh_mhz,
                scale: o.scale,
                transform: transform_name(o.transform),
                rotated: transform_rotates(o.transform),
            })
        })
        .collect();
    infos.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(infos)
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

        let mut fds = [
            libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            // fd < 0 entries are ignored by poll: one-shot runs have no
            // control socket.
            libc::pollfd {
                fd: app
                    .daemon
                    .as_ref()
                    .map(|(l, _)| l.as_raw_fd())
                    .unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout_ms) };
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
        if fds[1].revents & libc::POLLIN != 0 {
            app.accept_control(qh);
        }
    }
}

impl App {
    /// One connection, one request, one reply. A stalled or malicious
    /// client can cost us one 4 KB read at most; nothing here blocks the
    /// wallpaper.
    fn accept_control(&mut self, qh: &QueueHandle<Self>) {
        use std::io::{Read, Write};
        let Some((listener, _)) = self.daemon.as_ref() else {
            return;
        };
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while buf.len() < 4096 {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    let b = byte[0];
                    buf.push(b);
                    if b == b'\n' {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let reply = match crate::daemon::decode(&buf) {
            Some(req) => self.handle_control(req, qh),
            None => Err(anyhow::anyhow!("unparseable request")),
        };
        let text = match reply {
            Ok(msg) => format!("ok {msg}\n"),
            Err(e) => format!("error: {e:#}\n"),
        };
        let _ = stream.write_all(text.as_bytes());
        // `stream` drops here: the client sees EOF after the reply.
    }

    fn handle_control(
        &mut self,
        req: crate::daemon::Request,
        qh: &QueueHandle<Self>,
    ) -> Result<String> {
        use crate::cli::Parsed;
        use crate::daemon::Action;
        match req.action {
            Action::Stop => {
                self.quit = true;
                Ok("stopping".to_string())
            }
            Action::Query => {
                let entries = self.state_entries();
                if entries.is_empty() {
                    Ok("nothing is being shown\n".to_string())
                } else {
                    let mut out = String::new();
                    for e in entries {
                        let src = match &e.source {
                            crate::state::SourceSel::Image(p) => p.display().to_string(),
                            crate::state::SourceSel::Color(c) => {
                                format!("solid #{:02x}{:02x}{:02x}", c[0], c[1], c[2])
                            }
                        };
                        out.push_str(&format!("{}: {} ({})\n", e.output, src, e.mode.name()));
                    }
                    Ok(out)
                }
            }
            Action::Restore => {
                let entries = crate::state::load();
                if entries.is_empty() {
                    bail!("nothing recorded yet");
                }
                let config = crate::state::to_config(&entries);
                let n = self.apply_config(config, qh)?;
                Ok(format!("restored {n} output(s)"))
            }
            Action::Set => match crate::cli::parse_from(req.args) {
                Ok(Parsed::Run(config)) => {
                    let n = self.apply_config(config, qh)?;
                    Ok(format!("applied to {n} output(s)"))
                }
                Ok(Parsed::Help(text)) => Ok(text),
                Ok(_) => bail!("set expects image/color options"),
                Err(e) => Err(e),
            },
        }
    }

    /// Swap what every surface shows, through the ordinary paint path:
    /// decode the new images (parallel, fail-fast), reassign sources and
    /// looks, mark everything dirty, drop sources nothing references
    /// anymore. Surfaces, buffers, the event loop and every performance
    /// property are untouched — this composes exactly what a fresh
    /// one-shot start would, minus the process restart.
    fn apply_config(
        &mut self,
        config: crate::cli::Config,
        qh: &QueueHandle<Self>,
    ) -> Result<usize> {
        let new_keys = source_keys(&config);
        let new_sources = open_sources(&new_keys)?;
        let total_new = new_sources.len();

        // Destroy surfaces whose group is now empty — the same rule a
        // fresh run applies — before deciding which old sources live on.
        let mut destroy: Vec<usize> = Vec::new();
        for (i, s) in self.surfaces.iter().enumerate() {
            let cfg = config.for_output(s.output_name.as_deref());
            if cfg.image.is_none() && cfg.color.is_none() {
                destroy.push(i);
            }
        }
        for i in destroy.into_iter().rev() {
            self.destroy_surface(i);
        }

        // Where each remaining surface's pixels come from: indices into
        // (new_sources ++ kept old sources). Old sources that no surface
        // references are dropped (their pixels were already going idle).
        let mut assignment: Vec<Option<usize>> = Vec::with_capacity(self.surfaces.len());
        let mut old_kept = vec![false; self.sources.len()];
        for s in &self.surfaces {
            let cfg = config.for_output(s.output_name.as_deref());
            let slot = match cfg.image.as_ref() {
                None => None,
                Some(path) => {
                    let key_match = |k: &(PathBuf, bool)| k.0 == *path && k.1 == cfg.animate;
                    match new_keys.iter().position(key_match) {
                        Some(p) => Some(p),
                        None => self
                            .sources
                            .iter()
                            .position(|st| key_match(&st.key))
                            .map(|p| {
                                old_kept[p] = true;
                                total_new + p
                            }),
                    }
                }
            };
            assignment.push(slot);
        }

        // Merge: everything new first, then the still-referenced old
        // ones, moved out of the old Vec wholesale (decoders are not
        // cloneable — and do not need to be).
        let mut merged: Vec<SourceState> = new_sources;
        for (p, kept) in old_kept.iter().enumerate() {
            if *kept {
                merged.push(std::mem::replace(
                    &mut self.sources[p],
                    SourceState {
                        key: (PathBuf::new(), false),
                        source: Source::Static {
                            path: PathBuf::new(),
                            decoded: None,
                        },
                        last_error: None,
                        host: None,
                        probe_at: None,
                    },
                ));
            }
        }
        let mut remap_old = vec![0usize; self.sources.len()];
        let mut next = total_new;
        for (p, kept) in old_kept.iter().enumerate() {
            if *kept {
                remap_old[p] = next;
                next += 1;
            }
        }

        // Reassign looks and sources; mark dirty so the next paint
        // recomposes through the ordinary path.
        for (i, s) in self.surfaces.iter_mut().enumerate() {
            let cfg = config.for_output(s.output_name.as_deref());
            s.look = Look {
                mode: cfg.mode,
                background: cfg.background,
                color: cfg.color,
            };
            s.source = assignment[i].map(|slot| {
                if slot < total_new {
                    slot
                } else {
                    remap_old[slot - total_new]
                }
            });
            s.dirty = true;
        }

        self.sources = merged;
        self.recorded = false; // the new configuration becomes the state
        let shown = self.surfaces.len();
        self.tick(qh); // paint the new configuration right away
        Ok(shown)
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
/// intermediate screen-sized buffer. Returns `(false, _)` when the frame
/// could not be produced; the previous frame stays on screen and retries
/// are throttled. On success the second element is the composed rect
/// (buffer coordinates): `Some` when only a changed sub-rect of the
/// canvas was resampled — the rest of the buffer is untouched — `None`
/// for a full compose.
fn compose_frame(
    sources: &mut [SourceState],
    si: usize,
    dst: &mut [u8],
    pw: u32,
    ph: u32,
    look: Look,
) -> (bool, Option<render::Rect>) {
    let st = &mut sources[si];
    if st.last_error.is_some_and(|at| at.elapsed() < ERROR_BACKOFF) {
        return (false, None);
    }
    let SourceState {
        key: path_key,
        source,
        last_error,
        ..
    } = st;
    // The dirty rect is Copy state — pull it before pixels() takes its
    // decode borrow.
    let dirty = source.frame_dirty();
    match source.pixels() {
        Ok(img) => {
            // A changed sub-rect of the canvas resamples alone; the rest
            // of the wallpaper is already on screen and stays untouched.
            let dims = img.dimensions();
            let rect = dirty.and_then(|d| render::dirty_dest(dims, look.mode, pw, ph, d));
            match rect {
                Some(r) => {
                    render::compose_into_rect(img, look.mode, pw, ph, look.background, dst, r);
                    if debug_level() >= 2 {
                        eprintln!(
                            "wally: compose rect {}x{}@{},{} of {}x{}",
                            r.w, r.h, r.x, r.y, pw, ph
                        );
                    }
                    *last_error = None;
                    (true, Some(r))
                }
                None => {
                    render::compose_into(img, look.mode, pw, ph, look.background, dst);
                    *last_error = None;
                    (true, None)
                }
            }
        }
        Err(e) => {
            *last_error = Some(Instant::now());
            eprintln!("wally: could not paint `{}`: {e:#}", path_key.0.display());
            (false, None)
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

/// Whether a surface's source may keep animating. Two inputs:
/// `frame_seen` — has this surface EVER been confirmed by a frame
/// callback — and `presented` — did the most recent committed frame's
/// callback fire. `frame_seen == None` keeps the old always-animate
/// behavior, so a compositor without (functioning) frame callbacks can
/// never slow playback down. Once evidence exists, `presented` decides;
/// a still-fresh callback (within [`GATE_GRACE`]) keeps the gate open
/// across the commit→callback gap of slower-than-animation displays.
fn gate_open(frame_seen: Option<Instant>, presented: bool, now: Instant) -> bool {
    match frame_seen {
        None => true,
        Some(t) => presented || now.duration_since(t) < GATE_GRACE,
    }
}

/// Whether any surface showing source `si` has presentation liveness.
fn source_visible(surfaces: &[Surface], si: usize, now: Instant) -> bool {
    surfaces.iter().any(|s| match s.source {
        Some(x) if x == si => gate_open(s.frame_seen, s.presented, now),
        _ => false,
    })
}

/// Attach, damage and commit a finished buffer, then record the serial
/// that is on screen. `rect` limits the compositor-side damage to the
/// pixels that actually changed (`None` = the whole buffer).
fn commit_surface(
    qh: &QueueHandle<App>,
    s: &mut Surface,
    pw: u32,
    ph: u32,
    serial: u64,
    rect: Option<render::Rect>,
) {
    if let Some(buffer) = &s.buffer {
        s.surface.attach(Some(buffer), 0, 0);
        match rect {
            Some(r) => s
                .surface
                .damage_buffer(r.x as i32, r.y as i32, r.w as i32, r.h as i32),
            None => s.surface.damage_buffer(0, 0, pw as i32, ph as i32),
        }
        // Ask the compositor to tell us when this frame has been
        // presented. The callback's arrival is the liveness signal that
        // keeps the animation at full speed; its absence (screen off,
        // locked, occluded) is what parks it. Must precede the commit —
        // the callback binds to the commit that follows the request.
        s.frame_cb = Some(s.surface.frame(qh, ()));
        s.presented = false;
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

/// PIDs of other running wally processes (cmdline argv[0] basename
/// match), excluding our own. The scan is read-only; the caller decides
/// what to signal.
fn find_other_wallies() -> Vec<u32> {
    let self_pid = std::process::id();
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Ok(cmdline) = std::fs::read(e.path().join("cmdline")) else {
            continue; // vanished, or not readable (not ours to signal)
        };
        let argv0 = cmdline.split(|&b| b == 0).next().unwrap_or(&cmdline[..]);
        use std::os::unix::ffi::OsStrExt as _;
        let base = std::path::Path::new(std::ffi::OsStr::from_bytes(argv0))
            .file_name()
            .and_then(|s| s.to_str());
        if base == Some("wally") {
            pids.push(pid);
        }
    }
    pids
}

impl App {
    // ------------------------------------------------------------- tick

    /// Animation + repaint bookkeeping. Called once per loop iteration.
    fn tick(&mut self, qh: &QueueHandle<Self>) {
        let now = Instant::now();

        // --transition bookkeeping: while active, every surface whose
        // last commit was presented (or that has not painted yet) gets a
        // fresh frame this tick; the alpha mask goes with the paint. When
        // the duration is over, one final fully-opaque commit runs, and
        // the buffers go back to plain XRGB on the next paint.
        let mut alpha: Option<render::TransitionMask> = None;
        let mut transition_done = false;
        if let Some((kind, start, dur)) = self.transition {
            let elapsed = now.saturating_duration_since(start);
            if elapsed >= dur {
                alpha = Some(render::TransitionMask::Opaque);
                transition_done = true;
            } else {
                let p = elapsed.as_secs_f32() / dur.as_secs_f32().max(0.001);
                alpha = Some(match kind {
                    crate::cli::TransitionKind::Fade => render::TransitionMask::Fade(p),
                    crate::cli::TransitionKind::Wipe => render::TransitionMask::Wipe(p),
                });
                for s in &mut self.surfaces {
                    // presented: the compositor showed the previous
                    // transition frame — time for the next one.
                    // frame_seen.is_none(): no callbacks at all (the
                    // gate fails open) — fall back to tick pacing.
                    if s.presented || s.frame_seen.is_none() || s.painted_serial == 0 {
                        s.dirty = true;
                    }
                }
            }
        }

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
            // Presentation gating: the compositor confirms every committed
            // frame via wl_surface.frame. When the last commit's callback
            // has not fired — output off, lid closed, session locked, the
            // layer occluded — stop paying full animation CPU for pixels
            // nobody can see, but keep a slow heartbeat step so the
            // animation can neither wedge nor fall arbitrarily behind.
            if !source_visible(surfaces, i, now) {
                match st.probe_at {
                    Some(t) if now < t => continue,
                    _ => {}
                }
                if debug_level() >= 2 {
                    eprintln!("wally: no presentation — probing animation (output off or locked?)");
                }
                st.probe_at = Some(now + ANIM_PROBE);
            } else if st.probe_at.take().is_some() && debug_level() >= 2 {
                eprintln!("wally: presentation resumed — animation at full speed");
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

        // --rotate: one image from the directory every interval, applied
        // through the ordinary path (apply_config). Foreground-only by
        // design; the file list is re-scanned when the cycle runs out so
        // files can be added or removed while running. A bad file or an
        // empty directory is skipped with a log line, not fatal.
        if let Some(rot) = self.rotate.as_mut() {
            if !self.rotating && now >= rot.due {
                self.rotating = true;
                rot.due = now + rot.interval;
                if rot.files.len() < 2 {
                    rot.files = rotation_files(&rot.dir);
                }
                rot.next_idx += 1;
                let result = match rot.files.get((rot.next_idx - 1) % rot.files.len().max(1)) {
                    Some(file) => {
                        let mut cfg = self.config.clone();
                        cfg.default.image = Some(file.clone());
                        cfg.transition = None;
                        cfg.replace = false;
                        if debug_level() >= 1 {
                            eprintln!("wally: rotating to {}", file.display());
                        }
                        self.apply_config(cfg, qh).map(|_| ())
                    }
                    None => Err(anyhow::anyhow!("no image files in `{}`", rot.dir.display())),
                };
                if let Err(e) = result {
                    eprintln!("wally: rotation skipped: {e:#}");
                }
                self.rotating = false;
            }
        }

        self.paint_dirty(qh, alpha);

        if transition_done {
            self.transition = None;
            for s in &mut self.surfaces {
                s.force_realloc = true;
            }
        }

        // --replace: the first frame round is committed and visible in
        // the compositor's stack above the old instances — now they can
        // go. Doing this here (not at startup) is what removes the
        // black-screen gap: the old pixels stay up until ours replace
        // them.
        if self.config.replace && !self.replaced && self.all_painted_once() {
            self.replaced = true;
            let others = find_other_wallies();
            for pid in &others {
                unsafe {
                    libc::kill(*pid as i32, libc::SIGTERM);
                }
            }
            match others.len() {
                0 => eprintln!("wally: no older instance to replace"),
                n => eprintln!("wally: replaced {n} older instance(s)"),
            }
        }

        // Record what is now on screen so --query/--restore can repeat it.
        if !self.recorded && self.all_painted_once() {
            self.recorded = true;
            let entries = self.state_entries();
            if let Err(e) = crate::state::write_to(&crate::state::state_file(), &entries) {
                eprintln!("wally: could not record state: {e:#}");
            } else if debug_level() >= 1 {
                eprintln!("wally: recorded {} output(s)", entries.len());
            }
        }

        // --daemon: the control socket only exists once the daemon is
        // actually serving wallpapers — a client that connects can rely
        // on a painted screen.
        if self.config.daemon && self.daemon.is_none() && self.all_painted_once() {
            match crate::daemon::listen() {
                Ok(l) => {
                    eprintln!(
                        "wally: daemon listening on {}",
                        crate::daemon::socket_path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    );
                    self.daemon = Some((l, crate::daemon::socket_path().ok().unwrap_or_default()));
                }
                Err(e) => {
                    eprintln!("wally: {e:#}");
                    self.quit = true;
                }
            }
        }

        self.release_idle();

        debug_rss();
    }

    fn source_has_surface(&self, idx: usize) -> bool {
        self.surfaces.iter().any(|s| s.source == Some(idx))
    }

    /// True once every current surface committed at least one frame —
    /// the earliest point where `--replace` may retire the old instances
    /// without a visible gap.
    fn all_painted_once(&self) -> bool {
        self.ever_had_surface
            && !self.surfaces.is_empty()
            && self.surfaces.iter().all(|s| s.painted_serial > 0)
    }

    /// What is on screen right now, one entry per named output.
    fn state_entries(&self) -> Vec<crate::state::StateEntry> {
        use crate::state::{SourceSel, StateEntry};
        self.surfaces
            .iter()
            .filter_map(|s| {
                let output = s.output_name.as_ref()?;
                let source = match (&s.look.color, s.source) {
                    (Some(c), _) => SourceSel::Color(*c),
                    (None, Some(si)) => SourceSel::Image(self.sources[si].key.0.clone()),
                    (None, None) => return None,
                };
                Some(StateEntry {
                    output: output.clone(),
                    source,
                    mode: s.look.mode,
                    background: s.look.background,
                    animate: s.source.map(|si| self.sources[si].key.1).unwrap_or(true),
                })
            })
            .collect()
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
                // Mirror tick()'s gating: a gated source wakes for its
                // next probe, not for its (already due) frame deadline —
                // otherwise poll(0) would busy-loop while paused.
                let wait = if source_visible(&self.surfaces, i, now) {
                    at.saturating_duration_since(now)
                } else {
                    match st.probe_at {
                        Some(t) => t.saturating_duration_since(now),
                        None => at.saturating_duration_since(now),
                    }
                };
                min = Some(match min {
                    Some(m) if m <= wait => m,
                    _ => wait,
                });
            }
        }
        let mut timeout = match min {
            Some(d) => d.as_millis().clamp(0, i32::MAX as u128) as i32,
            None => -1,
        };
        // While a transition runs, frames are paced by frame callbacks
        // when the compositor sends them; the 16 ms cap keeps the
        // no-callback fallback from stalling the animation.
        if self.transition.is_some() {
            timeout = timeout.min(16);
        }
        // Rotation wakes the loop even for an all-static wallpaper.
        if let Some(rot) = &self.rotate {
            let wait = rot
                .due
                .saturating_duration_since(Instant::now())
                .as_millis() as i32;
            timeout = timeout.min(wait.max(0));
        }
        timeout
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

    /// The buffer scale a surface should use: the output's reported
    /// integer scale, or the `--scale-force` override (the escape hatch
    /// for fractional-scale setups where the compositor reports the
    /// ceiling and downscales — forcing 1 renders 1:1 logical pixels).
    fn effective_scale(&self, output_scale: u32) -> u32 {
        self.config.scale_force.unwrap_or(output_scale).max(1)
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
        let scale = self.effective_scale(scale);
        self.surfaces.push(Surface {
            output: output.clone(),
            output_name: name.clone(),
            surface,
            layer,
            width: 0,
            height: 0,
            scale,
            configured: false,
            dirty: true,
            painted_serial: 0,
            source,
            look,
            shm: None,
            pool: None,
            buffer: None,
            pending_release: Vec::new(),
            frame_cb: None,
            // Optimistic until the first commit proves otherwise: the
            // very first frames must animate without waiting for a
            // callback that has not been requested yet.
            presented: true,
            frame_seen: None,
            force_realloc: false,
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
    fn paint_dirty(&mut self, qh: &QueueHandle<Self>, alpha: Option<render::TransitionMask>) {
        let mut jobs: Vec<usize> = Vec::new();
        for i in 0..self.surfaces.len() {
            if !self.surface_needs_paint(i) {
                continue;
            }
            let (pw, ph) = physical_size(&self.surfaces[i]);

            // (Re)allocate the shm buffer when the geometry changed — or
            // when a finished transition needs the buffer back in plain
            // XRGB.
            let needs_realloc = self.surfaces[i].force_realloc
                || self.surfaces[i]
                    .shm
                    .as_ref()
                    .is_none_or(|m| m.len() != pw as usize * ph as usize * 4);
            if needs_realloc && !self.realloc_buffer(i, pw, ph, qh) {
                continue;
            }
            self.surfaces[i].force_realloc = false;

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
            if let Some(mask) = alpha {
                render::apply_alpha(slice, pw, mask);
            }
            commit_surface(qh, s, pw, ph, 1, None);
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
            // `Some(rect)`: only the rect's pixels were (re)written this
            // frame — resample, sibling copies and compositor damage can
            // all be limited to it. `None`: the whole buffer.
            let mut painted_rect: Option<render::Rect> = None;
            if skip_ok(&surfaces[pi]) {
                skip_commit(&mut surfaces[pi], serial);
            } else {
                painted = match host_idx {
                    // Pixels are already in this buffer; the blended frame
                    // rect (buffer == canvas, 1:1) is all that changed.
                    Some(hi) if hi == pi => {
                        painted_rect = sources[si].source.frame_dirty();
                        true
                    }
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
                        painted_rect = sources[si]
                            .source
                            .frame_dirty()
                            .and_then(|d| render::dirty_dest((iw, ih), look.mode, pw, ph, d));
                        match painted_rect {
                            Some(r) => render::compose_xrgb_into_rect(
                                host_raw,
                                (iw, ih),
                                look.mode,
                                pw,
                                ph,
                                look.background,
                                dst,
                                r,
                            ),
                            None => render::compose_xrgb_into(
                                host_raw,
                                (iw, ih),
                                look.mode,
                                pw,
                                ph,
                                look.background,
                                dst,
                            ),
                        }
                        true
                    }
                    // The ordinary path: compose from the source's pixels.
                    None => {
                        let slice = surfaces[pi]
                            .shm
                            .as_mut()
                            .expect("realloc above")
                            .slice_mut();
                        let (ok, rect) = compose_frame(sources, si, slice, pw, ph, look);
                        painted_rect = rect;
                        ok
                    }
                };
            }
            if !painted {
                // Keep the previous frame on screen; retry after backoff.
                continue;
            }

            // --transition: alpha is applied to the primary's finished
            // bytes; identical siblings copy them verbatim below, so the
            // mask reaches every surface exactly once.
            if let Some(mask) = alpha {
                let slice = surfaces[pi]
                    .shm
                    .as_mut()
                    .expect("realloc above")
                    .slice_mut();
                render::apply_alpha(slice, pw, mask);
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
                    let dst = s.shm.as_mut().expect("realloc above").slice_mut();
                    match painted_rect {
                        // One frame behind: only the frame's rect can differ.
                        Some(r) if !s.dirty && s.painted_serial + 1 == serial => {
                            for y in r.y..r.y + r.h {
                                let off = (y as usize * pw as usize + r.x as usize) * 4;
                                let n = r.w as usize * 4;
                                dst[off..off + n].copy_from_slice(&src[off..off + n]);
                            }
                        }
                        _ => dst.copy_from_slice(src),
                    }
                }
            }

            for &i in &members {
                let s = &mut surfaces[i];
                if !s.dirty && s.painted_serial == serial {
                    continue; // skip_commit already recorded this frame
                }
                // Damage exactly the frame's rect when this surface is one
                // frame behind; anything else (fresh configure, missed
                // frames) needs the whole buffer re-scanned.
                let rect = if !s.dirty && s.painted_serial + 1 == serial {
                    painted_rect
                } else {
                    None
                };
                commit_surface(qh, s, pw, ph, serial, rect);
            }

            // The pixels are committed and live in the memfd the
            // compositor reads — for stills our pages are dead weight
            // between repaints, so drop them from the RSS. An animation
            // rewrites its buffer on the next tick (frame delays are
            // typically a few centiseconds): relaxing would only buy a
            // full re-fault of the buffer every frame, so animated
            // surfaces keep their pages resident — the same reason a
            // hosted GIF's host buffer is excluded. Long-idle animations
            // (frozen, or a multi-second frame delay) still relax.
            let animated = sources[si].source.is_animation();
            let relax_idle = !animated
                || sources[si]
                    .source
                    .next_step()
                    .is_none_or(|at| at > Instant::now() + ANIM_RELAX_IDLE);
            for &i in &members {
                if Some(i) == host_idx || !relax_idle {
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
        // ARGB8888 while a transition is running: the compositor blends
        // the partially-transparent transition frames with whatever is
        // below (the previous wallpaper). Afterwards the buffer goes back
        // to XRGB — the identity the rest of the pipeline is built on.
        let format = if self.transition.is_some() {
            wl_shm::Format::Argb8888
        } else {
            wl_shm::Format::Xrgb8888
        };
        let buffer = pool.create_buffer(0, pw as i32, ph as i32, (pw * 4) as i32, format, qh, ());
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
                    let want = state.effective_scale(scale);
                    if state.surfaces[s].scale != want {
                        state.surfaces[s].scale = want;
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

impl Dispatch<wl_callback::WlCallback, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !matches!(event, wl_callback::Event::Done { .. }) {
            return;
        }
        // The frame we asked for was presented: this output is alive.
        // Only the most recent request is tracked; older ones (probes
        // that piled up while paused) match nothing — one Done suffices,
        // and the next probe would re-arm the slot anyway.
        for s in &mut state.surfaces {
            if s.frame_cb.as_ref().is_some_and(|cb| cb == proxy) {
                s.frame_cb = None;
                s.presented = true;
                s.frame_seen = Some(Instant::now());
                break;
            }
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
