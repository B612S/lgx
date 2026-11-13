//! TCP transport: one connection per client carries HEVC video, input and
//! clipboard.
//!
//! Every client has a single writer thread; video frames, HELLO and clipboard
//! data are serialised there. Encoded frames are never copied to the CPU: the
//! writer takes ownership of the encoder's buffer.

use std::io;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use lgx_proto::secure::{self, Secure};
use lgx_proto::{codec, files, msg, Msg};

use crate::clip;
use crate::input::Cmd;

enum Out {
    Video(Vec<u8>),
    Clip(String),
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
    tx: SyncSender<Out>,
    /// A freshly registered client may only see an IDR; delayed packets from
    /// the previous client's encoder state are dropped for it.
    needs_idr: bool,
    secure: Arc<Secure>,
    file_busy: Arc<AtomicBool>,
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

    pub fn broadcast_video(&self, chunk: Vec<u8>) {
        let idr = is_idr(&chunk);
        let mut list = self.clients.lock().unwrap();
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

    pub fn broadcast_files(&self, paths: &[PathBuf]) {
        let list = self.clients.lock().unwrap();
        for c in list.iter() {
            if c.file_busy.swap(true, Ordering::Relaxed) {
                continue;
            }
            let secure = c.secure.clone();
            let busy = c.file_busy.clone();
            let paths = paths.to_vec();
            std::thread::spawn(move || {
                if let Err(e) = send_files_chunked(&secure, &paths) {
                    eprintln!("lgx-server: file send: {e}");
                }
                busy.store(false, Ordering::Relaxed);
            });
        }
    }
}

/// Streams a file clipboard to one client, locking the socket per message so
/// video packets interleave instead of freezing for the whole transfer.
fn send_files_chunked(secure: &Arc<Secure>, paths: &[PathBuf]) -> io::Result<()> {
    let local = files::collect(paths)?;
    if local.is_empty() {
        return Ok(());
    }
    let entries: Vec<_> = local.iter().map(|e| e.entry.clone()).collect();
    let offer = files::encode_offer(&entries);
    secure.send_msg(
        Msg::new(msg::FILE_BEGIN, 0, offer.len() as i32, 0),
        &offer,
    )?;
    let mut total = 0u64;
    for e in &local {
        if e.entry.kind == files::KIND_FILE {
            total += e.entry.size;
            files::stream_file(&e.source, &mut |chunk| {
                secure.send_msg(Msg::new(msg::FILE_DATA, 0, chunk.len() as i32, 0), chunk)
            })?;
        }
    }
    secure.send_msg(Msg::new(msg::FILE_END, 0, 0, 0), &[])?;
    eprintln!(
        "lgx-server: sent {} file(s), {} KiB",
        entries.iter().filter(|e| e.kind == files::KIND_FILE).count(),
        total / 1024
    );
    Ok(())
}

pub fn listen(
    addr: &str,
    hub: Arc<Hub>,
    input: std::sync::mpsc::Sender<Cmd>,
    clip_set: Arc<dyn Fn(&str) + Send + Sync>,
    token: Option<Arc<String>>,
    last_remote: Arc<Mutex<String>>,
) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    eprintln!("lgx-server: listening on {addr} (HEVC)");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let id = hub.next_id.fetch_add(1, Ordering::Relaxed);
        eprintln!("lgx-server: client #{id}: {:?}", stream.peer_addr());
        let _ = stream.set_nodelay(true);
        let hub2 = hub.clone();
        let input2 = input.clone();
        let clip2 = clip_set.clone();
        let token2 = token.clone();
        let remote2 = last_remote.clone();
        std::thread::Builder::new()
            .name(format!("lgx-client-{id}"))
            .spawn(move || {
                if let Err(e) = handle(stream, id, hub2, input2, clip2, token2, remote2) {
                    eprintln!("lgx-server: client #{id}: {e}");
                }
                eprintln!("lgx-server: client #{id} disconnected");
            })
            .ok();
    }
    Ok(())
}

