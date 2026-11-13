# lgx

Low-latency HEVC remote desktop for Wayland, built like Sunshine/Moonlight for
the case where the guest and the host are on the same machine.

The guest encodes HEVC on its GPU; the host decodes HEVC on **its** GPU
(NVDEC/CUDA on NVIDIA, VAAPI on Intel/AMD) and presents the frames through
Vulkan 1.3+. There is no ffmpeg subprocess and no pipe.

```
guest: wlr-screencopy -> GBM dma-buf -> VPP RGB->NV12 -> VAAPI HEVC -> TCP
host:  TCP -> libavcodec HEVC -> NV12 -> Vulkan (R8/R8G8) -> swapchain
```

The server targets the compositor's actual refresh rate. On a 1080p@160 panel
it encodes at 160 fps; `--fps` is only an override, not the default.

## Why this design

| | Sunshine + Moonlight | Looking Glass | lgx |
|---|---|---|---|
| guest | Windows/Linux | Windows | Linux (Wayland) |
| capture | DXGI/KMS | ivshmem | wlr-screencopy + GBM |
| encode | NVENC/VAAPI/AMF | none | VAAPI HEVC (Intel VDENC) |
| client decode | NVDEC/VAAPI/DXVA | n/a | CUDA/NVDEC, VAAPI, software |
| transport | UDP/RTP | ivshmem | TCP |
| target rate | panel refresh | panel refresh | panel refresh (auto) |

## Features

- HEVC Main profile, 8-bit 4:2:0, full-range BT.709.
- Intel low-power VDENC (`low_power=1`) and asynchronous encode
  (`async_depth=2` by default): the encoder overlaps the next capture.
- New clients get an IDR immediately; delayed packets from the previous client
  are dropped until that IDR, so the first video packet is always decodable.
- Capture, encode and display geometry come from the selected output; the
  server reads the refresh rate from `wl_output`.
- Vulkan presentation with a full-range BT.709 shader, immediate present mode
  and letterboxing; no OpenGL/EGL dependency.
- Only one client instance can run per server IP. A second launch for the same
  IP exits immediately without a window; different IPs can run side by side.
- Client decoder selection is automatic: the Vulkan device vendor says NVIDIA
  -> CUDA/NVDEC, otherwise VAAPI, with a software fallback. `--decoder`
  overrides it.
- Decoded frames are transferred as NV12 and uploaded to two Vulkan textures
  (R8 and R8G8); hardware decoding stays on the GPU. A Vulkan dma-buf import
  path is the next step.
- Only the newest decoded frame is kept, so a slow window never builds a
  queue of stale frames.
- Input through kernel-level `uinput` (every key and compositor shortcut),
  absolute pointer, **gamepads** (up to four, hot-plugged), rich-text
  clipboard, and low-latency raw audio
  (48 kHz stereo, captured from the default sink monitor and played on the
  host).
- The window title is just `lgx — <server IP>`; FPS/bandwidth stay in the
  per-second `LGX_DEBUG_FPS` log.
- Token authentication before a client receives any frame.

## Settings

Both sides use sectioned INI files:

`~/.config/lgx/client.conf`:

```ini
[network]
server = 192.168.122.25:5902

[video]
width = 1920
height = 1080
fullscreen = false
vsync = false
decoder = auto
cursor = remote

[audio]
enabled = true
connect =
```

`~/.config/lgx/server.conf`:

