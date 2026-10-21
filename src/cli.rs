//! Command-line parsing, swaybg style.
//!
//! Options apply to *every* output by default. After `-o <NAME>` they apply
//! to that output only, until the next `-o`. Unmatched outputs fall back to
//! the default group.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

pub type Color = [u8; 3];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    /// Distort the image to exactly fill the output.
    Stretch,
    /// Scale to cover the output, cropping the overflow. (default)
    #[default]
    Fill,
    /// Scale to fit inside the output, letterboxing the rest.
    Fit,
    /// Show the image 1:1, centered.
    Center,
    /// Repeat the image at its original size.
    Tile,
}

impl Mode {
    pub(crate) fn parse(s: &str) -> Result<Self> {
        match s {
            "stretch" => Ok(Self::Stretch),
            "fill" | "cover" => Ok(Self::Fill),
            "fit" | "contain" => Ok(Self::Fit),
            "center" | "centre" | "none" => Ok(Self::Center),
            "tile" => Ok(Self::Tile),
            other => bail!("unknown mode `{other}` (expected: stretch, fill, fit, center, tile)"),
        }
    }

    /// The canonical spelling, as accepted by [`Mode::parse`] and used in
    /// the state file.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Stretch => "stretch",
            Self::Fill => "fill",
            Self::Fit => "fit",
            Self::Center => "center",
            Self::Tile => "tile",
        }
    }
}

/// The kind of first-frame transition `--transition` requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionKind {
    /// Ramp the whole surface's alpha from 0 to opaque.
    Fade,
    /// Reveal left-to-right behind a moving vertical edge.
    Wipe,
}

impl TransitionKind {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "fade" => Ok(Self::Fade),
            "wipe" => Ok(Self::Wipe),
            other => bail!("unknown transition `{other}` (expected: fade, wipe)"),
        }
    }
}

/// What one output needs to draw itself.
#[derive(Debug, Clone)]
pub struct OutputCfg {
    pub image: Option<PathBuf>,
    pub color: Option<Color>,
    pub mode: Mode,
    /// Letterbox color behind fit/center, and fallback when no image is set.
    pub background: Color,
    /// `false` (`-A`) shows only the first frame of an animation.
    pub animate: bool,
}

impl Default for OutputCfg {
    fn default() -> Self {
        Self {
            image: None,
            color: None,
            mode: Mode::Fill,
            background: [0, 0, 0],
            animate: true,
        }
    }
}

impl OutputCfg {
    fn is_empty(&self) -> bool {
        self.image.is_none() && self.color.is_none()
    }
}

/// Everything the process was asked to do (global options + per-output
/// groups).
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Applies to outputs without a matching `-o` group.
    pub default: OutputCfg,
    /// Per-output overrides, in the order they were given.
    pub targets: Vec<(String, OutputCfg)>,
    /// Take over from other running wally instances after the first
    /// frame is committed: the new process renders first, the old ones
    /// are asked to exit second — no black-screen gap.
    pub replace: bool,
    /// Animate the first frames (kind, duration in milliseconds) by
    /// blending against whatever is below the new surface — the previous
    /// wallpaper when `--replace` is used, the empty background otherwise.
    pub transition: Option<(TransitionKind, u64)>,
    /// Stay resident and serve `wally set|query|restore|stop` on a Unix
    /// socket. Strictly opt-in; the one-shot path is unchanged.
    pub daemon: bool,
    /// Buffer-scale override (`--scale-force N`): render at this integer
    /// scale instead of the output's reported one.
    pub scale_force: Option<u32>,
    /// Foreground rotation (`--rotate DIR --interval DUR`): cycle the
    /// images in DIR, one every interval, through the ordinary path.
    pub rotate: Option<(PathBuf, u64)>,
}