fn handle(
    stream: TcpStream,
    id: u64,
    hub: Arc<Hub>,
    input: std::sync::mpsc::Sender<Cmd>,
    clip_set: Arc<dyn Fn(&str) + Send + Sync>,
    token: Option<Arc<String>>,
    last_remote: Arc<Mutex<String>>,
) -> io::Result<()> {
    let token_secret = token.as_ref().map(|s| s.as_str()).unwrap_or("");
    let secure = match secure::server_handshake(stream, token_secret) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("lgx-server: client #{id}: secure handshake failed: {e}");
            return Ok(());
        }
    };
    let (m, payload) = secure.recv_msg()?;
    if let Some(expected) = &token {
        let ok = m.typ == msg::AUTH
            && lgx_proto::auth::eq(
                String::from_utf8_lossy(&payload).trim(),
                expected,
            );
        if !ok {
            eprintln!("lgx-server: client #{id}: authentication failed");
            secure.close();
            return Ok(());
        }
        eprintln!("lgx-server: client #{id}: authenticated");
    }

    // HELLO before the writer thread: the client needs the geometry and codec
    // before it can size its window or create a decoder.
    {
        let (w, h) = hub.geometry();
        secure.send_msg(Msg::new(msg::HELLO, codec::HEVC, w as i32, h as i32), &[])?;
    }

    let (tx, rx) = std::sync::mpsc::sync_channel::<Out>(2);
    let file_busy = Arc::new(AtomicBool::new(false));
    hub.clients.lock().unwrap().push(Client {
        id,
        tx: tx.clone(),
        needs_idr: true,
        secure: secure.clone(),
        file_busy: file_busy.clone(),
    });
    hub.request_keyframe();

    // A newly connected client should see the current clipboard; the poll
    // may have marked it as sent while nobody was connected.
    match crate::clip::get() {
        Ok(clip::Clip::Text(text)) if !text.is_empty() => {
            *last_remote.lock().unwrap() =
                clip::signature(&clip::Clip::Text(text.clone()));
            let _ = tx.try_send(Out::Clip(text));
        }
        Ok(clip::Clip::Files(paths)) => {
            *last_remote.lock().unwrap() = files::signature(&paths);
            if !file_busy.swap(true, Ordering::Relaxed) {
                let secure = secure.clone();
                let busy = file_busy.clone();
                std::thread::spawn(move || {
                    if let Err(e) = send_files_chunked(&secure, &paths) {
                        eprintln!("lgx-server: file send: {e}");
                    }
                    busy.store(false, Ordering::Relaxed);
                });
            }
        }
        _ => {}
    }

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
                };
                if !ok {
                    hubw.request_keyframe_soon();
                    break;
                }
            }
        })?;

    let mut file_recv: Option<files::Receiver> = None;
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
            msg::FILE_BEGIN => match files::decode_offer(&payload) {
                Some(entries) => {
                    eprintln!("lgx-server: incoming file clipboard: {} entries", entries.len());
                    file_recv = files::Receiver::begin(entries).ok();
                }
                None => eprintln!("lgx-server: malformed file offer"),
            },
            msg::FILE_DATA => {
                if let Some(r) = file_recv.as_mut() {
                    if let Err(e) = r.data(&payload) {
                        eprintln!("lgx-server: file receive: {e}");
                        file_recv = None;
                    }
                }
            }
            msg::FILE_END => {
                if let Some(mut r) = file_recv.take() {
                    match r.finish() {
                        Ok(paths) if !paths.is_empty() => {
                            *last_remote.lock().unwrap() = files::signature(&paths);
                            if let Err(e) = crate::clip::set_files(&paths) {
                                eprintln!("lgx-server: wl-copy files: {e}");
                            } else {
                                eprintln!(
                                    "lgx-server: file clipboard ready ({} item(s))",
                                    paths.len()
                                );
                            }
                        }
                        Ok(_) => eprintln!("lgx-server: empty file transfer"),
                        Err(e) => eprintln!("lgx-server: file receive: {e}"),
                    }
                }
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
