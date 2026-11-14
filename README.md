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
# --cursor local|remote
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
cursor = remote

[audio]
enabled = true
mute_unfocused = true

[input]
gamepads = true
clipboard = true
clipboard_rich = true
clipboard_max_bytes = 1048576

[archlinux]
server = 192.168.122.25:5902
token = <token>
```

`lgx-client archlinux` selects the `[archlinux]` section; the section name is
also the public client ID. `[video]`, `[audio]` and `[input]` are shared;
each profile has only `server` and `token`. The file contains tokens and is
`0600`.

- `mute_unfocused = true` discards received audio and emits silence while the
  client window is unfocused; the native PipeWire stream stays active.
  `--no-mute-unfocused` disables it.
- `[input] gamepads = false` (or `--no-gamepads`) skips gamepad forwarding.
- `clipboard = false` disables sync; `clipboard_rich = false` sends only plain
  text; `clipboard_max_bytes` limits both directions (`0` = unlimited).
- `[audio] connect` is an optional audio-port override; normally omit it and
  derive `host:5903` from `server`.

Server `~/.config/lgx/server.conf`:

```ini
[network]
listen = 0.0.0.0:5902
audio_listen = 0.0.0.0:5903
pair_listen = 0.0.0.0:5904

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

Command-line flags override the file for the current run. Missing files are
created with defaults.

## Audio

Audio is captured from the default sink monitor by a native PipeWire capture
stream (`stream.capture.sink = true`) and played by a native PipeWire playback
stream on the host. Both are 48 kHz s16le stereo. A muted host sink is the
only external thing that can silence it; `[audio] mute_unfocused` intentionally
discards audio while the client window is unfocused.

## Pairing and revocation

```bash
lgx-server --pair --pair-name archlinux
lgx-client archlinux --pair 192.168.122.25:5904 --pair-code CODE
lgx-client archlinux
```

`--pair` binds the pair port (`0.0.0.0:5904` by default), prints a one-time
code, accepts one client for three minutes, and exits. The client writes
`[archlinux]` with `server` and the new token. Without `--pair-name` the
server generates a random ID, which cannot be launched as a named profile.

```bash
lgx-server --list
lgx-server --revoke archlinux      # ID or name
lgx-server --rotate archlinux      # new token; update client.conf
```

The registry lives in `~/.config/lgx/clients`; tokens are AES-256-GCM sealed
under `~/.config/lgx/master.key` (32 random bytes, `0600`). Back up
`master.key` with the registry: losing it requires pairing every client again.
The running server reloads the registry every second and closes a revoked
client's stream immediately.

All traffic — video, input, clipboard and audio — is inside AES-256-GCM
records. The token is the PSK and is never transmitted in plaintext.

## Hotkeys and clipboard

| key | action |
|---|---|
| Ctrl+Shift+Alt+F | toggle fullscreen (never forwarded) |
| Ctrl+Shift+Alt+M | toggle background audio for this run (OSD) |
| Ctrl+Alt+Q | quit |
| Ctrl+Alt+G | release/grab compositor shortcuts |

Rich text (`text/html` plus a plain fallback) is synchronized automatically in
both directions with normal Ctrl+C/Ctrl+V. Files and directories are not
synchronized; there are no clipboard hotkeys and no paste injection.
`Ctrl+Shift+Alt+M` is runtime-only and resets to `[audio] mute_unfocused` on
restart. F9/F10/F11/F12 are ordinary guest keys.

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
