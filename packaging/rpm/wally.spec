# wally.spec — binary RPM built from a release binary.
#
# packaging/build-packages.sh drives it:
#
#   rpmbuild -bb wally.spec --define "wally_version 0.2.1" \
#       --define "wally_binary /path/to/wally" \
#       --define "wally_root /path/to/checkout" \
#       --target x86_64
#
# Nothing is compiled and no source archive is unpacked: the binary is the
# release artifact, and the docs/man page/licence come from the checkout.

Name:           wally
Version:        %{wally_version}
Release:        1
Summary:        Minimal Wayland wallpaper setter with animated GIF and WebP
License:        GPL-3.0-or-later
URL:            https://github.com/SONGOKU00mjk/WALLY
Requires:       glibc
Requires:       libgcc
AutoReqProv:    no
BuildArch:      %{_target_cpu}

%global debug_package %{nil}
# The binary is already stripped release output, and a foreign-architecture
# binary cannot be stripped by the build host anyway.
%global __strip /bin/true

%description
wally is a wallpaper setter for compositors that implement
wlr-layer-shell (Sway, Hyprland, river, niri, Wayfire, labwc, KDE Plasma).
It is a single self-contained binary with no runtime library dependencies.
Still images, animated GIF and animated WebP are supported, animated
wallpapers stop consuming CPU when no output can present them, and an
optional daemon serves set/query/restore/stop on a Unix socket.

%prep
# no source archive: the release binary is supplied by the caller

%build
# nothing to compile

%install
install -Dm755 "%{wally_binary}" "%{buildroot}%{_bindir}/wally"
install -Dm644 "%{wally_root}/packaging/man/wally.1" "%{buildroot}%{_mandir}/man1/wally.1"
install -Dm644 "%{wally_root}/README.md" "%{buildroot}%{_docdir}/wally/README.md"
install -Dm644 "%{wally_root}/CHANGELOG.md" "%{buildroot}%{_docdir}/wally/CHANGELOG.md"
install -Dm644 "%{wally_root}/LICENSE" "%{buildroot}%{_licensedir}/wally/LICENSE"

%files
%{_bindir}/wally
%{_mandir}/man1/wally.1*
%{_docdir}/wally/README.md
%{_docdir}/wally/CHANGELOG.md
%license %{_licensedir}/wally/LICENSE

%changelog
* Thu Nov 26 2026 SONGOKU00mjk <songokubesto@outlook.com> - 0.2.1-1
- Binary package built from the upstream release artifact.
