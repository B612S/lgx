//! Audio playback over the native PipeWire stream API.
//!
//! PCM received from the server is written into a ring buffer; the PipeWire
//! process callback drains it into the playback stream. No `pacat` child and
//! no PulseAudio compatibility layer are involved, so there is nothing that
//! can outlive the client and the mixer sees the stream's real properties.
//!
//! Timestamp-aware servers prefix every record with `LGXA`, the server
//! timestamp of the first sample and the PCM length. The playback callback
//! keeps that timeline at its head; the video presenter uses it to hold a
//! frame until audio reaches the same instant. Raw PCM from an older server is
//! still accepted and simply plays without A/V sync.
//!
//! With `mute_unfocused = true`, focus loss still drains the network stream
//! but discards it; the playback stream keeps running with silence, and any
//! data already queued is dropped by the callback instead of being replayed
//! on focus regain.

use std::collections::VecDeque;
use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lgx_proto::{msg, secure, Msg};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::pod::Pod;
use ringbuf::traits::{Consumer as _, Observer as _, Producer as _, Split};
use ringbuf::HeapRb;

const RING_BYTES: usize = 64 * 1024;
/// Playout cushion: three server chunks at 48 kHz stereo s16le (~64 ms).
/// Two chunks left the stream with almost no margin for scheduler/TCP jitter.
const TARGET_BYTES: usize = 12 * 1024;
/// Only trim a runaway backlog (clock drift), never the normal cushion.
const HIGH_WATER: usize = 32 * 1024;
const LOW_WATER: usize = 16 * 1024;
/// Stereo s16le at 48 kHz: one second of PCM.
const BYTES_PER_SEC: u64 = 48_000 * 4;
/// Audio records from a timestamp-aware server start with this magic. Old
/// servers send raw PCM and are played without A/V sync.
const AUDIO_MAGIC: &[u8; 4] = b"LGXA";

/// Shared A/V clock. `head_us` is the server capture/write timestamp of the
/// next sample the playback callback will consume; the video presenter holds a
/// frame until the audio reaches the same point on the server timeline.
pub struct Sync {
    pub head_us: AtomicU64,
    pub active: AtomicBool,
    pub paused: AtomicBool,
}

impl Sync {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            head_us: AtomicU64::new(0),
            active: AtomicBool::new(false),
            paused: AtomicBool::new(false),
        })
    }
}

#[derive(Clone, Copy)]
struct ChunkInfo {
    /// Absolute producer byte offset of this chunk in the PCM stream.
    start: u64,
    len: u32,
    /// Server timestamp of the chunk's first sample, in microseconds.
    ts_us: u64,
}

#[derive(Default)]
struct Timeline {
    infos: VecDeque<ChunkInfo>,
    produced: u64,
    consumed: u64,
}

impl Timeline {
    fn push(&mut self, ts_us: u64, len: usize) {
        if ts_us == 0 || len == 0 {
            return;
        }
        self.infos.push_back(ChunkInfo {
            start: self.produced,
            len: len as u32,
            ts_us,
        });
        self.produced += len as u64;
        // A stalled consumer must not grow an unbounded metadata queue.
        while self.infos.len() > 1024 {
            self.infos.pop_front();
        }
    }

    fn refresh(&mut self, sync: &Sync) {
        while let Some(front) = self.infos.front() {
            if self.consumed >= front.start + front.len as u64 {
                self.infos.pop_front();
            } else {
                break;
            }
        }
        if let Some(front) = self.infos.front() {
            let delta = self.consumed.saturating_sub(front.start);
            let head = front.ts_us + delta * 1_000_000 / BYTES_PER_SEC;
            sync.head_us.store(head, Ordering::Relaxed);
            sync.active.store(true, Ordering::Relaxed);
        } else {
            sync.active.store(false, Ordering::Relaxed);
        }
    }
}

struct PlaybackState {
    cons: ringbuf::HeapCons<u8>,
    /// Set while the window is unfocused and `mute_unfocused` is active.
    paused: Arc<AtomicBool>,
    /// Callbacks that ran out of queued audio and had to emit silence.
    underruns: Arc<AtomicU64>,
    /// Wait for one quantum before starting, instead of playing half a
    /// buffer followed by silence.
    prebuffer: bool,
    timeline: Arc<Mutex<Timeline>>,
    sync: Arc<Sync>,
    /// Consumed bytes not yet accounted for while the producer holds the
    /// timeline lock.
    pending_consumed: u64,
}

impl PlaybackState {
    fn advance(&mut self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.pending_consumed += bytes as u64;
        if let Ok(mut timeline) = self.timeline.try_lock() {
            timeline.consumed += self.pending_consumed;
            self.pending_consumed = 0;
            timeline.refresh(&self.sync);
        }
    }
}

