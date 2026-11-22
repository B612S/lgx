//! Client networking: one TCP stream for HEVC video, input and clipboard.

use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lgx_proto::secure::{self, Secure};
use lgx_proto::{codec, file, msg, Msg};

use crate::dec::Chunk;

pub struct Hello {
    pub width: u32,
    pub height: u32,
    pub codec: u16,
}

#[derive(Clone, Copy)]
pub struct ClipPolicy {
    pub enabled: bool,
    pub rich: bool,
    /// 0 means no limit.
    pub max_bytes: usize,
}

impl Default for ClipPolicy {
    fn default() -> Self {
        Self { enabled: true, rich: true, max_bytes: 1024 * 1024 }
    }
}

impl ClipPolicy {
    fn allows(&self, len: usize) -> bool {
        self.enabled && (self.max_bytes == 0 || len <= self.max_bytes)
    }
}

/// One cursor shape sent by a server that asks the client to draw the pointer.
#[derive(Clone)]
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub rgba: Vec<u8>,
}

impl CursorShape {
    fn parse(body: &[u8]) -> Option<Self> {
        if body.len() < 16 {
            return None;
        }
        let width = u32::from_le_bytes(body[0..4].try_into().ok()?);
        let height = u32::from_le_bytes(body[4..8].try_into().ok()?);
        let hot_x = i32::from_le_bytes(body[8..12].try_into().ok()?);
        let hot_y = i32::from_le_bytes(body[12..16].try_into().ok()?);
        if width == 0 || height == 0 || width > 256 || height > 256 {
            return None;
        }
        if hot_x < 0 || hot_y < 0 || hot_x >= width as i32 || hot_y >= height as i32 {
            return None;
        }
        let need = 16 + (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
        if body.len() != need {
            return None;
        }
        Some(Self {
            width,
            height,
            hot_x,
            hot_y,
            rgba: body[16..].to_vec(),
        })
    }
}

pub struct Conn {
    secure: Arc<Secure>,
    pub last_remote: Arc<Mutex<String>>,
    pub bytes: AtomicU64,
    /// Set by the reader when the server closes or the stream fails.
    pub closed: std::sync::atomic::AtomicBool,
    /// Server asked for a local cursor (it does not composite the pointer).
    pub local_cursor: std::sync::atomic::AtomicBool,
    /// Latest cursor shape for the locally drawn pointer.
    pub cursor: Arc<Mutex<Option<CursorShape>>>,
    /// The guest hid its pointer (for example over a video); hide the local one.
    pub cursor_hidden: std::sync::atomic::AtomicBool,
    /// Latest `RESIZE` from the server: (width, height, refresh Hz).
    pub resize: Mutex<Option<(u32, u32, u32)>>,
}

impl Conn {
    pub fn send(&self, m: Msg, payload: &[u8]) -> std::io::Result<()> {
        self.secure.send_msg(m, payload)
    }

    /// Consumes a display-mode change announced by the server.
    pub fn take_resize(&self) -> Option<(u32, u32, u32)> {
        self.resize.lock().unwrap().take()
    }

    /// Test hook (`LGX_FAKE_RESIZE`) that exercises the display dialog without
    /// a real compositor mode change.
    pub fn inject_resize(&self, w: u32, h: u32, fps: u32) {
        *self.resize.lock().unwrap() = Some((w, h, fps));
    }

    pub fn take_bytes(&self) -> u64 {
        self.bytes.swap(0, Ordering::Relaxed)
    }

