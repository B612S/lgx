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

## File clipboard

`FILE_BEGIN` carries a compact offer:

```
u32 count
per entry: u8 kind (0 = file, 1 = directory)
           u32 path_len, path bytes (relative, '/'-separated)
           u64 size (0 for directories)
```

`FILE_DATA` messages follow in entry order; each carries up to 64 KiB of the
current file. Directories have no data. `FILE_END` closes the transfer. The
receiver writes into `~/.cache/lgx/clipboard/<pid>-<stamp>/`, sanitises paths,
and sets `text/uri-list` to the top-level received paths.

## Gamepad

`GAMEPAD` is a 24-byte snapshot:

```
u8  slot         0..3
u8  connected
u16 reserved
u32 buttons      bit order: South, East, North, West, Back, Guide, Start,
                 LeftStick, RightStick, LeftShoulder, RightShoulder,
                 DPadUp, DPadDown, DPadLeft, DPadRight, Misc1..Misc5,
                 RightPaddle1, LeftPaddle1, RightPaddle2, LeftPaddle2,
                 Touchpad
i16 lx, ly, rx, ry, lt, rt
i8  hat_x, hat_y
u8  reserved[2]
```

Sticks are -32768..32767; triggers 0..32767. The server forwards the snapshot
to the root uinput helper, which applies only changed buttons/axes to a
`lgx virtual gamepad`.

## Encrypted transport

Every TCP connection (video/control and audio) starts with `LGX1`:

```
client -> server: u32 len=68, "LGX1", X25519 public key (32), random (32)
server -> client: u32 len=68, "LGX1", X25519 public key (32), random (32)
```

Both sides run X25519, then HKDF-SHA256 with the shared auth token as IKM and
the two random values as salt. Directional 32-byte AES-256-GCM keys are
derived. The token never appears on the wire.

A record is:

```
u32 total_plaintext_len
repeated:
  u32 ciphertext_len
  ciphertext (plaintext chunk <= 32 KiB + 16-byte tag)
```

The total length, chunk index and chunk length are authenticated as AEAD
associated data. The 12-byte lgx message header and its payload are both
inside the plaintext, so message types and HEVC frames are not visible.

## Audio

A separate TCP connection on the port following the control port. The first
message is `AUTH` with the same token, then raw s16le 48 kHz stereo frames.
