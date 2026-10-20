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
    fn parse(s: &str) -> Result<Self> {
        match s {
            "stretch" => Ok(Self::Stretch),
            "fill" | "cover" => Ok(Self::Fill),
            "fit" | "contain" => Ok(Self::Fit),
            "center" | "centre" | "none" => Ok(Self::Center),
            "tile" => Ok(Self::Tile),
            other => bail!("unknown mode `{other}` (expected: stretch, fill, fit, center, tile)"),
        }
    }
}

/// Everything one output needs to draw itself.
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

#[derive(Debug, Clone)]
pub struct Config {
    /// Applies to outputs without a matching `-o` group.
    pub default: OutputCfg,
    /// Per-output overrides, in the order they were given.
    pub targets: Vec<(String, OutputCfg)>,
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
    Run(Config),
}

const HELP_BODY: &str = "\
USAGE:
    wally [OPTIONS]
    wally <IMAGE> [OPTIONS]

OPTIONS:
    -i, --image <PATH>        Image to display
    -c, --color <COLOR>       Solid color instead of an image (#RGB or #RRGGBB)
    -m, --mode <MODE>         stretch | fill | fit | center | tile  [default: fill]
    -o, --output <NAME>       Apply the options that follow to output NAME
    -b, --background <COLOR>  Padding color for fit/center  [default: #000000]
    -A, --no-animate          Show only the first frame of an animation
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
fn parse_from<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed> {
    let mut args = args.into_iter();
    let mut default = OutputCfg::default();
    let mut targets: Vec<(String, OutputCfg)> = Vec::new();
    // The group new options mutate: None = default (all outputs).
    let mut current: Option<String> = None;

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
            args.next().with_context(|| format!("{what} expects a value"))
        };

        match arg.as_str() {
            "-h" | "--help" => {
                return Ok(Parsed::Help(format!(
                    "wally {} — a minimal Wayland wallpaper setter\n\n{HELP_BODY}",
                    env!("CARGO_PKG_VERSION")
                )));
            }
            "-v" | "--version" => return Ok(Parsed::Version),
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

    let config = Config { default, targets };
    if config.default.is_empty() && config.targets.iter().all(|(_, c)| c.is_empty()) {
        bail!("nothing to show — pass an image path, or `-c '#RRGGBB'` for a solid color");
    }
    Ok(Parsed::Run(config))
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
        assert_eq!(cfg.for_output(Some("eDP-1")).image.as_deref().unwrap(), Path::new("a.png"));
        assert_eq!(cfg.for_output(None).image.as_deref().unwrap(), Path::new("a.png"));
    }

    #[test]
    fn output_groups_override_default() {
        let cfg = parse_args(&[
            "-i",
            "a.png",
            "-o",
            "DP-1",
            "-i",
            "b.png",
            "-m",
            "stretch",
            "-o",
            "eDP-1",
            "-i",
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
        assert!(matches!(parse_from(["--help".to_string()]), Ok(Parsed::Help(_))));
        assert!(matches!(parse_from(["-v".to_string()]), Ok(Parsed::Version)));
    }
}
