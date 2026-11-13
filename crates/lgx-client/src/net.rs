//! Client networking: one TCP stream for HEVC video, input and clipboard.

use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lgx_proto::secure::{self, Secure};
use lgx_proto::{codec, msg, Msg};

use crate::dec::Chunk;

pub struct Hello {
    pub width: u32,
    pub height: u32,
    pub codec: u16,
}

pub struct Conn {
    secure: Arc<Secure>,
    pub last_remote: Arc<Mutex<String>>,
    pub bytes: AtomicU64,
    /// Set by the reader when the server closes or the stream fails.
    pub closed: std::sync::atomic::AtomicBool,
}

impl Conn {
    pub fn send(&self, m: Msg, payload: &[u8]) -> std::io::Result<()> {
        self.secure.send_msg(m, payload)
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
    video: std::sync::mpsc::Sender<Chunk>,
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

    let last_remote: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let lr = last_remote.clone();
    let conn = Arc::new(Conn {
        secure: secure.clone(),
        last_remote,
        bytes: AtomicU64::new(0),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    let conn_for_reader = conn.clone();
    std::thread::Builder::new()
        .name("lgx-reader".into())
        .spawn(move || reader(secure, conn_for_reader, lr, video))
        .expect("reader thread");
    Ok((conn, hello))
}


fn reader(
    secure: Arc<Secure>,
    conn: Arc<Conn>,
    last_remote: Arc<Mutex<String>>,
    video: std::sync::mpsc::Sender<Chunk>,
) {
    loop {
        let (m, payload) = match secure.recv_msg() {
            Ok(v) => v,
            Err(_) => break,
        };
        match m.typ {
            msg::VIDEO => {
                conn.bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
                if video.send(Chunk { data: payload, at: std::time::Instant::now() }).is_err() {
                    break;
                }
            }
            msg::CLIP_DATA_HTML => {
                if let Some((html, text)) = lgx_proto::decode_rich(&payload) {
                    *last_remote.lock().unwrap() = crate::clip::signature(&crate::clip::Clip::Rich {
                        html: html.clone(),
                        text: text.clone(),
                    });
                    if let Err(e) = crate::clip::set_rich(&html, &text) {
                        eprintln!("lgx-client: rich clipboard: {e}");
                    }
                }
            }
            msg::CLIP_DATA => {
                let text = String::from_utf8_lossy(&payload).to_string();
                *last_remote.lock().unwrap() = text.clone();
                if let Err(e) = crate::clip::set_text(&text) {
                    eprintln!("lgx-client: wl-copy: {e}");
                }
            }
            msg::FILE_BEGIN | msg::FILE_DATA | msg::FILE_END => {
                eprintln!("lgx-client: ignoring file clipboard (text only)");
            }
            _ => {}
        }
    }
    conn.closed.store(true, Ordering::Relaxed);
}
