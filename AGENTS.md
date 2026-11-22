# AGENTS.md

Rules for agents working on this repository.

## Project

`lgx` is a low-latency HEVC remote desktop. The server runs inside a wlroots
compositor on Linux (VAAPI) or as a macOS app (ScreenCaptureKit/VideoToolbox);
the client runs on Linux/Wayland and decodes with NVDEC/CUDA or VAAPI into a
Vulkan swapchain.

Server and client may run on the same machine (VM + host) or on different
machines on a LAN. The transport is TCP; there is no discovery, so the client
addresses the server's LAN IP and explicit ports.

## Layout

```
crates/lgx-caps/     runtime DRM/VAAPI/Vulkan/NVENC capability probes + policy
crates/lgx-proto/    wire protocol (messages, auth)
crates/lgx-server/   capture, VPP/GL NV12 conversion, VAAPI helper client, input
crates/lgx-client/   in-process HEVC decoder, Vulkan renderer, input
crates/lgx-client/src/dec_shim.c   libavcodec C shim (built by build.rs)
crates/lgx-server-macos/  ScreenCaptureKit/VideoToolbox server for macOS
helper/lgx-vaenc.c   VAAPI HEVC encoder that exports dma-buf surfaces
helper/lgx-nvenc.c   NVENC HEVC encoder that takes CPU NV12
helper/lgx-uinput.c  root helper that creates the uinput devices
helper/lgx-cginput.c macOS CGEvent input injection
helper/lgx-frontmost.m frontmost-application PID/bundle ID for the macOS helper
helper/lgx-media.m   NX_SYSDEFINED media keys for the macOS helper
helper/lgx-macos-cursor.m  NSCursor shape helper for the local pointer
scripts/             fake-server.py
tests/               integration tests (wire protocol, secure transport,
                     token registry, client config, end-to-end sessions)
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
- **Capture is capability-selected.** `zwlr_screencopy_manager_v1` present
  (wlroots) -> `capture.rs` with GBM dma-buf; otherwise
  `org.freedesktop.portal.ScreenCast` -> `portal.rs` with a PipeWire stream.
  `--capture auto|wlr|portal` (config `video.capture`) overrides the choice.
  KDE/GNOME never expose wlr-screencopy, so they always take the portal.
- The portal requests shared-memory (MemFd) BGRx/BGRA buffers and converts
  them to NV12 on the CPU (parallel over rows). `LGX_PORTAL_DMABUF=1` is an
  experimental Intel VPP path that advertises an I915 Y-tiled modifier and
  imports the compositor buffer into the VAAPI helper (plane offset is carried
  in the `'R'` meta word 7). It is off by default: the idle-keyframe replay
  has no NV12 cache in that mode and the nested-KWin test could not produce
  enough damage to confirm the import. Do not enable it by default until it
  is verified on a real Intel KDE session. The portal is damage-driven: it produces no frames while the
  desktop is idle, so the wlr 5 s no-packet watchdog must not apply to it.
  A 100 ms PipeWire timer re-encodes the last NV12 frame as an IDR when a
  client needs a keyframe on an idle desktop; without it a newly connected
  client stays black until the next damage event.
- Ask the portal for the cursor mode it advertises; do not hardcode
  `CursorMode::EMBEDDED`. KDE supports it, GNOME does not (selecting it makes
  `SelectSources` fail with "Unavailable cursor mode 2"). Prefer EMBEDDED, then
  HIDDEN. When the pointer is not embedded, call `Hub::set_cursor_local(true)`
  so clients draw their own pointer (`CURSOR_LOCAL`); otherwise GNOME sessions
  would have no visible cursor at all.
- **Encoder is capability-selected.** `lgx-caps::encoder_for_node` picks
  NVENC on an NVIDIA capture node (and `helper/lgx-nvenc.c` is spawned),
  VAAPI elsewhere. NVENC takes system-memory NV12; VAAPI owns its surfaces.
  `low_power` is the VAAPI VDENC entry point and must be gated on
  `VaCaps::hevc_encode_low_power` (Mesa/AMD has no `EncSliceLP`; `lgx-vaenc`
  also retries once with `low_power=0`).
- `wlr-screencopy` -> GBM dma-buf -> VPP RGB->NV12 -> `lgx-vaenc` (Intel).
  `LGX_NO_VPP=1` falls back to the `glconv` RGB->NV12 path. AMD uses the GL
  path by default because its VPP RGB import is unverified; linear capture
  buffers are requested there, never the Intel Y-tiled modifier.
- **Idle frames are not re-encoded.** A wlr session is captured continuously,
  so a static desktop used to be encoded and resent forever. `capture.rs` asks
  screencopy v2+ for the damage report and skips the encode when the
  compositor saw no damage. Hyprland reports the whole frame as damaged on
  every copy (its screencopy has a TODO for a real damage ring), so the
  capture buffer is additionally hashed (xxh3 over the CPU mapping of the
  dma-buf, wrapped in `DMA_BUF_IOCTL_SYNC`) and compared with the digest of
  the frame that was actually sent. A pending keyframe always encodes, or a
  client that connects while the desktop is idle would never get its IDR.
  Skipped frames do not trip the 5 s no-packet watchdog.
  `LGX_NO_DAMAGE_SKIP=1` disables the skip. The CPU mapping costs one 8 KiB
  memcpy worth of bandwidth per 1080p frame; if a compositor cannot map the
  buffer the server simply encodes every frame.
- **VAAPI rate control is VBR by default** (`rc_mode=3`, `rc_max_rate` and
  `rc_buffer_size` at the configured bitrate). CBR pads a static desktop up to
  the target, so an idle 1080p160 session cost 4.7 MiB/s; VBR costs 1.2 MiB/s
  with the same encode time and no fps loss. `LGX_RC_MODE` overrides the mode
  (0 auto, 1 CQP, 2 CBR, 3 VBR, 4 ICQ). NVENC still requests CBR until its
  VBR path is verified on NVIDIA hardware.
- The helper is a separate process because it owns the VAAPI/VPP surfaces.
  With VPP the server only sends the captured frame fd and metadata; the GL
  fallback renders into the helper's dma-bufs.
- Encoder: `hevc_vaapi`, profile Main, `low_power=1` (VDENC, Intel only),
  `async_depth` default 2, CBR, `max_b_frames = 0` requested,
  `AV_CODEC_FLAG_LOW_DELAY`. VDENC ignores the B-frame request (see below).
  NVENC uses `hevc_nvenc` preset p1, tune `ull`, CBR, `zerolatency=1`,
  `delay=0`, `surfaces=4`, `forced-idr=1`. `pict_type=I` alone is NOT an IDR
  on NVENC (it becomes `FORCEINTRA`/TRAIL_R and the client's NAL 19/20 gate
  never opens).
- VPP imports the captured tiled ARGB dma-buf directly (DRM_PRIME_2 + a
  `VASurfaceAttribPixelFormat` attribute is mandatory), converts full-range
  BT.709 RGB->NV12 on the media engine, and the helper encodes that surface.
  The server sends only the frame metadata and fd; no RGB->NV12 GL work.
  Capture buffers request an explicit `I915_FORMAT_MOD_Y_TILED` modifier
  (falling back to linear): never trust the implicit modifier from
  `gbm_bo_create`, because Mesa's GBM can return one that neither iHD nor EGL
  accepts (mesa 26.3 i915 returns 0xb, which made every VPP import fail with
  ALLOCATION_FAILED). Linear import works but costs the compositor its tiled
  blit: capture dropped from 158 to ~140 fps at 1080p160.
- `LGX_CPU_CONVERT=1` is an experimental linear-capture CPU conversion path.
  It is slower than VPP and is not the default. `LGX_TEST_NO_PACKETS=1` is a
  test hook: it drops the VPP stream's packets so the 5 s no-packet watchdog
  and the GL fallback can be exercised; the fallback stream is unaffected.
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
  pointer feel. `Ctrl+Alt+Shift+S` shows a live statistics card (fps, draw
  p50/p99, decoded, overwritten) for five seconds; `LGX_STATS_OSD=1` keeps it
  visible. Its font is resolved once and cached: calling `fc-match`/`xrdb`
  per update stalled the presenter and dropped frames. When the server's
  capture mode changes, `lgx-resize-qt` (a child process; the client never
  links Qt) asks `Change` / `Change and save` / `Cancel`; "save" writes
  width/height to the profile, "cancel" keeps the window and scales the new
  mode, and the same mode is not asked about twice. `LGX_FAKE_RESIZE=1280x800@60`
  injects one display change for testing. The
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
  `video_port`, `audio_port`, `pair_port` and `token`. A profile may override
  any shared setting (bare key such as `vsync`, qualified `video.vsync`, or
  the global field name such as `enabled`); CLI flags still win over the
  profile and the profile wins over the global section. There is no discovery;
  the client connects exactly to those ports. `[input] gamepads = false` (or
  `--no-gamepads`) skips the SDL gamepad subsystem and sends no `GAMEPAD`
  packets; keyboard/mouse are unaffected. `clipboard = false` disables sync,
  `clipboard_rich = false` forces plain text, and `clipboard_max_bytes` limits
  text payloads in both directions (0 = unlimited). The file clipboard has a
  fixed 16 MiB limit. `[input] cursor_grab = true` confines the
  pointer to the window; `Ctrl+Alt+Shift+G` toggles the confinement; cursor
  visibility stays remote-style (guest cursor while focused, host cursor when
  unfocused). Launch with
  `lgx-client archlinux`; the section name is the public client ID. There is no
  `[network]`, no `[default]`, no `client_id` field and no config migration.
  The file is mode 0600. `[audio] connect` is an optional audio-address
  override. Server settings are `~/.config/lgx/server.conf` (`network`,
  `video`, `audio`, `input`); `network.interface`, `video_port`, `audio_port`
  and `pair_port` determine the bound interface and ports. CLI flags override.
  `video.drm_node` (or `--drm-node`) pins the DRM render node for GBM, VAAPI
  and the encoder helper; otherwise the server prefers the Intel render node.
  The choice is logged at startup, and the helper inherits `LGX_DRM_NODE`.
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
- Every SDL mouse motion event is forwarded immediately; do not coalesce
  motion per frame or per event batch. Coalescing cut the record count but
  measurably changed the pointer feel, which matters more than the syscalls.

## Client decoding

- `src/dec_shim.c` is compiled by `build.rs` against the installed FFmpeg
  headers. Do not mirror libavcodec structs in Rust.
- Backends: software (0), VAAPI (1), CUDA/NVDEC (2), Vulkan Video (3).
  `pick_format` uses `ctx->opaque` to request the matching hardware format.
- Vulkan Video uses FFmpeg's Vulkan hwaccel (`AV_HWDEVICE_TYPE_VULKAN`,
  `AV_PIX_FMT_VULKAN`) and is tried first when the renderer's physical device
  advertises `VK_KHR_video_queue` + `VK_KHR_video_decode_queue` +
  `VK_KHR_video_decode_h265`. Its frames are downloaded to CPU NV12: FFmpeg's
  Vulkan -> DRM_PRIME map exports the memory object but no layer planes
  (`nb_planes = 0`, even with linear tiling or `DISABLE_MULTIPLANE`), so the
  dma-buf import path cannot consume them. It is not an fps cap: the backend
  sustains the full 158-160 fps stream. VAAPI/CUDA/software remain fallbacks.
- The client asks the decoder for dma-buf frames when the Vulkan device
  advertises `VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf` and
  `VK_EXT_image_drm_format_modifier`; `LGX_NO_DMABUF=1` forces the CPU path.
  The shim reports the DRM fourcc, modifier, per-plane fd/offset/pitch and
  whether the frame is one NV12 layer or two (R8 luma + R8G8 chroma) layers.
  `av_hwframe_map` only produces a DRM descriptor when the destination frame's
  format is `AV_PIX_FMT_DRM_PRIME`; without that it quietly maps to CPU NV12.
  FFmpeg's CUDA hwcontext has no DRM_PRIME export, so NVDEC frames always take
  the CPU path.
- `src/vk.rs` imports those planes with
  `VkImageDrmFormatModifierExplicitCreateInfoEXT` + `VkExternalMemoryImageCreateInfo`
  (DMA_BUF), imports each fd with `VkImportMemoryFdInfoKHR`, and samples R8 /
  R8G8 views of the result. A single NV12 layer becomes one two-plane image
  (views use `VK_IMAGE_ASPECT_PLANE_0/1_BIT`, which requires `MUTABLE_FORMAT`
  and a format list); two decoder layers become two single-plane images. Any
  failure is logged once and that frame — and every later dma-buf frame — is
  downloaded to CPU NV12 instead, so an unsupported modifier cannot black out
  the window.
- Import requires the decoder and the renderer to use the same GPU driver
  (Intel VAAPI + Intel Vulkan, for example). `LGX_VK_DEVICE=<substring>` pins
  the Vulkan physical device by name; without it a discrete GPU is preferred
  and cross-driver modifiers usually fail the import (with a logged reason).
- `src/vk.rs` also owns the Vulkan instance, surface (from SDL), swapchain,
  render pass and pipeline. On the CPU path two textures (R8 for Y, R8G8 for
  UV) are uploaded from staging buffers each frame; a full-range BT.709 shader
  converts to RGB either way.
- Shaders are SPIR-V files committed under `src/shaders/`. Regenerate after
  editing GLSL:
  `glslangValidator -V shaders/video.vert -o shaders/video.vert.spv`.
- Decoder selection happens **after** Vulkan init and uses `lgx-caps`: Vulkan
  Video first when the driver advertises the H.265 decode extensions, then the
  backend that matches the renderer's DRM node (CUDA on nvidia-drm, VAAPI on
  the same node otherwise), then any VAAPI device, software last. `--decoder`
  forces one backend. Do not go back to matching the GPU marketing name: a
  hybrid system needs the node match to know whether dma-buf import can work.
- The native Vulkan Video decoder does not implement the SPS conformance
  window. NVENC codes 720-line video as 1280x736 plus a 16-line crop, so the
  client detects `SpsInfo::display_*() != coded size` and falls back to
  FFmpeg (which crops correctly) instead of presenting 736 lines. Do not
  remove that check without cropping the DPB views.
- The VDENC low-power encoder ignores `max_b_frames = 0` and emits
  hierarchical B-slices: the live 1920x1080@160 stream has `slice_type = 0`
  (PPS `num_extra_slice_header_bits = 0`). FFmpeg decodes it; a native Vulkan
  decoder must do POC/DPB reordering, never assume IPPP. `LGX_DUMP_BITSTREAM=<path>`
  writes the received Annex-B access units for offline decoder tests.
- `LGX_VULKAN_VIDEO=native` selects the in-tree Vulkan Video H.265 decoder
  (`lgx-vkvideo::decoder::NativeDecoder`) instead of FFmpeg: it owns its
  instance/device/video decode queue, builds the session from the in-band
  VPS/SPS/PPS, keeps a two-slot DPB and returns CPU NV12 frames. It sustains
  157-159 fps. The DPB images are created with a DRM modifier and exported as
  dma-buf; the client wraps them in `Pixels::Dmabuf` and the renderer samples
  them directly, so the native path is zero-copy (no NV12 readback). It is
  the default when the device supports Vulkan Video and dma-buf import;
  `LGX_VULKAN_VIDEO=ffmpeg` (or `0`), `--decoder` other than Vulkan, and any
  native setup/decode error fall back to FFmpeg (Vulkan/VAAPI/software).
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
- macOS function keys must be posted with `CGEventPostToPid(frontmost)`
  (`helper/lgx-cginput.c`), not at `kCGHIDEventTap`: macOS consumes F11 there
  as Show Desktop, so the focused app never sees it. This applies only to
  F1-F12; every other key stays on the HID tap so Cmd+Tab and Mission
  Control's Ctrl+Left/Right keep working.
- macOS media keys (mute, volume, play/pause, next/previous, brightness) are
  `NX_SYSDEFINED` events, not keyboard keycodes; `helper/lgx-media.m` builds
  them and `lgx-cginput` maps the Linux evdev consumer codes to
  `NX_KEYTYPE_*`. The client maps SDL media scancodes 262..=272 to those evdev
  codes; mute/volume already come from the HID table. SDL3 exposes no
  brightness scancode, so the helper's brightness mapping waits for a client
  that can produce evdev 224/225.
- macOS synthetic clicks use the last motion point, not the current system
  cursor (a cursor warp can lag), carry a computed
  `kCGMouseEventClickState` for double/triple clicks, and keep at least 30 ms
  between down and up. The client resends its pointer position before a press
  but does not set `SDL_MOUSE_FOCUS_CLICKTHROUGH`: the click that focuses the
  window must only focus it, never activate a guest control under the pointer.
- macOS has no system-wide back/forward for mouse buttons 3/4: the
  WindowServer gives them to the app under the pointer, and Safari/Finder
  ignore them. `lgx-cginput` mirrors Mac Mouse Fix's Universal Back and
  Forward: the app under the pointer picks the method. A synthetic trackpad
  navigation swipe (the undocumented CGEvent fields Mac Mouse Fix and
  SensibleSideButtons use, posted at `kCGSessionEventTap`) is the default for
  Safari, Finder and the other `com.apple.*` apps; Preview, Music, TV, Books,
  App Store, System Settings, Zotero and Warp get Cmd+[ / Cmd+], Notes and
  Freeform get Opt+Cmd+[, Calendar and Acrobat get Cmd+Left/Right; everything
  else gets the raw button 3/4 so Chrome, Firefox and editors keep their own
  handling. The menu-shortcut fallback uses ANSI keycodes; Mac Mouse Fix also
  emulates Apple's key-equivalent localization for non-ABC layouts, which this
  helper does not.
- macOS cursor shape: `helper/lgx-macos-cursor.m` polls
  `NSCursor.currentSystemCursor` (Objective-C; the Swift binding is obsolete),
  crops the alpha bounds and the server forwards `CURSOR_IMAGE` (1x RGBA plus
  hotspot). It polls at 60 Hz; the client checks for a shape every iteration
  and its idle SDL wait is 10 ms, because a static macOS capture produces no
  frames to wake it. The client installs the shape as an SDL color cursor, so
  resize/move/wait shapes show while the pointer itself is still drawn
  locally. There is no idle-based hiding: the local pointer stays visible
  until the server sends CURSOR_HIDDEN. The protocol carries that message for
  a server that can detect a hidden guest cursor; the public macOS APIs no
  longer expose that state. Never composite the cursor into the video: it
  trails the pointer.
- `--display N` input: `lgx-vtenc` reports the ScreenCaptureKit display id,
  the server passes it as `LGX_DISPLAY_ID` to `lgx-cginput`, and motion/clicks
  are mapped against `CGDisplayBounds` of that display, not the main display.
- macOS smooth scrolling is driven by `CVDisplayLink`: one pixel scroll event
  per display frame with `kCGScrollWheelEventScrollPhase`
  began/changed/ended. A background `nanosleep` is coalesced by macOS to
  ~30 Hz and makes the animation stepped even on a 120 Hz panel.
  `take_slice(0)` must return 0, or every vertical event also jitters
  horizontally by a pixel. `LGX_SMOOTH_SCROLL=0` falls back to line events.
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
  text MIME fallbacks use it; never put HTML first.
- On connect the client's watcher sends its current host clipboard; the server
  ignores its watcher's first event, so the host clipboard wins.
- One **light regular file** can be synchronized in each direction (16 MiB
  limit, `lgx_proto::file::MAX_BYTES`). It uses FILE_BEGIN (u32 LE name length,
  UTF-8 name, u64 LE size), FILE_DATA and FILE_END. The receiver writes a temp
  file under `~/.cache/lgx/clipboard` and offers it as a file URL
  (`text/uri-list` + `x-special/gnome-copied-files` on Linux; an NSURL file
  pasteboard item on macOS). Multiple files, directories and oversized files
  are left untouched rather than turned into text; a file offer must never
  fall back to text. Receivers record the file signature to suppress the
  clipboard echo.
- The Linux server unit no longer uses `PrivateTmp=yes`: a private /tmp made
  files copied from /tmp unreadable by the server while still setting the
  clipboard URI. Keep /tmp visible to the service.
- Never add clipboard hotkeys, `CLIP_GET`/`CLIP_ACK` or a paste injection back.
  Normal Ctrl+C/Ctrl+V is the whole UX.
- Audio is on by default (`--no-audio` disables it) and uses the same secure
  handshake/record layer with the same token PSK. Both sides use native
  PipeWire streams (`pipewire-rs`); do not reintroduce `parec`/`pacat`,
  PulseAudio compatibility or helper processes. The server opens an
  `Audio/Capture` stream with `stream.capture.sink = true` and `AUTOCONNECT`
  so it captures the default sink monitor, not the default source
  (microphone); the client opens an `Audio/Playback` stream with
  `AUTOCONNECT`. The server captures **float32** (the graph's native format,
  so PipeWire converts nothing) and converts to s16le 48 kHz stereo itself
  with TPDF dither (`audio::f32_to_s16_dithered`, scale 2^15, then clamp) —
  that is the only quantization step, so do not hand the conversion back to
  PipeWire. The client requests s16le and counts playback underruns; both use
  an `AudioInfoRaw` format pod. The client's stream carries `application.name = lgx - <profile>`,
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
  Timestamped records use the `LGXA` framing described under A/V sync; the
  8 KiB cushion is unchanged by sync.
  The sink monitor carries the sink's **input**, so the guest's own volume
  (volume keys, `wpctl`, a mixer) is applied after the tap and the client would
  always hear 100%. The server therefore follows the default sink's
  `SPA_PARAM_Props` (`audio::watch_default_sink_volume`: the `default`
  metadata gives the sink name, the bound sink node reports `volume`, `mute`
  and `channelVolumes`) and scales the float samples by that gain before
  quantizing. Do not drop this: without it changing the guest volume has no
  effect on the stream. On macOS the same applies to the process tap, so
  `lgx-macos-audio` reads the default output device's volume/mute and scales
  the samples. When FineTune is running (it re-renders apps through its own
  tap), LGX would capture both the raw app stream and FineTune's copy, which
  is audible doubling: the helper then captures only the mixer's output plus
  the system sound daemon (`systemsoundserverd`, which FineTune filters out
  of its own taps), and a second mute-only tap silences everything else
  locally. Keep that mute tap: the capture tap alone leaves audio that is
  not in the capture audible on the Mac. The capture tap mutes the mixer and
  the sound daemon themselves. Both topologies are rebuilt when the mixer or
  the sound daemon appears or quits.
  `LGX_AUDIO_MIXERS` overrides the comma-separated bundle-ID list and
  `LGX_AUDIO_TAP=global` disables the mixer topology for debugging. FineTune
  also lists the audio helper itself as `server`: it is a Core Audio client
  with the app's bundle ID (`dev.b612s.lgx.server`) but no
  `NSRunningApplication`, so FineTune falls back to the last bundle-ID
  component. It produces no audible output; hide it with FineTune's ignore
  (eye) button instead of changing the helper's signing identifier, which
  TCC relies on.

## A/V sync

- Video access units carry a 48-bit capture timestamp in microseconds in
  `Msg.code` (high 16 bits) and `Msg.b` (low 32 bits); `a` stays the payload
  length. `lgx-vtenc` timestamps at the ScreenCaptureKit callback, before
  encoding, and frames it as `u32 BE length, u64 BE timestamp, AU`. Zero means
  unknown (older servers).
- The macOS audio helper frames each PCM write as `u64 LE timestamp, u32 LE
  byte count, PCM`. The server forwards `LGXA, u64 LE ts_us, u32 LE length,
  PCM`; the client also accepts raw PCM from an old server and plays it
  without sync. Update client and server together for the macOS path.
- The client tracks the server timestamp at the audio ring's head. The
  video hold is **opt-in** (`LGX_AV_SYNC=1`): holding the newest frame until
  the audio head reaches its capture timestamp made the presenter miss slots
  and drop frames at 60 Hz, while the natural offset is only a few tens of
  milliseconds. With the hold enabled `AV_MAX_HOLD_US` caps it at 30 ms; a
  head that has not advanced for 200 ms disables it. `LGX_DEBUG_AV=1` prints
  head/video/wait, and `LGX_AV_LEAD_US` tunes the target. Do not make the hold
  larger or enable it by default: a deep audio backlog would become video lag.
- Timestamps are written by the same host clock: `mach_absolute_time` on
  macOS for video and `DispatchTime.now()` for the audio helper. Do not mix a
  wall-clock epoch into one side only.
- The Linux server stamps video with `CLOCK_MONOTONIC` microseconds in the
  same `Msg.code`/`Msg.b` fields and frames audio as `LGXA` once the client
  opts in (one `AUDIO_SYNC` message after the token). Both sides use the same
  clock. At 1080p160 the audio head trails the video capture by ~50 ms (the
  8 KiB playout cushion), stable within a few ms; the video hold stays
  opt-in and capped at 30 ms, so the default presents immediately.
- `lgx-vtenc` logs `SCK x fps, encoded y fps, encode errors n` every 5 s and
  whether VideoToolbox chose the hardware encoder. A low HEVC fps with a much
  higher SCK rate means the encoder is dropping input; both equal means the
  display content itself changed at that rate. The first frame of every
  session is forced to an IDR.

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
- The macOS app keeps its token in `~/.config/lgx/macos.conf` (0600), never in
  the LaunchAgent: startup removes a legacy `--token` pair from
  `ProgramArguments` (launchd caches the plist, so restart the agent once).
  `--rotate` only rewrites the config; `--token` on the command line is
  deprecated because it is visible in `ps`.

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
cargo test --workspace   # includes the `tests/` integration suite
gcc -O2 -o lgx-vaenc helper/lgx-vaenc.c -lavcodec -lavutil -lva -lva-drm
gcc -O2 -o lgx-nvenc helper/lgx-nvenc.c -lavcodec -lavutil
scripts/check.sh          # build + tests + both C helpers, run before pushing
# CI runs the same script in an archlinux:latest container
./packaging/build.sh
```

