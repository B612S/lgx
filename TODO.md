# TODO

Open issues from the 2026-11-22 review. The goal is to land all of them
without regressing the existing HEVC path, packaging or tests.

**Item 8 is a hard requirement:** Vulkan Video H.265 decode at the full stream
rate (160 fps on the reference host). Do not cap the client at 120/144 fps and
do not add an fps-negotiation compromise.

Legend: `[ ]` open, `[~]` in progress, `[x]` done. A `Done when` line is the
acceptance check for that item.

## P0 — correctness and robustness

- [x] **1. File clipboard must not stall video.** `Hub::broadcast_file`
      holds the `clients` mutex for the whole 16 MiB transfer while the capture
      loop blocks in `broadcast_video`. Snapshot the secure handles under the
      lock, drop it, then stream 32 KiB chunks; keep per-message sealing. The
      clipboard channel must not silently drop updates when the write queue is
      full (`broadcast_clip`/`broadcast_rich` currently treat `Full` as
      success).
      *Done:* the lock is released before writing and clipboard updates retry
      for 250 ms outside the lock. End-to-end: three 16 MiB files copied from
      the guest arrived on the host with matching sha256 while the server kept
      158.3-158.4 fps, 4.7 MiB/s, 0 dropped.
      *Done when:* a 16 MiB file copy leaves the video frame rate and pacing at
      the current level (drops follow the normal policy, not a freeze), and the
      lock is not held across socket writes.

- [x] **2. Stream health watchdog.** A connected client with `0.0 MiB/s` is
      invisible today (2026-11-21: 35 minutes of a silent dead stream). In
      `Stream::run`, if `frames > 0` and `bytes == 0` for 5 s, log an error and
      fall back (rebuild the stream with `vpp=false`) or exit cleanly for
      systemd. Also bound consecutive capture errors (Wayland gone): after N
      failures exit instead of retrying every 50 ms forever.
      *Done:* a 5 s no-packet watchdog rebuilds the stream with `vpp=false`
      and 20 consecutive capture errors exit for systemd. Verified end to end
      with the `LGX_TEST_NO_PACKETS=1` hook (0.4.0-31): the VPP stream logged
      `0.0 MiB/s`, the watchdog fired once, the GL stream came up at
      158 fps / 4.7 MiB/s (render 2.2 ms) and the client kept decoding
      without a reconnect.
      *Done when:* a forced VPP import failure produces one clear log line and
      either a working GL fallback or a clean restart, with no 160 Hz log
      flood.

- [~] **3. VPP surface cache in `lgx-vaenc`.** The table holds 8 entries and
      has no eviction: once full, a new surface is created, never cached and
      never destroyed, leaking one VA surface per frame. It is also keyed by
      the server's fd number, which the kernel reuses, so a recreated buffer
      can hit a stale surface. Give every `GbmBuffer` a unique id, send that to
      the helper, and evict the least-recently-used entry with
      `vaDestroySurfaces`.
      *Status:* `GbmBuffer` now carries a unique id (sent as the cache key),
      and the helper evicts the least-recently-used surface instead of leaking
      when the table is full; deployed in 0.4.0-23/24. A resize-churn test is
      still needed.
      *Done when:* repeated resize/fallback cycles do not grow the VA surface
      count and a recreated buffer can never return stale content.

- [~] **4. Rate-limit helper errors.** `vpp rgb surface failed` is printed for
      every frame (160 lines/s in the 2026-11-21 incident). Log the first
      failure in full and then at most once per second, with a suppressed
      counter.
      *Status:* implemented (one line per second, suppressed repeats) and the
      helper is deployed from 0.4.0-22 via `LGX_VAENC`; needs a failure run to
      observe.
      *Done when:* a reproduced import failure writes O(seconds) lines, not
      O(frames).

