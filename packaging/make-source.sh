#!/bin/sh
# Build the PKGBUILD source tarball from the git repository.
set -eu
cd "$(dirname "$0")/.."
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
git archive --prefix="lgx-$ver/" -o "packaging/lgx-$ver.tar.gz" HEAD
echo "packaging/lgx-$ver.tar.gz"