fn pipewire_thread(
    state: PlaybackState,
    quit: pw::channel::Receiver<()>,
    ready: std::sync::mpsc::SyncSender<()>,
    app_name: &str,
    media_name: &str,
) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
    // The mixer labels the application from the PipeWire client, not only
    // from the stream node; set the client properties as well.
    let client_props = properties! {
        *pw::keys::APP_NAME => app_name,
        *pw::keys::APP_PROCESS_BINARY => "lgx-client",
        *pw::keys::APP_ICON_NAME => "lgx",
    };
    let core = context
        .connect_rc(Some(client_props))
        .map_err(|e| format!("core: {e}"))?;

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
    };
    props.insert(*pw::keys::APP_NAME, app_name);
    props.insert(*pw::keys::MEDIA_NAME, media_name);
    props.insert(*pw::keys::APP_PROCESS_BINARY, "lgx-client");
    props.insert(*pw::keys::APP_ICON_NAME, "lgx");
    // Some mixers label native streams by their node, not by
    // application.name; keep both in sync with the window title.
    props.insert(*pw::keys::NODE_NAME, app_name);
    props.insert(*pw::keys::NODE_DESCRIPTION, app_name);
    // Do not let WirePlumber suspend the stream while it is silent: the
    // first audio after a pause would otherwise be delayed or clipped.
    props.insert(*pw::keys::NODE_ALWAYS_PROCESS, "true");
    props.insert(*pw::keys::NODE_PAUSE_ON_IDLE, "false");
    props.insert("node.suspend-on-idle", "false");
    // Keep the graph latency low; the actual quantum still adapts to the
    // other clients in the graph, and the ring target follows the buffer.
    props.insert(*pw::keys::NODE_LATENCY, "1024/48000");

    let stream = pw::stream::StreamBox::new(&core, "lgx audio", props).map_err(|e| format!("stream: {e}"))?;
    let mut ready = Some(ready);
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(move |_, _, _, new| {
            if new == pw::stream::StreamState::Streaming {
                if let Some(tx) = ready.take() {
                    let _ = tx.send(());
                }
            }
        })
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            // `requested()` is the number of frames the graph wants this
            // cycle. This is the real quantum; the mapped buffer can be much
            // larger, and `chunk.size()` is not a reliable requested length.
            let frames = buffer.requested() as usize;
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let Some(slice) = data.data() else {
                return;
            };
            let requested = if frames > 0 {
                frames.saturating_mul(4)
            } else {
                TARGET_BYTES
            };
            let size = requested.min(slice.len()) & !3;
            let slice = &mut slice[..size];
            let target = TARGET_BYTES.max(size).min(RING_BYTES / 2);
            if state.paused.load(Ordering::Relaxed) {
                // Drop whatever was queued before the window lost focus.
                let mut scratch = [0u8; 8192];
                let mut drained = 0usize;
                loop {
                    let n = state.cons.pop_slice(&mut scratch);
                    if n == 0 {
                        break;
                    }
                    drained += n;
                }
                state.advance(drained);
                state.prebuffer = true;
                slice[..size].fill(0);
            } else if state.prebuffer {
                if state.cons.occupied_len() < target {
                    slice[..size].fill(0);
                } else {
                    state.prebuffer = false;
                    let n = state.cons.pop_slice(&mut slice[..size]);
                    if n < size {
                        slice[n..size].fill(0);
                    }
                    state.advance(n);
                }
            } else {
                // Keep the normal cushion. Only a runaway backlog (a very
                // long stall or clock drift) is trimmed, and then only down
                // to LOW_WATER, not on every callback: trimming to a single
                // quantum discarded most of each server chunk and produced
                // constant underrun/stutter.
                if state.cons.occupied_len() > HIGH_WATER {
                    let mut scratch = [0u8; 8192];
                    let mut dropped = 0usize;
                    while state.cons.occupied_len() > LOW_WATER {
                        let n = state.cons.pop_slice(&mut scratch);
                        if n == 0 {
                            break;
                        }
                        dropped += n;
                    }
                    state.advance(dropped);
                }
                let n = state.cons.pop_slice(&mut slice[..size]);
                if n < size {
                    slice[n..size].fill(0);
                    state.underruns.fetch_add(1, Ordering::Relaxed);
                    // Rebuild the cushion instead of continuing to run on
                    // empty; otherwise one late chunk becomes a run of tiny
                    // gaps and clipped syllables.
                    state.prebuffer = true;
                }
                state.advance(n);
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = 4;
            *chunk.size_mut() = size as u32;
        })
        .register()
        .map_err(|e| format!("listener: {e}"))?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(48000);
    audio_info.set_channels(2);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    audio_info.set_position(position);

    let object = spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Format,
        id: spa::sys::SPA_PARAM_EnumFormat,
        properties: audio_info.into(),
    };
    let values = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(|e| format!("format pod: {e}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or("invalid format pod")?];
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| format!("connect: {e}"))?;

    let _quit = quit.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    mainloop.run();
    Ok(())
}

/// Producer side of the playback ring plus the handles the network loop needs.
type Playback = (
    ringbuf::HeapProd<u8>,
    pw::channel::Sender<()>,
    std::thread::JoinHandle<()>,
    Arc<AtomicBool>,
    Arc<AtomicU64>,
    std::sync::mpsc::Receiver<()>,
    Arc<Mutex<Timeline>>,
);

