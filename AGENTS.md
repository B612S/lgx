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
- The window title is `lgx — <name>` and must not contain live stats or the
  server address; the profile name is enough. The client reconnects with exponential backoff after a lost session by
  exec'ing itself (window/audio are re-created); `--no-reconnect` disables it.
  The reconnect wait uses the child `lgx-reconnect-qt` (Qt Widgets, system
  style, indeterminate progress bar, Cancel -> exit 2); the client itself must
  not link Qt.
  `--list`/`--check NAME` inspect profiles without printing tokens. The `<name>` is the profile section selected on the command line.
  `LGX_DEBUG_FPS` still prints per-second diagnostics to stderr.
- Client hotkeys are `Ctrl+Alt+Shift+G` (release/recapture the pointer), `Ctrl+Shift+Alt+F` (fullscreen),
  `Ctrl+Shift+Alt+M` (runtime-only toggle of background audio, not persisted;
  startup value comes from `[audio] mute_unfocused`; the OSD is a luma-style
  top-center text card (40 px, radius 8, 85% theme background, 20% border)
  rendered inside the Vulkan swapchain, not a separate window; audio, pointer
  and fullscreen toggles all select an OSD card; its font is
  resolved from KDE `kdeglobals` via `fc-match`, then `sans-serif`; point
  size/DPI come from kdegobals/Xft/SDL scale and it is rasterized by FreeType
  with the fontconfig hintstyle) and `Ctrl+Alt+Shift+Q` (quit). There is no
  manual compositor-shortcut release hotkey. `[input] cursor_grab = true`
  confines the pointer to the window with SDL mouse grab and keeps the
  original absolute motion. It must not change cursor visibility: while
  focused the host cursor is hidden and the guest cursor is composited into
  the video; focus loss shows the host cursor and excludes the guest cursor.
  `Ctrl+Alt+Shift+G` toggles only the confinement, and `cursor_grab = false`
  starts unconfined. Do not use SDL relative mode for this: it changed the
  pointer feel. The
  keydown/keyup is swallowed and modifier key-ups clear the guest state.
  F9/F10/F11/F12 are ordinary keys and must reach the guest. The window
  decoration's close button must be handled as
  `WindowEvent::CloseRequested` and set `quit`; SDL does not send
  `Event::Quit` for it, and otherwise the process keeps running without a
  window.
- `[video] present_hz` (or `--present-hz`) caps how often a decoded frame is
  uploaded and presented: `0` (default) presents every frame, `auto` uses the
  display refresh. Skipped frames are dropped, not delayed, so the newest one
  still wins; keep the default for the lowest input latency.
- After a fullscreen toggle the client skips drawing until SDL reports the
  new window size (1.2 s cap). Vulkan acquire uses a 100 ms timeout and
  recreates the swapchain/semaphores on timeout; a UI watchdog calls
  `_exit(2)` if the main loop has not advanced for 3 s instead of leaving a
  frozen window.
- Client profiles: `~/.config/lgx/client.conf` has shared `video`/`audio`/
  `input` sections plus `[archlinux]`, `[client2]` sections with `server`,
  `video_port`, `audio_port`, `pair_port` and `token`. There is no discovery;
  the client connects exactly to those ports. `[input] gamepads = false` (or
  `--no-gamepads`) skips the SDL gamepad subsystem and sends no `GAMEPAD`
  packets; keyboard/mouse are unaffected. `clipboard = false` disables sync,
  `clipboard_rich = false` forces plain text, and `clipboard_max_bytes` limits
  both directions (0 = unlimited). `[input] cursor_grab = true` confines the
  pointer to the window; `Ctrl+Alt+Shift+G` toggles the confinement; cursor
  visibility stays remote-style (guest cursor while focused, host cursor when
  unfocused). Launch with
  `lgx-client archlinux`; the section name is the public client ID. There is no
  `[network]`, no `[default]`, no `client_id` field and no config migration.
  The file is mode 0600. `[audio] connect` is an optional audio-address
  override. Server settings are `~/.config/lgx/server.conf` (`network`,
  `video`, `audio`, `input`); `network.interface`, `video_port`, `audio_port`
  and `pair_port` determine the bound interface and ports. CLI flags override.
  `[audio] mute_unfocused = true` discards received audio and outputs silence
  while the window is unfocused (the native PipeWire playback stream stays
  active); the secure audio stream is still drained.