Packaging layout: `packaging/build.sh` builds `lgx-client` and `lgx-server`
from the committed tree and puts the `*.pkg.tar.zst` files in the repository
root. `make-source.sh` writes the only source tarball,
`packaging/lgx-<version>.tar.gz`, and refuses a dirty tree. makepkg's work
trees belong in `target/pkgbuild` (git-ignored); never commit or keep
`packaging/*/src`, `packaging/*/pkg` or per-package copies of the tarball.
Bump `pkgrel` in a PKGBUILD for every rebuild; the build script fails when the
result would be older than the installed package. `build.sh` also writes a
temporary PKGBUILD copy with the real `sha256sums` (the tracked one keeps
`SKIP`), so makepkg verifies the source archive instead of trusting it.

Portal pipeline without a compositor dialog (test hook): the
`examples/pw_screencast.rs` producer exports a BGRx node and prints its id;
run the server with `LGX_TEST_PW_NODE=<id>` plus `--capture portal` to exercise
format negotiation, CPU NV12 conversion and the encoder end to end. The
producer must announce `SPA_PARAM_Buffers` (including `dataType`) or PipeWire
never calls its process callback; `LGX_TEST_PW_FREEZE_AFTER=N` stops it after
N frames so the idle-keyframe path can be tested. `LGX_TEST_PW_NODE` is
debug-only and is ignored unless set. `LGX_TEST_NO_ENCODER=1` makes the server
count and log delivered portal frames without creating an encoder, which is how
the GNOME capture path is checked in a VM without a hardware encoder.

