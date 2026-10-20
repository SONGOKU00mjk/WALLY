#!/bin/sh
# wally installer — detects your distribution, installs the build
# dependencies with its package manager, clones the repository and builds
# wally from source.
#
# Copy-paste install (nothing else needed — the script clones for you):
#
#   curl -fsSL https://raw.githubusercontent.com/SONGOKU00mjk/WALLY/main/install.sh | sh
#
# Or clone first and run the script from inside the checkout:
#
#   git clone https://github.com/SONGOKU00mjk/WALLY.git && cd WALLY && ./install.sh
#
#   ./install.sh               detect distro, install deps, clone, build, install system-wide
#   ./install.sh --user        install for the current user only (~/.local), never root
#   ./install.sh --no-avif     build without the AVIF decoder (no nasm needed on x86_64)
#   ./install.sh --prebuilt    download the static release binary instead of building
#   ./install.sh --check       report what this machine has
#   ./install.sh --uninstall   remove every installed copy of wally
#   PREFIX=/opt/wally ./install.sh ...    choose the install prefix
#   WALLY_REF=v0.5.6 ./install.sh ...     build a tag/branch instead of main
#
# The default install is system-wide (/usr/local/bin) so every user on the
# machine gets wally. Upgrades are clean: after a successful build the
# installer removes old copies (system-wide and per-user) before placing
# the new binary. Only package installation and the final copy escalate via
# sudo/doas — cloning and building always run as your own user. Building
# needs git, a C compiler, Rust >= 1.98 (rustup is installed automatically
# if missing) and, on x86_64, nasm for the AVIF decoder. The finished wally
# binary itself has no runtime dependencies.

set -eu

REPO_URL="${WALLY_REPO:-https://github.com/SONGOKU00mjk/WALLY.git}"
REPO_WEB="https://github.com/SONGOKU00mjk/WALLY"
MSRV="1.98" # avif-decode (rav1d) needs this; everything else is far older

log() { printf '%s\n' "install.sh: $*"; }
die() { printf '%s\n' "install.sh: error: $*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# One-line description of the available C compiler.
cc_report() {
    if have gcc; then gcc --version 2>/dev/null | head -n1
    elif have cc; then cc --version 2>/dev/null | head -n1
    elif have clang; then clang --version 2>/dev/null | head -n1
    fi
}

arch() {
    case "$(uname -m)" in
        x86_64) echo "x86_64" ;;
        aarch64 | arm64) echo "aarch64" ;;
        *) echo "$(uname -m)" ;;
    esac
}

help() {
    cat <<'EOF'
usage: install.sh [OPTION]...

Detects your distribution, installs build dependencies with its package
manager, clones the wally repository, builds it and installs the binary.

  (no options)   build + install system-wide (/usr/local/bin, asks for root)
  --user         install for the current user only (~/.local/bin)
  --no-avif      build without the AVIF decoder (skips nasm on x86_64)
  --prebuilt     download the static release binary instead of building
  --check        show what this machine has
  --uninstall    remove every installed copy of wally
  -h, --help     this help

Environment:
  PREFIX=DIR     install under DIR instead of /usr/local (or ~/.local with --user)
  WALLY_REF=REF  build tag/branch REF instead of the default branch
  WALLY_REPO=URL clone URL instead of the official repository

The one-line install without cloning first:
  curl -fsSL https://raw.githubusercontent.com/SONGOKU00mjk/WALLY/main/install.sh | sh
EOF
}

# Run a command with root privileges. Only package installation and the
# final copy touch system directories; downloading and building stay
# unprivileged.
as_root() {
    if [ "$(id -u)" = "0" ]; then
        "$@"
    elif have sudo; then
        sudo "$@"
    elif have doas; then
        doas "$@"
    else
        die "this step needs root — run as root, install sudo/doas, or pass --user"
    fi
}

# Best-effort root command: never dies, reports failure via exit status.
try_root() {
    if [ "$(id -u)" = "0" ]; then
        "$@"
    elif have sudo; then
        sudo "$@" || return 1
    elif have doas; then
        doas "$@" || return 1
    else
        return 1
    fi
}

# ------------------------------------------------------- distro detection

os_field() { # $1 = os-release file, $2 = key; prints the value, quotes stripped
    sed -n "s/^$2=//p" "$1" 2>/dev/null | head -n1 | tr -d "\"'"
}

