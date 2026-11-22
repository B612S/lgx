//! TCP transport: one connection per client carries HEVC video, input and
//! clipboard.
//!
//! Every client has a single writer thread; video frames, HELLO and clipboard
//! data are serialised there. Encoded frames are never copied to the CPU: the
//! writer takes ownership of the encoder's buffer.

use std::collections::HashMap;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lgx_proto::secure::{self, Secure};
use lgx_proto::{codec, file, msg, Msg};

use crate::clients;
use std::net::{SocketAddr, UdpSocket};

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

/// One client's UDP video socket. The client's address is learned from the
/// hello it sends after the server advertises the port; until then (and if it
/// never arrives) video stays on TCP.
pub(crate) struct UdpLink {
    sock: UdpSocket,
    tx: lgx_proto::udp::Tx,
    key: [u8; lgx_proto::udp::KEY_LEN],
    peer: Mutex<Option<SocketAddr>>,
    next_frame: AtomicU32,
    /// Recently sent access units, so a NACK can be answered without the
    /// encoder: datagrams are stored sealed and resent byte for byte.
    recent: Mutex<std::collections::VecDeque<SentFrame>>,
}

struct SentFrame {
    frame_id: u32,
    datagrams: Vec<Vec<u8>>,
    bytes: usize,
}

/// Cache bound: a few hundred milliseconds of video. Staying small keeps the
/// retransmission useful (a frame the client still needs) without pinning
/// megabytes of capture output.
const RECENT_FRAMES: usize = 64;
const RECENT_BYTES: usize = 4 << 20;