`examples/kde_probe.rs` binds KWin's private `zkde_screencast_unstable_v1`,
requests the first output and prints the PipeWire node id; it is a diagnostic
for testing KDE capture without the portal consent dialog and must not become
a production capture path. Do not try to test the GNOME portal with
`gnome-shell --headless`: it does not export `org.gnome.Mutter.ScreenCast`, so
`xdg-desktop-portal-gnome` cannot provide ScreenCast there. Use a real GNOME
session.

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

## macOS app icon

- `assets/AppIcon.icon` is the Icon Composer source (PNG layers + `icon.json`);
  `assets/make-macos-icon.py` renders it, and
  `scripts/compile-macos-icon.sh` compiles it with `actool` into
  `assets/Assets.car` + `assets/lgx.icns`.
- The app must ship `Assets.car` and `CFBundleIconName=AppIcon`; the `.icns`
  is the pre-macOS 26 fallback. Keep the Liquid Glass groups and the
  light/dark/tinted specializations in the `.icon`; a flat `.icns` loses them.
- The top-level fill covers the full 1024 square; the system applies the
  squircle. Transparent margins make macOS wrap the icon in a white platter.

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
17. `av_hwframe_map` maps to CPU memory unless `dst->format` asks for
    `AV_PIX_FMT_DRM_PRIME`; the destination frame also must outlive the
    Vulkan import because it owns the descriptor and its fds. Importing an fd
    transfers it to Vulkan, so the client `dup()`s it first. A DRM-modifier
    image must be validated with `vkGetPhysicalDeviceImageFormatProperties2`
    (chained `VkPhysicalDeviceImageDrmFormatModifierInfoEXT`) and its plane
    count with `VkDrmFormatModifierPropertiesListEXT` before
    `vkCreateImage`, otherwise the failure only shows up as
    `VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`.
