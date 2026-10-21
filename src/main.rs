//! wally — a minimal Wayland wallpaper setter.
//!
//! Modules:
//! - [`cli`]: swaybg-style argument parsing into a per-output [`cli::Config`]
//! - [`source`]: image loading (static, GIF, animated WebP, AVIF, JPEG XL)
//! - [`render`]: pure composition into XRGB8888 buffers
//! - [`shm`]: memfd-backed framebuffers
//! - [`wayland`]: layer-shell surfaces and the event loop

mod cli;
mod daemon;
mod render;
mod shm;
mod source;
mod state;
mod wayland;

use anyhow::{bail, Result};
use cli::{Config, Mode, OutputCfg};

/// Keep the glibc allocator honest about large allocations. Without
/// this, glibc raises its mmap threshold after the first big block is
/// freed and starts serving multi-megabyte buffers from the sbrk arena,
/// where freed pages stay resident — a still wallpaper would keep its
/// 8 MB decode buffer in RSS forever. Pinning the threshold keeps every
/// large buffer a direct mmap, so freeing it returns the pages to the
/// OS immediately: the same behavior the musl build had. (musl has no
/// mallopt and its allocator already behaves this way.)
pub fn tune_allocator() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 128 * 1024);
    }
}

fn main() {
    tune_allocator();

    match cli::parse() {
        cli::Parsed::Help(text) => print!("{text}"),
        cli::Parsed::Version => println!("wally {}", env!("CARGO_PKG_VERSION")),
        cli::Parsed::ListOutputs => {
            if let Err(e) = cmd_list_outputs() {
                eprintln!("wally: {e:#}");
                std::process::exit(1);
            }
        }
        cli::Parsed::Query => {
            let entries = state::load();
            if entries.is_empty() {
                println!("wally: nothing recorded yet");
                return;
            }
            for e in &entries {
                let src = match &e.source {
                    state::SourceSel::Image(p) => p.display().to_string(),
                    state::SourceSel::Color(c) => {
                        format!("solid #{:02x}{:02x}{:02x}", c[0], c[1], c[2])
                    }
                };
                println!(
                    "{}: {} ({}, bg #{:02x}{:02x}{:02x}{})",
                    e.output,
                    src,
                    e.mode.name(),
                    e.background[0],
                    e.background[1],
                    e.background[2],
                    if e.animate { "" } else { ", first frame only" }
                );
            }
        }
        cli::Parsed::Restore => {
            let entries = state::load();
            if entries.is_empty() {
                eprintln!("wally: nothing recorded yet — run wally once first");
                std::process::exit(1);
            }
            let config = state::to_config(&entries);
            if let Err(e) = wayland::run(config) {
                eprintln!("wally: {e:#}");
                std::process::exit(1);
            }
        }
        cli::Parsed::Control { action, args } => match daemon::transact(action, &args) {
            Ok(reply) => print!("{reply}"),
            Err(e) => {
                eprintln!("wally: {e:#}");
                std::process::exit(1);
            }
        },
        cli::Parsed::DryRun(config) => {
            if let Err(e) = cmd_dry_run(&config) {
                eprintln!("wally: {e:#}");
                std::process::exit(1);
            }
        }
        cli::Parsed::Run(config) => {
            if let Err(e) = wayland::run(config) {
                eprintln!("wally: {e:#}");
                std::process::exit(1);
            }
        }
    }
}

// ----------------------------------------------------------- list-outputs

fn refresh_hz(mhz: u32) -> String {
    if mhz == 0 {
        return "?Hz".to_string();
    }
    let hz = f64::from(mhz) / 1000.0;
    if (hz - hz.round()).abs() < 0.05 {
        format!("{:.0}Hz", hz)
    } else {
        format!("{:.2}Hz", hz)
    }
}