- [x] **5. Display changes with a user prompt.** A mode change rebuilds the
      encoder, sends `RESIZE` (width/height/refresh) and the client asks via
      the `lgx-resize-qt` child process: `Change` (resize the window),
      `Change and save` (also write width/height to the profile) or `Cancel`
      (keep the window, scale the new mode). No silent auto-resize.
      *Status:* live-verified on the VM. The output uses a custom modeline, so
      a plain `mode = "1280x800@60"` is ignored; `hl.monitor` with
      `mode = "modeline 71.00 1280 1328 1360 1440 800 803 809 823 +hsync -vsync"`
      switched to 1280x800, the server rebuilt the encoder
      (1280x800@60) and sent `RESIZE`; the client showed
      `lgx-resize-qt --old 1920x1080 --new 1280x800` with the text
      "1920x1080 → 1280x800" (no refresh) and video continued at 60 fps.
      Restored with the original
      `modeline 373.12 1920 1928 1960 2000 1080 1152 1160 1166 +hsync -vsync`.
      *Done:* all three actions verified with `LGX_TEST_RESIZE_ANSWER`:
      `Change` resized the window to 1280x800, `Change and save` also wrote
      `width`/`height = 1280`/`800` to the profile (restored afterwards), and
      `Cancel` left the window at 1920x1080.
      *Done when:* 1920x1080 → 1280x800 → back offers the dialog, all three
      buttons do the right thing, and video continues without a reconnect.

- [~] **6. Render node discovery.** `/dev/dri/renderD128` is hardcoded in
      `gbm.rs`, the Intel check in `main.rs` and `lgx-vaenc`; VAAPI is opened
      on the same fixed node. On a machine where the Intel GPU is not node 128
      (dGPU first) VPP silently turns off and GBM/VAAPI can end up on different
      GPUs. Enumerate render nodes, pick the VAAPI-capable one by vendor and
      pass the choice to the helper.
      *Status:* auto-detection prefers the Intel node; it can be overridden in
      `server.conf` (`[video] drm_node = ...`) or with `--drm-node PATH`
      (`LGX_DRM_NODE` is the env fallback). Startup logs the node and whether
      it came from config or auto-detection, and the helper inherits it.
      Verified on the VM: configured override wins, a bogus path fails with a
      clear "drm node: No such file" error, auto-detection restored. The
      two-GPU case has no local hardware to test.
      *Done when:* with `renderD128` = NVIDIA and `renderD129` = Intel the
      server still selects Intel for capture and encode (and vice versa).

## P1 — latency and quality

- [~] **7. Socket latency limits.** Only `TCP_NODELAY` is set. With a slow
      client the kernel can buffer megabytes of video, so the existing
      userspace drop/IDR logic engages too late. Set `TCP_NOTSENT_LOWAT`
      (~32 KiB) and a bounded `SO_SNDBUF` on the video socket.
      *Status:* `TCP_NOTSENT_LOWAT` = 32 KiB on every accepted server socket;
      deployed. A throttled-client measurement is still needed.
      *Done when:* with a throttled client the server keeps the drop policy and
      end-to-end queueing stays bounded instead of growing to seconds.

