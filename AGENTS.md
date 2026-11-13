# AGENTS.md

Rules for agents working on this repository.

## Project

`lgx` is a HEVC remote desktop for Wayland: a server inside a wlroots
compositor session encodes HEVC with VAAPI, and a client on the host decodes it
with NVDEC/CUDA or VAAPI and presents it through Vulkan.

The guest and host are expected to be on the same machine (VM + host), but the
transport is TCP so the same binaries work across a LAN.

## Layout

```
crates/lgx-proto/    wire protocol (messages, auth)
crates/lgx-server/   capture, VPP/GL NV12 conversion, VAAPI helper client, input
crates/lgx-client/   in-process HEVC decoder, Vulkan renderer, input
crates/lgx-client/src/dec_shim.c   libavcodec C shim (built by build.rs)
helper/lgx-vaenc.c   VAAPI HEVC encoder that exports dma-buf surfaces
helper/lgx-uinput.c  root helper that creates the uinput devices
scripts/             fake-server.py
```

## Language and commits

**English only.** Every comment, docstring, user-facing message, log line,
README/docs section, commit message and PR description must be written in
English. Do not add Cyrillic text anywhere in the repository.

Commit format: `area: short imperative summary`, for example
`client: fall back to CUDA when VAAPI is unavailable`.

## Codec and video path

- **HEVC only.** Do not add an H.264 path, tile streaming or shared-memory
  transport; they were removed on purpose.
- `wlr-screencopy` -> GBM dma-buf -> VPP RGB->NV12 -> `lgx-vaenc` (Intel).
  `LGX_NO_VPP=1` falls back to the `glconv` RGB->NV12 path.
- The helper is a separate process because it owns the VAAPI/VPP surfaces.
  With VPP the server only sends the captured frame fd and metadata; the GL
  fallback renders into the helper's dma-bufs.
- Encoder: `hevc_vaapi`, profile Main, `low_power=1` (VDENC), `async_depth`
  default 2, CBR, no B-frames, `AV_CODEC_FLAG_LOW_DELAY`.
- VPP imports the captured tiled ARGB dma-buf directly (DRM_PRIME_2 + a
  `VASurfaceAttribPixelFormat` attribute is mandatory), converts full-range
  BT.709 RGB->NV12 on the media engine, and the helper encodes that surface.
  The server sends only the frame metadata and fd; no RGB->NV12 GL work.
- `LGX_CPU_CONVERT=1` is an experimental linear-capture CPU conversion path.
  It is slower than VPP and is not the default.
- `async_depth > 1` makes the encoder return **more than one packet** for one
  sent frame. The helper sends `u32 count` followed by `count` packets; the
  Rust client reads exactly that many and broadcasts each access unit
  separately. Never go back to a single `len + bytes` response after `'E'`.
- A new client must not see delayed packets from the previous client. The
  server registers a client with `needs_idr = true` and drops video packets
  until it sees an HEVC IDR (NAL type 19 or 20); only then does it send.
  `CURSOR`/`CLIP_*` are unaffected.
- When forcing an IDR on the reused imported surface, set `pict_type = I` for
  that frame and reset it to `NONE` (and clear `AV_FRAME_FLAG_KEY`) on later
  frames; otherwise every frame becomes an IDR.
- Width, height, frame rate and bitrate are dynamic. The server reads the
  selected output's refresh from `wl_output`; `--fps` is only an override.
  Never hardcode 1920x1080/160.

## Client UI

- One instance per server host: the client takes a non-blocking `flock` on
  `$XDG_RUNTIME_DIR/lgx-client-<uid>-<hash>.lock`. A second launch for the same
  host exits before connecting or creating a window; different hosts use
  different locks.
- The window title is `lgx — <server IP>` and must not contain live stats.
  `LGX_DEBUG_FPS` still prints per-second diagnostics to stderr.
- `Ctrl+Shift+Alt+F` is the only client hotkey; it toggles fullscreen. The F
  keydown/keyup is swallowed; the modifiers were already forwarded, so the
  handler sends their key-up events to clear the guest state. F9/F10/F11/F12
  are ordinary keys and must reach the guest.
- After a fullscreen toggle the client skips drawing until SDL reports the
  new window size (1.2 s cap). Vulkan acquire uses a 100 ms timeout and
  recreates the swapchain/semaphores on timeout; a UI watchdog calls
  `_exit(2)` if the main loop has not advanced for 3 s instead of leaving a
  frozen window.