# Map the distribution to a package manager: apt, dnf, pacman, zypper, apk,
# xbps, emerge or nix. Empty output means unknown.
detect_pm() {
    f="${OS_RELEASE:-/etc/os-release}"
    [ -r "$f" ] || return 0
    distro_id="$(os_field "$f" ID)"
    id_like="$(os_field "$f" ID_LIKE)"
    case "$distro_id $id_like" in
        *debian* | *ubuntu* | *mint*) echo "apt" ;;
        *fedora* | *rhel* | *centos* | *rocky* | *alma* | *amzn*) echo "dnf" ;;
        *arch* | *garuda* | *cachyos*) echo "pacman" ;;
        *suse*) echo "zypper" ;;
        *alpine*) echo "apk" ;;
        *void*) echo "xbps" ;;
        *gentoo*) echo "emerge" ;;
        *nix*) echo "nix" ;;
        *) echo "" ;;
    esac
}

# ------------------------------------------------------------- toolchain

rust_ok() {
    have rustc || return 1
    rustc_ver="$(rustc --version 2>/dev/null | sed 's/rustc \([0-9.]*\).*/\1/')"
    [ -n "$rustc_ver" ] || return 1
    [ "$(printf '%s\n%s\n' "$MSRV" "$rustc_ver" | sort -V | head -n1)" = "$MSRV" ]
}

ensure_rust() {
    rust_ok && return 0
    log "usable Rust >= $MSRV not found; installing rustup (stable, minimal)…"
    if have curl; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
            sh -s -- -y --profile minimal --default-toolchain stable
    elif have wget; then
        wget -qO- https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
    else
        die "neither curl nor wget is available to install Rust — install Rust >= $MSRV yourself"
    fi
    export PATH="$HOME/.cargo/bin:$PATH"
    rust_ok || die "Rust >= $MSRV is still missing after the rustup installation"
}

# Tools needed to build wally: git, a C compiler, a downloader for rustup
# (only when the Rust toolchain is missing/old) and nasm on x86_64 unless
# --no-avif. Sets MISSING (possibly empty).
missing_tools() {
    MISSING=""
    have git || MISSING="$MISSING git"
    if ! have gcc && ! have cc && ! have clang; then
        MISSING="$MISSING gcc"
    fi
    if ! rust_ok && ! have curl && ! have wget; then
        MISSING="$MISSING curl"
    fi
    if [ "$(arch)" = "x86_64" ] && [ "${no_avif:-}" != "1" ] && ! have nasm; then
        MISSING="$MISSING nasm"
    fi
}

install_packages() { # $1: space-separated package names (may be empty)
    [ -n "$1" ] || return 0
    case "$(detect_pm)" in
        apt)
            log "installing with apt:$1"
            # shellcheck disable=SC2086
            as_root apt-get update &&
                as_root apt-get install -y --no-install-recommends $1
            ;;
        dnf)
            log "installing with dnf:$1"
            # shellcheck disable=SC2086
            if have dnf; then as_root dnf install -y $1; else as_root yum install -y $1; fi
            ;;
        pacman)
            log "installing with pacman:$1"
            # shellcheck disable=SC2086
            as_root pacman -Sy --needed --noconfirm $1
            ;;
        zypper)
            log "installing with zypper:$1"
            # shellcheck disable=SC2086
            as_root zypper --non-interactive install $1
            ;;
        apk)
            log "installing with apk:$1"
            # shellcheck disable=SC2086
            as_root apk add --no-cache musl-dev $1
            ;;
        xbps)
            log "installing with xbps:$1"
            # shellcheck disable=SC2086
            as_root xbps-install -Sy $1
            ;;
        emerge)
            die "missing:$1 — on Gentoo run: emerge -a $1 … then re-run install.sh"
            ;;
        nix)
            die "NixOS detected — open a shell with the tools and re-run:
  nix-shell -p git gcc rustup nasm curl
  ./install.sh"
            ;;
        *)
            die "unknown distribution — install:$1 manually, then re-run install.sh"
            ;;
    esac
}

ensure_deps() {
    missing_tools
    if [ -z "$MISSING" ]; then
        log "all build dependencies are already present"
        return 0
    fi
    log "detected a '$(detect_pm)'-style distribution; installing:$MISSING"
    log "(you may be asked for your password to install system packages)"
    install_packages "$MISSING"
    missing_tools
    [ -z "$MISSING" ] || die "still missing after package installation:$MISSING — install them manually and re-run"
}