- [~] **8. Vulkan Video H.265 decode at 160 fps.** The reference client
      (RTX 4070, NVIDIA Vulkan) cannot import dma-buf from the FFmpeg decoders
      and falls back to CPU NV12, decoding ~145 fps against a 158-160 fps
      stream. Implement a native Vulkan Video backend
      (`VK_KHR_video_queue`, `VK_KHR_video_decode_queue`,
      `VK_KHR_video_decode_h265`):
      - pick a device and queue family with video decode support (respect
        `LGX_VK_DEVICE`), create the decode session and DPB;
      - parse Annex-B AUs (VPS/SPS/PPS/slices), manage reference slots and
        IDR/CRA;
      - decode into optimal-tiling images and sample them directly in the
        existing renderer (zero copy, no CPU NV12 download);
      - keep FFmpeg VAAPI/CUDA and software as fallbacks, selected when the
        extension or the coded profile is unsupported;
      - land behind `LGX_VULKAN_VIDEO=1` first, then make it the default when
        the device supports it.
      *Explicit non-goals:* no 120/144 fps cap, no "negotiate the client's
      ceiling".
      *Status:* backend `3` (FFmpeg Vulkan hwaccel) is implemented and chosen
      by default in windowed runs when the device advertises the video
      extensions. Measured on the reference host: `HEVC decoder ready
      (Vulkan Video)`, 158-159 fps, decoded 158-159, 0 overwritten, CPU 25% —
      the full stream rate, no cap. **Zero-copy is blocked by FFmpeg**: mapping its Vulkan frames to
      DRM_PRIME returns no layer planes (`nb_planes = 0`) even for linear
      tiling or `AV_VK_FRAME_FLAG_DISABLE_MULTIPLANE`, so the client downloads
      NV12. A true zero-copy path needs an in-client Vulkan Video decoder (own
      VkDevice + H.265 session/DPB/slice parsing), not FFmpeg's opaque
      `AVVkFrame`.
      *Native prototype:* `crates/lgx-vkvideo` (selected only with
      `LGX_VULKAN_VIDEO=native`) parses Annex-B VPS/SPS/PPS/slices
      (7 unit tests on real stream bytes), converts them to `StdVideoH265*`
      and on the RTX 4070 creates a `VkVideoSessionKHR` plus
      `VkVideoSessionParametersKHR` (`session parameters created`). The probe
      also allocates the NV12 DPB/output image, the aligned bitstream buffer
      and a command buffer, and submits the first IDR through
      `vkCmdBeginVideoCodingKHR` / `vkCmdDecodeVideoKHR` /
      `vkCmdEndVideoCodingKHR`; the fence signals (`IDR submitted and decoded`,
      137999-byte IDR). The NV12 IDR is copied back and its Y plane is
      **byte-identical to FFmpeg's decode** (MAD 0.000, PSNR infinite). The
      B-frame reference lists are parsed from the slice header (in-slice RPS:
      the live B slice at POC 1 has one used negative reference at POC 0,
      verified against FFmpeg's `trace_headers`). A two-frame decode decodes
      the IDR correctly; passing a reference slot initially segfaulted the
      NVIDIA driver because the slot lacked the
      `VkVideoDecodeH265DpbSlotInfoKHR`/`StdVideoDecodeH265ReferenceInfo`
      chain. With that chain, separate DPB images and the B's reference, both
      the IDR and the B (POC 1) reconstruct **bit-exactly** against FFmpeg
      (Y MAD 0.000). The reconstructed picture lives in the DPB image (the
      separate dst image is black), which is what the renderer should sample.
      A generalized loop decodes 10 consecutive pictures (IDR + 9 low-delay
      B-frames) with a two-slot DPB, POC mapping and slot reuse; every frame is
      **bit-exact** against FFmpeg (Y MAD 0.000 on all 10). The slot policy is
      `lgx-vkvideo::dpb` (unit-tested). `lgx-vkvideo::decoder::NativeDecoder`
      now owns its Vulkan instance/device/video queue, builds the session from
      VPS/SPS/PPS, manages the two-slot DPB and returns CPU NV12 per frame; the
      `native_decode` bin decodes 10/10 frames bit-exactly. DPB images are now
      created with a DRM modifier and exported as dma-buf (`export_probe`
      verified the layout on the RTX 4070); the client wraps them in
      `Pixels::Dmabuf` and the existing import path samples them directly.
      A 12 s windowed run holds 158-159 fps, CPU 35-47%, 0 dropped, server
      158 fps 0 drops, and the window shows the remote desktop correctly. The
      native path is now the default when the device supports it;
      `LGX_VULKAN_VIDEO=ffmpeg`/`0`, non-Vulkan `--decoder`, and any native
      error fall back to FFmpeg. A 10-minute stability run remains.
      *Done when:* `lgx-client --bench` on the reference host keeps up with the
      full 158-160 fps stream with `decoder frames: Vulkan Video (zero-copy)`,
      a 10-minute session shows no decode errors and no server-side drops, and
      all fallback backends still work.

- [x] **9. Metrics and OSD.** The log only has 5 s averages. Add p50/p99 for
      capture wait, encode and send on the server, decode and present on the
      client, plus the end-to-end capture→present figure once item 10 exists.
      Show it in the existing OSD on a hotkey and write
      `~/.cache/lgx/status.json` once per stats tick for scripts and the macOS
      menu bar.
      *Done:* the server logs wait/render/encode p50/p99 and writes
      `~/.cache/lgx/status.json` atomically; the client's `LGX_DEBUG_FPS`
      prints draw p50/p99, and `Ctrl+Alt+Shift+S` (or `LGX_STATS_OSD=1`) shows
      a live card ("158 fps 0.3/0.8 ms dec 158 drop 0") rendered at runtime
      into a reserved atlas slot. The font is cached so a one-second update
      does not stall the presenter (158-159 fps, 0 overwritten with the card
      visible).
      *Done when:* `LGX_DEBUG_FPS=1` prints p50/p99, the OSD shows latency and
      fps, and `status.json` matches the log.

- [x] **10. Linux timestamps and A/V sync.** The Linux server leaves the video
      timestamp zero and sends raw PCM, so `LGX_AV_SYNC` is a no-op and lip
      sync relies on the audio ring cushion. Timestamp video at capture
      (`CLOCK_MONOTONIC`, `Msg.code`/`Msg.b`) and audio with the `LGXA` framing
      already used on macOS; keep the opt-in hold.
      *Done:* video and audio now share `CLOCK_MONOTONIC`; the Linux server
      sends `LGXA` audio records and the client enables its timeline
      (`0.4.0-26`). A windowed run measured video capture ahead of the audio
      head by 46-54 ms (the 8 KiB playout cushion), stable within a few ms;
      the opt-in hold stays capped at 30 ms as documented.
      *Done when:* `LGX_AV_SYNC=1` + `LGX_DEBUG_AV=1` shows a stable A/V offset
      (target under ~20 ms of drift) and the default path is unchanged.

## P2 — packaging, tests, security

- [~] **11. Package dependencies.** `lgx-server` links `libgbm`, `libEGL`,
      `libGLESv2` and `libdrm`, but `depends` has no `mesa`/`libdrm`; a clean
      install fails to start. `libpulse` is unused by both packages (native
      PipeWire). Fix `depends`/`makedepends` and verify with a clean-chroot
      build.
      *Status:* server `depends` now includes `mesa` (libgbm/EGL/GLESv2), both
      packages dropped `libpulse`, `makedepends` gained `mesa`/`pipewire` (and
      `sdl3` for the client); both packages build (0.4.0-22 / 0.4.0-31) and
      every `ldd` library maps to a package in the depend closure. A clean
      chroot build is still pending.
      *Done when:* `ldd` needs are covered by `depends`, `makepkg` succeeds in
      a clean chroot, and `libpulse` is gone.

- [~] **12. macOS token must not be in argv.** The LaunchAgent passes
      `--token`; it is visible in `ps`/`pgrep`. Use the encrypted registry
      (`~/.config/lgx/clients` + `master.key`) like Linux, or a 0600 env file
      read by the launcher.
      *Status:* the macOS server now reads the token from `macos.conf` (0600)
      and removes a legacy `--token` pair from the LaunchAgent at startup;
      `--rotate` rewrites the config and scrubs the plist. Compiles on the Mac
      (`cargo check --release -p lgx-server-macos`, arm64) and the scrub was
      verified on a copy of the real plist (args 3/4 removed, rest intact;
      config and plist token matched before). The running app still carries
      the old plist until the next `scripts/sign-and-install.sh` run.
      *Done when:* no secret appears in the process list and pairing/rotation
      still work.

- [x] **13. Tests and small cleanups.** Add unit tests for
      `pick_capture_mods` (advertised Y_TILED, fallback to linear, empty list =
      implicit) and for the file-broadcast locking change. Fix the stale
      "H.264 decoder" comment in `crates/lgx-client/src/dec_shim.c`.
      *Done:* five unit tests cover the capture modifier choice and
      `broadcast_file_releases_the_clients_lock` proves the `clients` mutex is
      free while a 16 MiB transfer is blocked in `send_msg` (and that the
      transfer really is blocked). The stale "H.264 decoder" comment is fixed.
      `cargo test --workspace` / `scripts/check.sh` are green.
      *Done when:* `cargo test --workspace` covers the new paths and
      `scripts/check.sh` is green.

## Order of work

1. 1, 2, 4, 11 — small, high value.
2. 3, 6, 7 — small/medium, latency and leaks.
3. 9, 10 — measurement first, then sync.
4. 5 — display prompt (server side already re-added).
5. 8 — the long pole; prototype behind `LGX_VULKAN_VIDEO=1` early, flip the
   default only when the bench holds the full rate.

## Out of scope for this pass

- macOS menu bar item and status menu (the status file from item 9 unblocks it).
- Image clipboard, IME/non-ASCII input, HEVC Main10/HDR.
- H.264 output, tile streaming and shared-memory transport (removed on
  purpose).
- Gamepad injection on macOS (requires an Apple-issued entitlement).
