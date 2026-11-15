# lgx

Low-latency HEVC remote desktop for Wayland, for the case where the guest and
the host run on the same machine.

The guest encodes on its GPU; the host decodes on **its** GPU (NVDEC/CUDA on
NVIDIA, VAAPI on Intel/AMD) and presents frames through Vulkan. There is no
ffmpeg subprocess and no shared-memory transport.

```
guest: wlr-screencopy -> GBM dma-buf -> VPP RGB->NV12 -> VAAPI HEVC -> TCP
host:  TCP -> libavcodec HEVC -> NV12 -> Vulkan -> swapchain
```

The server captures at the selected output's refresh rate. `--fps` is only an
override.

## Features

- HEVC Main profile, 8-bit 4:2:0, full-range BT.709.
- Intel VDENC (`low_power = true`) and asynchronous encoding
  (`async_depth = 2` by default).
- A new client waits for an IDR; delayed packets from the previous client are
  never delivered.
- Geometry, refresh rate and bitrate follow the selected `wl_output`.
- Vulkan presentation with letterboxing; no OpenGL/EGL dependency.
- One client instance per server host; different hosts can run side by side.
- Automatic decoder selection: NVIDIA -> CUDA/NVDEC, otherwise VAAPI, with a
  software fallback. `--decoder` overrides it.
- Only the newest decoded frame is kept, so a slow window never builds a
  queue of stale frames.
- Input through kernel `uinput`, absolute pointer, up to four hot-plugged
  gamepads, and rich-text clipboard.
- Native PipeWire audio: 48 kHz stereo from the guest's default sink monitor
  to the host's default sink. No `pacat`, `parec` or PulseAudio layer.
- Per-client tokens, AES-256-GCM transport, and revocation at runtime.
- Window title `lgx — <profile>`; FPS/bandwidth diagnostics behind
  `LGX_DEBUG_FPS`.

## Requirements

Server (guest): a wlroots compositor (Hyprland, Sway, ...), VAAPI with HEVC
encode and video processing (VPP), FFmpeg and libva for the helper,
`wl-clipboard`, and PipeWire (the native `pipewire` client library).

Client (host): Wayland, SDL3, a Vulkan 1.3 driver (`vulkan-icd-loader` plus
NVIDIA/Mesa), FFmpeg libavcodec/libavutil headers at build time,
`libxkbcommon`, `wl-clipboard`, and PipeWire (the native `pipewire` client
library). On NVIDIA the runtime driver provides NVDEC/CUDA; on Intel/AMD VAAPI
is used.

## Build

```bash
cargo build --release
gcc -O2 -o lgx-vaenc helper/lgx-vaenc.c -lavcodec -lavutil -lva -lva-drm
gcc -O2 -o lgx-uinput helper/lgx-uinput.c
```

The client build compiles `crates/lgx-client/src/dec_shim.c` against the
installed FFmpeg headers (see `build.rs`). On Arch the build dependencies
include `ffmpeg`, `sdl3`, `vulkan-icd-loader`, `libxkbcommon`, `pipewire` and
your GPU's Vulkan driver.

The server looks for the encoder helper at `/usr/lib/lgx/lgx-vaenc`. From a
source tree, set `LGX_VAENC=$PWD/lgx-vaenc`.

## Packaging

```bash
./packaging/build.sh                # both packages
./packaging/build.sh lgx-client     # a single package
```

`build.sh` archives the committed tree as `packaging/lgx-<version>.tar.gz`
(it refuses to package a dirty working tree), builds the PKGBUILDs under
`packaging/`, and leaves the finished `*.pkg.tar.zst` files in the project
root. makepkg's `src`/`pkg` work trees go to `target/pkgbuild`, so `packaging/`
only ever holds PKGBUILDs, unit files and the desktop entry. Bump `pkgrel` in
the PKGBUILD before a rebuild meant to replace an installed package; the
script stops instead of producing a version older than the one pacman has.

## Usage

Start the server inside the guest's graphical session:

```bash
lgx-server --listen 0.0.0.0:5902 --bitrate 40000000
# --fps N            override the detected refresh rate
# --async-depth N    1 = lowest latency, 2 = default throughput
# --no-low-power      disable Intel VDENC
# --output NAME       pick an output (hyprctl monitors)
# --no-audio          disable the raw s16le 48 kHz stereo stream
```

The shipped server service runs `lgx-server --listen 0.0.0.0:5902 --audio`.

Start the client on the host with a profile name:

```bash
lgx-client archlinux
# --decoder auto|cuda|vaapi|software
# --size 1920x1080
# --cursor-grab|--no-cursor-grab
# --no-window --frames 300 --screenshot shot.ppm
```

The client reads the geometry from the server's `HELLO` and sizes the window
on the first run. `lgx-client --list` prints profiles without tokens;
`lgx-client --check <name>` validates a profile.

After a lost session the client reconnects with exponential backoff and
restarts the session (window and audio are re-created). While waiting it shows
a separate Qt `lgx-reconnect-qt` window in the desktop style with an
indeterminate progress line and a Cancel button; the SDL/Vulkan client does
not link Qt. Use `--no-reconnect` for tests.

## Configuration

Both sides use sectioned INI files. Client `~/.config/lgx/client.conf`:

```ini
[video]
width = 1920
height = 1080
fullscreen = false
vsync = false
decoder = auto

[audio]
enabled = true
mute_unfocused = true

[input]
gamepads = true
cursor_grab = true
clipboard = true
clipboard_rich = true
clipboard_max_bytes = 1048576

[archlinux]
server = 192.168.122.25
video_port = 5902
audio_port = 5903
pair_port = 5904
token = <token>
```

