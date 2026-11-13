# lgx protocol

One TCP connection per client carries authentication, the HEVC stream, input
and clipboard. Audio uses a second connection on the next port.

## Message (12 bytes)

| offset | size | field | meaning |
|---|---|---|---|
| 0 | 1 | `type` | message type |
| 1 | 1 | `flags` | message-specific |
| 2 | 2 | `code` | evdev code / codec / modifier bits |
| 4 | 4 | `a` | first argument or payload length |
| 8 | 4 | `b` | second argument |

For `AUTH`, `VIDEO`, `KEYMAP`, `CLIP_SET` and `CLIP_DATA` the payload follows
the header and its length is in `a`.

## Types

| type | name | fields | payload |
|---|---|---|---|
| 1 | AUTH | `a` = token length | token |
| 2 | HELLO | `code` = codec (1 = HEVC), `a` = width, `b` = height | — |
| 3 | VIDEO | `a` = length | one Annex-B HEVC access unit |
| 4 | KEY | `code` = evdev, `a` = 0/1 | — |
| 5 | BTN | `code` = evdev, `a` = 0/1 | — |
| 6 | MOTION | `a` = x, `b` = y (0..65535) | — |
| 7 | WHEEL | `code` = axis, `a` = steps | — |
| 8 | MODS | `a` = depressed, `b` = latched, `code` = locked, `flags` = group | — |
| 9 | KEYMAP | `a` = length | xkb keymap |
| 10 | CLIP_SET | `a` = length | text |
| 12 | CLIP_DATA | `a` = length | text |
| 13 | CURSOR | `code` = 1 overlay the guest pointer, 0 draw locally | — |
| 14 | PING | — | — |
| 15 | FILE_BEGIN | `a` = length | file offer |
| 16 | FILE_DATA | `a` = length | sequential file bytes |
| 17 | FILE_END | — | — |
| 18 | GAMEPAD | `a` = 24 | one gamepad snapshot |

## Handshake

1. Client sends `AUTH` with the shared token.
2. Server verifies it and sends `HELLO` (codec, width, height) before
   registering the client for video.
3. Client may send `KEYMAP` and `CURSOR`.
4. Server sends `VIDEO` messages. A newly registered client receives only
   packets containing an HEVC IDR (NAL 19/20) until the first one is sent.

## Video

Each `VIDEO` message is one complete access unit in Annex-B form. The server
uses VAAPI HEVC Main, low-power VDENC, CBR and no B-frames. A keyframe request
produces VPS/SPS/PPS/IDR; the encoder may return several access units for one
frame, and each is sent as its own `VIDEO` message.

## Clipboard

Only text is synchronized. Each side polls its local clipboard every 300 ms
and sends only text: the client uses `CLIP_SET`, the server broadcasts
`CLIP_DATA`. `last_remote`/`last_sent` signatures prevent echo loops. File
clipboard contents are not synchronized, and there are no clipboard hotkeys
or automatic pastes.

## Pairing and revocation

`lgx-server --pair` opens a temporary listener (default `0.0.0.0:5904`) and
prints a 32-hex one-time code. The client connects with the same `LGX1`
handshake, using the code instead of the long-term token as the PSK. After the
handshake the server sends `id\ntoken` as one secure record; the client stores
it mode 0600 and answers `ok`. The server then adds the entry to its registry.
The pairing listener accepts one client, expires after three minutes and stops
after five failed attempts.

Normal connections start with a public client-ID frame:

```
client -> server: u32 len, id bytes (ASCII, 1..128)
client -> server: the usual LGX1 handshake, PSK = that client's token
```

The server looks the ID up in `~/.config/lgx/clients`; an unknown or revoked ID
is rejected. The running server watches the registry every second and closes
active sessions whose ID disappeared. `lgx-server --list` and
`lgx-server --revoke ID|NAME` manage the registry.

## Audio

A separate TCP connection on the port following the control port. The first
message is `AUTH` with the same token, then raw s16le 48 kHz stereo frames.
