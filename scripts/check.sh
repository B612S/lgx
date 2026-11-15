#!/bin/sh
# Local gate before pushing: release build, tests and the C helpers.
set -eu
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

cargo build --release --locked
cargo test --release --locked --workspace
gcc -O2 -Wall -Wextra -Wno-unused-parameter -o "$tmp/lgx-uinput" helper/lgx-uinput.c
gcc -O2 -Wall -Wextra -Wno-unused-parameter -o "$tmp/lgx-vaenc" \
  helper/lgx-vaenc.c -lavcodec -lavutil -lva -lva-drm
echo "check: ok"
