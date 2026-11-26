#!/bin/sh
# build-packages.sh — build distro packages from a prebuilt wally binary.
#
# usage: packaging/build-packages.sh --version VERSION --triple TRIPLE
#          --binary PATH [--outdir DIR] [--formats deb,rpm,arch]
#
# TRIPLE is the release target (x86_64-unknown-linux-gnu or
# aarch64-unknown-linux-gnu). The packages are written to OUTDIR (default
# "dist") with the names the release workflow and install.sh expect:
#
#   wally_VERSION_ARCH.deb                  (ARCH = amd64 | arm64)
#   wally-VERSION-1.RPMARCH.rpm             (RPMARCH = x86_64 | aarch64)
#   wally-VERSION-1-ARCH.pkg.tar.zst        (ARCH = x86_64 | aarch64)
#
# Building a package never executes the binary, so packages for a foreign
# architecture can be produced on any host. .rpm output needs rpmbuild;
# the other formats need only tar, gzip and ar.
set -eu

version=
triple=
binary=
outdir=dist
formats=deb,rpm,arch
root=$(cd "$(dirname "$0")/.." && pwd)

log() { printf 'build-packages: %s\n' "$*"; }
die() { printf 'build-packages: error: %s\n' "$*" >&2; exit 1; }

usage() {
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) version=${2:?--version needs a value}; shift 2 ;;
        --triple) triple=${2:?--triple needs a value}; shift 2 ;;
        --binary) binary=${2:?--binary needs a value}; shift 2 ;;
        --outdir) outdir=${2:?--outdir needs a value}; shift 2 ;;
        --formats) formats=${2:?--formats needs a value}; shift 2 ;;
        -h | --help) usage ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

[ -n "$version" ] || die "--version is required"
[ -n "$triple" ] || die "--triple is required"
[ -n "$binary" ] || die "--binary is required"
[ -f "$binary" ] || die "no such binary: $binary"

case "$triple" in
    x86_64-unknown-linux-gnu) arch=x86_64; debarch=amd64 ;;
    aarch64-unknown-linux-gnu) arch=aarch64; debarch=arm64 ;;
    *) die "unsupported triple: $triple" ;;
esac

version=${version#v}
case "$version" in
    *[!0-9A-Za-z.~+-]*) die "version $version contains unsupported characters" ;;
esac

binary=$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary")
mkdir -p "$outdir"
outdir=$(cd "$outdir" && pwd)

work=$(mktemp -d "${TMPDIR:-/tmp}/wally-pkg.XXXXXX")
trap 'rm -rf "$work"' EXIT INT TERM

stage="$work/root"
mkdir -p "$stage/usr/bin" "$stage/usr/share/man/man1" \
    "$stage/usr/share/doc/wally" "$stage/usr/share/licenses/wally"
install -Dm755 "$binary" "$stage/usr/bin/wally"
gzip -9n -c "$root/packaging/man/wally.1" >"$stage/usr/share/man/man1/wally.1.gz"
install -Dm644 "$root/README.md" "$stage/usr/share/doc/wally/README.md"
install -Dm644 "$root/CHANGELOG.md" "$stage/usr/share/doc/wally/CHANGELOG.md"
install -Dm644 "$root/LICENSE" "$stage/usr/share/licenses/wally/LICENSE"
install -Dm644 "$root/LICENSE" "$stage/usr/share/doc/wally/copyright"

want() { case ",$formats," in *",$1,"*) return 0 ;; *) return 1 ;; esac; }