/// Parse a `--interval` duration: `"90"`, `"30s"`, `"30m"`, `"2h"`,
/// `"1h30m"` — seconds when bare, otherwise a sum of s/m/h parts.
/// At least 5 seconds: anything faster is a misuse of a wallpaper setter.
pub(crate) fn parse_interval(s: &str) -> Result<u64> {
    let t = s.trim();
    if t.is_empty() {
        bail!("empty interval");
    }
    if t.bytes().all(|b| b.is_ascii_digit()) {
        return parse_interval_secs(t.parse::<u64>().unwrap_or(u64::MAX), t);
    }
    let mut total: u64 = 0;
    let mut num: Option<u64> = None;
    let mut unit = ' ';
    let mut any = false;
    let flush = |num: &mut Option<u64>, unit: char, total: &mut u64| -> Result<()> {
        let n = num
            .take()
            .ok_or_else(|| anyhow::anyhow!("interval `{s}`: unit `{unit}` without a number"))?;
        let secs = match unit {
            's' => n,
            'm' => n
                .checked_mul(60)
                .ok_or_else(|| anyhow::anyhow!("interval `{s}` overflows"))?,
            'h' => n
                .checked_mul(3600)
                .ok_or_else(|| anyhow::anyhow!("interval `{s}` overflows"))?,
            _ => bail!("interval `{s}`: expected s, m or h"),
        };
        *total = (*total)
            .checked_add(secs)
            .ok_or_else(|| anyhow::anyhow!("interval `{s}` overflows"))?;
        Ok(())
    };
    for c in t.chars() {
        if c.is_ascii_digit() {
            if unit != ' ' {
                flush(&mut num, unit, &mut total)?;
                unit = ' ';
            }
            let n = num.get_or_insert(0);
            *n = n
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(c as u8 - b'0')))
                .ok_or_else(|| anyhow::anyhow!("interval `{s}` overflows"))?;
        } else if c == 's' || c == 'm' || c == 'h' {
            if unit != ' ' {
                bail!("interval `{s}`: two units in a row");
            }
            unit = c;
        } else {
            bail!("interval `{s}`: unexpected character `{c}`");
        }
        any = true;
    }
    if !any {
        bail!("empty interval");
    }
    if num.is_some() {
        // A pending number flushes with its unit when one was given
        // ("30m"), as seconds otherwise ("90").
        flush(&mut num, if unit == ' ' { 's' } else { unit }, &mut total)?;
    } else if unit != ' ' {
        flush(&mut num, unit, &mut total)?;
    }
    parse_interval_secs(total, s)
}

fn parse_interval_secs(total: u64, original: &str) -> Result<u64> {
    if total < 5 {
        bail!("interval `{original}` is under 5 s — this is a wallpaper setter, not a slideshow");
    }
    Ok(total)
}

impl Config {
    pub fn for_output(&self, name: Option<&str>) -> &OutputCfg {
        match name {
            Some(n) => self
                .targets
                .iter()
                .find(|(t, _)| t == n)
                .map(|(_, c)| c)
                .unwrap_or(&self.default),
            None => &self.default,
        }
    }
}

/// What the user asked the process to do.
#[derive(Debug)]
pub enum Parsed {
    Help(String),
    Version,
    /// Enumerate connected outputs and exit; no surfaces are created.
    ListOutputs,
    /// Print what wally last set, from the state file.
    Query,
    /// Re-apply what wally last set.
    Restore,
    /// A daemon subcommand: `wally set|query|restore|stop ...`.
    Control {
        action: crate::daemon::Action,
        args: Vec<String>,
    },
    /// Validate and describe the configuration without drawing anything.
    DryRun(Config),
    Run(Config),
}

const HELP_BODY: &str = "\
USAGE:
    wally [OPTIONS]
    wally <IMAGE> [OPTIONS]
    wally set|query|restore|stop [OPTIONS]   (talk to a --daemon instance)

