# wally

A minimal Wayland wallpaper setter, written in Rust. One small binary, one
image, every output — like `swaybg`, with first-class GIF and **animated
WebP** support.

```
wally ~/pictures/wall.jpg
wally -i anim.gif -m fill
wally -o DP-1 -i left.jpg -o eDP-1 -i right.jpg
```

## Why wally

- **Tiny**: one self-contained binary, no runtime libraries (the Wayland client is
  pure Rust; nothing needs libwayland, libpng, or libjpeg at run time).
- **Everyday formats**: PNG, JPEG, GIF, WebP, BMP, TIFF, TGA, PNM, QOI,
  ICO, DDS, HDR, Farbfeld — plus AVIF (pure-Rust rav1d) and JPEG XL
  (jxl-oxide) behind optional features.
- **Animated wallpapers that behave**: GIF *and* animated WebP play at the
  correct speed, stay in sync across monitors, and freeze gracefully on
  their last good frame if the stream is broken — instead of spinning the
  CPU.
- **Swaybg-style output control**: options apply to all outputs, or only to
  the output named by the preceding `-o`.
- **Polite about memory**: a static image is decoded once, composed
  straight into the output's buffer, then released. After every paint the
  buffer's pages are handed back to the kernel (`madvise`) — the wallpaper
  lives on in the memfd the compositor reads, **not in wally's RSS**, so a
  still wallpaper costs resident memory only for the moment it is drawn.
  A screen-sized GIF (wallpaper natively matching an output's buffer —
  the usual case) composites **directly into that buffer**: the frame on
  screen *is* the GIF canvas, so no private copy of the wallpaper exists
  anywhere. Other animations keep exactly one canvas, and the next frame
  is never prefetched. Identical monitors share one compose and pay a
  plain copy each. Animated surfaces keep their (changing) framebuffer
  resident between frames — handing its pages back would just buy a
  full re-fault of the buffer every single frame — and still wallpapers
  keep the after-paint handback.
- **Polite about CPU too**: a frame that decodes to the same pixels as
  the previous one (optimized GIFs repeat frames all the time to pace
  playback) is detected during the blend — nothing is written, no copy
  is made and **the compositor is never woken** for that frame. When a
  GIF frame only redraws a sub-rect of the canvas — which is most real
  GIFs — only that region is resampled, copied to other monitors and
  damaged at the compositor: measured 5x cheaper per frame than a full
  recompose on a 1080p panel. Resample filter tables are built once per
  geometry, not once per frame.
- **Observable**: set `WALLY_DEBUG=1` and wally prints its resident set
  (in kB) whenever it changes plus its cumulative CPU time (in ms) as it
  grows — handy to compare honestly against `swaybg` or `awww` in htop.
  `WALLY_DEBUG=2` also traces skipped commits and per-frame compose
  rects.

## Formats

| Kind     | Formats                                                                 |
| -------- | ----------------------------------------------------------------------- |
| Static   | png, jpeg, gif, webp, bmp, tiff, tga, pnm, qoi, hdr, dds, ff, ico       |
| Animated | **gif**, **webp** (animated WebP, lossless and lossy)                   |
| Optional | avif (on by default), jxl (build with `--features jxl`)                 |

Animated GIFs and animated WebPs are detected automatically; a still image
inside an animation container is shown as a static wallpaper. Frame delays
follow the browser convention (0–1 cs GIF delays clamp to 100 ms, with a
20 ms floor for everything) so malformed files can never busy-loop.

## Install

Copy this one line into your terminal and press Enter — nothing needs to be
cloned first. The script detects your distribution, installs the build
dependencies with your package manager, clones the repository, builds wally
and installs it. You are only asked for your password when system packages
are missing or `/usr/local/bin` needs to be written:

```sh
curl -fsSL https://raw.githubusercontent.com/SONGOKU00mjk/WALLY/main/install.sh | sh
```

`wget` works too:

```sh
wget -qO- https://raw.githubusercontent.com/SONGOKU00mjk/WALLY/main/install.sh | sh
```

Prefer to look before you leap? Clone first and run the same script from
inside the checkout — it detects it is already in a wally checkout and
skips the clone:

```sh
git clone https://github.com/SONGOKU00mjk/WALLY.git
cd WALLY
./install.sh
```

What the installer does, in order:

1. reads `/etc/os-release` and picks apt / dnf / pacman / zypper / apk /
   xbps (Gentoo and NixOS get exact manual instructions instead);
2. installs **only what is missing**: `git`, a C compiler, `nasm` on x86_64
   (AVIF SIMD), and Rust ≥ 1.98 via rustup when no usable toolchain exists;
3. `git clone`s the repository into a temporary directory that is removed
   afterwards (skipped when run from a checkout);
4. `cargo build --release`;
5. removes old wally copies — system-wide (`/usr/local/bin`, `/usr/bin`)
   and per-user (`~/.local/bin`, `~/.cargo/bin`) — so every upgrade cleanly
   replaces the previous version;
