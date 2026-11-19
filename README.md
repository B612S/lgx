# lgx

Low-latency HEVC remote desktop over a LAN.

- **Server:** Linux/Wayland (wlroots + VAAPI) or macOS (ScreenCaptureKit +
  VideoToolbox).
- **Client:** Linux/Wayland (Vulkan, NVDEC/CUDA or VAAPI).

The server and the client can run on the same machine or on different machines
on a LAN. The transport is TCP and always encrypted; there is no discovery.

```
Linux server:  wlr-screencopy -> GBM dma-buf -> VPP RGB->NV12 -> VAAPI HEVC -> secure TCP
macOS server:  ScreenCaptureKit -> VideoToolbox HEVC -> secure TCP
client:        secure TCP -> libavcodec HEVC -> NV12 -> Vulkan -> swapchain
```

There is no FFmpeg subprocess and no shared-memory transport. The client keeps
only the newest decoded frame, so a slow window never builds a queue.

## Features

- HEVC Main profile, 8-bit 4:2:0, full-range BT.709.
- **Linux server:** VAAPI encoding with Intel VDENC (`low_power = true`) and
  asynchronous encoding (`async_depth = 2`); VPP converts RGB to NV12 on the
  media engine, with a GL fallback.
- **macOS server:** ScreenCaptureKit capture and VideoToolbox HEVC, including
  120 Hz displays, adaptive bitrate, system audio capture, native clipboard,
  CGEvent input, cursor shapes, media keys and display-synced smooth scrolling.
- **Client:** Vulkan presentation with letterboxing; automatic decoder
  selection (NVIDIA -> CUDA/NVDEC, otherwise VAAPI, software fallback);
  `--decoder` overrides it.
- Geometry, refresh rate and bitrate follow the selected output. `--fps` is
  only an override.
- A new client waits for an IDR; delayed packets from a previous client are
  never delivered.
- Per-client tokens and AES-256-GCM transport on the Linux server; a single
  configured token on the macOS server. Pairing is server-initiated.
- Input through `uinput` on Linux and CGEvent on macOS; absolute pointer,
  gamepads (Linux server), rich-text clipboard, and a locally drawn pointer
  with the guest's cursor shape on macOS.
- Native PipeWire audio on Linux and a Core Audio process tap on macOS;
  float32 capture converted once to 48 kHz s16le stereo with TPDF dither.
- Profiles, per-profile overrides, automatic reconnect, and window title
  `lgx — <profile>`. FPS/bandwidth diagnostics behind `LGX_DEBUG_FPS`.

## Requirements

### Client (Linux)

Wayland, SDL3, a Vulkan 1.3 driver (`vulkan-icd-loader` plus NVIDIA/Mesa),
FFmpeg libavcodec/libavutil headers at build time, `libxkbcommon`,
`wl-clipboard`, PipeWire (the native `pipewire` client library), and Qt6
Widgets for the separate reconnect window. On NVIDIA the runtime driver
provides NVDEC/CUDA; on Intel/AMD VAAPI is used.

### Linux server (Wayland)

A wlroots compositor (Hyprland, Sway, ...), VAAPI with HEVC encode and video
processing (VPP), FFmpeg and libva for the helper, `wl-clipboard`, and PipeWire.

### macOS server

macOS 14.2 or newer, Xcode Command Line Tools (`clang` and `swiftc`), the Rust
toolchain, and a stable code-signing identity if TCC grants must survive
rebuilds. The app needs Screen Recording, Accessibility and System Audio
Recording permissions. See `scripts/build-macos-app.sh` and
`scripts/sign-and-install.sh`.

The macOS server is a background app launched by a LaunchAgent; there is no
macOS client in this repository.

## Build

### Linux (client and Linux server)

```bash
cargo build --release
gcc -O2 -o lgx-vaenc helper/lgx-vaenc.c -lavcodec -lavutil -lva -lva-drm
gcc -O2 -o lgx-uinput helper/lgx-uinput.c
```

The client build compiles `crates/lgx-client/src/dec_shim.c` against the
installed FFmpeg headers (see `build.rs`). On Arch the build dependencies
include `ffmpeg`, `sdl3`, `vulkan-icd-loader`, `libxkbcommon`, `pipewire`,
`qt6-base` and the GPU's Vulkan driver.

The Linux server looks for the encoder helper at `/usr/lib/lgx/lgx-vaenc`.
From a source tree set `LGX_VAENC=$PWD/lgx-vaenc`.

### macOS server