    /// Shuts the socket down so the reader thread leaves recv_msg() and drops
    /// the video sender, which lets the decoder thread exit before GL/CUDA
    /// teardown.
    pub fn close(&self) {
        self.secure.close();
    }
}

pub fn connect(
    addr: &str,
    keymap: &[u8],
    client_id: &str,
    token: &str,
    video: std::sync::mpsc::SyncSender<Chunk>,
    clipboard: ClipPolicy,
) -> std::io::Result<(Arc<Conn>, Hello)> {
    let stream = TcpStream::connect(addr)?;
    let secure = Arc::new(secure::client_handshake_id(stream, client_id, token)?);
    secure.send_msg(
        Msg::new(msg::AUTH, 0, token.len() as i32, 0),
        token.as_bytes(),
    )?;
    secure.send_msg(Msg::new(msg::KEYMAP, 1, keymap.len() as i32, 0), keymap)?;

    secure.set_read_timeout(Some(Duration::from_secs(5)))?;
    let (hello_msg, _) = secure.recv_msg()?;
    secure.set_read_timeout(None)?;
    if hello_msg.typ != msg::HELLO {
        return Err(std::io::Error::other(format!(
            "expected HELLO, got message {:#x}",
            hello_msg.typ
        )));
    }
    let hello = Hello {
        width: hello_msg.a.max(0) as u32,
        height: hello_msg.b.max(0) as u32,
        codec: hello_msg.code,
    };
    if hello.codec != codec::HEVC {
        return Err(std::io::Error::other(format!(
            "server uses codec {}, this client only speaks HEVC",
            hello.codec
        )));
    }

    // Opt in to cursor-shape updates. Older clients never send this, so the
    // server can keep sending them only a video stream with no unknown
    // CURSOR_IMAGE payloads.
    secure.send_msg(Msg::new(msg::CURSOR_SHAPE, 0, 0, 0), &[])?;
    let last_remote: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let lr = last_remote.clone();
    let conn = Arc::new(Conn {
        secure: secure.clone(),
        last_remote,
        bytes: AtomicU64::new(0),
        closed: std::sync::atomic::AtomicBool::new(false),
        local_cursor: std::sync::atomic::AtomicBool::new(false),
        cursor: Arc::new(Mutex::new(None)),
        cursor_hidden: std::sync::atomic::AtomicBool::new(false),
        resize: Mutex::new(None),
    });
    let conn_for_reader = conn.clone();
    std::thread::Builder::new()
        .name("lgx-reader".into())
        .spawn(move || reader(secure, conn_for_reader, lr, video, clipboard))
        .expect("reader thread");
    Ok((conn, hello))
}


fn reader(
    secure: Arc<Secure>,
    conn: Arc<Conn>,
    last_remote: Arc<Mutex<String>>,
    video: std::sync::mpsc::SyncSender<Chunk>,
    clipboard: ClipPolicy,
) {
    // Decoded frames are dropped, never queued without bound: a slow decoder
    // used to turn a 120 fps stream into smooth but seconds-old video. When the
    // queue is full the reader skips to the next IDR and asks the server for
    // one, so latency stays bounded and the reference chain heals quickly.
    let mut skipping = false;
    let mut last_idr_request: Option<std::time::Instant> = None;
    // Incoming file clipboard transfer.
    let mut incoming_file: Option<(String, u64, Vec<u8>)> = None;
    // Test hook: write the received Annex-B access units to a file.
    let mut dump = std::env::var_os("LGX_DUMP_BITSTREAM").map(|path| {
        eprintln!("lgx-client: dumping the bitstream to {}", path.to_string_lossy());
        std::fs::File::create(path)
    });
    while let Ok((m, payload)) = secure.recv_msg_raw() {
        let body = &payload[secure::MSG_HEADER..];
        match m.typ {
            msg::RESIZE => {
                // The encoder was rebuilt for a new display mode; the user
                // decides whether the window follows it.
                let w = m.a.max(0) as u32;
                let h = m.b.max(0) as u32;
                if w > 0 && h > 0 {
                    *conn.resize.lock().unwrap() = Some((w, h, m.code as u32));
                }
            }
            msg::VIDEO => {
                if let Some(Ok(file)) = dump.as_mut() {
                    let _ = std::io::Write::write_all(file, body);
                }
                conn.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                if skipping {
                    if !is_idr(body) {
                        continue;
                    }
                    skipping = false;
                }
                // 48-bit capture timestamp in microseconds, 0 if the server
                // does not timestamp video.
                let capture_us = ((m.code as u64) << 32) | (m.b as u32 as u64);
                let chunk = Chunk {
                    data: payload,
                    off: secure::MSG_HEADER,
                    at: std::time::Instant::now(),
                    capture_us,
                };
                match video.try_send(chunk) {
                    Ok(()) => {}
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        skipping = true;
                        let due = last_idr_request
                            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_millis(500));
                        if due {
                            last_idr_request = Some(std::time::Instant::now());
                            let _ = conn.send(Msg::new(msg::IDR, 0, 0, 0), &[]);
                        }
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                }
            }
            msg::CLIP_DATA_HTML => {
                if !clipboard.allows(body.len()) {
                    continue;
                }
                if let Some((html, text)) = lgx_proto::decode_rich(body) {
                    if clipboard.rich {
                        *last_remote.lock().unwrap() =
                            crate::clip::signature(&crate::clip::Clip::Rich {
                                html: html.clone(),
                                text: text.clone(),
                            });
                        if let Err(e) = crate::clip::set_rich(&html, &text) {
                            eprintln!("lgx-client: rich clipboard: {e}");
                        }
                    } else if !text.is_empty() {
                        *last_remote.lock().unwrap() = text.clone();
                        if let Err(e) = crate::clip::set_text(&text) {
                            eprintln!("lgx-client: wl-copy: {e}");
                        }
                    }
                }
            }
            msg::CURSOR_LOCAL => {
                conn.local_cursor
                    .store(m.a != 0, std::sync::atomic::Ordering::Relaxed);
            }
            msg::CURSOR_IMAGE => {
                if let Some(shape) = CursorShape::parse(body) {
                    if conn.cursor_hidden.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        eprintln!("lgx-client: guest cursor visible");
                    }
                    *conn.cursor.lock().unwrap() = Some(shape);
                }
            }
            msg::CURSOR_HIDDEN => {
                eprintln!("lgx-client: guest cursor hidden");
                conn.cursor_hidden
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            msg::CLIP_DATA => {
                if !clipboard.allows(body.len()) {
                    continue;
                }
                let text = String::from_utf8_lossy(body).to_string();
                *last_remote.lock().unwrap() = text.clone();
                if let Err(e) = crate::clip::set_text(&text) {
                    eprintln!("lgx-client: wl-copy: {e}");
                }
            }
            msg::FILE_BEGIN => {
                eprintln!("lgx-client: file begin ({} bytes)", body.len());
                if !clipboard.enabled {
                    incoming_file = None;
                    continue;
                }
                match file::decode_begin(body) {
                    Some((name, size)) if size <= file::MAX_BYTES => {
                        incoming_file = Some((name, size, Vec::new()));
                    }
                    _ => incoming_file = None,
                }
            }
            msg::FILE_DATA => {
                let Some((_, size, data)) = incoming_file.as_mut() else {
                    continue;
                };
                if data.len() as u64 + body.len() as u64 > *size {
                    incoming_file = None;
                    continue;
                }
                data.extend_from_slice(body);
            }
            msg::FILE_END => {
                let Some((name, size, data)) = incoming_file.take() else {
                    continue;
                };
                if data.len() as u64 != size {
                    continue;
                }
                eprintln!("lgx-client: file end {} ({} bytes)", name, data.len());
                if let Some(sig) = Some(crate::clip::signature(&crate::clip::Clip::File {
                    name: name.clone(),
                    data: data.clone(),
                })) {
                    *last_remote.lock().unwrap() = sig;
                }
                if let Err(e) = crate::clip::set_file(&name, &data) {
                    eprintln!("lgx-client: file clipboard: {e}");
                }
            }
            _ => {}
        }
    }
    conn.closed.store(true, Ordering::Relaxed);
}

