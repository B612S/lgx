#!/bin/sh
# Build the lgx packages.
#
# Usage: packaging/build.sh [lgx-client|lgx-server ...]
#
# The source tarball is written to packaging/, makepkg's src/pkg work trees go
# to target/pkgbuild/, and the finished packages land in the project root.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
# Generated PKGBUILDs with real checksums are temporary.
trap 'rm -f "$root"/packaging/*/PKGBUILD.sha256' EXIT
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)

if [ "$#" -eq 0 ]; then
  set -- lgx-client lgx-server
fi

"$root/packaging/make-source.sh"

for pkg in "$@"; do
  dir="$root/packaging/$pkg"
  [ -f "$dir/PKGBUILD" ] || { echo "build.sh: no $dir/PKGBUILD" >&2; exit 1; }
  pkgver=$(sed -n 's/^pkgver=//p' "$dir/PKGBUILD")
  pkgrel=$(sed -n 's/^pkgrel=//p' "$dir/PKGBUILD")
  if [ "$pkgver" != "$version" ]; then
    echo "build.sh: $pkg PKGBUILD has pkgver=$pkgver, Cargo.toml has $version" >&2
    exit 1
  fi
  built="$pkgver-$pkgrel"
  if installed=$(pacman -Q "$pkg" 2>/dev/null | cut -d' ' -f2); then
    case $(vercmp "$built" "$installed") in
      -1) echo "build.sh: $pkg $built is older than the installed $installed; bump pkgrel in $dir/PKGBUILD" >&2
          exit 1 ;;
      0) echo "build.sh: $pkg $built is already installed; bump pkgrel to reinstall it" >&2 ;;
    esac
  fi
  echo "==> $pkg $built"
  # Verify the freshly archived source instead of trusting SKIP.
  sum=$(sha256sum "$root/packaging/lgx-$version.tar.gz" | cut -d' ' -f1)
  sed "s|^sha256sums=.*|sha256sums=('$sum')|" "$dir/PKGBUILD" > "$dir/PKGBUILD.sha256"
  (
    cd "$dir"
    BUILDDIR="$root/target/pkgbuild" SRCDEST="$root/packaging" PKGDEST="$root" \
      makepkg -p PKGBUILD.sha256 -f --noconfirm
  )
  # Keep only the package just built; older versions in the root are stale.
  for old in "$root/$pkg-"*.pkg.tar.zst; do
    [ -e "$old" ] || continue
    [ "$old" = "$root/$pkg-$built-x86_64.pkg.tar.zst" ] || rm -f "$old"
  done
done

ls -1 "$root"/*.pkg.tar.zst
