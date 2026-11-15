#!/bin/sh
# Archive the current commit as the PKGBUILD source tarball.
#
# The tarball is produced from HEAD, so uncommitted changes are not packaged;
# build.sh calls this before every package build.
set -eu
cd "$(dirname "$0")/.."
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "make-source.sh: working tree is dirty; commit before packaging" >&2
  exit 1
fi
git archive --prefix="lgx-$ver/" -o "packaging/lgx-$ver.tar.gz" HEAD
# Only the tarball for the current version is kept.
for old in packaging/lgx-*.tar.gz; do
  [ -e "$old" ] || continue
  [ "$old" = "packaging/lgx-$ver.tar.gz" ] || rm -f "$old"
done
echo "packaging/lgx-$ver.tar.gz"