- Pairing is server-initiated and explicit: `lgx-server --pair
  --pair-name archlinux` prints a one-time code; the client runs
  `lgx-client archlinux --pair HOST:PAIRPORT --pair-code CODE`. The code
  protects the X25519/AES-GCM exchange; the token and the server's current
  video/audio ports arrive encrypted and are written to the profile. Pairing
  accepts one client for three minutes and five failed attempts. There is no
  LAN discovery, list-paired/unpair command or multi-client fan-out.
- SDL3 gamepads are opened through `GamepadSubsystem`; duplicate
  `ControllerDeviceAdded` events are filtered by device path.
- Mouse motion is coalesced per SDL event batch: only the newest motion before
  a non-motion event is sent, so a 1000 Hz mouse does not produce 1000 secure
  records per second. Button, wheel and key events are never coalesced, which
  keeps click positions exact.

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
  `/run/lgx-input.sock`. The helper chowns the socket to `LGX_INPUT_UID`
  (default 1000) with mode 0600, checks `SO_PEERCRED`, and writes the gamepad
  marker 0644. Never make this socket world-writable; it injects input.
- Gamepads are forwarded as 24-byte `GAMEPAD` state snapshots (slot, buttons
  bitmask, six i16 axes, two hat bytes). The helper creates up to four
  `lgx virtual gamepad N` devices lazily and emits only changed buttons/axes.
  Button bit order is shared with `client/src/pad.rs`; do not reorder it.
- The old helper must be updated before the server sends `G`: an unknown
  command would desynchronise its input protocol. Install the server package
  and restart `lgx-input`.
- The helper unlinks and re-binds `/run/lgx-input.sock` when it restarts, so
  the server's input thread must keep reconnecting to it. A one-shot connect
  silently disables all guest input until the server restarts, which is what
  a package upgrade used to do.
- Client scancodes must be masked with `& 0x1ff`.
- `MODS` is sent before every `KEY`; the virtual keyboard needs explicit
  modifiers.
- **Rich text only, event-driven.** Both sides use
  `wl-clipboard_rs::watch::Watcher`; no 300 ms polling and no `wl-paste`
  subprocess. The server skips the watcher's initial event so an old guest
  clipboard cannot overwrite the host on connect; the client sends its initial
  host clipboard. If the offer contains `text/html`, the reader returns
  `Clip::Rich { html, text }`; otherwise `Clip::Text`. The client sends
  `CLIP_SET_HTML` (u32 html length + HTML + plain) and the server broadcasts
  `CLIP_DATA_HTML`; plain uses `CLIP_SET`/`CLIP_DATA`.
- Rich clipboard is installed with `wl-clipboard-rs::copy_multi`, offering
  plain text first and `text/html` second. Plain must be first so the extra
  text MIME fallbacks use it; never put HTML first. Files are not synchronized.
- On connect the client's watcher sends its current host clipboard; the server
  ignores its watcher's first event, so the host clipboard wins.
- Never add file synchronization, clipboard hotkeys, `CLIP_GET`/`CLIP_ACK` or
  a paste injection back. Normal Ctrl+C/Ctrl+V is the whole UX.
