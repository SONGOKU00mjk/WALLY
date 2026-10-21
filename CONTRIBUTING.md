# Contributing to WALLY

Thank you for wanting to make wally better. This document covers the
project's ground rules and how to work on it. WALLY is deliberately small:
one binary, no GUI toolkit, no heavyweight dependencies, a one-shot CLI that
stays the default. Changes that grow the project without growing what it
does for users are usually rejected.

## Prerequisites

- A Rust toolchain ≥ 1.98 (`rustup` stable is fine) — 1.98 is required by
  the AVIF decoder (rav1d); the rest of the tree is older-friendly.
- On x86_64: `nasm` (the AVIF decoder's SIMD assembly). aarch64 needs
  nothing beyond the Rust target.
- `git` and any C compiler for linking (the code itself is pure Rust).
- For animation work: the fixtures under `tests/fixtures/` and a
  layer-shell compositor (sway, Hyprland, niri, river, …) for manual
  checks. CI and the test suite run without a compositor.

## Building and testing

```sh
cargo fmt --check                                        # formatting
cargo clippy --all-targets -- -D warnings                # lints, warnings are errors
cargo test                                               # unit + integration tests
cargo build --release                                    # the binary
```

Every change must build in **all four feature shapes** — the slim
`--no-default-features` binary (1.45 MB, no AVIF) is a documented product,
and the `jxl` feature must stay optional in both directions:

```sh
cargo build --release
cargo build --release --no-default-features
cargo build --release --features jxl
cargo build --release --no-default-features --features jxl
```

## Fixture conventions (animation changes)

Animation regressions are pinned by fixtures in `tests/fixtures/`:

- `anim.gif` / `anim.webp` — normal multi-frame animations;
- `anim-repeat.gif` — explicit loop extension;
- `anim-lossy.webp` — lossy VP8 frames;
- `still.gif` / `still.webp` / `photo.png` — static images that must never
  animate.

When you change animation behaviour, add the smallest fixture that
demonstrates the case and wire it into the existing
`*_sequence_repeats_after_eof`-style regression tests. Truncated or
malformed containers are generated at test time by slicing valid fixtures —
do not commit broken binary blobs when a slice will do.

## Commit style

One concern per commit. The message subject says what changes, the body
says why and how it was verified. No drive-by reformatting of files a
change does not otherwise touch. Example:

```
render: skip the second pass when the dirty rect spans the full canvas

The parity test ... pins byte-exactness; measured 1.9x faster ...
```

## Non-negotiables

1. **No GUI toolkit** — no GTK, Qt, Electron, webview. `wlr-layer-shell` +
   pure Rust only.
2. **No new heavyweight dependencies.** State why `std` or an existing
   dependency cannot do the job before adding a crate. Test/bench-only
   crates (`proptest`, `criterion`, `assert_cmd`, `predicates`) are
   pre-approved as `[dev-dependencies]`.
3. **The one-shot path stays the default and stays correct.**
   `wally image.png` must keep working exactly as it does now. Any daemon
   mode is strictly opt-in and must reuse the existing compose/animation
   loop.
4. **Do not regress the performance identity** — zero CPU when nothing is
   presented, identical-frame skip, sub-rect recompose, `madvise` handback.
   Say so before editing if your change touches those paths.
5. **No public CLI or semantics break.** New flags are additive.
6. **Every change passes** fmt, clippy `-D warnings`, tests, and the four
   feature-shape builds above.

## Testing the installer

The installer is verified against a mock release server so you never need
to cut a real release to test it:

```sh
# one-time: build the artifacts the mock serves
mkdir -p dist-ci-sim
tar czf dist-ci-sim/wally-x86_64-unknown-linux-gnu.tar.gz -C target/x86_64-unknown-linux-gnu/release wally
(cd dist-ci-sim && sha256sum wally-*.tar.gz > SHA256SUMS)

# serve and point the installer at it
python3 -m http.server 8731 --bind 127.0.0.1   # from dist-ci-sim's parent
WALLY_RELEASE_URL=http://127.0.0.1:8731/download \
WALLY_REF=<tag-with-assets> ./install.sh --user PREFIX=/tmp/wally-test
```

`WALLY_RELEASE_URL` replaces the GitHub release download base; `WALLY_REF`
selects the tag directory. `PREFIX` keeps the test off your system paths.

## Repository metadata (maintainers)

The repository's discoverability settings are reproducible with `gh`:

```sh
# homepage + discussions
gh repo edit SONGOKU00mjk/WALLY \
  --homepage "https://github.com/SONGOKU00mjk/WALLY" \
  --enable-discussions

# topics (max 20; these are the curated set)
gh repo edit SONGOKU00mjk/WALLY \
  --add-topic wayland --add-topic wallpaper --add-topic wallpaper-setter \
  --add-topic swaybg-alternative --add-topic animated-wallpaper \
  --add-topic gif --add-topic webp --add-topic layer-shell \
  --add-topic rust --add-topic niri --add-topic hyprland \
  --add-topic sway
```

`Cargo.toml` mirrors the same metadata (`homepage`, `keywords`,
`categories`) so crates.io and GitHub agree.

## Releases

Releases are tag-triggered: push a `vX.Y.Z` tag and the release workflow
builds, tests, smoke-runs, and attaches `wally-<triple>.tar.gz` +
`SHA256SUMS` for x86_64 and aarch64 glibc to the GitHub Release. The
asset names are a contract with `install.sh` — do not rename them without
updating the installer and `.github/workflows/release.yml` together.

## Questions

Open a Discussion for design questions before building large things; open
an issue (with `WALLY_DEBUG=1` output) for bugs.