/// True when an Annex-B access unit contains an HEVC IDR NAL (type 19 or 20).
fn is_idr(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let mut nal = i + 3;
            // 4-byte start codes leave a zero byte before the NAL header.
            if nal < data.len() && data[nal] == 0 {
                nal += 1;
            }
            if nal < data.len() {
                let typ = (data[nal] >> 1) & 0x3f;
                if typ == 19 || typ == 20 {
                    return true;
                }
                i = nal;
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    false
}

/// Frames buffered between the reader and the decoder. Six frames at 120 Hz
/// bound the extra latency to ~50 ms.
pub const VIDEO_QUEUE: usize = 6;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_image_payload_is_validated() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&2u32.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&1i32.to_le_bytes());
        payload.extend_from_slice(&0i32.to_le_bytes());
        payload.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let shape = CursorShape::parse(&payload).expect("valid shape");
        assert_eq!((shape.width, shape.height), (2, 1));
        assert_eq!((shape.hot_x, shape.hot_y), (1, 0));
        assert_eq!(shape.rgba.len(), 8);
        let mut bad = payload.clone();
        bad.pop();
        assert!(CursorShape::parse(&bad).is_none());
        let mut bad_hotspot = payload.clone();
        bad_hotspot[8..12].copy_from_slice(&2i32.to_le_bytes());
        assert!(CursorShape::parse(&bad_hotspot).is_none());
    }

    #[test]
    fn clipboard_policy_limits() {
        let mut p = ClipPolicy::default();
        assert!(p.allows(1024));
        assert!(!p.allows(1024 * 1024 + 1));
        p.max_bytes = 0;
        assert!(p.allows(usize::MAX));
        p.enabled = false;
        assert!(!p.allows(1));
    }
}