fn cmd_list_outputs() -> Result<()> {
    let outputs = wayland::list_outputs()?;
    for o in &outputs {
        println!(
            "{} {}x{}@{} scale {} transform {}",
            o.name,
            o.width,
            o.height,
            refresh_hz(o.refresh_mhz),
            o.scale,
            o.transform
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- dry-run

/// The distinct images across every group, decoded and described in
/// parallel — the same fail-fast preflight a real run performs.
fn describe_sources(config: &Config) -> Result<Vec<(std::path::PathBuf, source::FileInfo)>> {
    let keys = wayland::source_keys(config);
    std::thread::scope(|scope| {
        let opened: Vec<_> = keys
            .iter()
            .map(|k| scope.spawn(|| source::Source::describe(&k.0, k.1)))
            .collect();
        let mut out = Vec::with_capacity(keys.len());
        let mut first_error = None;
        for (handle, key) in opened.into_iter().zip(&keys) {
            match handle.join() {
                Ok(Ok(info)) => out.push((key.0.clone(), info)),
                Ok(Err(e)) if first_error.is_none() => {
                    first_error = Some(e.context(format!("could not load `{}`", key.0.display())))
                }
                Ok(Err(_)) => {}
                Err(_) => first_error = Some(anyhow::anyhow!("image decoder thread panicked")),
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(out),
        }
    })
}

/// One output's scaling decision, as prose with numbers, from the same
/// `render::geometry` the painter uses.
fn scaling_decision(cfg: &OutputCfg, iw: u32, ih: u32, pw: u32, ph: u32) -> String {
    if let Some(color) = cfg.color {
        return format!(
            "solid #{:02x}{:02x}{:02x} -> fills {}x{}",
            color[0], color[1], color[2], pw, ph
        );
    }
    let Some(iw) = (iw != 0).then_some(iw) else {
        return "no image".to_string();
    };
    let ih = ih.max(1);
    let (x0, y0, sw, sh, tw, th) = render::geometry(iw, ih, cfg.mode, pw.max(1), ph.max(1));
    let scale = f64::from(tw) / f64::from(sw).max(1.0);
    let covers = (tw, th) == (pw, ph);
    let placement = if covers {
        "covers".to_string()
    } else {
        format!("centered on {pw}x{ph}, background elsewhere")
    };
    let verb = match cfg.mode {
        Mode::Stretch => "stretch",
        Mode::Fill => "fill",
        Mode::Fit => "fit",
        Mode::Center => "center",
        Mode::Tile => "tile",
    };
    if cfg.mode == Mode::Tile {
        return format!("{verb}: {iw}x{ih} repeated at original size over {pw}x{ph}");
    }
    let cropped = (sw, sh) != (iw, ih);
    let crop = if cropped {
        format!("crop {sw}x{sh} @({x0},{y0}) -> ")
    } else {
        String::new()
    };
    format!("{verb}: {iw}x{ih} -> {crop}{tw}x{th} ({scale:.2}x, {placement})")
}

fn cmd_dry_run(config: &Config) -> Result<()> {
    // 1. Validate and describe every distinct image. Any failure exits
    //    non-zero before the compositor is touched.
    let files = describe_sources(config)?;
    for (path, info) in &files {
        let kind = if info.animated { "animated" } else { "static" };
        println!(
            "{}: {}x{}, {}, {kind}",
            path.display(),
            info.width,
            info.height,
            info.format
        );
    }
    // What each configured group would draw, mirroring run()'s semantics:
    // image and/or color; a group with neither is skipped entirely.
    let describe_group = |name: &str, cfg: &OutputCfg| -> String {
        if let Some(p) = &cfg.image {
            format!("{name}: {}", p.display())
        } else if let Some(color) = cfg.color {
            format!(
                "{name}: solid #{:02x}{:02x}{:02x}",
                color[0], color[1], color[2]
            )
        } else {
            format!("{name}: nothing configured (an output matching this group is skipped)")
        }
    };
    if config.default.image.is_some() || config.default.color.is_some() {
        println!(
            "  {}",
            describe_group("default (all outputs)", &config.default)
        );
    }
    for (name, cfg) in &config.targets {
        println!("  {}", describe_group(name, cfg));
    }

    // 2. Per-output scaling decisions. Output enumeration is read-only
    //    (registry + wl_output; no surfaces, nothing drawn). Without a
    //    compositor the configuration is still printed — file validation
    //    was the point.
    let outputs = match wayland::list_outputs() {
        Ok(o) => o,
        Err(e) => {
            println!("note: no Wayland compositor ({e:#}) — showing the configuration, not per-output scaling");
            return Ok(());
        }
    };

    for o in &outputs {
        let cfg = config.for_output(Some(&o.name));
        // The compose target is the buffer in physical pixels; rotated
        // outputs swap the axes (the mode is reported pre-rotation).
        let (pw, ph) = if o.rotated {
            (o.height, o.width)
        } else {
            (o.width, o.height)
        };
        let dims = files
            .iter()
            .find(|(p, _)| Some(p) == cfg.image.as_ref())
            .map(|(_, f)| (f.width, f.height))
            .unwrap_or((0, 0));
        println!(
            "{} {}x{}@{} scale {} transform {}",
            o.name,
            o.width,
            o.height,
            refresh_hz(o.refresh_mhz),
            o.scale,
            o.transform
        );
        if cfg.image.is_none() && cfg.color.is_none() {
            println!("  skipped (no image or color configured for this output)");
            continue;
        }
        if pw == 0 || ph == 0 {
            println!("  (mode size unknown — cannot compute the scaling decision)");
            continue;
        }
        if let Some(img) = cfg.image.as_ref() {
            if !files.iter().any(|(p, _)| p == img) {
                bail!(
                    "image `{}` is not part of this configuration",
                    img.display()
                );
            }
        }
        println!("  {}", scaling_decision(cfg, dims.0, dims.1, pw, ph));
    }
    Ok(())
}