- Audio is on by default (`--no-audio` disables it) and uses the same secure
  handshake/record layer with the same token PSK. Both sides use native
  PipeWire streams (`pipewire-rs`); do not reintroduce `parec`/`pacat`,
  PulseAudio compatibility or helper processes. The server opens an
  `Audio/Capture` stream with `stream.capture.sink = true` and `AUTOCONNECT`
  so it captures the default sink monitor, not the default source
  (microphone); the client opens an `Audio/Playback` stream with
  `AUTOCONNECT`. Both request 48 kHz s16le stereo via an `AudioInfoRaw` format
  pod. The client's stream carries `application.name = lgx - <profile>`,
  `media.name = lgx audio (<profile>)`, `application.process.binary =
  lgx-client` and `application.icon_name = lgx`; host mixers show the profile
  like the window title. Also set `node.name` and `node.description` to
  `lgx - <profile>`, because some mixers label native streams by the node.
  Both streams set `node.always-process = true`, `node.pause-on-idle = false`
  and `node.suspend-on-idle = false`: otherwise WirePlumber suspends the idle
  stream and the first sound after a pause is delayed or clipped. Both also
  request a low graph latency; the client uses `1024/48000` and the server
  `512/48000` (a 256/512 client quantum halved the video frame rate on this
  host, so 1024 is the low-latency setting that still leaves the compositor
  enough CPU). The client waits for the playback stream
  to reach `Streaming` before connecting the audio socket, so the server
  cannot fill the ring with a startup backlog. The ring is 64 KiB with an
  8 KiB playout cushion (two server chunks); it is never trimmed to one
  quantum on a normal callback, because that discarded most of each ~4 KiB
  server chunk and caused constant underrun. A high-water trim from 48 KiB
  to 16 KiB only handles a runaway backlog/clock drift. The server's
  capture/fanout channels hold four chunks. `mute_unfocused`
  discards received chunks and the process callback drains the ring buffer
  and emits silence; no external process is stopped or left behind.

## Pairing and clients

- `lgx-server --pair --pair-name NAME` binds the configured pair port on the
  configured interface, prints a one-time code, accepts one client for three
  minutes, and exits after five failed attempts. It is a local admin action
  and must not become an always-on endpoint.
- The client enters that code with `lgx-client NAME --pair HOST:PAIRPORT
  --pair-code CODE`; the pairing channel is protected by the code as PSK. The
  long-term token and the server's current video/audio ports are sent only
  inside that channel and written to `[NAME]`.
- The server is single-client: at most one video and one audio connection at
  a time; a second one is rejected. The client may keep profiles for several
  servers and run one instance per server host.
- Per-client registry: `~/.config/lgx/clients`, mode 0600, tab-separated
  `id<TAB>name<TAB>enc:nonce:token-ciphertext<TAB>created`. Tokens are
  AES-256-GCM sealed under `~/.config/lgx/master.key` (32 random bytes, 0600),
  so the registry alone is useless. New tokens are 256-bit; legacy 128-bit
  tokens are re-encrypted on startup. The registry and master key are the
  source of truth; do not add a global token path back.
- Back up `master.key` with the registry. Losing it makes encrypted tokens
  undecryptable and requires re-pairing every client; never log or commit it.
- Pairing generates a new token and sends it over the code-authenticated
  secure channel, waits for `ok`, and only then adds the entry. A failed
  pairing must not create an entry.
- The client sends its ID in a plaintext frame before `LGX1`. The ID is public;
  the server looks up the token in the registry for the handshake. Never send
  the token before the handshake.
- `lgx-server --rotate ID|NAME` replaces the token and prints it; the running
  server watches token fingerprints and closes the old session within a
  second. Update the matching `[NAME] token` in `client.conf`.
- `lgx-server --list` prints the registry. `lgx-server --revoke ID|NAME`
  removes entries. A watcher in the running server reloads the registry every
  second and closes the secure stream of any active client whose ID vanished.
  The client reader sets `Conn::closed`, and the UI/headless loop exits on it.
- The token is both the auth secret and the transport PSK. Keep the registry
  and client credential files 0600.

## Transport

- **No plaintext TCP.** Both the main port and the audio port start with a
  public client-ID frame followed by an `LGX1` handshake: ephemeral X25519, two random 32-byte nonces, and the auth
  token mixed as the HKDF-SHA256 IKM. Each direction gets an AES-256-GCM key
  and an independent nonce counter. Never add a plaintext fallback.
