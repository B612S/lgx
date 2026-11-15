//! Audio playback over the native PipeWire stream API.
//!
//! PCM received from the server is written into a ring buffer; the PipeWire
//! process callback drains it into the playback stream. No `pacat` child and
//! no PulseAudio compatibility layer are involved, so there is nothing that
//! can outlive the client and the mixer sees the stream's real properties.
//!
//! With `mute_unfocused = true`, focus loss still drains the network stream
//! but discards it; the playback stream keeps running with silence, and any
//! data already queued is dropped by the callback instead of being replayed
//! on focus regain.

use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lgx_proto::secure;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::pod::Pod;
use ringbuf::traits::{Consumer as _, Observer as _, Producer as _, Split};
use ringbuf::HeapRb;

const RING_BYTES: usize = 64 * 1024;
/// Playout cushion: two server chunks at 48 kHz stereo s16le.
const TARGET_BYTES: usize = 8192;
/// Only trim a runaway backlog (clock drift), never the normal cushion.
const HIGH_WATER: usize = 48 * 1024;
const LOW_WATER: usize = 16 * 1024;

struct PlaybackState {
    cons: ringbuf::HeapCons<u8>,
    /// Set while the window is unfocused and `mute_unfocused` is active.
    paused: Arc<AtomicBool>,
    /// Callbacks that ran out of queued audio and had to emit silence.
    underruns: Arc<AtomicU64>,
    /// Wait for one quantum before starting, instead of playing half a
    /// buffer followed by silence.
    prebuffer: bool,
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
                while state.cons.pop_slice(&mut scratch) > 0 {}
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
                }
            } else {
                // Keep the normal cushion. Only a runaway backlog (a very
                // long stall or clock drift) is trimmed, and then only down
                // to LOW_WATER, not on every callback: trimming to a single
                // quantum discarded most of each server chunk and produced
                // constant underrun/stutter.
                if state.cons.occupied_len() > HIGH_WATER {
                    let mut scratch = [0u8; 8192];
                    while state.cons.occupied_len() > LOW_WATER {
                        if state.cons.pop_slice(&mut scratch) == 0 {
                            break;
                        }
                    }
                }
                let n = state.cons.pop_slice(&mut slice[..size]);
                if n < size {
                    slice[n..size].fill(0);
                    state.underruns.fetch_add(1, Ordering::Relaxed);
                }
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

fn spawn_playback(
    app_name: &str,
    media_name: &str,
) -> io::Result<(
    ringbuf::HeapProd<u8>,
    pw::channel::Sender<()>,
    std::thread::JoinHandle<()>,
    Arc<AtomicBool>,
    Arc<AtomicU64>,
    std::sync::mpsc::Receiver<()>,
)> {
    let ring = HeapRb::<u8>::new(RING_BYTES);
    let (prod, cons) = ring.split();
    let paused = Arc::new(AtomicBool::new(false));
    let underruns = Arc::new(AtomicU64::new(0));
    let state = PlaybackState {
        cons,
        paused: paused.clone(),
        underruns: underruns.clone(),
        prebuffer: true,
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
    Ok((prod, quit_tx, handle, paused, underruns, ready_rx))
}

pub fn run(
    addr: &str,
    client_id: &str,
    token: &str,
    mute_unfocused: Arc<AtomicBool>,
    focus: Arc<AtomicBool>,
) -> std::io::Result<()> {
    let app_name = format!("lgx - {client_id}");
    let media_name = format!("lgx audio ({client_id})");
    let (mut audio_tx, quit, handle, paused, underruns, ready) =
        spawn_playback(&app_name, &media_name)?;
    // Connect the audio socket only after the playback stream is streaming.
    // Otherwise the server can send hundreds of milliseconds of audio while
    // the stream is still linking, and that backlog becomes permanent
    // latency in the ring buffer.
    let _ = ready.recv_timeout(Duration::from_secs(2));

    let result = (|| -> std::io::Result<()> {
        let stream = TcpStream::connect(addr)?;
        let secure = secure::client_handshake_id(stream, client_id, token)?;
        secure.send_bytes(token.as_bytes())?;

        let mut total = 0u64;
        let mut last = Instant::now();
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
            if mute {
                // Keep draining the secure stream instead of backing it up.
                continue;
            }
            let _ = audio_tx.push_slice(&chunk);
            total += chunk.len() as u64;
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
