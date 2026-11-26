# Changelog

All notable changes to wally are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
inside the 0.x line.

## [0.2.0] — 2026-11-26

The "it actually works on a real compositor" release. The project line is
re-numbered to 0.2.0: the old 1.0.0 tag was aspirational, and this release
is the first one whose documented behaviour is verified against a live
wlr-layer-shell session (niri, 1080p, 180 Hz) rather than only reasoned
about from protocol text.

### Added

- `--speed <FACTOR>`: animation playback rate from 0.1x to 10x. Applies
  to GIF and animated WebP, is floored at 5 ms per frame (so 10x cannot
  busy-loop the event loop), and is part of the source identity, so a
  daemon `set --speed` reopens the animation instead of reusing it.
- `--shuffle`: with `--rotate`, visit the directory in random order
  (Fisher-Yates, seeded from the clock and pid; no new dependency). Every
  file is still shown once per cycle, and each new cycle is reseeded.
- `--json`: machine-readable output for `--list-outputs` and `--query`
  (a JSON array each), with a hand-rolled escaper so paths containing
  quotes, backslashes or control characters stay valid. Useful for bars,
  pickers and shell scripts; `--json` on a normal run is rejected with a
  clear message instead of being silently ignored.
- Rotation now paints the first image immediately at startup, re-reads the
  directory when a cycle ends (files added or removed while running are
  picked up), skips hidden files, and fails fast at startup when the
  directory holds no supported images instead of after the first interval.
- A real end-to-end verification pass on a live compositor: per-output
  configuration, rotation, transitions, the daemon subcommands, and the
  state file are now exercised, not just unit-tested.
- Unit tests for rotation (`RotateState` cycling, empty-directory error,
  shuffle permutation), speed scaling, JSON escaping, JSON number
  formatting, and the new CLI validation rules; integration tests for
  `--query --json`, empty state, and rejected `--json`.
- README rewritten around the actual workflow: install, per-output,
  animation, rotation, transitions, daemon/scripting, inspection, formats,
  performance, troubleshooting.

### Fixed

- **`--rotate` was unusable.** The empty-configuration check ran after the
  rotation directory had already been consumed, so `wally --rotate DIR`
  always exited with "nothing to show" — and adding `-i` was rejected
  because rotation picks its own images. Rotation is now a complete
  configuration on its own.
- **Per-output options were ignored on compositors that send
  `wl_output.done` before `wl_output.name`** (niri does). Surfaces were
  created against the default group while the output was still unnamed, and
  the name was never applied afterwards: `-o DP-1 -i …` skipped the output
  and `--query` recorded nothing. The name event now re-resolves the
  output's group, destroys a surface that matches no configured group, and
  re-records the state after repainting.
- **The daemon kept a stale configuration.** `apply_config` never stored
  the new config, so a monitor plugged in after `wally set …` was resolved
  against the config the process started with (and a later rotation step
  could restore an old image). The live config is now updated with the
  applied one.
- Integration tests assumed the machine had no compositor, so they failed
  on any developer session (and on CI with a headless compositor). They now
  scrub `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR` and use a temporary
  `XDG_STATE_HOME`.
- Installer: the install prefix is created before testing whether it is
  writable (the old test could ask for a password unnecessarily, or fail
  after the `mkdir`); `SHA256SUMS` matching uses `grep -F` so dots in the
  tarball name cannot act as regex wildcards; Arch package installation no
  longer performs a partial upgrade (`pacman -Sy`).
- Metadata: repository URL casing (`wally`, not `WALLY`), removed the
  redundant `homepage` field that made every `cargo` invocation warn, and
  synced the version across Cargo, the AUR PKGBUILDs and the Nix flake.

### Changed

- Version renumbered to **0.2.0**. Releases and tags from the retired
  1.0.0 line (`v0.5.7` … `v1.0.0`) have been removed from GitHub; the
  source history and this file's earlier content remain in git.
- README no longer claims a 4.0 MB binary: measured release builds are
  ~1.9 MB without AVIF and ~4.8 MB with it on the current toolchain.
- The project name is consistently spelled `wally` in documentation,
  packaging metadata and installer output.

### Removed

- Stale `dependabot/*` branches left over from automated dependency PRs.

## Retired versions

Versions up to and including 1.0.0 were published before the project was
re-versioned. Their tags and release artifacts are no longer available on
GitHub; the full history, including the original 1.0.0 changelog entry, is
preserved in the git history of this file.