# ------------------------------------------------------------ install dir

pick_prefix() {
    if [ "${user_install:-}" = "1" ]; then
        prefix="${PREFIX:-$HOME/.local}"
    else
        prefix="${PREFIX:-/usr/local}"
    fi
}

# Put a ready binary at $prefix/bin/wally. Escalate to root only when the
# destination is not writable by the current user — so --user installs and
# PREFIX= locations you own never ask for a password.
place_wally() { # $1: source file, $2: cp or mv
    if [ -w "$prefix/bin" ] || [ -w "$prefix" ] ||
        { [ ! -e "$prefix" ] && [ -w "$(dirname "$prefix")" ]; }; then
        mkdir -p "$prefix/bin"
        "$2" "$1" "$prefix/bin/wally"
    else
        as_root mkdir -p "$prefix/bin"
        as_root "$2" "$1" "$prefix/bin/wally"
    fi
}

# Remove previously installed wally copies so the new version takes over
# cleanly (also avoids "Text file busy" when wally is still running).
remove_old() {
    for p in /usr/local/bin/wally /usr/bin/wally; do
        [ -e "$p" ] || continue
        if [ -w "$(dirname "$p")" ]; then
            if rm -f "$p"; then log "removed old system-wide copy: $p"; else log "warning: could not remove $p"; fi
        elif try_root rm -f "$p"; then
            log "removed old system-wide copy: $p"
        else
            log "warning: could not remove $p (no root) — it may shadow the new install"
        fi
    done
    for p in "$HOME/.local/bin/wally" "$HOME/.cargo/bin/wally"; do
        [ -e "$p" ] || continue
        if rm -f "$p"; then
            log "removed old copy: $p"
        else
            log "warning: could not remove $p"
        fi
    done
}

# ----------------------------------------------------------------- source

scratch=""
cleanup() {
    if [ -n "$scratch" ] && [ -d "$scratch" ]; then
        rm -rf "$scratch" || true
    fi
}
trap 'cleanup' EXIT
trap 'cleanup; exit 130' INT TERM

# Build from this checkout when the script lives inside a wally checkout
# (zip extraction or git clone flow); otherwise clone the repository into a
# scratch directory that is removed on exit.
fetch_source() {
    here="$(dirname "$0")"
    if [ -f "$here/Cargo.toml" ] && [ -f "$here/src/main.rs" ] &&
        grep -q '^name = "wally"' "$here/Cargo.toml" 2>/dev/null; then
        SRC_DIR="$here"
        log "building from this checkout: $here"
    else
        scratch="$(mktemp -d "${TMPDIR:-/tmp}/wally-build.XXXXXX")"
        SRC_DIR="$scratch/WALLY"
        log "cloning $REPO_URL"
        git clone --depth 1 ${WALLY_REF:+"--branch=$WALLY_REF"} "$REPO_URL" "$SRC_DIR" ||
            die "git clone failed — check your internet connection and $REPO_URL"
        log "clone ready (the scratch directory is removed on exit)"
    fi
    cd "$SRC_DIR"
}

build_source() {
    ensure_rust
    if [ "$(arch)" = "x86_64" ] && [ "${no_avif:-}" != "1" ] && ! have nasm; then
        die "nasm is missing (the AVIF decoder needs it on x86_64) — install it or pass --no-avif"
    fi
    if [ "${no_avif:-}" = "1" ]; then
        log "building without the AVIF decoder (--no-avif)"
        cargo build --release --no-default-features
    else
        cargo build --release
    fi
    chmod +x target/release/wally
}

place_and_report() { # $1: binary path, $2: cp or mv
    place_wally "$1" "$2"
    log "installed $( "$prefix/bin/wally" --version ) to $prefix/bin/wally"
    case ":$PATH:" in
        *":$prefix/bin:"*) ;;
        *) log "note: $prefix/bin is not in PATH — add it to your shell profile" ;;
    esac
    log "try it: wally ~/pictures/wall.jpg"
}

install_source() {
    ensure_deps
    fetch_source
    build_source
    remove_old
    place_and_report "target/release/wally" cp
}

# --------------------------------------------------------------- prebuilt

