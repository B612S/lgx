//! TCP transport: one connection per client carries HEVC video, input and
//! clipboard.
//!
//! Every client has a single writer thread; video frames, HELLO and clipboard
//! data are serialised there. Encoded frames are never copied to the CPU: the
//! writer takes ownership of the encoder's buffer.

use std::collections::HashMap;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lgx_proto::secure::{self, Secure};
use lgx_proto::{codec, msg, Msg};

use crate::clients;
use crate::clip;
use crate::input::Cmd;

enum Out {
    Video(Arc<Vec<u8>>),
    Clip(String),
    ClipHtml(Vec<u8>),
}

pub struct Hub {
    clients: Mutex<Vec<Client>>,
    next_id: AtomicU64,
    geometry: Mutex<(u32, u32)>,
    keyframe: AtomicBool,
    last_keyframe_ms: AtomicU64,
    cursor_overlay: AtomicBool,
    dropped: AtomicU64,
}

struct Client {
    id: u64,
    /// Registry id of the authenticated client; used for revocation.
    client: String,
    /// Fingerprint of the token used at handshake; token rotation closes the
    /// old session on the next registry watcher tick.
    token: [u8; 32],
    tx: SyncSender<Out>,
    /// A freshly registered client may only see an IDR; delayed packets from
    /// the previous client's encoder state are dropped for it.
    needs_idr: bool,
    secure: Arc<Secure>,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            clients: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            geometry: Mutex::new((0, 0)),
            keyframe: AtomicBool::new(false),
            last_keyframe_ms: AtomicU64::new(0),
            cursor_overlay: AtomicBool::new(true),
            dropped: AtomicU64::new(0),
        })
    }

    pub fn set_geometry(&self, width: u32, height: u32) {
        *self.geometry.lock().unwrap() = (width, height);
    }

    pub fn geometry(&self) -> (u32, u32) {
        *self.geometry.lock().unwrap()
    }

    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }

    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
        self.last_keyframe_ms.store(now_ms() as u64, Ordering::Relaxed);
    }

    /// Ask for an IDR, but never more often than twice a second; a slow client
    /// that drops frames must not turn the whole stream into IDRs.
    fn request_keyframe_soon(&self) {
        let now = now_ms() as u64;
        let last = self.last_keyframe_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= 500 {
            self.request_keyframe();
        }
    }

    pub fn take_keyframe(&self) -> bool {
        self.keyframe.swap(false, Ordering::Relaxed)
    }

    pub fn set_cursor_overlay(&self, on: bool) {
        self.cursor_overlay.store(on, Ordering::Relaxed);
    }

    pub fn cursor_overlay(&self) -> bool {
        self.cursor_overlay.load(Ordering::Relaxed)
    }

    pub fn revoke_missing(&self, valid: &HashMap<String, [u8; 32]>) {
        let mut list = self.clients.lock().unwrap();
        list.retain(|c| {
            if valid.get(&c.client).is_some_and(|h| *h == c.token) {
                true
            } else {
                eprintln!("lgx-server: revoking active client {}", c.client);
                c.secure.close();
                false
            }
        });
    }

    pub fn broadcast_video(&self, chunk: Vec<u8>) {
        let chunk = Arc::new(chunk);
        let mut list = self.clients.lock().unwrap();
        // Scanning every packet for an IDR is only needed while a newly
        // registered client still waits for its first one.
        let idr = list.iter().any(|c| c.needs_idr) && is_idr(&chunk);
        let mut dropped = false;
        list.retain_mut(|c| {
            if c.needs_idr && !idr {
                return true;
            }
            match c.tx.try_send(Out::Video(chunk.clone())) {
                Ok(()) => {
                    c.needs_idr = false;
                    true
                }
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    dropped = true;
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
        drop(list);
        if dropped {
            self.request_keyframe_soon();
        }
    }

    pub fn broadcast_clip(&self, text: &str) {
        let mut list = self.clients.lock().unwrap();
        list.retain(|c| match c.tx.try_send(Out::Clip(text.to_string())) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    pub fn broadcast_rich(&self, html: &str, text: &str) {
        let payload = lgx_proto::encode_rich(html, text);
        let mut list = self.clients.lock().unwrap();
        list.retain(|c| match c.tx.try_send(Out::ClipHtml(payload.clone())) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

}

/// Streams a file clipboard to one client, locking the socket per message so
/// video packets interleave instead of freezing for the whole transfer.

pub fn listen(
    addr: &str,
    hub: Arc<Hub>,
    input: std::sync::mpsc::Sender<Cmd>,
    clip_set: Arc<dyn Fn(&str) + Send + Sync>,
    last_remote: Arc<Mutex<String>>,
) -> io::Result<()> {
    const MAX_CLIENTS: usize = 1;
    let active = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind(addr)?;
    eprintln!("lgx-server: listening on {addr} (HEVC)");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if active.load(Ordering::Relaxed) >= MAX_CLIENTS {
            eprintln!("lgx-server: too many clients, rejecting");
            continue;
        }
        active.fetch_add(1, Ordering::Relaxed);
        let id = hub.next_id.fetch_add(1, Ordering::Relaxed);
        eprintln!("lgx-server: client #{id}: {:?}", stream.peer_addr());
        let _ = stream.set_nodelay(true);
        let hub2 = hub.clone();
        let input2 = input.clone();
        let clip2 = clip_set.clone();
        let remote2 = last_remote.clone();
        let active2 = active.clone();
        std::thread::Builder::new()
            .name(format!("lgx-client-{id}"))
            .spawn(move || {
                if let Err(e) = handle(stream, id, hub2, input2, clip2, remote2) {
                    eprintln!("lgx-server: client #{id}: {e}");
                }
                eprintln!("lgx-server: client #{id} disconnected");
                active2.fetch_sub(1, Ordering::Relaxed);
            })
            .ok();
    }
    Ok(())
}

fn handle(
    mut stream: TcpStream,
    id: u64,
    hub: Arc<Hub>,
    input: std::sync::mpsc::Sender<Cmd>,
    clip_set: Arc<dyn Fn(&str) + Send + Sync>,
    last_remote: Arc<Mutex<String>>,
) -> io::Result<()> {
    let client_id = match secure::server_read_id(&mut stream) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lgx-server: client #{id}: cannot read client id: {e}");
            return Ok(());
        }
    };
    let token = match clients::token_for(&client_id) {
        Ok(Some(t)) => t,
        Ok(None) => {
            eprintln!("lgx-server: client #{id}: unknown client id {client_id}");
            return Ok(());
        }
        Err(e) => {
            eprintln!("lgx-server: client #{id}: registry: {e}");
            return Ok(());
        }
    };
    let secure = match secure::server_handshake(stream, &token) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("lgx-server: client #{id}: secure handshake failed: {e}");
            return Ok(());
        }
    };
    secure.set_read_timeout(Some(Duration::from_secs(5)))?;
    let (m, payload) = secure.recv_msg()?;
    secure.set_read_timeout(None)?;
    let ok = m.typ == msg::AUTH
        && lgx_proto::auth::eq(String::from_utf8_lossy(&payload).trim(), &token);
    if !ok {
        eprintln!("lgx-server: client #{id}: authentication failed");
        secure.close();
        return Ok(());
    }
    eprintln!("lgx-server: client #{id}: authenticated as {client_id}");

    // HELLO before the writer thread: the client needs the geometry and codec
    // before it can size its window or create a decoder.
    {
        let (w, h) = hub.geometry();
        secure.send_msg(Msg::new(msg::HELLO, codec::HEVC, w as i32, h as i32), &[])?;
    }

    let (tx, rx) = std::sync::mpsc::sync_channel::<Out>(2);
    let token_hash = clients::token_fingerprint(&token);
    hub.clients.lock().unwrap().push(Client {
        id,
        client: client_id,
        token: token_hash,
        tx: tx.clone(),
        needs_idr: true,
        secure: secure.clone(),
    });
    hub.request_keyframe();

    let w2 = secure.clone();
    let hubw = hub.clone();
    let writer = std::thread::Builder::new()
        .name(format!("lgx-writer-{id}"))
        .spawn(move || {
            while let Ok(item) = rx.recv() {
                let ok = match item {
                    Out::Video(chunk) => {
                        w2.send_msg(Msg::new(msg::VIDEO, 0, chunk.len() as i32, 0), &chunk)
                            .is_ok()
                    }
                    Out::Clip(text) => w2
                        .send_msg(
                            Msg::new(msg::CLIP_DATA, 0, text.len() as i32, 0),
                            text.as_bytes(),
                        )
                        .is_ok(),
                    Out::ClipHtml(payload) => w2
                        .send_msg(
                            Msg::new(msg::CLIP_DATA_HTML, 0, payload.len() as i32, 0),
                            &payload,
                        )
                        .is_ok(),
                };
                if !ok {
                    hubw.request_keyframe_soon();
                    break;
                }
            }
        })?;

    loop {
        let (m, payload) = match secure.recv_msg() {
            Ok(v) => v,
            Err(_) => break,
        };
        match m.typ {
            msg::KEYMAP => {
                let _ = input.send(Cmd::Keymap(payload));
            }
            msg::KEY => {
                let _ = input.send(Cmd::Key {
                    code: m.code as u32,
                    state: m.a as u32,
                    time: now_ms(),
                });
            }
            msg::BTN => {
                let _ = input.send(Cmd::Btn {
                    code: m.code as u32,
                    state: m.a as u32,
                    time: now_ms(),
                });
            }
            msg::MOTION => {
                let _ = input.send(Cmd::Motion {
                    x: m.a as u32,
                    y: m.b as u32,
                    time: now_ms(),
                });
            }
            msg::MODS => {
                let _ = input.send(Cmd::Mods {
                    depressed: m.a as u32,
                    latched: m.b as u32,
                    locked: m.code as u32,
                    group: m.flags as u32,
                });
            }
            msg::WHEEL => {
                let _ = input.send(Cmd::Wheel {
                    axis: m.code as u32,
                    steps: m.a,
                    time: now_ms(),
                });
            }
            msg::CLIP_SET => {
                if let Ok(text) = String::from_utf8(payload) {
                    clip_set(&text);
                }
            }
            msg::CLIP_SET_HTML => {
                if let Some((html, text)) = lgx_proto::decode_rich(&payload) {
                    *last_remote.lock().unwrap() = clip::signature(&clip::Clip::Rich {
                        html: html.clone(),
                        text: text.clone(),
                    });
                    if let Err(e) = crate::clip::set_rich(&html, &text) {
                        eprintln!("lgx-server: rich clipboard: {e}");
                    }
                }
            }
            msg::FILE_BEGIN | msg::FILE_DATA | msg::FILE_END => {
                eprintln!("lgx-server: ignoring file clipboard (text only)");
            }
            msg::GAMEPAD => {
                if payload.len() >= msg::GAMEPAD_LEN {
                    let pad = payload[0];
                    let connected = payload[1] != 0;
                    let buttons =
                        u32::from_le_bytes(payload[4..8].try_into().unwrap());
                    let mut axes = [0i16; 6];
                    for (i, a) in axes.iter_mut().enumerate() {
                        *a = i16::from_le_bytes(
                            payload[8 + i * 2..10 + i * 2].try_into().unwrap(),
                        );
                    }
                    let hat = [payload[20] as i8, payload[21] as i8];
                    let _ = input.send(Cmd::Gamepad {
                        pad,
                        connected,
                        buttons,
                        axes,
                        hat,
                    });
                }
            }
            msg::CURSOR => hub.set_cursor_overlay(m.code != 0),
            msg::PING => {}
            _ => {}
        }
    }

    hub.clients.lock().unwrap().retain(|c| c.id != id);
    drop(tx);
    let _ = writer.join();
    secure.close();
    Ok(())
}

/// True when an Annex-B chunk contains an HEVC IDR NAL (type 19 or 20).
fn is_idr(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                let t = (data[i + 3] >> 1) & 0x3f;
                if t == 19 || t == 20 {
                    return true;
                }
                i += 3;
                continue;
            }
            if data[i + 2] == 0 && data[i + 3] == 1 {
                let t = (data[i + 4] >> 1) & 0x3f;
                if t == 19 || t == 20 {
                    return true;
                }
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    false
}

fn now_ms() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}