- Settings files are sectioned: client `~/.config/lgx/client.conf`
  (`network`, `video`, `audio`), server `~/.config/lgx/server.conf`
  (`network`, `video`, `audio`, `input`). CLI flags override the file; missing
  files are created with defaults and flat client configs are migrated.
- SDL3 gamepads are opened through `GamepadSubsystem`; duplicate
  `ControllerDeviceAdded` events are filtered by device path.

## Client decoding

- `src/dec_shim.c` is compiled by `build.rs` against the installed FFmpeg
  headers. Do not mirror libavcodec structs in Rust.
- Backends: software (0), VAAPI (1), CUDA/NVDEC (2). `pick_format` uses
  `ctx->opaque` to request the matching hardware format.
- The client currently asks the decoder for CPU NV12 (`want_dmabuf = false`)
  and uploads it to Vulkan. The C shim still has the dma-buf export code; it
  will be used when the Vulkan dma-buf import path lands.
- `src/vk.rs` owns the Vulkan instance, surface (from SDL), swapchain, render
  pass and pipeline. Two textures (R8 for Y, R8G8 for UV) are uploaded from
  staging buffers each frame; a full-range BT.709 shader converts to RGB.
- Shaders are SPIR-V files committed under `src/shaders/`. Regenerate after
  editing GLSL:
  `glslangValidator -V shaders/video.vert -o shaders/video.vert.spv`.
- Decoder selection happens **after** Vulkan init, using the Vulkan device
  name: NVIDIA -> CUDA first, otherwise VAAPI first, software last.
- One decode thread owns libavcodec; the window takes only the newest frame
  from a one-slot mailbox.
- Gamepad state snapshots go over the same TCP stream; the server's input
  thread writes a fixed `G` packet to the uinput helper.
- The Vulkan window is created with `.vulkan()` after
  `video.vulkan_load_library_default()`; there is no GL context anymore.

## Colour

Full-range BT.709 end to end. VPP is explicitly configured for
`VA_SOURCE_RANGE_FULL` on both sides (the GL fallback shader is full-range),
the Vulkan shader converts full-range BT.709, and the HEVC VUI says
`yuvj420p`/BT.709.

## Input and clipboard

- Input goes through the root `lgx-uinput` helper (uinput devices) over
  `/run/lgx-input.sock`; the Wayland virtual keyboard remains a fallback.
- Gamepads are forwarded as 24-byte `GAMEPAD` state snapshots (slot, buttons
  bitmask, six i16 axes, two hat bytes). The helper creates up to four
  `lgx virtual gamepad N` devices lazily and emits only changed buttons/axes.
  Button bit order is shared with `client/src/pad.rs`; do not reorder it.
- The old helper must be updated before the server sends `G`: an unknown
  command would desynchronise its input protocol. Install the server package
  and restart `lgx-input`.
- Client scancodes must be masked with `& 0x1ff`.
- `MODS` is sent before every `KEY`; the virtual keyboard needs explicit
  modifiers.
- Clipboard is polled every 300 ms on both sides, with `last_remote`/`last_sent`
  signature strings (`t:<text>` or `f:<paths>`) to avoid echo loops.
- `CLIP_SET` only replaces the guest clipboard. Never inject a paste: text and
  files synchronise automatically in both directions, and the user presses
  Ctrl+V in the target application.
- Files/directories are exchanged with `FILE_BEGIN`/`FILE_DATA`/`FILE_END`.
  The offer lists `(kind, relative path, size)`; data is sequential 64 KiB
  chunks. Both sides write into `~/.cache/lgx/clipboard/<pid>-<stamp>/`,
  sanitise paths (`..`/absolute rejected), create dirs 0700 and files 0600,
  and set `text/uri-list` to the top-level received paths.
- The server sends files from a per-client thread that locks the socket per
  message, so video keeps flowing during large transfers. Do not hold the
  writer mutex for a whole file.
- A newly connected client receives the server's current clipboard (text or
  files) in `handle()`, and the server sets `last_remote` so its poll does not
  immediately echo it back.
- Audio is on by default (`--no-audio` disables it) and uses the same secure
  handshake/record layer with the same token PSK. The server records
  `parec --device=@DEFAULT_MONITOR@` (48 kHz s16le stereo, 20 ms latency) and
  the client plays `pacat --playback --device=@DEFAULT_SINK@`. `pw-cat` is not
  used: its `--target` auto-connected to `easyeffects_source` (the microphone)
  on the test VM and produced silence.

## Transport

- **No plaintext TCP.** Both the main port and the audio port start with an
  `LGX1` handshake: ephemeral X25519, two random 32-byte nonces, and the auth
  token mixed as the HKDF-SHA256 IKM. Each direction gets an AES-256-GCM key
  and an independent nonce counter. Never add a plaintext fallback.