install_prebuilt() {
    a="$(arch)"
    case "$a" in
        x86_64) t="x86_64-unknown-linux-musl" ;;
        aarch64) t="aarch64-unknown-linux-musl" ;;
        *) die "no prebuilt binary for $(uname -m); build from source instead (the default)" ;;
    esac

    # Newest release tag according to the GitHub API, fallback to latest tag
    # redirects so we still work when the API is rate-limited.
    ver="$(curl -fsSL "https://api.github.com/repos/SONGOKU00mjk/WALLY/releases/latest" 2>/dev/null |
        sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)" || ver=""
    [ -n "$ver" ] || ver="latest"

    scratch="$(mktemp -d "${TMPDIR:-/tmp}/wally-dl.XXXXXX")"
    url="$REPO_WEB/releases/${ver:+download/}$ver/wally-$t.tar.gz"
    log "downloading $url"
    if have curl; then
        curl -fL --progress-bar -o "$scratch/wally.tar.gz" "$url" ||
            die "download failed — check $REPO_WEB/releases for assets"
    else
        wget -q --show-progress -O "$scratch/wally.tar.gz" "$url" ||
            die "download failed — check $REPO_WEB/releases for assets"
    fi

    if have sha256sum && curl -fsSL -o "$scratch/SHA256SUMS" \
        "$REPO_WEB/releases/${ver:+download/}$ver/SHA256SUMS" 2>/dev/null; then
        (cd "$scratch" && grep "wally-$t.tar.gz" SHA256SUMS | sha256sum -c -) ||
            die "checksum mismatch — aborting"
        log "checksum ok"
    else
        log "sha256sum unavailable or checksums missing; skipping verification"
    fi

    tar -xzf "$scratch/wally.tar.gz" -C "$scratch"
    chmod +x "$scratch/wally" # before moving: once root-installed it is root-owned
    remove_old
    place_and_report "$scratch/wally" mv
}

# ----------------------------------------------------------------- checks

check_env() {
    log "arch:      $(uname -m)"
    pm="$(detect_pm)"
    if [ -n "$pm" ]; then
        log "distro:    $pm (from ${OS_RELEASE:-/etc/os-release})"
    else
        log "distro:    unknown (no usable ${OS_RELEASE:-/etc/os-release})"
    fi
    have git && log "git:       $(git --version 2>/dev/null | head -n1)" || log "git:       missing"
    if have gcc || have cc || have clang; then
        log "cc:        $(cc_report)"
    else
        log "cc:        missing"
    fi
    if rust_ok; then
        log "rustc:     $(rustc --version | head -n1)  (meets $MSRV)"
    elif have rustc; then
        log "rustc:     $(rustc --version | head -n1)  (too old for $MSRV; rustup will be installed)"
    else
        log "rustc:     missing (rustup will be installed)"
    fi
    if [ "$(arch)" = "x86_64" ]; then
        if have nasm; then
            log "nasm:      $(nasm --version 2>/dev/null | head -n1) (AVIF SIMD)"
        else
            log "nasm:      missing (only needed to build the AVIF decoder on x86_64)"
        fi
    fi
    log "wayland:   no runtime deps — wally is pure Rust (wlr-layer-shell compositors only)"
}

uninstall() {
    removed=""
    for p in "$prefix/bin/wally" /usr/local/bin/wally /usr/bin/wally \
        "$HOME/.local/bin/wally" "$HOME/.cargo/bin/wally"; do
        [ -e "$p" ] || continue
        if [ -w "$(dirname "$p")" ]; then
            if rm -f "$p"; then log "removed $p"; removed="1"; else log "warning: could not remove $p"; fi
        elif try_root rm -f "$p"; then
            log "removed $p"
            removed="1"
        else
            log "could not remove $p — no root privileges"
        fi
    done
    [ -n "$removed" ] || log "no installed wally found"
}

# ------------------------------------------------------------------- main

main() {
    mode="source" # default: clone + build from source
    no_avif=""
    user_install=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --user) user_install="1" ;;
            --no-avif) no_avif="1" ;;
            --prebuilt) mode="prebuilt" ;;
            --from-source) ;; # the default since 0.5.6; accepted for compatibility
            --check) mode="check" ;;
            --uninstall) mode="uninstall" ;;
            -h | --help) help; exit 0 ;;
            *) die "unknown option: $1 (see --help)" ;;
        esac
        shift
    done
    pick_prefix
    case "$mode" in
        check) check_env ;;
        uninstall) uninstall ;;
        prebuilt) install_prebuilt ;;
        source) install_source ;;
    esac
}

main "$@"