18. Continuous capture at the panel rate is intentional. A damage-driven
    `copy_with_damage` experiment reduced idle CPU/GPU but made mouse motion
    lag badly: a hardware cursor can move without committing a new output
    frame, so the pending capture has no damage to complete on. If this is
    revisited, keep a continuous plain-`copy` window while the pointer is
    moving; do not make the whole stream damage-driven.
19. NVENC: `pict_type = I` sets `NV_ENC_PIC_FLAG_FORCEINTRA` and yields a
    TRAIL_R slice, not an IDR. `forced-idr=1` is required for `K` to produce
    NAL 19/20. `bf` is not an AVOption on `hevc_nvenc`; use
    `enc->max_b_frames = 0`. System-memory NV12 input is a host memcpy into a
    locked NVENC buffer, not a CUDA/PCIe upload; measured 1080p `'F'`
    throughput is ~597 fps, 4K only ~138 fps, so 4K160 needs a CUDA/fd path.
20. VAAPI `low_power=1` must only be requested when the driver advertises
    `VAEntrypointEncSliceLP`; AMD/Mesa rejects it at `avcodec_open2`. The
    helper retries without VDENC, and the server passes the gated flag.
21. The portal PipeWire stream is damage-driven and its MemFd buffers are
    larger than one frame: use `chunk().stride()`/`chunk().offset()`, do not
    derive the stride from the buffer length, and do not apply the wlr
    no-packet watchdog to it.
22. HEVC 4:2:0 cannot carry RGB subpixel antialiasing. Guest text rendered
    with `rgba=rgb` (GTK3 `gtk-xft-rgba=rgb`, KDE sub-pixel rendering) shows
    coloured fringes after encoding, especially in screenshots. The fix is
    grayscale antialiasing in the guest, not a colour-matrix change in the
    Vulkan shader. HDR is unsupported end to end.
