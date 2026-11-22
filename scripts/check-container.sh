#!/bin/sh
# check-container.sh — run scripts/check.sh inside archlinux:latest.
#
# CI runs the same script in that image (.github/workflows/ci.yml). This is the
# local equivalent for a host without podman or docker: the image is fetched
# through the registry HTTP API and booted with bubblewrap, so nothing has to
# be installed on the host itself.
#
# The unpacked image lives in target/archimg (git-ignored) and is reused; the
# package set is installed once, inside the image.
#
#   scripts/check-container.sh            # build + tests + C helpers
#
# Environment: KEEP_IMAGE=1 keeps the extracted root filesystem for debugging.
set -eu
cd "$(dirname "$0")/.."
root=$(pwd)
img="$root/target/archimg"
rootfs="$root/target/archroot"

if [ ! -d "$rootfs/usr/bin" ]; then
  mkdir -p "$img" "$rootfs"
  echo "==> fetching archlinux:latest"
  python3 - "$img" <<'PY'
import json, os, sys, urllib.request

img = sys.argv[1]
token = json.load(
    urllib.request.urlopen(
        "https://auth.docker.io/token?service=registry.docker.io"
        "&scope=repository:library/archlinux:pull"
    )
)["token"]


def get(path, accept=None):
    req = urllib.request.Request(
        f"https://registry-1.docker.io/v2/library/archlinux/{path}",
        headers={"Authorization": f"Bearer {token}"},
    )
    if accept:
        req.add_header("Accept", accept)
    return urllib.request.urlopen(req)


man = json.load(
    get(
        "manifests/latest",
        "application/vnd.oci.image.index.v1+json,"
        "application/vnd.docker.distribution.manifest.list.v2+json",
    )
)
if "manifests" in man:
    digest = [
        m["digest"]
        for m in man["manifests"]
        if m.get("platform", {}).get("architecture") == "amd64"
        and m["platform"].get("os") == "linux"
    ][0]
    man = json.load(
        get(
            f"manifests/{digest}",
            "application/vnd.oci.image.manifest.v1+json,"
            "application/vnd.docker.distribution.manifest.v2+json",
        )
    )
for i, layer in enumerate(man["layers"]):
    out = os.path.join(img, f"layer{i}.tgz")
    if os.path.exists(out) and os.path.getsize(out) == layer["size"]:
        continue
    with get(f"blobs/{layer['digest']}") as r, open(out, "wb") as f:
        while chunk := r.read(1 << 20):
            f.write(chunk)
    print(f"  layer {i}: {layer['size']} bytes")
PY
  for layer in "$img"/layer*.tgz; do
    tar -xzf "$layer" -C "$rootfs" 2>/dev/null || true
  done
  # The image's root is 0555; the build directory (target dir) lives there.
  chmod u+w "$rootfs"
fi

# Same package set as .github/workflows/ci.yml; the second and later runs are
# a no-op.
echo "==> preparing the container"
bwrap \
  --bind "$rootfs" / \
  --dev /dev --proc /proc --tmpfs /tmp \
  --bind /etc/resolv.conf /etc/resolv.conf \
  --bind "$root" /work --chdir /work \
  --unshare-pid --unshare-user --uid 0 --gid 0 \
  /bin/bash -c '
set -eu
if ! command -v rustc >/dev/null; then
  # This is a throwaway image: gpg cannot initialise its keyring inside the
  # user namespace (gpg-agent blocks), so package signatures are skipped here.
  # CI installs the same set in a real container, where the keyring works.
  cat > /tmp/pacman.conf <<EOF
[options]
Architecture = auto
SigLevel = Never
[core]
Include = /etc/pacman.d/mirrorlist
[extra]
Include = /etc/pacman.d/mirrorlist
EOF
  pacman -Syu --config /tmp/pacman.conf --noconfirm --needed \
    nodejs git rust gcc pkgconf clang \
    ffmpeg sdl3 vulkan-headers vulkan-icd-loader mesa \
    libxkbcommon pipewire libva libpulse wl-clipboard \
    qt6-base freetype2 ttf-dejavu >/dev/null
fi
export HOME=/root CARGO_TARGET_DIR=/build CARGO_HOME=/root/.cargo
git config --global --add safe.directory /work
./scripts/check.sh
'

if [ "${KEEP_IMAGE:-0}" != "1" ]; then
  echo "==> done (image kept in target/archimg for the next run)"
fi
