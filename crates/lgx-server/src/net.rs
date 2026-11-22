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
use std::time::{Duration, Instant};

use lgx_proto::secure::{self, Secure};
use lgx_proto::{codec, file, msg, Msg};

use crate::clients;
use crate::clip;
use crate::input::Cmd;

/// How long a clipboard update may wait for room in the writer queue instead
/// of being dropped behind a burst of video frames.
const CLIP_QUEUE_TIMEOUT: Duration = Duration::from_millis(250);

enum Out {
    Resize(u32, u32, u32),
    Video(Arc<Vec<u8>>, u64),
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
    /// The server does not composite a pointer into the video (GNOME portal);
    /// clients should draw their own.
    cursor_local: AtomicBool,
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

/// Retry a clipboard message for a bounded time so a burst of video frames in
/// the writer queue does not silently drop it. Called without the clients
/// lock held.
fn send_with_retry(tx: &SyncSender<Out>, mut out: Out) {
    let deadline = Instant::now() + CLIP_QUEUE_TIMEOUT;
    loop {
        match tx.try_send(out) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => return,
            Err(TrySendError::Full(returned)) => {
                out = returned;
                if Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
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
            cursor_local: AtomicBool::new(false),
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

    /// Tell clients whether the pointer is part of the video. GNOME's portal
    /// cannot embed it, so those clients must keep drawing their own.
    pub fn set_cursor_local(&self, on: bool) {
        self.cursor_local.store(on, Ordering::Relaxed);
        let clients = self.clients.lock().unwrap();
        for c in clients.iter() {
            let _ = c
                .secure
                .send_msg(Msg::new(msg::CURSOR_LOCAL, 0, on as i32, 0), &[]);
        }
    }

    pub fn cursor_local(&self) -> bool {
        self.cursor_local.load(Ordering::Relaxed)
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

    pub fn broadcast_video(&self, chunk: Vec<u8>, ts_us: u64) {
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
            match c.tx.try_send(Out::Video(chunk.clone(), ts_us)) {
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
        // Send outside the clients lock: a clipboard update may wait for a
        // video burst, but the capture thread must never wait for it.
        let senders: Vec<SyncSender<Out>> = self
            .clients
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.tx.clone())
            .collect();
        for tx in senders {
            match tx.try_send(Out::Clip(text.to_string())) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => {}
                Err(TrySendError::Full(out)) => {
                    send_with_retry(&tx, out);
                }
            }
        }
    }

    pub fn broadcast_rich(&self, html: &str, text: &str) {
        let payload = lgx_proto::encode_rich(html, text);
        let senders: Vec<SyncSender<Out>> = self
            .clients
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.tx.clone())
            .collect();
        for tx in senders {
            match tx.try_send(Out::ClipHtml(payload.clone())) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => {}
                Err(TrySendError::Full(out)) => {
                    send_with_retry(&tx, out);
                }
            }
        }
    }

    /// Tell every connected client that the capture geometry changed; the
    /// client asks the user before resizing its window.
    pub fn broadcast_resize(&self, w: u32, h: u32, fps: u32) {
        let senders: Vec<SyncSender<Out>> = self
            .clients
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.tx.clone())
            .collect();
        for tx in senders {
            match tx.try_send(Out::Resize(w, h, fps)) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => {}
                Err(TrySendError::Full(out)) => {
                    send_with_retry(&tx, out);
                }
            }
        }
    }

    /// A file clipboard is a short burst; send it directly on each client's
    /// secure stream, one sealed message at a time, so video can interleave.
    /// The clients lock is released first: holding it for a 16 MiB transfer
    /// would stop the capture thread from enqueueing frames.
    pub fn broadcast_file(&self, name: &str, data: &[u8]) {
        if data.len() as u64 > file::MAX_BYTES {
            return;
        }
        let begin = file::encode_begin(name, data.len() as u64);
        let streams: Vec<Arc<Secure>> = {
            let list = self.clients.lock().unwrap();
            eprintln!("lgx-server: broadcasting file {} to {} client(s)", name, list.len());
            list.iter().map(|c| c.secure.clone()).collect()
        };
        for secure in streams {
            if secure
                .send_msg(Msg::new(msg::FILE_BEGIN, 0, begin.len() as i32, 0), &begin)
                .is_err()
            {
                continue;
            }
            for chunk in data.chunks(32 * 1024) {
                if secure
                    .send_msg(Msg::new(msg::FILE_DATA, 0, chunk.len() as i32, 0), chunk)
                    .is_err()
                {
                    break;
                }
            }
            let _ = secure.send_msg(Msg::new(msg::FILE_END, 0, 0, 0), &[]);
        }
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
        secure::set_low_latency(&stream);
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
        if hub.cursor_local() {
            secure.send_msg(Msg::new(msg::CURSOR_LOCAL, 0, 1, 0), &[])?;
        }
    }

    let (tx, rx) = std::sync::mpsc::sync_channel::<Out>(2);
    let mut incoming_file: Option<(String, u64, Vec<u8>)> = None;
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
                    Out::Resize(w, h, fps) => w2
                        .send_msg(
                            Msg::new(msg::RESIZE, fps as u16, w as i32, h as i32),
                            &[],
                        )
                        .is_ok(),
                    Out::Video(chunk, ts_us) => {
                        // 48-bit capture timestamp: high 16 bits in `code`,
                        // low 32 in `b` (the client aligns audio with it).
                        w2.send_msg(
                            Msg::new(
                                msg::VIDEO,
                                (ts_us >> 32) as u16,
                                chunk.len() as i32,
                                ts_us as i32,
                            ),
                            &chunk,
                        )
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

    while let Ok((m, payload)) = secure.recv_msg() {
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
            msg::FILE_BEGIN => {
                incoming_file = match file::decode_begin(&payload) {
                    Some((name, size)) if size <= file::MAX_BYTES => {
                        Some((name, size, Vec::new()))
                    }
                    _ => None,
                };
            }
            msg::FILE_DATA => {
                let Some((_, size, data)) = incoming_file.as_mut() else {
                    continue;
                };
                if data.len() as u64 + payload.len() as u64 > *size {
                    incoming_file = None;
                    continue;
                }
                data.extend_from_slice(&payload);
            }
            msg::FILE_END => {
                let Some((name, size, data)) = incoming_file.take() else {
                    continue;
                };
                if data.len() as u64 != size {
                    continue;
                }
                *last_remote.lock().unwrap() = clip::signature(&clip::Clip::File {
                    name: name.clone(),
                    data: data.clone(),
                });
                if let Err(e) = clip::set_file(&name, &data) {
                    eprintln!("lgx-server: file clipboard: {e}");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    /// A connected secure pair whose server side never reads, so a large
    /// broadcast blocks in the socket.
    fn secure_pair(token: &'static str) -> (Arc<Secure>, Secure) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let id = secure::server_read_id(&mut stream).unwrap();
            assert_eq!(id, "test");
            secure::server_handshake(stream, token).unwrap()
        });
        let stream = TcpStream::connect(addr).unwrap();
        let client = secure::client_handshake_id(stream, "test", token).unwrap();
        let server = handle.join().unwrap();
        (Arc::new(client), server)
    }

    fn hub_with_client(secure: Arc<Secure>) -> Hub {
        let (tx, _rx) = sync_channel(1);
        Hub {
            clients: Mutex::new(vec![Client {
                id: 1,
                client: "test".into(),
                token: [0; 32],
                tx,
                needs_idr: false,
                secure,
            }]),
            next_id: AtomicU64::new(2),
            geometry: Mutex::new((1920, 1080)),
            keyframe: AtomicBool::new(false),
            last_keyframe_ms: AtomicU64::new(0),
            cursor_overlay: AtomicBool::new(false),
            cursor_local: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        }
    }

    #[test]
    fn broadcast_file_releases_the_clients_lock() {
        let (client, server) = secure_pair("file-lock-test");
        let hub = hub_with_client(client);
        let data = vec![0u8; file::MAX_BYTES as usize];
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                hub.broadcast_file("big.bin", &data);
                done.store(true, Ordering::Relaxed);
            });
            // Let the writer fill the socket and block in send_msg.
            std::thread::sleep(Duration::from_millis(300));
            assert!(
                !done.load(Ordering::Relaxed),
                "the test needs a blocked file transfer"
            );
            assert!(
                hub.clients.try_lock().is_ok(),
                "the clients lock is held during the file transfer"
            );
            // Closing the unread peer makes the blocked write fail.
            drop(server);
        });
    }
}