fn spawn_playback(app_name: &str, media_name: &str, sync: Arc<Sync>) -> io::Result<Playback> {
    let ring = HeapRb::<u8>::new(RING_BYTES);
    let (prod, cons) = ring.split();
    let paused = Arc::new(AtomicBool::new(false));
    let underruns = Arc::new(AtomicU64::new(0));
    let timeline = Arc::new(Mutex::new(Timeline::default()));
    let state = PlaybackState {
        cons,
        paused: paused.clone(),
        underruns: underruns.clone(),
        prebuffer: true,
        timeline: timeline.clone(),
        sync,
        pending_consumed: 0,
    };
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let app_name = app_name.to_string();
    let media_name = media_name.to_string();
    let handle = std::thread::Builder::new()
        .name("lgx-pw-playback".into())
        .spawn(move || {
            if let Err(e) = pipewire_thread(state, quit_rx, ready_tx, &app_name, &media_name) {
                eprintln!("lgx-client: PipeWire playback: {e}");
            }
        })?;
    Ok((prod, quit_tx, handle, paused, underruns, ready_rx, timeline))
}

pub fn run(
    addr: &str,
    client_id: &str,
    token: &str,
    mute_unfocused: Arc<AtomicBool>,
    focus: Arc<AtomicBool>,
    sync: Arc<Sync>,
) -> std::io::Result<()> {
    let app_name = format!("lgx - {client_id}");
    let media_name = format!("lgx audio ({client_id})");
    let (mut audio_tx, quit, handle, paused, underruns, ready, timeline) =
        spawn_playback(&app_name, &media_name, sync.clone())?;
    // Connect the audio socket only after the playback stream is streaming.
    // Otherwise the server can send hundreds of milliseconds of audio while
    // the stream is still linking, and that backlog becomes permanent
    // latency in the ring buffer.
    let _ = ready.recv_timeout(Duration::from_secs(2));

    let result = (|| -> std::io::Result<()> {
        let stream = TcpStream::connect(addr)?;
        // Audio is many small records; Nagle would batch them and add tens
        // of milliseconds of jitter, which is audible as clipped syllables.
        let _ = stream.set_nodelay(true);
        let secure = secure::client_handshake_id(stream, client_id, token)?;
        secure.send_bytes(token.as_bytes())?;
        // Opt in to the timestamped LGXA records. An old server ignores this
        // frame (it is client -> server, so it cannot corrupt playback) and
        // keeps sending raw PCM, which the parser below still accepts.
        secure.send_msg(Msg::new(msg::AUDIO_SYNC, 0, 0, 0), &[])?;

        let mut total = 0u64;
        let mut last = Instant::now();
        let mut logged_sync = false;
        loop {
            let chunk = match secure.recv_bytes() {
                Ok(chunk) => chunk,
                Err(e) => break Err(e),
            };
            if chunk.is_empty() {
                break Ok(());
            }
            let mute = mute_unfocused.load(Ordering::Relaxed) && !focus.load(Ordering::Relaxed);
            paused.store(mute, Ordering::Relaxed);
            sync.paused.store(mute, Ordering::Relaxed);
            if mute {
                // Keep draining the secure stream instead of backing it up.
                continue;
            }
            // A timestamp-aware server prefixes every record with LGXA, the
            // u64 LE timestamp in microseconds and the u32 LE PCM length. A
            // plain PCM record from an old server is played without sync.
            let (ts_us, pcm) = if chunk.len() >= 16 && &chunk[..4] == AUDIO_MAGIC {
                let ts = u64::from_le_bytes(chunk[4..12].try_into().unwrap());
                let len = u32::from_le_bytes(chunk[12..16].try_into().unwrap()) as usize;
                if len > 0 && len.is_multiple_of(4) && 16 + len == chunk.len() {
                    (ts, &chunk[16..])
                } else {
                    (0, &chunk[..])
                }
            } else {
                (0, &chunk[..])
            };
            let pushed = audio_tx.push_slice(pcm);
            if pushed > 0 && ts_us != 0 {
                if !logged_sync {
                    logged_sync = true;
                    eprintln!("lgx-client: audio A/V timestamps enabled");
                }
                if let Ok(mut timeline) = timeline.lock() {
                    timeline.push(ts_us, pushed);
                }
            }
            total += pcm.len() as u64;
            if last.elapsed() >= Duration::from_secs(5) {
                // Underruns are the audible kind of quality problem; surface
                // them instead of hiding the gaps as silence.
                let missed = underruns.swap(0, Ordering::Relaxed);
                eprintln!(
                    "lgx-client: audio {:.1} KiB/s, {missed} underrun(s)",
                    total as f64 / 1024.0 / last.elapsed().as_secs_f64()
                );
                total = 0;
                last = Instant::now();
            }
        }
    })();

    let _ = quit.send(());
    let _ = handle.join();
    result
}
