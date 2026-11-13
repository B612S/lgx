//! Client networking: one TCP stream for HEVC video, input and clipboard.

use std::io;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lgx_proto::secure::{self, Secure};
use lgx_proto::{codec, files, msg, Msg};

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

    let (hello_msg, _) = secure.recv_msg()?;
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
    // Host text wins over a pre-existing guest clipboard on connect.
    if let Ok(crate::clip::Clip::Text(text)) = crate::clip::get() {
        if !text.is_empty() {
            let _ = secure.send_msg(
                Msg::new(msg::CLIP_SET, 0, text.len() as i32, 0),
                text.as_bytes(),
            );
            *conn.last_remote.lock().unwrap() = text;
        }
    }
    let conn_for_reader = conn.clone();
    std::thread::Builder::new()
        .name("lgx-reader".into())
        .spawn(move || reader(secure, conn_for_reader, lr, video))
        .expect("reader thread");
    Ok((conn, hello))
}

/// Sends files/directories from the host clipboard to the guest. The stream
/// is multiplexed with video; each message takes the write lock only for its
/// own duration.
pub fn send_files(conn: &Conn, paths: &[PathBuf]) -> io::Result<()> {
    let local = files::collect(paths)?;
    if local.is_empty() {
        return Ok(());
    }
    let entries: Vec<_> = local.iter().map(|e| e.entry.clone()).collect();
    let offer = files::encode_offer(&entries);
    conn.send(Msg::new(msg::FILE_BEGIN, 0, offer.len() as i32, 0), &offer)?;
    let mut total = 0u64;
    for e in &local {
        if e.entry.kind == files::KIND_FILE {
            total += e.entry.size;
            files::stream_file(&e.source, &mut |chunk| {
                conn.send(Msg::new(msg::FILE_DATA, 0, chunk.len() as i32, 0), chunk)
            })?;
        }
    }
    conn.send(Msg::new(msg::FILE_END, 0, 0, 0), &[])?;
    eprintln!(
        "lgx-client: sent {} file(s), {} KiB",
        entries.iter().filter(|e| e.kind == files::KIND_FILE).count(),
        total / 1024
    );
    Ok(())
}

fn reader(
    secure: Arc<Secure>,
    conn: Arc<Conn>,
    last_remote: Arc<Mutex<String>>,
    video: std::sync::mpsc::Sender<Chunk>,
) {
    let mut recv: Option<files::Receiver> = None;
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
            msg::CLIP_DATA => {
                let text = String::from_utf8_lossy(&payload).to_string();
                *last_remote.lock().unwrap() = text.clone();
                if let Err(e) = crate::clip::set_text(&text) {
                    eprintln!("lgx-client: wl-copy: {e}");
                }
            }
            msg::FILE_BEGIN => match files::decode_offer(&payload) {
                Some(entries) => {
                    eprintln!("lgx-client: incoming file clipboard: {} entries", entries.len());
                    recv = files::Receiver::begin(entries).ok();
                }
                None => eprintln!("lgx-client: malformed file offer"),
            },
            msg::FILE_DATA => {
                if let Some(r) = recv.as_mut() {
                    if let Err(e) = r.data(&payload) {
                        eprintln!("lgx-client: file receive: {e}");
                        recv = None;
                    }
                }
            }
            msg::FILE_END => {
                if let Some(mut r) = recv.take() {
                    match r.finish() {
                        Ok(paths) if !paths.is_empty() => {
                            if let Err(e) = crate::clip::set_files(&paths) {
                                eprintln!("lgx-client: wl-copy files: {e}");
                            } else {
                                eprintln!(
                                    "lgx-client: file clipboard ready ({} item(s))",
                                    paths.len()
                                );
                            }
                        }
                        Ok(_) => eprintln!("lgx-client: empty file transfer"),
                        Err(e) => eprintln!("lgx-client: file receive: {e}"),
                    }
                }
            }
            _ => {}
        }
    }
    conn.closed.store(true, Ordering::Relaxed);
}
