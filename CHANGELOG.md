# Changelog

All notable changes to wally are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); 1.0.0 starts the
stability contract — breaking CLI or behavior changes will be called out
here and gated behind a major version.

## [1.0.0] — 2026-10-22

First tagged and packaged release. The 0.7.0 work below was distributed as
source snapshots before the release pipeline existed; it ships together
with the adoption infrastructure as 1.0.0.

### Added

- Presentation gating: `wl_surface.frame` callbacks drive animation, and
  when nothing can be presented (screen off, lid closed, locked, occluded
  output) the animation drops to a 5-second probe instead of rendering for
  nobody — animation CPU goes to ~0 whenever no output is visible, and
  resumes full speed on the first presented frame.
- `--list-outputs`: connector name, current mode (resolution and refresh),
  scale, and transform, one per line, from a read-only client that never
  creates a surface.
- `--dry-run`: validates every image through the same parallel decode
  preflight a real run uses, prints decoded dimensions/format/animation
  and the per-output scaling decision from the same placement math the
  painter uses; without a compositor it degrades to file validation.
- `--replace`: take over from older wally instances after the first frame
  is committed — no black-screen gap when changing wallpapers, no
  `pkill; sleep 1` loop.
- `--transition fade|wipe [--duration MS]`: animate the first frame by
  blending against the previous wallpaper (ARGB buffers only while the
  transition runs; the pipeline returns to plain XRGB afterwards).
- State record under `$XDG_STATE_HOME/wally/state.conf` with `--query`
  (print what is set) and `--restore` (re-apply it).
- Opt-in daemon: `wally --daemon` serves `wally set|query|restore|stop`
  over a Unix socket in `$XDG_RUNTIME_DIR`; the daemon reuses the exact
  compose/animation path, so zero-CPU-when-occluded, identical-frame skip
  and sub-rect damage all apply inside it. One-shot stays the default.
- `--scale-force N`: buffer-scale escape hatch for fractional-scale
  outputs (the integer mechanism, chosen deliberately over
  viewport/buffer_scale mixing).
- `--rotate DIR --interval DUR`: foreground-only image cycling through the
  ordinary paint path; not a wallpaper manager.
- Criterion benchmarks (`cargo bench`) for full vs sub-rect recompose,
  wally's Triangle compose vs `image`'s generic resize, and animated WebP
  frame decode; methodology and reference numbers in `docs/benchmarks.md`.
- Resampler property tests (no panics, XRGB invariant, 1:1 identity within
  ±1 LSB, rect-vs-full parity) and adversarial container tests (truncated
  GIF/WebP, single-frame ANIM containers, extension/content mismatch,
  1x1 and extreme-aspect sources), plus a full GIF/WebP delay-clamp table.
- Feature-matrix CI (`ci.yml`): fmt, clippy `-D warnings`, tests and
  release builds in all four feature shapes, plus install.sh syntax
  checks, on every push and PR.
- Tag-triggered release workflow shipping verified, smoke-tested glibc
  tarballs (`wally-<triple>.tar.gz` + `SHA256SUMS`) for x86_64 and aarch64.
- AUR PKGBUILD (+ `-git`) and a Nix flake, both building the default
  feature set so packaged binaries match what install.sh produces.
- `install.sh` now installs the prebuilt release binary by default (with
  `SHA256SUMS` verification) and falls back to a source build only when no
  artifact matches the machine; `--from-source` skips the prebuilt attempt,
  `--no-avif` implies a source build, `WALLY_RELEASE_URL` supports mirrors,
  and `--check` reports the detected libc.
- Session-integration recipes under `contrib/` (sway systemd user unit,
  Hyprland `exec-once`, niri `spawn-at-startup`) with the one-shot loop
  problem documented.
- Contributor scaffolding: dependabot (grouped weekly), issue templates,
  PR checklist, CONTRIBUTING.md, CHANGELOG.md.
- `Cargo.toml` metadata: homepage, readme, keywords, categories.
- Extension-based format hints: a `.jxl`/`.avif` file in a build without
  that feature reports "support is not compiled in; rebuild with …"
  instead of a generic decoder error.

### Fixed

- `install.sh --prebuilt` could never verify a real release: the downloaded
  tarball was saved under a name the release `SHA256SUMS` does not list.

### Changed

- The slim `--no-default-features` build and the `jxl` feature are built
  and tested in CI in every combination, not assumed.
- Version 1.0.0: the CLI (`-i -c -m -o -b -A` plus the flags above) and the
  one-shot default behavior are a stable contract from here on; breaking
  changes will require a 2.x and will be called out in this file.

[1.0.0]: https://github.com/SONGOKU00mjk/WALLY/releases/tag/v1.0.0
