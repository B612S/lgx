//! lgx wire protocol.
//!
//! One TCP connection carries authentication, the HEVC video stream, input,
//! and clipboard. Audio uses a second connection on the next port. Every
//! message starts with a 12-byte little-endian header; only a few message
//! types carry a payload whose length is stored in `a`.

use std::io::{self, Read, Write};

pub const VERSION: u16 = 2;

pub mod msg {
    pub const AUTH: u8 = 1;
    /// Server greeting: `code` = codec, `a` = width, `b` = height.
    pub const HELLO: u8 = 2;
    /// One Annex-B HEVC access unit; length in `a`.
    pub const VIDEO: u8 = 3;
    pub const KEY: u8 = 4;
    pub const BTN: u8 = 5;
    pub const MOTION: u8 = 6;
    pub const WHEEL: u8 = 7;
    /// `a` = depressed, `b` = latched, `code` = locked, `flags` = group.
    pub const MODS: u8 = 8;
    pub const KEYMAP: u8 = 9;
    pub const CLIP_SET: u8 = 10;
    pub const CLIP_DATA: u8 = 12;
    /// Rich text: u32 html length, HTML bytes, then plain text.
    pub const CLIP_SET_HTML: u8 = 20;
    pub const CLIP_DATA_HTML: u8 = 21;
    /// `code` = 1 to overlay the guest pointer, 0 to draw it on the client.
    pub const CURSOR: u8 = 13;
    /// `a` = 1: the server does not composite the pointer; the client shows
    /// its own cursor while the window is focused (a smooth local pointer).
    pub const CURSOR_LOCAL: u8 = 22;
    pub const PING: u8 = 14;
    /// File-clipboard offer: list of entries; length in `a`.
    pub const FILE_BEGIN: u8 = 15;
    /// Sequential file bytes; length in `a`.
    pub const FILE_DATA: u8 = 16;
    pub const FILE_END: u8 = 17;
    /// One gamepad state snapshot; see `GAMEPAD_LEN`.
    pub const GAMEPAD: u8 = 18;
    /// Client asks for an immediate keyframe after it had to drop packets, so
    /// the HEVC reference chain heals without waiting for the next GOP.
    pub const IDR: u8 = 23;
    /// Cursor shape for a locally drawn pointer. Payload: u32 LE width,
    /// u32 LE height, i32 LE hotspot_x, i32 LE hotspot_y, then RGBA pixels
    /// (top-down, straight alpha).
    pub const CURSOR_IMAGE: u8 = 24;
    /// Client asks the server to send cursor shapes. Clients that do not send
    /// this never receive CURSOR_IMAGE, so older clients keep working.
    pub const CURSOR_SHAPE: u8 = 25;
    /// Server-side cursor is hidden (for example a video player hides it).
    /// The client must hide its locally drawn pointer until the next
    /// CURSOR_IMAGE.
    pub const CURSOR_HIDDEN: u8 = 27;
    /// Client tells the audio server that it understands the LGXA timestamp
    /// framing. Sent once after the token; an older server ignores it and an
    /// older client never sends it, so both keep raw PCM compatibility.
    pub const AUDIO_SYNC: u8 = 26;
    pub const GAMEPAD_LEN: usize = 24;
}

pub mod codec {
    pub const HEVC: u16 = 1;
}

/// File clipboard (one regular file at a time, light files only).
///
/// FILE_BEGIN payload: u32 LE name length, UTF-8 name, u64 LE size.
/// FILE_DATA payload: raw file bytes. FILE_END: empty.
/// A transfer is only accepted when the total size is within `MAX_BYTES`.
pub mod file {
    pub const MAX_BYTES: u64 = 16 * 1024 * 1024;
    pub const MAX_NAME: usize = 255;

    /// Keep only the final path component; reject empty, dot and NUL names.
    pub fn sanitize_name(name: &str) -> Option<String> {
        let base = std::path::Path::new(name).file_name()?.to_str()?;
        if base.is_empty() || base == "." || base == ".." || base.contains('\0') {
            return None;
        }
        if base.len() > MAX_NAME {
            return None;
        }
        Some(base.to_string())
    }

    pub fn encode_begin(name: &str, size: u64) -> Vec<u8> {
        let name = sanitize_name(name).unwrap_or_else(|| "file".to_string());
        let mut out = Vec::with_capacity(12 + name.len());
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out
    }

    pub fn decode_begin(payload: &[u8]) -> Option<(String, u64)> {
        if payload.len() < 12 {
            return None;
        }
        let name_len = u32::from_le_bytes(payload[0..4].try_into().ok()?) as usize;
        if name_len == 0 || name_len > MAX_NAME || payload.len() != 12 + name_len {
            return None;
        }
        let name = std::str::from_utf8(&payload[4..4 + name_len]).ok()?;
        let name = sanitize_name(name)?;
        let size = u64::from_le_bytes(payload[4 + name_len..12 + name_len].try_into().ok()?);
        Some((name, size))
    }

    /// Record used between the macOS helper and the Rust server:
    /// the FILE_BEGIN header followed by the complete file data.
    pub fn encode_record(name: &str, data: &[u8]) -> Vec<u8> {
        let mut out = encode_begin(name, data.len() as u64);
        out.extend_from_slice(data);
        out
    }

