# TODO

Ideas that fit the current architecture (HEVC only — no H.264, no tile
streaming, no shared-memory transport). Ordered by value for the effort; none
of this is a promise.

## Next

- [ ] **Menu bar item in the macOS app.** The launcher is already a background
      `NSApplication`; add an `NSStatusItem` that shows `idle`/`streaming`, the
      connected client, fps, bitrate and dropped frames, with actions:
      rotate the token, open `/tmp/lgx-server.log`, stop streaming, quit.
      Today the app has no UI at all.
- [ ] **Status file.** Write `~/.cache/lgx/status.json` once per stats tick
      (fps, send ms, dropped, client id, bitrate, adaptive state) so the menu
      bar and scripts never have to parse the launchd log.
- [ ] **End-to-end latency in the OSD.** `lgx-vtenc` stamps each access unit
      with its capture time, the client computes capture → present and shows it
      in the OSD on a hotkey. Right now only the socket write time is measured,
      which cannot separate capture, network and decode.

## Later

- [ ] **Separate secure connection for input and clipboard.** Today a key press
      queues behind video on the same TCP stream; a ~100 KiB frame at 120 fps
      is visible in a burst. A third connection (like audio already has) keeps
      input latency flat regardless of the video rate.
- [ ] **Runtime display switching.** `--display N` only works at spawn; a
      `display N` command to `lgx-vtenc` that recreates the `SCStream`, plus a
      fresh `HELLO` and a client swapchain rebuild, would allow switching
      monitors mid-session.
- [ ] **Saturate the wired link.** The bitrate ceiling is now 200 Mbit/s at
      1200p120 (about 0.7 bit/pixel). Verify what VideoToolbox actually emits
      under motion, whether its auto level clamps the rate, and benchmark text
      sharpness against 80 Mbit/s. Add a `--quality`/CQ mode only if VT exposes
      one that beats CBR.
- [ ] **Power policy.** On battery: 60 fps and a lower bitrate ceiling; on AC:
      full rate. Pause capture when the lid closes. `lgx-vtenc` already accepts
      `bitrate N`; a matching `fps N` command is the missing piece.
- [ ] **Image clipboard.** PNG in both directions for screenshots and diagrams
      (text and HTML already work; files stay out of scope).
- [ ] **IME / non-ASCII input.** Only evdev keycodes are forwarded, so dead
      keys and CJK input do not work. A path through
      `CGEventKeyboardSetUnicodeString` would cover both.
- [ ] **HEVC Main10 / HDR.** ScreenCaptureKit can deliver EDR, VideoToolbox
      encodes Main10; the client would need R16/R10G10B10 views, a PQ/HLG
      shader and colour management. HDR is currently unsupported end to end.

## Not planned

- Opus audio: the transport is a wired 1 GbE link, so 187 KiB/s of raw PCM is
  noise next to the video. Raw PCM stays the deliberate low-latency choice;
  revisit only if a Wi-Fi client ever shows up.

- H.264 output, tile streaming and shared-memory transport (removed on
  purpose).
- File clipboard synchronisation; the clipboard is text/HTML only.
- Gamepad injection on macOS: `IOHIDUserDeviceCreateWithProperties` requires
  `com.apple.developer.hid.virtual.device`, which needs an Apple Developer
  provisioning profile. A self-signed certificate cannot create the device
  (tested: it returns nil), so `GAMEPAD` packets are ignored on macOS.

## Done recently (for context)

- macOS server: config file `~/.config/lgx/macos.conf`, token rotation,
  `--pair`, adaptive bitrate, `--display`, smooth wheel scrolling with an
  inverted direction option, timestamps and a send-latency metric in the log.
- macOS app: stable signing, drag-to-Applications DMG, frost icon, permission
  window that checks Screen Recording and Accessibility once and only starts
  the server when they are granted, display sleep blocked during a session.
- Client: bounded decode queue with an IDR request when frames are dropped,
  fractional and horizontal wheel deltas, dma-buf import path.
- macOS clipboard: fixed the rich-text byte order and stopped clearing the
  pasteboard on a malformed record.