OPTIONS:
    -i, --image <PATH>        Image to display
    -c, --color <COLOR>       Solid color instead of an image (#RGB or #RRGGBB)
    -m, --mode <MODE>         stretch | fill | fit | center | tile  [default: fill]
    -o, --output <NAME>       Apply the options that follow to output NAME
    -b, --background <COLOR>  Padding color for fit/center  [default: #000000]
    -A, --no-animate          Show only the first frame of an animation
    --replace                 Take over from other running wally instances
                              after the first frame is on screen (no
                              black-screen gap when changing wallpapers)
    --query                   Print the last set wallpaper per output
    --restore                 Re-apply the last set wallpaper per output
                              (combine with --replace to swap without a gap)
    --transition <KIND>       Animate the first frame: fade or wipe
    --duration <MS>           Transition length in milliseconds  [default: 300]
    --daemon                  Stay resident and serve the subcommands below
    --scale-force <N>         Render at this integer buffer scale instead of
                              the output's reported scale (fractional-scale
                              escape hatch)
    --rotate <DIR>            Foreground-only: cycle the images in DIR.
                              Reuses the ordinary paint path; not a
                              wallpaper manager (no schedules, no history)
    --interval <DUR>          Rotation period: 30m, 90s, 2h, 1h30m
                              [default: 30m, with --rotate only]
    --list-outputs            Print each connected output (name, resolution,
                              scale, transform), one per line, and exit
    --dry-run                 Validate the images, print the decoded
                              dimensions and the scaling decision per output,
                              then exit without creating any surface
    -h, --help                Print this help and exit
    -v, --version             Print version and exit

FORMATS:
    Static:   png jpeg webp gif bmp tiff tga pnm qoi hdr dds farbfeld ico
    Animated: gif, webp
    Optional: avif (on by default), jxl (build with `--features jxl`)

MODES:
    stretch   distort to fill          fill     scale to cover, crop (default)
    fit       scale to fit, pad        center   1:1, centered
    tile      repeat at original size

NOTES:
    Options after `-o <NAME>` apply only to that output; other outputs keep
    the default settings. wally needs a compositor implementing
    wlr-layer-shell: Sway, Hyprland, river, niri, Wayfire, labwc, KDE, …
    GNOME is out of scope.

    --dry-run enumerates outputs read-only (no surfaces are created and
    nothing is drawn); without a compositor it still validates the files
    and prints the configuration.

    Subcommands control a `--daemon` instance: `wally set -i new.png`
    applies a new configuration in the running daemon (same rendering
    path, zero CPU when occluded), `wally query` prints what it shows,
    `wally restore` re-applies the last state, `wally stop` ends it.
    (`--query` without a daemon prints the state file instead.)

EXAMPLES:
    wally ~/pictures/wall.jpg
    wally -i anim.gif -m fill
    wally -o DP-1 -i left.jpg -o eDP-1 -i right.jpg
    wally -c '#1e1e2e'
";

/// Expand a leading `~/` using $HOME (no shell involved).
fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

fn parse_color(s: &str) -> Result<Color> {
    let t = s.trim().trim_start_matches('#');
    let hex = |a: &str, b: &str| -> Result<u8> {
        u8::from_str_radix(&format!("{a}{b}"), 16).with_context(|| format!("invalid color `{s}`"))
    };
    match t.len() {
        3 => Ok([
            hex(&t[0..1], &t[0..1])?,
            hex(&t[1..2], &t[1..2])?,
            hex(&t[2..3], &t[2..3])?,
        ]),
        6 => Ok([hex(&t[0..2], "")?, hex(&t[2..4], "")?, hex(&t[4..6], "")?]),
        _ => bail!("expected a #RGB or #RRGGBB color like `#1e1e2e`, got `{s}`"),
    }
}

/// Parse the process arguments.
pub fn parse() -> Parsed {
    match parse_from(std::env::args().skip(1)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("wally: {e:#}");
            std::process::exit(2);
        }
    }
}