    pub fn decode_record(payload: &[u8]) -> Option<(String, Vec<u8>)> {
        if payload.len() < 12 {
            return None;
        }
        let name_len = u32::from_le_bytes(payload[0..4].try_into().ok()?) as usize;
        if name_len == 0 || name_len > MAX_NAME || payload.len() < 12 + name_len {
            return None;
        }
        let name = std::str::from_utf8(&payload[4..4 + name_len]).ok()?;
        let name = sanitize_name(name)?;
        let size = u64::from_le_bytes(payload[4 + name_len..12 + name_len].try_into().ok()?);
        if size > MAX_BYTES || size as usize != payload.len() - 12 - name_len {
            return None;
        }
        Some((name, payload[12 + name_len..].to_vec()))
    }
}

pub mod evdev {
    pub const BTN_LEFT: u16 = 0x110;
    pub const BTN_RIGHT: u16 = 0x111;
    pub const BTN_MIDDLE: u16 = 0x112;
    pub const BTN_SIDE: u16 = 0x113;
    pub const BTN_EXTRA: u16 = 0x114;
    pub const BTN_FORWARD: u16 = 0x115;
    pub const BTN_BACK: u16 = 0x116;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Msg {
    pub typ: u8,
    pub flags: u8,
    pub code: u16,
    pub a: i32,
    pub b: i32,
}

impl Msg {
    pub fn new(typ: u8, code: u16, a: i32, b: i32) -> Self {
        Self { typ, flags: 0, code, a, b }
    }

    pub fn to_bytes(self) -> [u8; 12] {
        let mut out = [0u8; 12];
        out[0] = self.typ;
        out[1] = self.flags;
        out[2..4].copy_from_slice(&self.code.to_le_bytes());
        out[4..8].copy_from_slice(&self.a.to_le_bytes());
        out[8..12].copy_from_slice(&self.b.to_le_bytes());
        out
    }

    pub fn from_bytes(bytes: &[u8; 12]) -> Self {
        Self {
            typ: bytes[0],
            flags: bytes[1],
            code: u16::from_le_bytes([bytes[2], bytes[3]]),
            a: i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            b: i32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        }
    }
}

pub fn payload_len(m: &Msg) -> usize {
    match m.typ {
        msg::AUTH
        | msg::VIDEO
        | msg::KEYMAP
        | msg::CLIP_SET
        | msg::CLIP_DATA
        | msg::CLIP_SET_HTML
        | msg::CLIP_DATA_HTML
        | msg::FILE_BEGIN
        | msg::FILE_DATA
        | msg::GAMEPAD
        | msg::CURSOR_IMAGE => m.a.max(0) as usize,
        _ => 0,
    }
}

pub fn encode_rich(html: &str, text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + html.len() + text.len());
    out.extend_from_slice(&(html.len() as u32).to_le_bytes());
    out.extend_from_slice(html.as_bytes());
    out.extend_from_slice(text.as_bytes());
    out
}

pub fn decode_rich(payload: &[u8]) -> Option<(String, String)> {
    if payload.len() < 4 {
        return None;
    }
    let n = u32::from_le_bytes(payload[..4].try_into().ok()?) as usize;
    if 4 + n > payload.len() {
        return None;
    }
    let html = std::str::from_utf8(&payload[4..4 + n]).ok()?.to_string();
    let text = std::str::from_utf8(&payload[4 + n..]).ok()?.to_string();
    Some((html, text))
}

pub fn write_msg<W: Write>(w: &mut W, m: Msg, payload: &[u8]) -> io::Result<()> {
    w.write_all(&m.to_bytes())?;
    if !payload.is_empty() {
        w.write_all(payload)?;
    }
    Ok(())
}

pub fn read_msg<R: Read>(r: &mut R) -> io::Result<(Msg, Vec<u8>)> {
    let mut header = [0u8; 12];
    r.read_exact(&mut header)?;
    let m = Msg::from_bytes(&header);
    let len = payload_len(&m);
    let mut payload = Vec::new();
    if len > 0 {
        payload.resize(len, 0);
        r.read_exact(&mut payload)?;
    }
    Ok((m, payload))
}

pub mod auth;
pub mod secure;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let m = Msg { typ: msg::MOTION, flags: 2, code: 0x110, a: -1, b: 65535 };
        assert_eq!(Msg::from_bytes(&m.to_bytes()), m);
    }

    #[test]
    fn payload_rules() {
        assert_eq!(payload_len(&Msg::new(msg::VIDEO, 0, 33, 0)), 33);
        assert_eq!(payload_len(&Msg::new(msg::KEY, 30, 1, 0)), 0);
        assert_eq!(payload_len(&Msg::new(msg::CLIP_DATA, 0, 7, 0)), 7);
    }

    #[test]
    fn stream_roundtrip() {
        let mut buf = Vec::new();
        write_msg(&mut buf, Msg::new(msg::HELLO, 1, 1920, 1080), &[]).unwrap();
        write_msg(&mut buf, Msg::new(msg::VIDEO, 0, 5, 0), b"hello").unwrap();
        let mut cur = buf.as_slice();
        let (hello, _) = read_msg(&mut cur).unwrap();
        assert_eq!((hello.typ, hello.code, hello.a, hello.b), (msg::HELLO, 1, 1920, 1080));
        let (video, data) = read_msg(&mut cur).unwrap();
        assert_eq!(video.typ, msg::VIDEO);
        assert_eq!(data, b"hello");
    }
}