```ini
[network]
listen = 0.0.0.0:5902
audio_listen = 0.0.0.0:5903

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
created with defaults; old flat client configs are migrated on load.

Fullscreen transitions are guarded: the client pauses drawing until SDL has
the new window size, rebuilds the swapchain and semaphores, and a watchdog
closes the client if the UI still stalls for three seconds.

## Gamepads

Connect a controller on the host; SDL3 opens it and the client sends compact
state snapshots. The guest `lgx-uinput` helper creates a kernel gamepad lazily
on the first snapshot, so Steam/Proton and native games see a normal evdev
device with sticks, triggers, D-pad, face/shoulder buttons and paddles.
The helper runs as root; restart `lgx-input` after updating the server package.

## Requirements

Server (guest): wlroots compositor (Hyprland, Sway, ...), Intel/AMD VAAPI with
HEVC encode, FFmpeg + libva for the helper, `wl-clipboard`, and a
Pulse-compatible audio server (`pipewire-pulse`/PulseAudio) for `parec`.

Client (host): Wayland, SDL3, a Vulkan 1.3 driver (`vulkan-icd-loader` plus
NVIDIA/Mesa), FFmpeg libavcodec/libavutil (headers at build time),
`libxkbcommon`, `wl-clipboard`, and `libpulse` for `pacat`. On NVIDIA the
runtime driver provides NVDEC/CUDA; on Intel/AMD VAAPI is used.

Server (guest): VAAPI with VPP (Intel iHD in this VM), libva headers and the
VAAPI HEVC encoder.

## Build

```bash
cargo build --release
gcc -O2 -o lgx-vaenc helper/lgx-vaenc.c -lavcodec -lavutil -lva -lva-drm
gcc -O2 -o lgx-uinput helper/lgx-uinput.c
```

The client build compiles `crates/lgx-client/src/dec_shim.c` against the
installed FFmpeg headers (see `build.rs`), so FFmpeg development headers are
required. On Arch: `pacman -S ffmpeg sdl3 vulkan-icd-loader libxkbcommon` (plus your GPU's
Vulkan driver).

The server looks for the encoder helper at `/usr/lib/lgx/lgx-vaenc`. From a
source tree, set `LGX_VAENC=$PWD/lgx-vaenc`.

## Usage

Server, inside the graphical session:

```bash
lgx-server --listen 0.0.0.0:5902 --bitrate 40000000
# --fps N            override the detected refresh rate
# --async-depth N    1 = lowest latency, 2 = default throughput
# --no-low-power      disable Intel VDENC
# --output NAME      pick an output (hyprctl monitors)
# --no-audio         disable the raw s16le 48 kHz stereo stream (on by default)
```

Audio is captured from `@DEFAULT_MONITOR@` with `parec` and played on the host
with `pacat --device=@DEFAULT_SINK@` at 20 ms latency. A muted host sink is
the only thing that can silence it after that.

The server logs the actual target, for example:

```
lgx-server: HEVC ready (1920x1080@160, 40 Mbit/s, low_power=true, pitch 1920)
```

Client, on the host:

```bash
lgx-client --connect 192.168.122.25:5902
# --decoder auto|cuda|vaapi|software
# --size 1920x1080
# --cursor local|remote
# --no-window --frames 300 --screenshot shot.ppm
```

The client reads the server geometry from `HELLO` and sizes the window to it
on the first run; the size is stored in `~/.config/lgx/client.conf`.

### Client configuration

```ini
server = 192.168.122.25:5902
width = 1920
height = 1080
fullscreen = false
vsync = false
audio = true           # raw 48 kHz stereo, 20 ms
audio_connect = 192.168.122.25:5903
decoder = auto          # auto, cuda, vaapi, software
cursor = remote         # remote (streamed) or local (zero latency)
```

Command-line flags win over the file. Unknown keys survive a save round trip.

#### Text clipboard

**Rich text** is synchronized automatically with normal Ctrl+C / Ctrl+V in
both directions. The client and server copy the source `text/html` plus a
plain-text fallback, and the peer offers both MIME types, so rich editors keep
bold/links/lists while terminals still paste plain text. Files and directories
are **not** synchronized at all, and there are no clipboard hotkeys.

### Revoking a client

```bash
lgx-server --list
lgx-server --revoke laptop          # ID or name
lgx-server --revoke <client-id>
```

Revocation removes the registry entry. The running server reloads the registry
every second: an already-connected revoked client is disconnected immediately
and new connections with that ID fail. Other clients are unaffected.

An old single-line `~/.config/lgx/token` is treated as client `default`. On the
first run after upgrading, the server migrates the old global token into a
`default` registry entry, so an existing client keeps working. Old clients
that do not send an ID must be updated.

`--pair-listen` defaults to `0.0.0.0:5904`; the address printed in the example
is the server's LAN address.

## Hotkeys

| key | action |
|---|---|
| Ctrl+Shift+Alt+F | toggle client fullscreen (never forwarded) |
| Ctrl+Alt+Q | quit |
| Ctrl+Alt+G | release/grab compositor shortcuts |

Only text is synchronized automatically in both directions: copy on one side
and press Ctrl+V on the other. File clipboard contents are ignored. There is
no paste injection and no transfer hotkey. F9/F10/F11/F12 are ordinary keys
for the guest.

All traffic — video, input, clipboard and audio — is inside AES-256-GCM
records; pairing uses the same channel with a one-time code. Each client has
its own token, so a revoked client does not affect the others. AES-NI makes the cost negligible at the stream's few MB/s; only
record sizes are visible to a passive observer, never HEVC frames or tokens.

## Measured (VM: Hyprland 1920x1080@160, Intel UHD 770; host RTX 4070 SUPER)

| check | result |
|---|---|
| server, HEVC 40 Mbit/s, low_power, async_depth=2 | 157.8 fps, 4.5 MiB/s |
| server breakdown | wait 0.83 ms, render 2.84 ms, encode 0.59 ms |
| client, RTX 4070 SUPER, CUDA/NVDEC + Vulkan, 960x540 window | 171.0 fps |
| client, RTX 4070 SUPER, CUDA/NVDEC + Vulkan, 1920x1080 window | 166.7 fps |
| server, encrypted transport, HEVC 1080p | 157.9 fps |
| client, encrypted transport + audio | 165.5 fps, 0 dropped |
| server, VPP conversion, two 4K60 videos + workspace switches | 152-156 fps |
| server, GL conversion, same load | 130-137 fps |
| client Vulkan device | Vulkan 1.4.363, NVIDIA GeForce RTX 4070 SUPER |
| target rate | automatically 160 Hz from `wl_output` |

The server issues the next `wlr-screencopy` request and flushes the Wayland
connection immediately, so the compositor's blit overlaps the NV12 conversion
and the encode. Without that flush the copy starts on the next blocking
dispatch and the pipeline drops to ~120 fps. The panel refresh rate, not the
encoder, is now the limit.

## Limitations

- wlroots compositors only (wlr-screencopy).
- HEVC only, 8-bit, one output, no HDR.
- Audio is raw PCM (no Opus): ~187 KiB/s.
- The Vulkan path currently uploads NV12; a Vulkan dma-buf import path is the
  next optimization. Decoding is still hardware (CUDA/NVDEC or VAAPI).
- KMS capture is not supported: without DRM master `GETFB2` returns zero
  handles, so the compositor keeps the wlr-screencopy path. VPP already moves
  the conversion off the 3D engine.
- VPP needs a VAAPI driver with video processing (Intel iHD here). Other
  drivers use the GL path; the server picks it automatically.
- Vulkan shaders are precompiled SPIR-V; regenerate with
  `glslangValidator -V` if you edit the GLSL.

## License

MIT.