```bash
./scripts/build-macos-app.sh                 # /Applications/LGX Server.app
./scripts/build-macos-app.sh /tmp/lgx.app    # elsewhere
./scripts/sign-and-install.sh                # build, sign, install, restart
./scripts/build-dmg.sh                       # drag-to-Applications DMG
```

`build-macos-app.sh` builds the Rust server and the Swift/C helpers
(`lgx-vtenc`, `lgx-launcher`, `lgx-macos-audio`, `lgx-pb`, `lgx-cginput`,
`lgx-macos-cursor`), signs every executable in the bundle and the bundle
itself. `codesign` needs an unlocked login keychain, so run the signing scripts
from the logged-in GUI session, not from a bare SSH shell. The app must remain
owned by the login user; `build-macos-app.sh` replaces it without sudo.

The app icon is a Liquid Glass `.icon` source at `assets/AppIcon.icon` with
light, dark and tinted appearances. `assets/make-macos-icon.py` renders its
layers; `scripts/compile-macos-icon.sh` compiles it to `assets/Assets.car` and
`assets/lgx.icns` with `actool` (Xcode, or the clean-room `actool` from
`https://github.com/viraptor/actool`). The app copies both and sets
`CFBundleIconName=AppIcon`; the `.icns` is only the pre-macOS 26 fallback.

## Packaging

```bash
./packaging/build.sh                # both Arch packages
./packaging/build.sh lgx-client     # a single package
```

`build.sh` archives the committed tree as `packaging/lgx-<version>.tar.gz`
(it refuses to package a dirty working tree), builds the PKGBUILDs under
`packaging/`, and leaves the finished `*.pkg.tar.zst` files in the project
root. makepkg's `src`/`pkg` work trees go to `target/pkgbuild`, so `packaging/`
only ever holds PKGBUILDs, unit files and the desktop entry. Bump `pkgrel` in
the PKGBUILD before a rebuild meant to replace an installed package; the
script stops instead of producing a version older than the one pacman has.

## Quick start over a LAN

The client never discovers a server. Run the server, pair once, then start the
client with the profile that pairing wrote.

### Linux server

Start it inside the graphical session, bound to the LAN interface:

```bash
lgx-server --listen 192.168.1.50:5902 --bitrate 40000000
# --fps N            override the detected refresh rate
# --async-depth N    1 = lowest latency, 2 = default throughput
# --no-low-power      disable Intel VDENC
# --output NAME       pick an output (hyprctl monitors)
# --no-audio          disable the audio stream
```

The shipped service reads `~/.config/lgx/server.conf` and binds the IPv4
address of `network.interface`; nothing binds `0.0.0.0`.

### macOS server

Build and install the app as described above. The LaunchAgent
`~/Library/LaunchAgents/dev.b612s.lgx.server.plist` starts
`Contents/MacOS/lgx-launcher` at login with the listen/token arguments; edit
those arguments with `PlistBuddy` or reinstall with new ones. Its persistent
settings live in `~/.config/lgx/macos.conf`. A typical LAN setup:

```ini
[network]
listen = 192.168.1.50:5902
token = <64 hex characters>
pair_port = 5904

[video]
fps = 120
bitrate = 80000000
display = 0
wheel_invert = true
adaptive = true
```

The LaunchAgent's command-line arguments override the file. `--rotate`
generates a new token and mirrors it into the LaunchAgent; the client must be
re-paired or its profile token updated afterwards.

### Client

On a different machine, pair once and then connect:

```bash
lgx-client macbook --pair 192.168.1.50:5904 --pair-code CODE
lgx-client macbook
```

`lgx-client --list` prints profiles without tokens;
`lgx-client --check macbook` validates the profile. The profile name must match
the server's `--pair-name` (the default on macOS is `macbook`; on the Linux
server it is the `--pair-name` passed to `--pair`).

The client reads the geometry from the server's `HELLO` and sizes its window on
the first run. After a lost session it reconnects with exponential backoff and
restarts the session (window and audio are re-created). While waiting it shows
a separate Qt `lgx-reconnect-qt` window in the desktop style with an
indeterminate progress line and a Cancel button; the SDL/Vulkan client does not
link Qt. Use `--no-reconnect` for tests.

### LAN checklist

- Use the server's LAN address, not `127.0.0.1`, in the client profile.
- Allow TCP **5902** (video/control) and **5903** (audio) through the server
  firewall. TCP **5904** is only needed while pairing.
- The transport is AES-256-GCM; do not expose the ports to untrusted networks.
  Pairing is a one-shot local admin action, not an always-on endpoint.
- One client per server. Different servers can be used side by side; the client
  takes one instance lock per server host.
