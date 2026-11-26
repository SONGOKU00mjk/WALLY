# Packaging

wally ships four kinds of binary artifact per release, all built from the
same binaries:

| Artifact | Used by |
| --- | --- |
| `wally-<triple>.tar.gz` | `install.sh`, manual installs, any Linux |
| `wally_<version>_<arch>.deb` | Debian, Ubuntu, derivatives |
| `wally-<version>-1.<arch>.rpm` | Fedora, RHEL, derivatives |
| `wally-<version>-1-<arch>.pkg.tar.zst` | Arch Linux |

`<triple>` is `x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu`;
package architectures are `amd64`/`arm64` (deb), `x86_64`/`aarch64`
(rpm and Arch).

## Asset names are a contract

`install.sh --package` and the release workflow both depend on these exact
names. If you change one, change all of:

1. `packaging/build-packages.sh` (the builder),
2. `install.sh` (`package_asset`),
3. `.github/workflows/release.yml` (validation and upload globs).

## Building packages

`packaging/build-packages.sh` turns a prebuilt release binary into packages.
It never compiles or executes the binary, so foreign-architecture packages
are built on any host:

```sh
# one format
./packaging/build-packages.sh \
  --version 0.2.1 \
  --triple x86_64-unknown-linux-gnu \
  --binary target/release/wally \
  --formats deb \
  --outdir dist

# everything this host can build
./packaging/build-packages.sh \
  --version 0.2.1 --triple x86_64-unknown-linux-gnu \
  --binary target/release/wally --outdir dist
```

Requirements: `tar`, `gzip`, `ar` for `.deb` and `.pkg.tar.zst`; `zstd`
for the Arch package; `rpmbuild` (package `rpm`/`rpm-tools`) for `.rpm`.
Without `rpmbuild` the script builds the other formats and says so.

The packages contain:

- `/usr/bin/wally`
- `/usr/share/man/man1/wally.1.gz` (from `packaging/man/wally.1`)
- `/usr/share/doc/wally/{README.md,CHANGELOG.md,copyright}`
- `/usr/share/licenses/wally/LICENSE` (and `%license` for the RPM)

## Packages and PATH

A package owns `/usr/bin/wally`. `install.sh --package` removes an older
single-binary copy from `/usr/local/bin`, `~/.local/bin` or `~/.cargo/bin`
so it cannot shadow the packaged binary. It never deletes the
package-managed file behind the package manager's back.

## Arch: PKGBUILD vs release package

`packaging/arch/PKGBUILD` and `PKGBUILD-git` build from source and are meant
for the AUR. The `.pkg.tar.zst` attached to a release is built from the
already-compiled release binary by `build-packages.sh`, which keeps it
identical to every other artifact of that release. Both install the same
files.

## Nix

`flake.nix` at the repository root builds the default feature set:

```sh
nix build github:SONGOKU00mjk/WALLY
nix run github:SONGOKU00mjk/WALLY -- --version
```

## Verifying a release

Every release has a `SHA256SUMS` covering all tarballs and packages:

```sh
sha256sum -c SHA256SUMS
```

`install.sh` does this automatically, for every artifact it downloads.