6. installs to `/usr/local/bin` by default so all users on the machine get
   wally, then prints the installed version.

Options:

```sh
./install.sh --user             # install for your user only (~/.local/bin), never asks for root
./install.sh --no-avif          # skip the AVIF decoder (no nasm needed on x86_64)
./install.sh --prebuilt         # download the prebuilt release binary instead of building
./install.sh --check            # see what this machine has
./install.sh --uninstall        # remove every installed copy
PREFIX=/opt/wally ./install.sh  # different prefix
WALLY_REF=v0.6.1 ./install.sh   # build a tagged release instead of main
```

Prebuilt glibc binaries for x86_64 and aarch64 Linux are attached to each
GitHub release, with SHA256 checksums (`--prebuilt` fetches and verifies
them).

Or with Cargo:

```sh
cargo install --path .
```

Building from source needs only git, a C compiler, a Rust toolchain (1.98+
for the AVIF decoder) and, on x86_64, `nasm` for the AVIF decoder's SIMD
assembly — aarch64 builds are pure Rust. There are **no** system library
requirements; building `--no-default-features` skips AVIF entirely (1.45 MB
binary vs 4.0 MB with it).

## Usage

```
wally [OPTIONS]
wally <IMAGE> [OPTIONS]

  -i, --image <PATH>        image to display
  -c, --color <COLOR>       solid color instead of an image (#RGB / #RRGGBB)
  -m, --mode <MODE>         stretch | fill | fit | center | tile  [default: fill]
  -o, --output <NAME>       apply the options that follow to output NAME
  -b, --background <COLOR>  padding color for fit/center  [default: #000000]
  -A, --no-animate          show only the first frame of an animation
  -h, --help                help
  -v, --version             version
```

Modes:

| Mode      | Meaning                                  |
| --------- | ---------------------------------------- |
| `stretch` | distort to fill the output               |
| `fill`    | scale to cover, crop the overflow (default) |
| `fit`     | scale to fit, letterbox with `--background` |
| `center`  | 1:1, centered                            |
| `tile`    | repeat at original size                  |

Per-output configuration, swaybg style — options after `-o NAME` affect
only that output; unnamed outputs keep the default settings:

```sh
# different wallpaper per monitor
wally -o DP-1 -i mount.jpg -o eDP-1 -i city.jpg

# animated on one output, static first frame on the other
wally -o DP-1 -i anim.webp -o eDP-1 -i anim.webp -A

# letterboxed with a custom background
wally -i photo.jpg -m fit -b '#1e1e2e'
```

HiDPI: wally renders at the output's integer scale (fractional-scale
outputs are rendered at the ceiling scale and downscaled by the
compositor, which is a slight softening, not a layout bug).

## Requirements

wally speaks `wlr-layer-shell`, so it works on Sway, Hyprland, river, niri,
Wayfire, labwc, other wlroots-based compositors, and KDE Plasma. GNOME does
not implement layer shell and is out of scope. There is no X11 support.

## Memory and performance notes

- Startup: distinct images are decoded in parallel before the compositor
  is touched; a broken file still fails fast with the same error.
- Static image: decoded once, composed straight into the output's buffer
  (one compose per distinct output size), then the decoded pixels are
  freed. A later reconfigure re-decodes from disk. What stays resident is
  the buffer the compositor scans — nothing else.
- Animation: a screen-sized GIF holds *nothing* beyond the output's own
  buffer — the compositing canvas lives in the scanout buffer itself and
  each frame is disposed/blended in place (keep / restore to background /
  restore to previous, pixel-pinned against the `image` crate's
  iterator). Animations that need resampling (source size differs from
  the output) keep one private canvas; animated WebP keeps the decoder's
  canvas only, dropping our full-frame snapshot between paints. Frames
  are stepped once per tick globally, so two monitors sharing a
  wallpaper show the same frame at the same time — and same-size
  monitors share the compose, each paying only a memcpy.
- Set `WALLY_DEBUG=1` to print the resident set in kB whenever it changes,
  plus cumulative CPU milliseconds as they grow. On one 1080p output a
  static wallpaper and a screen-sized GIF both sit *below* `swaybg`
  (which double-buffers); an animated WebP or a GIF that needs resampling
  runs one canvas above that. Animated-wallpaper CPU is the frame decode
  itself — compare the `cpu` lines between daemons before trusting
  htop's coarse percentages.
- The resampler is a hand-rolled separable Triangle filter working in
  byte buffers (pinned to the `image` crate's output within ±1 LSB by
  tests). A 1080p → 4K frame that took ~0.8 s through `image`'s generic
  path composes in ~70 ms here — straight into the output's buffer, with
  no intermediate screen-sized copy; a source that already matches the
  output is never resampled at all.
- Intended for wallpaper-rate animation (5–30 fps), not video.

## Development

```sh
cargo test      # unit tests; animated fixtures live in tests/fixtures/
cargo clippy --all-targets -- -D warnings
```

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).
