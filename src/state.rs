//! The state record: what wally last set, per output, under
//! `$XDG_STATE_HOME/wally/state.conf` (default `~/.local/state/...`).
//!
//! Every successful run rewrites the file after its first frame round is
//! on screen; `--query` reads it, `--restore` re-applies it. The format
//! is one tab-separated line per output — deliberately trivial, parseable
//! by hand and by scripts:
//!
//! ```text
//! DP-1        fill    true    000000  image   /home/u/pictures/wall.png
//! eDP-1       fill    true    1e1e2e  color   1e1e2e
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::{Color, Config, Mode, OutputCfg};

/// What one output is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSel {
    Image(PathBuf),
    Color(Color),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateEntry {
    pub output: String,
    pub source: SourceSel,
    pub mode: Mode,
    pub background: Color,
    pub animate: bool,
}

fn color_hex(c: &Color) -> String {
    format!("{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn parse_color_hex(s: &str) -> Option<Color> {
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some([
        u8::from_str_radix(&s[0..2], 16).ok()?,
        u8::from_str_radix(&s[2..4], 16).ok()?,
        u8::from_str_radix(&s[4..6], 16).ok()?,
    ])
}

/// `$XDG_STATE_HOME/wally/state.conf`, with the XDG default applied.
pub fn state_file() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".local/state")
        });
    base.join("wally/state.conf")
}

/// Rewrite the state file atomically (temp file + rename, directory
/// created on demand). Best-effort from the caller's perspective: the
/// error is returned, but the wallpaper is already set.
pub fn write_to(path: &Path, entries: &[StateEntry]) -> Result<()> {
    let mut body = String::from("# wally state; rewritten on every successful set\n");
    for e in entries {
        let (kind, payload) = match &e.source {
            SourceSel::Image(p) => ("image", p.display().to_string()),
            SourceSel::Color(c) => ("color", color_hex(c)),
        };
        body.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            e.output,
            e.mode.name(),
            if e.animate { "true" } else { "false" },
            color_hex(&e.background),
            kind,
            payload
        ));
    }

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create the state directory `{}`", dir.display()))?;
    }
    let tmp = path.with_extension("conf.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("could not write `{}`", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("could not update `{}`", path.display()))?;
    Ok(())
}

/// Parse a state file. Unparseable lines are skipped (an older or
/// hand-edited file must not make --restore fail wholesale).
pub fn read_from(path: &Path) -> Vec<StateEntry> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in body.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        let [output, mode, animate, bg, kind, payload] = f.as_slice() else {
            continue;
        };
        let Ok(mode) = Mode::parse(mode) else {
            continue;
        };
        let Some(background) = parse_color_hex(bg) else {
            continue;
        };
        let source = match *kind {
            "image" => Some(SourceSel::Image(PathBuf::from(*payload))),
            "color" => parse_color_hex(payload).map(SourceSel::Color),
            _ => None,
        };
        if let Some(source) = source {
            out.push(StateEntry {
                output: (*output).to_string(),
                source,
                mode,
                background,
                animate: *animate == "true",
            });
        }
    }
    out
}

pub fn load() -> Vec<StateEntry> {
    read_from(&state_file())
}

/// Turn recorded state into a config that re-applies exactly it: one
/// `-o` group per recorded output. The default group stays empty, so
/// outputs that were not recorded (or are new) are skipped — restoring
/// must not invent anything.
pub fn to_config(entries: &[StateEntry]) -> Config {
    Config {
        default: OutputCfg::default(),
        targets: entries
            .iter()
            .map(|e| {
                let mut cfg = OutputCfg {
                    mode: e.mode,
                    background: e.background,
                    animate: e.animate,
                    ..OutputCfg::default()
                };
                match &e.source {
                    SourceSel::Image(p) => cfg.image = Some(p.clone()),
                    SourceSel::Color(c) => cfg.color = Some(*c),
                }
                (e.output.clone(), cfg)
            })
            .collect(),
        replace: false,
        transition: None,
        daemon: false,
        scale_force: None,
        rotate: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Mode;

    fn tmp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wally-state-{}-{tag}.conf", std::process::id()))
    }

    #[test]
    fn roundtrip_preserves_every_field() {
        let path = tmp_path("rt");
        let entries = vec![
            StateEntry {
                output: "DP-1".into(),
                source: SourceSel::Image(PathBuf::from("/home/u/pic with space.png")),
                mode: Mode::Fit,
                background: [0x1e, 0x1e, 0x2e],
                animate: false,
            },
            StateEntry {
                output: "eDP-1".into(),
                source: SourceSel::Color([0, 255, 16]),
                mode: Mode::Tile,
                background: [0, 0, 0],
                animate: true,
            },
        ];
        write_to(&path, &entries).unwrap();
        let loaded = read_from(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded, entries);
    }

    #[test]
    fn to_config_targets_recorded_outputs_only() {
        let entries = vec![StateEntry {
            output: "DP-1".into(),
            source: SourceSel::Image(PathBuf::from("/x.png")),
            mode: Mode::Fill,
            background: [0, 0, 0],
            animate: true,
        }];
        let cfg = to_config(&entries);
        assert!(cfg.default.image.is_none() && cfg.default.color.is_none());
        assert_eq!(cfg.targets.len(), 1);
        assert_eq!(
            cfg.for_output(Some("DP-1")).image.as_deref().unwrap(),
            Path::new("/x.png")
        );
        // An output that was never recorded falls through to the empty
        // default group — nothing is invented.
        assert!(cfg.for_output(Some("HDMI-A-1")).image.is_none());
    }

    #[test]
    fn garbage_lines_are_skipped_not_fatal() {
        let path = tmp_path("bad");
        std::fs::write(
            &path,
            "# comment\nbroken line\nDP-1\tfill\ttrue\t000000\timage\t/x.png\nDP-2\tzoomy\ttrue\tzz\timage\t/y.png\n",
        )
        .unwrap();
        let loaded = read_from(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].output, "DP-1");
    }
}