# ------------------------------------------------------------------- deb
build_deb() {
    log "building .deb ($debarch)"
    mkdir -p "$work/deb"
    (
        cd "$stage"
        find . -type f | sed 's|^\./||' | LC_ALL=C sort | xargs md5sum
    ) >"$work/deb/md5sums"

    size=$(du -sk "$stage" | cut -f1)
    {
        printf 'Package: wally\n'
        printf 'Version: %s\n' "$version"
        printf 'Architecture: %s\n' "$debarch"
        printf 'Maintainer: SONGOKU00mjk <songokubesto@outlook.com>\n'
        printf 'Installed-Size: %s\n' "$size"
        printf 'Depends: libc6, libgcc-s1\n'
        printf 'Section: x11\n'
        printf 'Priority: optional\n'
        printf 'Homepage: https://github.com/SONGOKU00mjk/WALLY\n'
        printf 'Description: minimal Wayland wallpaper setter\n'
        printf ' wally sets a wallpaper on every output of a wlr-layer-shell\n'
        printf ' compositor (Sway, Hyprland, river, niri, Wayfire, labwc, KDE\n'
        printf ' Plasma), with first-class animated GIF and WebP support. It\n'
        printf ' also rotates directories, fades between wallpapers, and can\n'
        printf ' serve a control daemon.\n'
    } >"$work/deb/control"

    (cd "$work/deb" && tar -cJf "$work/control.tar.xz" ./control ./md5sums)
    (cd "$stage" && tar -cJf "$work/data.tar.xz" --owner=0 --group=0 ./usr)
    printf '2.0\n' >"$work/debian-binary"
    (cd "$work" && ar rc "$outdir/wally_${version}_${debarch}.deb" \
        debian-binary control.tar.xz data.tar.xz)
    log "wrote wally_${version}_${debarch}.deb"
}

# ------------------------------------------------------------------- rpm
build_rpm() {
    if ! command -v rpmbuild >/dev/null 2>&1; then
        log "rpmbuild not installed — skipping the .rpm (install rpm/rpm-tools)"
        return 0
    fi
    log "building .rpm ($arch)"
    top="$work/rpm"
    mkdir -p "$top/BUILD" "$top/BUILDROOT" "$top/RPMS" "$top/SOURCES" "$top/SPECS"
    if ! rpmbuild -bb "$root/packaging/rpm/wally.spec" \
        --define "_topdir $top" \
        --define "wally_version $version" \
        --define "wally_binary $binary" \
        --define "wally_root $root" \
        --define "debug_package %{nil}" \
        --target "$arch" >"$work/rpmbuild.log" 2>&1; then
        cat "$work/rpmbuild.log" >&2
        die "rpmbuild failed"
    fi
    find "$top/RPMS" -name '*.rpm' -exec cp {} "$outdir/" \;
    log "wrote wally-${version}-1.${arch}.rpm"
}

# ------------------------------------------------------------------ arch
build_arch() {
    log "building .pkg.tar.zst ($arch)"
    size=$(du -sb "$stage" | cut -f1)
    {
        printf 'pkgname = wally\n'
        printf 'pkgbase = wally\n'
        printf 'pkgver = %s-1\n' "$version"
        printf 'pkgdesc = Minimal Wayland wallpaper setter with first-class GIF and animated WebP support\n'
        printf 'url = https://github.com/SONGOKU00mjk/WALLY\n'
        printf 'builddate = %s\n' "$(date +%s)"
        printf 'packager = SONGOKU00mjk <songokubesto@outlook.com>\n'
        printf 'size = %s\n' "$size"
        printf 'arch = %s\n' "$arch"
        printf 'license = GPL-3.0-or-later\n'
        printf 'depend = glibc\n'
        printf 'depend = gcc-libs\n'
    } >"$stage/.PKGINFO"
    (cd "$stage" && tar --zstd -cf "$outdir/wally-${version}-1-${arch}.pkg.tar.zst" .PKGINFO usr)
    log "wrote wally-${version}-1-${arch}.pkg.tar.zst"
}

want deb && build_deb
want rpm && build_rpm
want arch && build_arch

log "packages in $outdir:"
for f in "$outdir"/*; do
    [ -f "$f" ] && printf '  %s\n' "$(basename "$f")"
done