- `crates/lgx-proto/src/secure.rs` owns the handshake and record layer:
  records are split into 32 KiB chunks; the length/index/chunk length are
  included in the AEAD associated data. `Secure::close` shuts down a raw clone
  because the reader may hold the recv mutex while blocked in `read_exact`.
- One TCP connection per client: 12-byte header + optional payload; `a` is the
  payload length for `AUTH`/`VIDEO`/`KEYMAP`/`CLIP_*`; the header and payload
  are inside a secure record.
- First message is `AUTH`; the server then sends `HELLO` (codec, width,
  height) before registering the client.
- One writer thread per client; video is never copied in the server beyond
  the `Vec` that moves the encoder output.
- Video packets dropped because a client is behind trigger a rate-limited IDR
  request (no more than twice a second).

## Build and checks

```bash
cargo build --release
cargo test --workspace
gcc -O2 -o lgx-vaenc helper/lgx-vaenc.c -lavcodec -lavutil -lva -lva-drm
```

Manual check on the VM:

```bash
# guest
LGX_VAENC=$PWD/lgx-vaenc lgx-server --listen 0.0.0.0:5902
# host
lgx-client --connect 192.168.122.25:5902
./scripts/fake-server.py ...
```

The client needs FFmpeg headers, SDL3, ash (Vulkan bindings) and a Vulkan
driver. The server needs libva headers for the helper.

## Known pitfalls

1. VAAPI HEVC with `async_depth > 1` emits several packets per frame; batching
   them incorrectly produced "PPS id out of range" and missing references.
2. `avcodec_flush_buffers` is not supported by the VAAPI encoder; do not rely
   on it to drop delayed packets. Gate new clients on the first IDR instead.
3. Leaving `pict_type = I` on a reused surface makes every frame an IDR.
4. `wlr-screencopy` costs a GPU blit (~5 ms at 1080p). After issuing the copy
   request, flush the Wayland connection immediately; otherwise the request
   is only sent on the next blocking dispatch and the blit is not overlapped
   (~120 fps instead of ~158 fps).
5. ivshmem with this VM's vfio iGPU crashes QEMU; it is not a transport here.
6. KMS capture is not available from a non-master process: `GETFB2` returns
   zero handles without DRM master. Do not build a KMS path that assumes
   otherwise. VPP removes most of the reason to want one.
7. VPP surface creation with DRM_PRIME_2 fails unless the surface attributes
   include `VASurfaceAttribPixelFormat` (`VA_FOURCC_ARGB` for ARGB8888,
   `VA_FOURCC_BGRX` for XRGB8888). The pipeline parameter must request
   `VAProcColorStandardExplicit` with `VA_SOURCE_RANGE_FULL` on both sides,
   otherwise VPP silently emits limited-range NV12 and the colours are
   crushed.
8. SDL3 needs `WAYLAND_DISPLAY`; on a systemd user service set it explicitly.
9. `pw-cat -r --target <sink>.monitor` is not reliable on PipeWire: it can
   auto-connect to the default source (`easyeffects_source`) and stream zeros.
   Use `parec @DEFAULT_MONITOR@` for capture and `pacat @DEFAULT_SINK@` for
   playback.
10. `wl-copy` over SSH keeps stdout open in its forked daemon; redirect
   stdout/stderr to `/dev/null` in manual tests or the SSH command hangs.
11. The Vulkan swapchain needs `SDL_WINDOW_VULKAN` (`.vulkan()`) and
   `SDL_Vulkan_LoadLibrary` (`vulkan_load_library_default`) before window
   creation. Vulkan NDC y=-1 is the top of the viewport; the fullscreen
   triangle maps UV (0,0) there, so no vertical flip is needed.
12. `VkSurfaceKHR` comes from SDL; the sdl3 crate must be built with its `ash`
    feature so the surface types match the `ash` crate. Do not mix a raw
    `u64` surface with `ash` handles.
13. `text/uri-list` lines end in CRLF; strip `\r` before using a path in shell
    tests. The Rust parser already does.
14. `vkAcquireNextImageKHR` with `UINT64_MAX` freezes the whole client during
    a Wayland fullscreen/resize transition. Never wait forever there. Reset
    the in-flight fence only after a successful acquire, and recreate the
    frame semaphores when the swapchain is recreated.
15. The secure layer is mandatory. An old plaintext client fails the `LGX1`
    handshake and is disconnected; update client and server together. The auth
    token is the PSK and is never transmitted, so it must be identical on both
    sides.