impl UdpLink {
    fn start(id: u64, port: u16) -> std::io::Result<Arc<Self>> {
        // Same number as the TCP control port: UDP and TCP do not collide, and
        // a firewall rule for the video port then covers both transports.
        let sock = UdpSocket::bind(("0.0.0.0", port))?;
        sock.set_read_timeout(Some(Duration::from_secs(2)))?;
        // One access unit can be several hundred kilobytes of datagrams; the
        // default send buffer drops the tail of a keyframe burst.
        unsafe {
            use std::os::fd::AsRawFd;
            let bytes: libc::c_int = 8 << 20;
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &bytes as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        let key = lgx_proto::udp::random_key()?;
        let link = Arc::new(Self {
            sock,
            tx: lgx_proto::udp::Tx::new(&key)?,
            key,
            peer: Mutex::new(None),
            next_frame: AtomicU32::new(1),
            recent: Mutex::new(std::collections::VecDeque::new()),
        });
        let l = link.clone();
        std::thread::Builder::new()
            .name(format!("lgx-udp-{id}"))
            .spawn(move || {
                let mut buf = [0u8; 32];
                // The read timeout ends the thread when the client ignores the
                // negotiation; TCP video keeps working in that case.
                while let Ok((n, addr)) = l.sock.recv_from(&mut buf) {
                    if n >= 4 && buf[..4] == *b"LGXU" {
                        *l.peer.lock().unwrap() = Some(addr);
                        eprintln!("lgx-server: video over UDP to {addr}");
                        return;
                    }
                }
            })
            .ok();
        Ok(link)
    }

    fn port(&self) -> u16 {
        self.sock.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    fn ready(&self) -> bool {
        self.peer.lock().unwrap().is_some()
    }

    /// Fragment and seal one access unit. False when the frame could not be
    /// sent (no hello yet, or the socket queue is full); the caller then falls
    /// back to TCP so the picture does not freeze.
    fn send(&self, chunk: &[u8], ts_us: u64, keyframe: bool) -> bool {
        let peer = match *self.peer.lock().unwrap() {
            Some(peer) => peer,
            None => return false,
        };
        let count = chunk.len().div_ceil(lgx_proto::udp::MAX_PAYLOAD);
        if count == 0 || count > lgx_proto::udp::MAX_FRAGMENTS {
            return false;
        }
        let frame_id = self.next_frame.fetch_add(1, Ordering::Relaxed);
        let mut datagram = Vec::with_capacity(lgx_proto::udp::MAX_DATAGRAM);
        let mut sent = Vec::with_capacity(count);
        for (i, part) in chunk.chunks(lgx_proto::udp::MAX_PAYLOAD).enumerate() {
            let header = lgx_proto::udp::Header {
                flags: if keyframe { lgx_proto::udp::FLAG_KEYFRAME } else { 0 },
                frame_id,
                frag_idx: i as u16,
                frag_count: count as u16,
                ts_us,
            };
            if !self.tx.seal(&header, part, &mut datagram) {
                return false;
            }
            if self.sock.send_to(&datagram, peer).is_err() {
                return false;
            }
            sent.push(datagram.clone());
        }
        {
            let bytes: usize = sent.iter().map(|d| d.len()).sum();
            let mut recent = self.recent.lock().unwrap();
            recent.push_back(SentFrame {
                frame_id,
                datagrams: sent,
                bytes,
            });
            while recent.len() > RECENT_FRAMES
                || recent.iter().map(|f| f.bytes).sum::<usize>() > RECENT_BYTES
            {
                recent.pop_front();
            }
        }
        true
    }

    /// Resend the fragments the client asked for. Returns how many datagrams
    /// actually went out; a frame older than the cache is simply gone and the
    /// client falls back to its IDR request.
    fn retransmit(&self, frame_id: u32, indices: &[u16]) -> usize {
        let peer = match *self.peer.lock().unwrap() {
            Some(peer) => peer,
            None => return 0,
        };
        let recent = self.recent.lock().unwrap();
        let Some(frame) = recent.iter().find(|f| f.frame_id == frame_id) else {
            return 0;
        };
        let mut sent = 0;
        for &idx in indices {
            if let Some(datagram) = frame.datagrams.get(idx as usize) {
                if self.sock.send_to(datagram, peer).is_ok() {
                    sent += 1;
                }
            }
        }
        sent
    }
}

/// Bind a UDP socket for one client and tell it the port and key. Video keeps
/// flowing over TCP until the client's hello datagram arrives.
fn udp_offer(secure: &Secure, id: u64, port: u16) -> Option<Arc<UdpLink>> {
    let link = match UdpLink::start(id, port) {
        Ok(link) => link,
        Err(e) => {
            eprintln!("lgx-server: client #{id}: udp: {e}");
            return None;
        }
    };
    let key = link.key;
    match secure.send_msg(
        Msg::new(msg::VIDEO_UDP, link.port(), lgx_proto::udp::KEY_LEN as i32, 0),
        &key,
    ) {
        Ok(()) => {
            eprintln!(
                "lgx-server: client #{id}: offering video over UDP port {}",
                link.port()
            );
            Some(link)
        }
        Err(e) => {
            eprintln!("lgx-server: client #{id}: udp offer: {e}");
            None
        }
    }
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
    /// `video.udp`: send HEVC access units as datagrams instead of on the TCP
    /// control stream.
    video_udp: AtomicBool,
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
    /// Set once UDP video was negotiated; `None` keeps the TCP path.
    udp: Option<Arc<UdpLink>>,
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
            video_udp: AtomicBool::new(false),
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

    /// Switch one client's video to UDP. The next access unit is forced to be
    /// an IDR so the datagram stream starts from a clean reference.
    /// Answer a client's NACK from the datagram cache. Called from the
    /// control reader thread, never from the capture loop.
    pub(crate) fn retransmit_udp(&self, id: u64, frame_id: u32, indices: &[u16]) -> usize {
        let link = {
            let list = self.clients.lock().unwrap();
            list.iter()
                .find(|c| c.id == id)
                .and_then(|c| c.udp.clone())
        };
        link.map(|l| l.retransmit(frame_id, indices)).unwrap_or(0)
    }

    pub(crate) fn attach_udp(&self, id: u64, link: Arc<UdpLink>) {
        {
            let mut list = self.clients.lock().unwrap();
            if let Some(c) = list.iter_mut().find(|c| c.id == id) {
                c.udp = Some(link);
                c.needs_idr = true;
            }
        }
        self.request_keyframe();
    }

    pub fn set_video_udp(&self, on: bool) {
        self.video_udp.store(on, Ordering::Relaxed);
    }

    pub fn video_udp(&self) -> bool {
        self.video_udp.load(Ordering::Relaxed)
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
        let is_key = crate::net::is_idr(&chunk);
        let idr = list.iter().any(|c| c.needs_idr) && is_key;
        if is_key && std::env::var_os("LGX_DEBUG_IDR").is_some() {
            eprintln!("lgx-server: access unit is an IDR ({} bytes)", chunk.len());
        }
        if idr && std::env::var_os("LGX_DEBUG_IDR").is_some() {
            eprintln!("lgx-server: sending an IDR ({} bytes)", chunk.len());
        }
        let mut dropped = false;
        list.retain_mut(|c| {
            if c.needs_idr && !idr {
                return true;
            }
            if let Some(link) = c.udp.as_ref() {
                if link.ready() {
                    // The datagram header carries the real keyframe flag, not
                    // just "somebody asked for one": the client uses it to
                    // resume after losing a frame.
                    if link.send(&chunk, ts_us, is_key) {
                        c.needs_idr = false;
                        return true;
                    }
                    // Fall through to TCP: a datagram burst the kernel refused
                    // must not freeze the picture.
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    dropped = true;
                }
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
    // The TCP local port doubles as the UDP video port, so one firewall rule
    // covers both transports.
    let udp_port = stream.local_addr().map(|a| a.port()).unwrap_or_default();
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
        udp: None,
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

    loop {
        let (m, payload) = match secure.recv_msg() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("lgx-server: client #{id}: recv: {e}");
                break;
            }
        };
        match m.typ {
            msg::IDR => {
                // The client lost frames (or is behind) and needs a fresh
                // reference; the capture loop picks this up before the next
                // access unit.
                if std::env::var_os("LGX_DEBUG_IDR").is_some() {
                    eprintln!("lgx-server: client #{id} asked for an IDR");
                }
                hub.request_keyframe();
            }
            msg::NACK => {
                // UDP video lost fragments: resend them from the cache.
                if payload.len() >= 6 {
                    let frame_id = u32::from_le_bytes(payload[0..4].try_into().unwrap());
                    let count = u16::from_le_bytes([payload[4], payload[5]]) as usize;
                    let mut indices = Vec::with_capacity(count.min(lgx_proto::udp::MAX_FRAGMENTS));
                    for i in 0..count {
                        let off = 6 + i * 2;
                        if off + 2 > payload.len() {
                            break;
                        }
                        indices.push(u16::from_le_bytes([payload[off], payload[off + 1]]));
                    }
                    let sent = hub.retransmit_udp(id, frame_id, &indices);
                    if std::env::var_os("LGX_DEBUG_NACK").is_some() {
                        eprintln!(
                            "lgx-server: NACK frame {frame_id}: asked {}, resent {sent}",
                            indices.len()
                        );
                    }
                }
            }
            msg::UDP_VIDEO => {
                // The client opted in. Only now does the server bind a UDP
                // socket, so an old client that never asks keeps TCP video.
                if hub.video_udp() {
                    match udp_offer(&secure, id, udp_port) {
                        Some(link) => {
                            hub.attach_udp(id, link);
                        }
                        None => continue,
                    }
                }
            }
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
            msg::PING => {
                // Echo the server clock; the client uses it to turn the
                // capture timestamps into an end-to-end latency.
                let now = crate::time::monotonic_us();
                let _ = secure.send_msg(Msg::new(msg::PONG, 0, 8, 0), &now.to_le_bytes());
            }
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
pub(crate) fn is_idr(data: &[u8]) -> bool {
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
                udp: None,
                secure,
            }]),
            next_id: AtomicU64::new(2),
            geometry: Mutex::new((1920, 1080)),
            keyframe: AtomicBool::new(false),
            last_keyframe_ms: AtomicU64::new(0),
            cursor_overlay: AtomicBool::new(false),
            cursor_local: AtomicBool::new(false),
            video_udp: AtomicBool::new(false),
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
