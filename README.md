# wally

**A tiny Wayland wallpaper setter — one binary, every output, animated GIF and WebP that actually behave.**

[![CI](https://github.com/SONGOKU00mjk/WALLY/actions/workflows/ci.yml/badge.svg)](https://github.com/SONGOKU00mjk/WALLY/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/SONGOKU00mjk/WALLY?display_name=tag)](https://github.com/SONGOKU00mjk/WALLY/releases)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/rust-1.98%2B-orange.svg)](https://www.rust-lang.org)

> [!WARNING]
> **This is a vibe-coded project.** Most of wally was produced by prompting
> AI coding agents, with the maintainer steering, testing and reviewing the
> result — not by writing every line by hand. It has unit and integration
> tests, CI, packages and a release pipeline, but it has **not** been
> independently audited and comes with no warranty. Treat it as a hobby
> project rather than production-grade software, and read the code before
> running it somewhere that matters.

`wally` is a wallpaper setter for compositors that implement `wlr-layer-shell`
(Sway, Hyprland, river, niri, Wayfire, labwc, KDE Plasma). It is a single
self-contained binary written in Rust: no GUI toolkit, no system
Wayland/Pixman/libpng/libjpeg libraries, nothing to link against at run time.

```console
$ wally ~/pictures/wall.jpg
$ wally -i anim.webp -m fill
$ wally -o DP-1 -i left.jpg -o eDP-1 -i right.jpg
```

It stays running while the wallpaper is set, plays animations at the right
speed, and goes to **zero CPU** when nobody can see the screen.

---

## Contents

- [Why wally](#why-wally)
- [Install](#install)
- [Usage](#usage)
  - [Modes](#modes)
  - [One wallpaper per output](#one-wallpaper-per-output)
  - [Animated wallpapers](#animated-wallpapers)
  - [Rotating a folder](#rotating-a-folder)
  - [Changing wallpapers without a black flash](#changing-wallpapers-without-a-black-flash)
  - [Daemon and scripting](#daemon-and-scripting)
  - [Inspecting the session](#inspecting-the-session)
- [Formats](#formats)
- [Compositor support](#compositor-support)
- [Performance and memory](#performance-and-memory)
- [Building from source](#building-from-source)
- [Troubleshooting](#troubleshooting)
- [Contributing](#contributing)
- [License](#license)

## Why wally

- **Small and dependency-free.** One binary, pure-Rust Wayland client. No
  `libwayland`, no `pkg-config`, no C library to link at run time. About
  **1.9 MB** without the optional AVIF decoder, **4.8 MB** with it.
- **Plays animations properly.** GIF and animated WebP run at the file's
  own timing, stay in sync across monitors, share one decode, and freeze on
  the last good frame if the file breaks — instead of spinning the CPU.
- **Does nothing when nothing can see it.** Every commit carries a
  `wl_surface.frame` callback. When the output stops being presented
  (screen off, lid closed, locked, occluded), animation drops to a slow
  probe instead of rendering for nobody, and resumes the moment a frame is
  presented again.
- **Cheap frames.** A frame identical to the previous one is detected
  during the blend and never committed — the compositor is not woken. When
  a GIF only redraws a sub-rectangle, only that region is resampled,
  copied and damaged (roughly 5–8x cheaper than a full recompose at
  1080p; see [docs/benchmarks.md](docs/benchmarks.md)).
- **Honest about memory.** A still wallpaper is decoded, composed into the
  output's buffer, then the decoded pixels are freed and the buffer's
  pages are handed back to the kernel. A screen-sized GIF composites
  *directly into the scanout buffer*, so no second copy of the wallpaper
  exists anywhere.
- **Per-output control.** `-o NAME` applies the options that follow to one
  output, exactly like `swaybg`. Different wallpaper, mode, colour and
  animation setting per monitor.
- **Scriptable.** `--list-outputs --json`, `--query --json`, `--dry-run`, a
  state file, and an optional daemon with `wally set|query|restore|stop`.

## Install

### One line

```sh
curl -fsSL https://raw.githubusercontent.com/SONGOKU00mjk/WALLY/main/install.sh | sh
```

The script downloads the prebuilt glibc binary for x86_64/aarch64 when one
matches the machine, verifies it against the release's `SHA256SUMS`, and
falls back to a source build otherwise. It asks for a password only when
system packages are missing or `/usr/local/bin` has to be written.

### From source

```sh
git clone https://github.com/SONGOKU00mjk/WALLY.git
cd wally
cargo build --release
sudo install -Dm755 target/release/wally /usr/local/bin/wally
```

or through Cargo:

```sh
cargo install --git https://github.com/SONGOKU00mjk/WALLY wally
```

Requirements: Linux, Rust 1.98+, a C compiler on x86_64 (`nasm` for the
AVIF decoder's SIMD assembly). aarch64 builds are pure Rust. There are no
system library requirements.

### Packages

Every release ships native packages for x86_64 and aarch64, built from the
same verified binaries as the tarballs:

| Distribution | Package |
| --- | --- |
| Debian / Ubuntu | `wally_<version>_amd64.deb` / `..._arm64.deb` |
| Fedora / RHEL | `wally-<version>-1.x86_64.rpm` / `...aarch64.rpm` |
| Arch Linux | `wally-<version>-1-x86_64.pkg.tar.zst` / `...-aarch64...` |
| Any Linux | `wally-<triple>.tar.gz` (one glibc binary, no installer) |

Download them from the [releases page](https://github.com/SONGOKU00mjk/WALLY/releases),
or let the installer pick the right one:

```sh
sudo apt install ./wally_0.2.1_amd64.deb          # Debian / Ubuntu
sudo dnf install ./wally-0.2.1-1.x86_64.rpm       # Fedora / RHEL
sudo pacman -U wally-0.2.1-1-x86_64.pkg.tar.zst   # Arch
```

or, without knowing the asset names:

```sh
./install.sh --package        # picks .deb/.rpm/.pkg.tar.zst, needs root
```

The packages install `/usr/bin/wally`, the manual page (`man wally`), the
licence and the documentation under `/usr/share`. The portable tarball is
still the zero-dependency option: one binary, nothing else.

AUR PKGBUILDs (`wally`, `wally-git`) and a Nix flake are also in the
repository; see [packaging/README.md](packaging/README.md).

### Installer options

```sh
./install.sh --user          # install to ~/.local/bin, never root
./install.sh --package      # native .deb/.rpm/.pkg.tar.zst (needs root)
./install.sh --from-source   # always build, never download
./install.sh --prebuilt      # prebuilt only, fail instead of building
./install.sh --no-avif       # source build without AVIF (no nasm needed)
./install.sh --check         # report what this machine provides
./install.sh --uninstall     # remove every installed copy
PREFIX=/opt/wally ./install.sh
WALLY_REF=v0.2.1 ./install.sh   # a specific tag or branch
```

## Usage

```
wally [OPTIONS]
wally <IMAGE> [OPTIONS]
wally set|query|restore|stop [OPTIONS]   # talk to a --daemon instance
```

| Option | Meaning |
| --- | --- |
| `-i, --image <PATH>` | image to display (a bare path works too) |
| `-c, --color <COLOR>` | solid colour instead of an image (`#RGB` / `#RRGGBB`) |
| `-m, --mode <MODE>` | `stretch`, `fill` (default), `fit`, `center`, `tile` |
| `-o, --output <NAME>` | apply the following options to this output only |
| `-b, --background <COLOR>` | padding colour for `fit`/`center` (default `#000000`) |
| `-A, --no-animate` | show only the first frame of an animation |
| `--speed <FACTOR>` | animation speed from 0.1 to 10 (default 1) |
| `--transition <fade\|wipe>` | animate the first frame |
| `--duration <MS>` | transition length (default 300) |
| `--replace` | take over from a running wally without a black gap |
| `--rotate <DIR>` | cycle the images in a directory |
| `--interval <DUR>` | rotation period: `90s`, `30m`, `2h`, `1h30m` |
| `--shuffle` | with `--rotate`: random order instead of sorted names |
| `--daemon` | stay resident and serve the control subcommands |
| `--list-outputs` | print outputs (name, mode, scale, transform) and exit |
| `--dry-run` | validate images and print the scaling decision, draw nothing |
| `--query` | print the wallpaper last set per output |
| `--restore` | re-apply the wallpaper last set per output |
| `--scale-force <N>` | force an integer buffer scale (fractional-scale escape hatch) |
| `--json` | machine-readable output for `--list-outputs`/`--query` |
| `-v, --version` | version |
| `-h, --help` | full help |

### Modes

| Mode | Behaviour |
| --- | --- |
| `fill` | scale to cover the output, crop the overflow (default) |
| `fit` | scale to fit inside, letterbox with `--background` |
| `stretch` | distort to fill |
| `center` | 1:1, centred |
| `tile` | repeat at the original size |

```sh
wally -i photo.jpg -m fit -b '#1e1e2e'
wally -c '#1e1e2e'                      # a solid colour, no image
```

### One wallpaper per output

Options after `-o` belong to that output until the next `-o`. Outputs
that are not named keep the default group.

```sh
# different image and mode per monitor
wally -o DP-1 -i mount.jpg -m fill -o eDP-1 -i city.jpg -m fit

# animated on the external panel, first frame only on the laptop
wally -o DP-1 -i anim.webp -o eDP-1 -i anim.webp -A
```

HiDPI: wally renders at the output's integer scale. Fractional-scale
outputs are rendered at the ceiling scale and downscaled by the
compositor; `--scale-force 1` or `2` is the escape hatch when you want
1:1 logical pixels instead.

### Animated wallpapers

GIF and animated WebP are detected automatically, including single-frame
files inside an animation container (shown as stills). Frame delays follow
the browser convention, so malformed files cannot busy-loop.

```sh
wally -i loop.gif
wally -i loop.webp --speed 0.5     # half speed
wally -i loop.gif --speed 2        # twice as fast
wally -i loop.gif -A               # just the first frame
```

Animations are stepped once per tick globally: two monitors showing the
same file show the same frame at the same time, and same-size monitors
share the compose.

### Rotating a folder

`--rotate` cycles through the image files in a directory. It starts with
the first image immediately, re-reads the directory at the end of every
cycle (so adding or removing files is picked up), and skips hidden files
and non-image extensions.

```sh
wally --rotate ~/pictures/wallpapers --interval 30m
wally --rotate ~/pictures/wallpapers --interval 1h --shuffle
```

This is foreground-only by design: no history, no scheduler, no state
beyond the current image. If you want a real wallpaper manager, that is a
different tool.

### Changing wallpapers without a black flash

`pkill wally; wally -i next.png` puts a black frame on screen while the
new process starts. `--replace` avoids that: the new process connects,
decodes, commits its first frame **above** the previous instance, and only
then asks the old one to exit.

```sh
wally --replace -i ~/pictures/next.png
wally --replace --transition fade --duration 600 -i next.png
wally --replace --restore          # re-apply the last recorded wallpaper
```

Ready-made session recipes (sway systemd unit, Hyprland `exec-once`, niri
`spawn-at-startup`) live in [contrib/](contrib/README.md).

### Daemon and scripting

By default wally is a one-shot process that stays alive to drive its
animation. `--daemon` additionally opens a control socket at
`$XDG_RUNTIME_DIR/wally-<display>.sock`, so later commands change the
running instance instead of starting another one:

```sh
wally --daemon -i ~/pictures/wall.jpg &
wally set -i ~/pictures/other.png      # same rendering path, no restart
wally set -c '#1e1e2e' -m fill
wally query                            # what is on screen
wally restore                          # re-apply the recorded state
wally stop
```

The state of the last one-shot run (or daemon `set`) is recorded in
`$XDG_STATE_HOME/wally/state.conf`, one line per output. `wally --query`
reads it; `wally --restore` replays it; both work without a daemon.

### Inspecting the session

```sh
$ wally --list-outputs
DP-1 2560x1440@165Hz scale 1 transform normal

$ wally --list-outputs --json
[{"name":"DP-1","width":2560,"height":1440,"refresh_hz":165,"scale":1,"transform":"normal","rotated":false}]

$ wally --query --json | jq -r '.[] | "(.output): (.source.path // .source.hex)"'
DP-1: /home/you/pictures/wall.jpg

$ wally --dry-run -i wall.png
wall.png: 3840x2160, png, static
  default (all outputs): wall.png
DP-1 2560x1440@165Hz scale 1 transform normal
  fill: 3840x2160 -> 2560x1440 (0.67x, covers)
```

`--dry-run` uses the same decode preflight and the same placement maths as
the real paint path, but creates no surface. Without a compositor it still
validates the files and prints the configuration.

## Formats

| Kind | Formats |
| --- | --- |
| Static | PNG, JPEG, GIF, WebP, BMP, TIFF, TGA, PNM, QOI, ICO, DDS, HDR, Farbfeld |
| Animated | **GIF**, **animated WebP** (lossless and lossy) |
| Optional | AVIF (pure-Rust rav1d, on by default), JPEG XL (`--features jxl`) |

Animated files are detected by content, not just by extension. AVIF/JXL
files in a build without the feature report a clear "rebuild with …"
error instead of a generic decoder failure.

**No SVG.** Every Rust SVG rasterizer is a heavyweight dependency tree, and
wally's value is a small, auditable binary. Rasterize SVG once with
`rsvg-convert` and point wally at the PNG — a wallpaper is static, so the
cost is zero at run time.

## Compositor support

wally needs `wlr-layer-shell` + `wl_shm`. That covers Sway, Hyprland,
river, niri, Wayfire, labwc, KDE Plasma (KWin) and other wlroots- or
Smithay-based compositors. **GNOME is out of scope**: it does not
implement layer shell, and supporting it would need a Shell extension.

CI has no compositor, so the table below is documented protocol fact plus
user reports, not a CI-tested claim:

| Compositor | Status | Notes |
| --- | --- | --- |
| Sway | expected ✓ | reference wlroots layer-shell implementation |
| Hyprland | expected ✓ | fractional scale: `--scale-force` |
| niri | expected ✓ | per-workspace backdrops need a wrapper |
| river | expected ✓ | |
| Wayfire | expected ✓ | |
| labwc | expected ✓ | |
| KDE Plasma | expected ✓ | KWin implements wlr-layer-shell |
| GNOME | out of scope | no layer shell |

Reports and corrections are welcome — `wally --list-outputs` plus
`WALLY_DEBUG=1` output makes a good bug report.

## Performance and memory

- **Startup.** Distinct images are decoded in parallel before the
  compositor is touched; a broken file fails fast with a real error.
- **Still images.** One decode, composed straight into the output's
  buffer, then the decoded pixels are freed. A reconfigure re-decodes from
  disk. What stays resident is the buffer the compositor scans.
- **Animation.** A screen-sized GIF holds nothing beyond the output's own
  buffer: the canvas *is* the scanout buffer, and frames are disposed and
  blended in place. Animations that need resampling keep one private
  canvas; animated WebP keeps only the decoder's canvas. Frames are never
  prefetched.
- **Idle cost.** After every commit the buffer's pages are returned to
  the kernel with `madvise`; a still wallpaper costs RSS only while it is
  being drawn. Animated buffers intentionally stay resident (handing them
  back would re-fault the whole buffer every frame).
- **Occluded output.** No frame callbacks means no presentation, which
  means one probe step every 5 s instead of full-speed rendering.

`WALLY_DEBUG=1` prints resident-set and cumulative CPU time as they
change; `WALLY_DEBUG=2` also traces skipped commits and per-frame compose
rectangles. Method and reference numbers:
[docs/benchmarks.md](docs/benchmarks.md).

## Building from source

```sh
cargo build --release                       # default: AVIF on, JXL off
cargo build --release --no-default-features # smallest build (~1.9 MB)
cargo build --release --features jxl        # add JPEG XL decoding
cargo test --release                        # unit + integration tests
cargo bench                                 # compose/resample benchmarks
```

- MSRV: **Rust 1.98** (the AVIF decoder needs it).
- x86_64 AVIF needs `nasm`; aarch64 uses rav1d's pure-Rust path.
- No system libraries are required; `--no-default-features` needs only a
  Rust toolchain.
- `.github/workflows/ci.yml` tests fmt, clippy `-D warnings`, the test
  suite and release builds in all four feature shapes.

## Troubleshooting

**"this compositor does not support wlr-layer-shell"** — you are on GNOME
(or a compositor without layer shell). wally cannot run there.

**Nothing appears.** Run `wally --list-outputs` to confirm the compositor
is reachable, then `WALLY_DEBUG=1 wally -i …`. With per-output options,
check the spelling of `-o`: an output name that matches nothing is skipped
by design.

**Fractional scaling looks soft.** The compositor downscales wally's
integer-scale buffer. Try `--scale-force 1` for 1:1 logical pixels.

**A monitor no longer gets a wallpaper.** Hotplug is handled; the output
gains its surface when the compositor announces it. If something looks
stuck, restart wally with `--replace`.

**Daemon refuses to start.** A stale socket may be left behind after a
crash: remove `$XDG_RUNTIME_DIR/wally-*.sock`. A live daemon is never
stolen — use `wally stop`.

**CPU is not zero on a static wallpaper.** It is: after the first frame
wally sleeps in `poll`. If you see burning CPU, it is the animation path —
check `WALLY_DEBUG=1` and the file's frame delays.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). In short: `cargo fmt`,
`cargo clippy --all-targets -- -D warnings`, `cargo test`, and keep the
binary dependency-light. Bug reports with `wally --list-outputs` and
`WALLY_DEBUG=1` output are the most useful kind.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