- Wired Ethernet is recommended. The default bitrate is 40 Mbit/s; the macOS
  server's adaptive mode lowers it when frames are dropped and recovers when
  the link clears. `LGX_DEBUG_FPS=1 lgx-client ...` prints the client's
  per-second diagnostics.

## Configuration

### Client

`~/.config/lgx/client.conf` contains shared `[video]`, `[audio]` and `[input]`
sections plus one section per server profile:

```ini
[video]
width = 1920
height = 1080
fullscreen = false
vsync = false
decoder = auto
# 0 = present every decoded frame (lowest latency), "auto" = panel refresh.
present_hz = 0

[audio]
enabled = true
mute_unfocused = true

[input]
gamepads = true
cursor_grab = true
clipboard = true
clipboard_rich = true
clipboard_max_bytes = 1048576

[macbook]
server = 192.168.1.50
video_port = 5902
audio_port = 5903
pair_port = 5904
token = <token>
# Optional overrides of the shared sections, only for this profile.
vsync = true
mute_unfocused = false
clipboard = false
```

`lgx-client macbook` selects the `[macbook]` section; the section name is also
the public client ID. `[video]`, `[audio]` and `[input]` are shared; each
profile has `server`, `video_port`, `audio_port`, `pair_port` and `token`, and
may override any shared setting. An override in the profile section wins over
the global section, and command-line flags still win over both. A profile can
use the bare key (`vsync`), the section-qualified key (`video.vsync`), or the
field name used inside the global section (`enabled` for `[audio] enabled`).
The file contains tokens and is `0600`. The ports are explicit; there is no
discovery.

- `mute_unfocused = true` discards received audio and emits silence while the
  client window is unfocused; the native PipeWire stream stays active.
- `[input] gamepads = false` (or `--no-gamepads`) skips gamepad forwarding.
- `clipboard = false` disables sync; `clipboard_rich = false` sends only plain
  text; `clipboard_max_bytes` limits both directions (`0` = unlimited).
- `[input] cursor_grab = true` confines the pointer to the window (SDL mouse
  grab) and keeps the original absolute motion. `Ctrl+Alt+Shift+G` toggles only
  the confinement; `cursor_grab = false` starts unconfined.
- `[audio] connect` is an optional audio-address override; normally omit it
  and use `server:audio_port`.

### Linux server

`~/.config/lgx/server.conf`:

```ini
[network]
interface = enp10s0
video_port = 5902
audio_port = 5903
pair_port = 5904

[video]
fps = auto
bitrate = 40000000
output =
low_power = true
async_depth = 2
vpp = true

[audio]
enabled = true

[input]
enabled = true
```

`interface` is the Linux interface to bind (find it with `ip -br addr`). All
listeners bind only that interface's IPv4 address. Command-line flags override
the file for the current run. Missing files are created with defaults.

### macOS server

`~/.config/lgx/macos.conf`:

```ini
[network]
listen = 192.168.1.50:5902
token = <64 hex characters>
pair_port = 5904

[video]
fps = 120
bitrate = 80000000
display = 0
wheel_invert = true
adaptive = true
```

The file survives rebuilds and keeps the token out of the LaunchAgent.
Command-line flags override it. `--rotate` writes a new token here and into
`~/Library/LaunchAgents/dev.b612s.lgx.server.plist`; restart the server after
rotating. The macOS server has one token and one client; it has no per-client
registry, `--list` or `--revoke`.

## Audio

Linux: audio is captured from the default sink monitor by a native PipeWire
capture stream (`stream.capture.sink = true`) in float32, the graph's native
format, so PipeWire does no conversion on capture. The server performs the
single float32 to s16le step itself with TPDF dither, which turns quantization
into noise instead of low-level distortion; the wire format stays 48 kHz s16le
stereo and the client plays it through a native PipeWire playback stream. The
client counts playback underruns and prints them in its five-second audio
statistics.

macOS: `lgx-macos-audio` captures the system audio mix with a Core Audio
process tap in float32 and paces silence itself, so the client gets a
continuous stream. The app needs the **System Audio Recording** permission.
Local playback is muted while streaming, matching the Linux server.

`[audio] mute_unfocused` intentionally discards audio while the client window
is unfocused, on both server backends.

## Pairing and clients

Pairing is explicit: the server prints a one-time code, the client enters it,
and the token plus the current video/audio ports arrive inside the
code-authenticated X25519/AES-GCM channel and are written to the profile.

Linux server:

```bash
lgx-server --pair --pair-name archlinux
# prints a one-time code and the client command
lgx-client archlinux --pair 192.168.1.50:5904 --pair-code CODE
lgx-client archlinux
```