/// Parse an explicit argument list (the single code path; `parse` wraps it).
pub(crate) fn parse_from<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed> {
    let argv: Vec<String> = args.into_iter().collect();
    // Daemon subcommand: the first word, exactly, selects the action;
    // everything after it is re-parsed by the daemon. A wallpaper file
    // named exactly "set" must be passed as `-i set` instead.
    if let Some(action) = argv.first().and_then(|w| crate::daemon::Action::parse(w)) {
        return Ok(Parsed::Control {
            action,
            args: argv[1..].to_vec(),
        });
    }
    let mut args = argv.into_iter();
    let mut default = OutputCfg::default();
    let mut targets: Vec<(String, OutputCfg)> = Vec::new();
    // The group new options mutate: None = default (all outputs).
    let mut current: Option<String> = None;
    let mut dry_run = false;
    let mut replace = false;
    let mut restore = false;
    let mut query = false;
    let mut transition: Option<TransitionKind> = None;
    let mut duration: Option<u64> = None;
    let mut daemon = false;
    let mut scale_force: Option<u32> = None;
    let mut rotate: Option<PathBuf> = None;
    let mut interval: Option<u64> = None;

    while let Some(arg) = args.next() {
        // The group this option mutates, creating the target entry lazily.
        macro_rules! group {
            () => {{
                match &current {
                    Some(name) => {
                        if let Some(i) = targets.iter().position(|(t, _)| t == name) {
                            &mut targets[i].1
                        } else {
                            targets.push((name.clone(), OutputCfg::default()));
                            &mut targets.last_mut().expect("just pushed").1
                        }
                    }
                    None => &mut default,
                }
            }};
        }
        let mut value = |what: &str| -> Result<String> {
            args.next()
                .with_context(|| format!("{what} expects a value"))
        };

        match arg.as_str() {
            "-h" | "--help" => {
                return Ok(Parsed::Help(format!(
                    "wally {} — a minimal Wayland wallpaper setter\n\n{HELP_BODY}",
                    env!("CARGO_PKG_VERSION")
                )));
            }
            "-v" | "--version" => return Ok(Parsed::Version),
            "--list-outputs" => return Ok(Parsed::ListOutputs),
            "--dry-run" => dry_run = true,
            "--replace" => replace = true,
            "--restore" => restore = true,
            "--query" => query = true,
            "--transition" => {
                transition = Some(TransitionKind::parse(&value("--transition")?)?);
            }
            "--daemon" => daemon = true,
            "--rotate" => {
                let v = value("--rotate")?;
                let p = expand_tilde(&v);
                if !p.is_dir() {
                    bail!("--rotate expects a directory, got `{}`", p.display());
                }
                rotate = Some(p);
            }
            "--interval" => {
                let v = value("--interval")?;
                interval = Some(parse_interval(&v)?);
            }
            "--scale-force" => {
                let v = value("--scale-force")?;
                let n: u32 = v.parse().map_err(|_| {
                    anyhow::anyhow!("--scale-force expects a positive integer, got `{v}`")
                })?;
                if n == 0 {
                    bail!("--scale-force must be at least 1");
                }
                scale_force = Some(n);
            }
            "--duration" => {
                let v = value("--duration")?;
                duration =
                    Some(v.parse::<u64>().map_err(|_| {
                        anyhow::anyhow!("--duration expects milliseconds, got `{v}`")
                    })?);
            }
            "-A" | "--no-animate" => group!().animate = false,
            "-i" | "--image" => group!().image = Some(expand_tilde(&value("--image")?)),
            "-c" | "--color" => group!().color = Some(parse_color(&value("--color")?)?),
            "-m" | "--mode" => group!().mode = Mode::parse(&value("--mode")?)?,
            "-b" | "--background" => {
                group!().background = parse_color(&value("--background")?)?;
            }
            "-o" | "--output" => current = Some(value("--output")?),
            _ if arg.starts_with('-') => bail!("unknown option `{arg}` (try `wally --help`)"),
            // Bare path shorthand for the current group.
            positional => group!().image = Some(expand_tilde(positional)),
        }
    }

    if let Some(0) = duration {
        bail!("--duration must be at least 1 ms");
    }
    if duration.is_some() && transition.is_none() {
        bail!("--duration belongs to --transition");
    }
    if interval.is_some() && rotate.is_none() {
        bail!("--interval belongs to --rotate");
    }
    if rotate.is_some() && (!default.image.is_none() || !default.color.is_none()) {
        bail!("--rotate picks the images itself — pass -m/-o options for the look, not -i/-c");
    }
    if rotate.is_some()
        && targets
            .iter()
            .any(|(_, c)| c.image.is_some() || c.color.is_some())
    {
        bail!("--rotate picks the images itself — per-output -i/-c is not supported");
    }
    let config = Config {
        default,
        targets,
        replace,
        transition: transition.map(|k| (k, duration.unwrap_or(300))),
        daemon,
        scale_force,
        rotate: rotate.map(|dir| {
            // --interval belongs to --rotate; the default is 30 minutes.
            (dir, interval.unwrap_or(1800))
        }),
    };
    if query && restore {
        bail!("pass either --query or --restore, not both");
    }
    if query || restore {
        if !config.default.is_empty() || config.targets.iter().any(|(_, c)| !c.is_empty()) {
            bail!(
                "pass either {} or an image/color, not both",
                if query { "--query" } else { "--restore" }
            );
        }
        return Ok(if query {
            Parsed::Query
        } else {
            Parsed::Restore
        });
    }
    if config.default.is_empty() && config.targets.iter().all(|(_, c)| c.is_empty()) {
        bail!("nothing to show — pass an image path, or `-c '#RRGGBB'` for a solid color");
    }
    if dry_run {
        Ok(Parsed::DryRun(config))
    } else {
        Ok(Parsed::Run(config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse_args(args: &[&str]) -> Result<Config> {
        match parse_from(args.iter().map(|s| s.to_string()))? {
            Parsed::Run(cfg) => Ok(cfg),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn colors_parse() {
        assert_eq!(parse_color("#1e1e2e").unwrap(), [0x1e, 0x1e, 0x2e]);
        assert_eq!(parse_color("ffffff").unwrap(), [255, 255, 255]);
        assert_eq!(parse_color("#f00").unwrap(), [255, 0, 0]);
        assert!(parse_color("#12345").is_err());
        assert!(parse_color("#12zz34").is_err());
        assert!(parse_color("").is_err());
    }

    #[test]
    fn modes_parse() {
        assert_eq!(Mode::parse("fill").unwrap(), Mode::Fill);
        assert_eq!(Mode::parse("cover").unwrap(), Mode::Fill);
        assert_eq!(Mode::parse("contain").unwrap(), Mode::Fit);
        assert_eq!(Mode::parse("centre").unwrap(), Mode::Center);
        assert!(Mode::parse("zoomy").is_err());
    }

    #[test]
    fn default_group_applies_to_all_outputs() {
        let cfg = parse_args(&["-i", "a.png", "-m", "tile"]).unwrap();
        assert_eq!(cfg.for_output(Some("DP-1")).mode, Mode::Tile);
        assert_eq!(
            cfg.for_output(Some("eDP-1")).image.as_deref().unwrap(),
            Path::new("a.png")
        );
        assert_eq!(
            cfg.for_output(None).image.as_deref().unwrap(),
            Path::new("a.png")
        );
    }

    #[test]
    fn output_groups_override_default() {
        let cfg = parse_args(&[
            "-i", "a.png", "-o", "DP-1", "-i", "b.png", "-m", "stretch", "-o", "eDP-1", "-i",
            "c.png",
        ])
        .unwrap();

        // DP-1: b.png + stretch; eDP-1: c.png + default mode;
        // everyone else: a.png + default mode.
        let dp = cfg.for_output(Some("DP-1"));
        assert_eq!(dp.image.as_deref().unwrap(), Path::new("b.png"));
        assert_eq!(dp.mode, Mode::Stretch);
        let edp = cfg.for_output(Some("eDP-1"));
        assert_eq!(edp.image.as_deref().unwrap(), Path::new("c.png"));
        assert_eq!(edp.mode, Mode::Fill);
        let other = cfg.for_output(Some("HDMI-A-1"));
        assert_eq!(other.image.as_deref().unwrap(), Path::new("a.png"));
        assert_eq!(other.mode, Mode::Fill);
    }

    #[test]
    fn reusing_output_name_merges() {
        let cfg = parse_args(&["-o", "DP-1", "-i", "a.png", "-o", "DP-1", "-m", "tile"]).unwrap();
        let dp = cfg.for_output(Some("DP-1"));
        assert_eq!(dp.image.as_deref().unwrap(), Path::new("a.png"));
        assert_eq!(dp.mode, Mode::Tile);
        assert_eq!(cfg.targets.len(), 1);
    }

    #[test]
    fn rejects_empty_config() {
        assert!(parse_args(&["-m", "tile"]).is_err());
    }

    #[test]
    fn rejects_unknown_option() {
        assert!(parse_args(&["--frobnicate"]).is_err());
    }

    #[test]
    fn help_and_version_are_actions() {
        assert!(matches!(
            parse_from(["--help".to_string()]),
            Ok(Parsed::Help(_))
        ));
        assert!(matches!(
            parse_from(["-v".to_string()]),
            Ok(Parsed::Version)
        ));
    }

    #[test]
    fn daemon_subcommands_route_as_control() {
        let parsed =
            parse_from(["set".to_string(), "-i".to_string(), "x.png".to_string()]).unwrap();
        let Parsed::Control { action, args } = parsed else {
            panic!("expected Control");
        };
        assert_eq!(action, crate::daemon::Action::Set);
        assert_eq!(args, vec!["-i".to_string(), "x.png".to_string()]);
        for word in ["query", "restore", "stop"] {
            let parsed = parse_from([word.to_string()]).unwrap();
            assert!(matches!(parsed, Parsed::Control { .. }));
        }
        // Not a subcommand: "s" is an ordinary bare-path one-shot.
        assert!(matches!(parse_from(["s".to_string()]), Ok(Parsed::Run(_))));
        let parsed = parse_from([
            "--daemon".to_string(),
            "-i".to_string(),
            "x.png".to_string(),
        ])
        .unwrap();
        let Parsed::Run(cfg) = parsed else {
            panic!("expected Run");
        };
        assert!(cfg.daemon);
    }

    #[test]
    fn interval_parser_table() {
        assert_eq!(parse_interval("90").unwrap(), 90);
        assert_eq!(parse_interval("90s").unwrap(), 90);
        assert_eq!(parse_interval("30m").unwrap(), 1800);
        assert_eq!(parse_interval("2h").unwrap(), 7200);
        assert_eq!(parse_interval("1h30m").unwrap(), 5400);
        assert_eq!(parse_interval("2h15s").unwrap(), 7215);
        assert!(parse_interval("4s").is_err(), "under the 5 s floor");
        assert!(parse_interval("").is_err());
        assert!(parse_interval("30x").is_err());
        assert!(parse_interval("hm").is_err());
        assert_eq!(
            parse_interval("1h30").unwrap(),
            3630,
            "trailing bare number = seconds"
        );
    }

    #[test]
    fn rotate_requires_a_directory_and_no_explicit_image() {
        assert!(parse_args(&["--rotate", "/tmp", "-i", "a.png"]).is_err());
        assert!(parse_args(&["--rotate", "/definitely/not/a/dir"]).is_err());
        assert!(parse_args(&["--interval", "10m", "-i", "a.png"]).is_err());
    }

    #[test]
    fn scale_force_parses_and_rejects_zero() {
        let cfg = parse_args(&["--scale-force", "2", "-i", "a.png"]).unwrap();
        assert_eq!(cfg.scale_force, Some(2));
        assert!(parse_args(&["--scale-force", "0", "-i", "a.png"]).is_err());
        assert!(parse_args(&["--scale-force", "x", "-i", "a.png"]).is_err());
        assert!(parse_args(&["-i", "a.png"]).unwrap().scale_force.is_none());
    }

    #[test]
    fn transition_parses_with_optional_duration() {
        let cfg = parse_args(&["--transition", "fade", "-i", "a.png"]).unwrap();
        assert_eq!(cfg.transition, Some((TransitionKind::Fade, 300)));
        let cfg =
            parse_args(&["--transition", "wipe", "--duration", "1200", "-i", "a.png"]).unwrap();
        assert_eq!(cfg.transition, Some((TransitionKind::Wipe, 1200)));
        assert!(parse_args(&["--transition", "zoom", "-i", "a.png"]).is_err());
        assert!(parse_args(&["--duration", "100", "-i", "a.png"]).is_err());
        assert!(parse_args(&["--transition", "fade", "--duration", "0", "-i", "a.png"]).is_err());
    }

    #[test]
    fn query_and_restore_are_standalone_actions() {
        assert!(matches!(
            parse_from(["--query".to_string()]),
            Ok(Parsed::Query)
        ));
        assert!(matches!(
            parse_from(["--restore".to_string()]),
            Ok(Parsed::Restore)
        ));
        // --restore rejects an image alongside it.
        assert!(parse_from(["--restore".to_string(), "x.png".to_string()]).is_err());
    }

    #[test]
    fn replace_is_a_global_flag() {
        let cfg = parse_args(&["--replace", "-i", "a.png"]).unwrap();
        assert!(cfg.replace);
        let cfg = parse_args(&["-i", "a.png"]).unwrap();
        assert!(!cfg.replace);
    }

    #[test]
    fn list_outputs_is_a_standalone_action() {
        assert!(matches!(
            parse_from(["--list-outputs".to_string()]),
            Ok(Parsed::ListOutputs)
        ));
    }

    #[test]
    fn dry_run_carries_the_config() {
        let parsed = parse_from([
            "--dry-run".to_string(),
            "-i".to_string(),
            "a.png".to_string(),
            "-m".to_string(),
            "tile".to_string(),
        ])
        .unwrap();
        let Parsed::DryRun(cfg) = parsed else {
            panic!("expected DryRun");
        };
        assert_eq!(cfg.for_output(None).mode, Mode::Tile);
        assert_eq!(
            cfg.for_output(Some("DP-1")).image.as_deref().unwrap(),
            Path::new("a.png")
        );
    }

    #[test]
    fn dry_run_still_rejects_an_empty_config() {
        assert!(parse_from(["--dry-run".to_string()]).is_err());
    }
}