`lgx-client archlinux` selects the `[archlinux]` section; the section name is
also the public client ID. `[video]`, `[audio]` and `[input]` are shared;
each profile has `server`, `video_port`, `audio_port`, `pair_port` and
`token`. The file contains tokens and is `0600`. The ports are explicit;
there is no discovery.

- `mute_unfocused = true` discards received audio and emits silence while the
  client window is unfocused; the native PipeWire stream stays active.
  `--no-mute-unfocused` disables it.
- `[input] gamepads = false` (or `--no-gamepads`) skips gamepad forwarding.
- `clipboard = false` disables sync; `clipboard_rich = false` sends only plain
  text; `clipboard_max_bytes` limits both directions (`0` = unlimited).
- `[input] cursor_grab = true` confines the pointer to the window (SDL mouse
  grab) and keeps the original absolute motion. It does not change which
  cursor is drawn: while the window is focused the host cursor is hidden and
  the guest cursor is shown in the video; losing focus shows the host cursor
  and hides the guest one. `Ctrl+Alt+Shift+G` only releases/recaptures the
  confinement.
- `[audio] connect` is an optional audio-address override; normally omit it
  and use `server:audio_port`.

Server `~/.config/lgx/server.conf`:

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

`interface` is the guest interface to bind (find it with `ip -br addr`). All
listeners bind only that interface's IPv4 address; nothing binds 0.0.0.0.
Command-line flags override the file for the current run. Missing files are
created with defaults.

## Audio

Audio is captured from the default sink monitor by a native PipeWire capture
stream (`stream.capture.sink = true`) and played by a native PipeWire playback
stream on the host. Both are 48 kHz s16le stereo. A muted host sink is the
only external thing that can silence it; `[audio] mute_unfocused` intentionally
discards audio while the client window is unfocused.

## Pairing and clients

```bash
lgx-server --pair --pair-name archlinux
# server prints a one-time code and the client command
lgx-client archlinux --pair 192.168.122.25:5904 --pair-code CODE
lgx-client archlinux
```

The server prints the code; the client enters it. The code protects the
X25519/AES-GCM channel used only for pairing; the long-term token and the
current video/audio ports arrive inside that channel and are written to
`[archlinux]`. Pairing accepts one client for three minutes and five failed
attempts.

The server is single-client by design: while one client is connected, another
video or audio connection is rejected. The client can hold profiles for
several servers and run one instance per server (the per-host instance lock
allows different hosts side by side).

```bash
lgx-server --list
lgx-server --revoke archlinux      # remove the paired client
lgx-server --rotate archlinux      # new token; update client.conf
```

The registry lives in `~/.config/lgx/clients`; tokens are AES-256-GCM sealed
under `~/.config/lgx/master.key` (32 random bytes, `0600`). Back up
`master.key` with the registry: losing it requires pairing every client again.
The running server reloads the registry every second and closes a revoked
client's stream immediately.

All video, input, clipboard and audio data is inside AES-256-GCM records; the
token is the PSK and is never transmitted in plaintext.

## Hotkeys and clipboard

| key | action |
|---|---|
| Ctrl+Shift+Alt+F | toggle fullscreen (never forwarded, OSD) |
| Ctrl+Shift+Alt+M | toggle background audio for this run (OSD) |
| Ctrl+Alt+Shift+G | release/recapture the pointer (OSD) |
| Ctrl+Alt+Shift+Q | quit |

`[input] cursor_grab = true` (the default) confines the pointer to the window
with SDL mouse grab while keeping the same absolute motion as before (no
relative/raw deltas). The client always draws exactly one cursor: while focused
it hides the host cursor and includes the guest cursor in the video; losing
focus shows the host cursor and excludes the guest one. `Ctrl+Alt+Shift+G`
toggles only the pointer confinement; `cursor_grab = false` starts unconfined.

Rich text (`text/html` plus a plain fallback) is synchronized automatically in
both directions with normal Ctrl+C/Ctrl+V. Files and directories are not
synchronized; there are no clipboard hotkeys and no paste injection.
`Ctrl+Shift+Alt+M` is runtime-only and resets to `[audio] mute_unfocused` on
restart. Audio, pointer capture and fullscreen toggles each show the top-center
OSD card for 1.6 s. F9/F10/F11/F12 are ordinary guest keys.

## Gamepads

Connect a controller on the host; SDL3 opens it and the client sends compact
state snapshots. The guest's root `lgx-uinput` helper creates an evdev gamepad
lazily on the first snapshot, so Steam/Proton and native games see a normal
device with sticks, triggers, D-pad, face/shoulder buttons and paddles.
Restart `lgx-input` after updating the server package.

## Limitations

- wlroots compositors only (`wlr-screencopy`).
- HEVC only, 8-bit 4:2:0, one output, no HDR.
- RGB subpixel antialiasing shows coloured fringes after 4:2:0 encoding. Use
  grayscale antialiasing in the guest (GTK3: `gtk-xft-rgba=none`; KDE:
  Sub-pixel rendering = None). This is a 4:2:0 limitation, not a shader bug.
- Audio is raw PCM (no Opus), about 187 KiB/s.
- The Vulkan path uploads NV12; dma-buf import is the next optimization.
  Decoding is still hardware (CUDA/NVDEC or VAAPI).
- KMS capture is not supported: without DRM master `GETFB2` returns zero
  handles, so the compositor keeps the `wlr-screencopy` path.
- VPP needs a VAAPI driver with video processing (Intel iHD here). Other
  drivers use the GL fallback automatically.
- Vulkan shaders are precompiled SPIR-V; regenerate with
  `glslangValidator -V` after editing GLSL.

## License

MIT.