The Linux server keeps a per-client registry in `~/.config/lgx/clients`; tokens
are AES-256-GCM sealed under `~/.config/lgx/master.key` (32 random bytes,
`0600`). It is single-client by design: while one client is connected, another
video or audio connection is rejected. The client can hold profiles for several
servers and run one instance per server.

```bash
lgx-server --list
lgx-server --revoke archlinux      # remove the paired client
lgx-server --rotate archlinux      # new token; update client.conf
```

The running server reloads the registry every second and closes a revoked
client's stream immediately. Back up `master.key` with the registry: losing it
requires pairing every client again.

macOS server:

```bash
"/Applications/LGX Server.app/Contents/MacOS/lgx-server-bin" \
  --pair --pair-name macbook
lgx-client macbook --pair 192.168.1.50:5904 --pair-code CODE
```

The pairing binary only needs the configured token and pair port; it does not
capture the screen, so running it from a terminal is fine.

It has one token, configured in `macos.conf` and mirrored into the LaunchAgent.
`--rotate` replaces it; re-pair the client (or update `[macbook] token`) after a
rotation. There is no per-client registry to list or revoke.

All video, input, clipboard and audio data is inside AES-256-GCM records; the
token is the PSK and is never transmitted in plaintext. Keep client profiles
and the server's `master.key`/`macos.conf` mode `0600`.

## Input, cursor and scrolling

Linux server: input goes through the root `lgx-uinput` helper (uinput devices)
over `/run/lgx-input.sock`; the compositor cursor is composited into the
captured frame, and the client hides its host cursor while focused so exactly
one cursor is visible.

macOS server: input is injected with CGEvent. The pointer is drawn locally by
the client for latency, and the server sends the current cursor shape
(`CURSOR_IMAGE`: arrow, resize, move, wait, ...) so the local pointer shows the
right cursor. Function keys are delivered to the focused app, media keys use
`NX_SYSDEFINED`, back/forward side buttons follow Mac Mouse Fix's mapping
(navigation swipe in Safari/Finder and most Apple apps, the app's menu
shortcut where needed, raw buttons 3/4 elsewhere), and smooth scrolling is
generated as a display-synced pixel-event gesture.

Clipboard is rich text only (`text/html` plus a plain fallback), event-driven
in both directions. Files and directories are not synchronized; there are no
clipboard hotkeys and no paste injection.

## Hotkeys

| key | action |
|---|---|
| Ctrl+Shift+Alt+F | toggle fullscreen (never forwarded, OSD) |
| Ctrl+Shift+Alt+M | toggle background audio for this run (OSD) |
| Ctrl+Alt+Shift+G | release/recapture the pointer (OSD) |
| Ctrl+Alt+Shift+Q | quit |

Audio, pointer capture and fullscreen toggles each show the top-center OSD card
for 1.6 s. F9/F10/F11/F12 are ordinary guest keys. Gamepads are forwarded by
the Linux server only; the macOS server does not create virtual gamepads.

## Limitations

- No macOS client; the client requires Linux/Wayland and Vulkan. The server is
  Linux/Wayland or macOS.
- Linux server: wlroots compositors only (`wlr-screencopy`).
- HEVC only, 8-bit 4:2:0, one output, no HDR.
- RGB subpixel antialiasing shows coloured fringes after 4:2:0 encoding. Use
  grayscale antialiasing on the server (GTK3: `gtk-xft-rgba=none`; KDE:
  Sub-pixel rendering = None). This is a 4:2:0 limitation, not a shader bug.
- Audio is raw PCM (no Opus), about 187 KiB/s.
- The Vulkan client imports decoded dma-buf planes directly when the device
  supports the decoder's DRM modifier, and otherwise downloads NV12 and
  uploads it. Import needs the decoder and the renderer on the same GPU driver
  (for example Intel VAAPI + Intel Vulkan); `LGX_VK_DEVICE=<substring>` pins the
  Vulkan device and `LGX_NO_DMABUF=1` forces the upload path. FFmpeg's CUDA
  hwcontext cannot export DRM_PRIME, so NVDEC frames always use the upload
  path.
- KMS capture is not supported on the Linux server: without DRM master
  `GETFB2` returns zero handles, so the compositor keeps the
  `wlr-screencopy` path.
- VPP needs a VAAPI driver with video processing (Intel iHD here). Other
  drivers use the GL fallback automatically.
- Vulkan shaders are precompiled SPIR-V; regenerate with
  `glslangValidator -V` after editing GLSL.
- There is no discovery and no relay: both sides need a direct TCP path to
  each other's ports.

## License

MIT, except the macOS back/forward handling adapted from Mac Mouse
Fix; see `THIRD_PARTY_NOTICES.md`.