- `crates/lgx-proto/src/secure.rs` owns the handshake and record layer. Small
  records are sealed in place in the reusable buffer (no per-frame temporary
  Vec/copy); keep this fast path when touching the record layer. Each record
  goes out with one `write_vectored` (length prefix + sealed chunk): with
  `TCP_NODELAY` two separate writes double the packet count. Receivers use
  `recv_msg_raw`, which keeps the 12-byte header in front of the payload so a
  video packet is never memmoved; `recv_msg` is only for small control
  messages. AES-256-GCM is AES-NI accelerated and is negligible at this
  bitrate.
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
scripts/check.sh          # build + tests + both C helpers, run before pushing
./packaging/build.sh
```

Packaging layout: `packaging/build.sh` builds `lgx-client` and `lgx-server`
from the committed tree and puts the `*.pkg.tar.zst` files in the repository
root. `make-source.sh` writes the only source tarball,
`packaging/lgx-<version>.tar.gz`, and refuses a dirty tree. makepkg's work
trees belong in `target/pkgbuild` (git-ignored); never commit or keep
`packaging/*/src`, `packaging/*/pkg` or per-package copies of the tarball.
Bump `pkgrel` in a PKGBUILD for every rebuild; the build script fails when the
result would be older than the installed package.

Manual check on the VM:

```bash
# guest
LGX_VAENC=$PWD/lgx-vaenc lgx-server --listen 0.0.0.0:5902
# host
lgx-client archlinux
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
9. Audio capture must set `stream.capture.sink = true` on an `Audio/Capture`
   stream; without it PipeWire autoconnects to the default source (the
   microphone) and the client hears silence. Do not use `@DEFAULT_MONITOR@`
   or `parec`; those belong to the removed PulseAudio path. On playback, fill
   exactly `buffer.requested()` frames (bytes = frames * 4) and set the chunk
   size to that. Do not use `data.chunk().size()`, which can be zero, and do
   not fill the whole mapped buffer: it can be 48 KiB even when the graph
   wants 1024 bytes, which adds hundreds of milliseconds of latency or
   silence. Set the PipeWire **client** properties via
   `connect_rc(Some(properties))` as well as the stream properties, because
   mixers label the application by the client.
10. `wl-copy` over SSH keeps stdout open in its forked daemon; redirect
   stdout/stderr to `/dev/null` in manual tests or the SSH command hangs.
11. The Vulkan swapchain needs `SDL_WINDOW_VULKAN` (`.vulkan()`) and
   `SDL_Vulkan_LoadLibrary` (`vulkan_load_library_default`) before window
   creation. Vulkan NDC y=-1 is the top of the viewport; the fullscreen
   triangle maps UV (0,0) there, so no vertical flip is needed.
12. `VkSurfaceKHR` comes from SDL; the sdl3 crate must be built with its `ash`
    feature so the surface types match the `ash` crate. Do not mix a raw
    `u64` surface with `ash` handles.
13. `vkAcquireNextImageKHR` with `UINT64_MAX` freezes the whole client during
    a Wayland fullscreen/resize transition. Never wait forever there. Reset
    the in-flight fence only after a successful acquire, and recreate the
    frame semaphores when the swapchain is recreated.
14. The secure layer is mandatory. An old client without the client-ID
    preface fails the handshake; update client and server together. Each
    client's registry token is its PSK; the token itself is never transmitted.
15. Revocation only works if `clients` stays the source of truth and the
    running server keeps watching it. Do not cache a token list at startup.
16. Every screencopy frame object must be destroyed after `ready` (see
    `release_frame`). wlroots and Hyprland keep the resource alive until the
    client destroys it; dropping the Rust proxy leaks one compositor object
    per captured frame.
17. Continuous capture at the panel rate is intentional. A damage-driven
    `copy_with_damage` experiment reduced idle CPU/GPU but made mouse motion
    lag badly: a hardware cursor can move without committing a new output
    frame, so the pending capture has no damage to complete on. If this is
    revisited, keep a continuous plain-`copy` window while the pointer is
    moving; do not make the whole stream damage-driven.
18. HEVC 4:2:0 cannot carry RGB subpixel antialiasing. Guest text rendered
    with `rgba=rgb` (GTK3 `gtk-xft-rgba=rgb`, KDE sub-pixel rendering) shows
    coloured fringes after encoding, especially in screenshots. The fix is
    grayscale antialiasing in the guest, not a colour-matrix change in the
    Vulkan shader. HDR is unsupported end to end.
