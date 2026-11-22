//! Audio capture and fan-out over the native PipeWire stream API.
//!
//! The guest opens a capture stream on the default sink monitor
//! (`stream.capture.sink`), so desktop audio is captured without PulseAudio's
//! `parec` compatibility layer. The stream is float32 (the graph's native
//! format, so PipeWire does no conversion), and the server converts it to
//! s16le 48 kHz stereo with TPDF dither before sending chunks to every
//! authenticated client. Doing the conversion here keeps the single
//! quantization step dithered instead of relying on PipeWire's plain rounding.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lgx_proto::msg;
use lgx_proto::secure::{self, Secure};
use pipewire as pw;
use pipewire::permissions::PermissionFlags;
use pipewire::properties::properties;
use pipewire::registry::GlobalObject;
use pw::spa;
use pw::spa::pod::Pod;

use crate::clients;

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;

/// Capture state owned by the PipeWire process callback.
struct Capture {
    pcm: SyncSender<(u64, Vec<u8>)>,
    rng: u64,
    /// Chunks the fanout could not take because the client is behind.
    dropped: Arc<AtomicU64>,
    /// Linear gain from the default sink's volume; 0 while it is muted.
    /// PipeWire's monitor ports carry the sink's input, so the sink's own
    /// volume is not in the signal and has to be applied here.
    gain: Arc<AtomicU32>,
    /// Gain applied to the previous chunk, so a volume/mute change ramps over
    /// one chunk instead of stepping (an audible click).
    last_gain: f32,
}

fn next_uniform(rng: &mut u64) -> f32 {
    let mut x = *rng;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *rng = x;
    let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    ((v >> 40) as f32) / (1u32 << 24) as f32
}

/// Converts interleaved float32 samples to s16le with TPDF dither.
///
/// Dither of 1 LSB peak-to-peak (the difference of two uniform values) turns
/// the quantization error into noise instead of low-level distortion. The
/// conversion is the only place where precision is lost in the whole path.
pub fn f32_to_s16_dithered(
    input: &[u8],
    out: &mut Vec<u8>,
    rng: &mut u64,
    gain_from: f32,
    gain_to: f32,
) {
    out.clear();
    out.reserve(input.len() / 2);
    let samples = input.len() / 4;
    let step = if samples > 0 {
        (gain_to - gain_from) / samples as f32
    } else {
        0.0
    };
    let mut gain = gain_from;
    for sample in input.as_chunks::<4>().0 {
        let v = f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]);
        let d = next_uniform(rng) - next_uniform(rng);
        // Scale by 2^15 so -1.0 reaches -32768 exactly; +1.0 saturates.
        let s = (v * gain * 32768.0 + d)
            .round()
            .clamp(-32768.0, 32767.0) as i16;
        out.extend_from_slice(&s.to_le_bytes());
        gain += step;
    }
}

/// Linear gain derived from the default sink's volume and mute.
///
/// The capture stream is linked to the sink's monitor ports, which carry the
/// sink's **input**: changing the sink volume (volume keys, `wpctl`, a mixer)
/// happens after the monitor tap, so the captured signal would always be at
/// 100%. Reading the sink node's `SPA_PARAM_Props` and scaling the samples
/// here makes the client follow the guest's volume and mute.
struct GainTracker {
    gain: Arc<AtomicU32>,
    volume: f32,
    muted: bool,
}

impl GainTracker {
    fn new(gain: Arc<AtomicU32>) -> Self {
        Self {
            gain,
            volume: 1.0,
            muted: false,
        }
    }

    fn publish(&self) {
        let g = if self.muted {
            0.0
        } else {
            self.volume.clamp(0.0, 4.0)
        };
        self.gain.store(g.to_bits(), Ordering::Relaxed);
    }

    /// Applies one `SPA_PARAM_Props` update. A pod may carry only some of the
    /// volume/mute keys, so keep the previous values for the missing ones.
    fn apply_props(&mut self, pod: &Pod) {
        let Ok((_, value)) = pw::spa::pod::deserialize::PodDeserializer::deserialize_from::<
            pw::spa::pod::Value,
        >(pod.as_bytes()) else {
            return;
        };
        let pw::spa::pod::Value::Object(object) = value else {
            return;
        };
        let mut volume = None;
        let mut channels = None;
        let mut mute = None;
        for prop in object.properties {
            if prop.key == spa::sys::SPA_PROP_volume {
                if let pw::spa::pod::Value::Float(v) = prop.value {
                    volume = Some(v);
                }
            } else if prop.key == spa::sys::SPA_PROP_channelVolumes {
                if let pw::spa::pod::Value::ValueArray(spa::pod::ValueArray::Float(v)) = prop.value {
                    if !v.is_empty() {
                        channels = Some(v);
                    }
                }
            } else if prop.key == spa::sys::SPA_PROP_mute
                || prop.key == spa::sys::SPA_PROP_softMute
            {
                if let pw::spa::pod::Value::Bool(b) = prop.value {
                    mute = Some(b);
                }
            }
        }
        if let Some(v) = volume {
            self.volume = v;
        } else if let Some(v) = channels {
            self.volume = v.iter().sum::<f32>() / v.len() as f32;
        }
        if let Some(m) = mute {
            self.muted = m;
        }
        self.publish();
    }
}

/// The default sink as reported by PipeWire's `default` metadata plus the
/// bound node proxy that reports its volume.
struct SinkWatch {
    sink_name: Option<String>,
    node_name: Option<String>,
    node_id: Option<u32>,
    /// `node.name` -> (global id, global version) for the sinks seen so far.
    nodes: HashMap<String, (u32, u32)>,
    metadata: Option<(pw::metadata::Metadata, pw::metadata::MetadataListener)>,
    node: Option<(pw::node::Node, pw::node::NodeListener)>,
}

/// Keeps the registry, the metadata listener and the bound sink node alive for
/// as long as the capture thread runs.
struct VolumeWatch {
    _registry: pw::registry::RegistryRc,
    _listener: pw::registry::Listener,
    _watch: Rc<RefCell<SinkWatch>>,
    _tracker: Rc<RefCell<GainTracker>>,
}

/// Extracts the sink name from the metadata value `{"name":"..."}`.
pub fn metadata_sink_name(value: &str) -> Option<String> {
    let rest = value.get(value.find("\"name\"")? + 6..)?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(rest.get(..rest.find('"')?)?.to_string())
}

fn bind_sink_node(
    registry: &pw::registry::RegistryRc,
    watch: &mut SinkWatch,
    tracker: &Rc<RefCell<GainTracker>>,
) {
    let Some(name) = watch.sink_name.clone() else {
        return;
    };
    if watch.node_name.as_deref() == Some(name.as_str()) {
        return;
    }
    let Some(&(id, version)) = watch.nodes.get(&name) else {
        return;
    };
    let global: GlobalObject<&spa::utils::dict::DictRef> = GlobalObject {
        id,
        permissions: PermissionFlags::empty(),
        type_: pw::types::ObjectType::Node,
        version,
        props: None,
    };
    let Ok(node) = registry.bind::<pw::node::Node, _>(&global) else {
        return;
    };
    let tracker = tracker.clone();
    let listener = node
        .add_listener_local()
        .param(move |_seq, param, _index, _next, pod| {
            if param == spa::param::ParamType::Props {
                if let Some(pod) = pod {
                    if let Ok(mut tracker) = tracker.try_borrow_mut() {
                        tracker.apply_props(pod);
                    }
                }
            }
        })
        .register();
    node.subscribe_params(&[spa::param::ParamType::Props]);
    node.enum_params(0, Some(spa::param::ParamType::Props), 0, 1);
    watch.node = Some((node, listener));
    watch.node_name = Some(name);
    watch.node_id = Some(id);
}

fn watch_default_sink_volume(
    core: &pw::core::CoreRc,
    gain: Arc<AtomicU32>,
) -> Result<VolumeWatch, String> {
    let registry = core.get_registry_rc().map_err(|e| format!("registry: {e}"))?;
    let watch = Rc::new(RefCell::new(SinkWatch {
        sink_name: None,
        node_name: None,
        node_id: None,
        nodes: HashMap::new(),
        metadata: None,
        node: None,
    }));
    let tracker = Rc::new(RefCell::new(GainTracker::new(gain)));
    tracker.borrow().publish();

    let listener = {
        let registry = registry.clone();
        let weak = Rc::downgrade(&watch);
        let tracker = tracker.clone();
        let remove_weak = weak.clone();
        let global_registry = registry.clone();
        let global_tracker = tracker.clone();
        registry
            .add_listener_local()
            .global(move |global| {
                let registry = &global_registry;
                let tracker = global_tracker.clone();
                let weak = &weak;
                let Some(watch) = weak.upgrade() else {
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Metadata => {
                        let is_default = global
                            .props
                            .and_then(|p| p.get("metadata.name"))
                            .is_some_and(|n| n == "default");
                        if !is_default {
                            return;
                        }
                        let mut w = watch.borrow_mut();
                        if w.metadata.is_some() {
                            return;
                        }
                        let Ok(metadata) = registry.bind::<pw::metadata::Metadata, _>(global)
                        else {
                            return;
                        };
                        let metadata_weak = Rc::downgrade(&watch);
                        let metadata_tracker = tracker.clone();
                        let metadata_registry = registry.clone();
                        let listener = metadata
                            .add_listener_local()
                            .property(move |_subject, key, _type, value| {
                                if key == Some("default.audio.sink") {
                                    if let Some(name) = value.and_then(metadata_sink_name) {
                                        if let Some(watches) = metadata_weak.upgrade() {
                                            let mut w = watches.borrow_mut();
                                            if w.sink_name.as_deref() != Some(name.as_str()) {
                                                w.sink_name = Some(name);
                                                w.node_name = None;
                                                w.node = None;
                                                w.node_id = None;
                                                bind_sink_node(
                                                    &metadata_registry,
                                                    &mut w,
                                                    &metadata_tracker,
                                                );
                                            }
                                        }
                                    }
                                }
                                0
                            })
                            .register();
                        w.metadata = Some((metadata, listener));
                    }
                    pw::types::ObjectType::Node => {
                        let Some(name) = global.props.and_then(|p| p.get("node.name")) else {
                            return;
                        };
                        let name = name.to_string();
                        let mut w = watch.borrow_mut();
                        w.nodes.insert(name.clone(), (global.id, global.version));
                        if w.sink_name.as_deref() == Some(name.as_str()) {
                            bind_sink_node(registry, &mut w, &tracker);
                        }
                    }
                    _ => {}
                }
            })
            .global_remove(move |id| {
                let Some(watch) = remove_weak.upgrade() else {
                    return;
                };
                let mut w = watch.borrow_mut();
                w.nodes.retain(|_, (node_id, _)| *node_id != id);
                if w.node_id == Some(id) {
                    w.node = None;
                    w.node_name = None;
                    w.node_id = None;
                }
            })
            .register()
    };

    Ok(VolumeWatch {
        _registry: registry,
        _listener: listener,
        _watch: watch,
        _tracker: tracker,
    })
}

struct Client {
    id: String,
    tx: SyncSender<(u64, Vec<u8>)>,
    secure: Arc<Secure>,
}

fn capture_thread(
    capture: Capture,
    ready: SyncSender<Result<(), String>>,
    quit: pw::channel::Receiver<()>,
) {
    let result = (|| -> Result<(), String> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
        let context =
            pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
        let core = context.connect_rc(None).map_err(|e| format!("core: {e}"))?;
        // Follow the default sink's volume/mute; the monitor tap is upstream
        // of the sink's own volume stage.
        let _volume_watch = watch_default_sink_volume(&core, capture.gain.clone())?;

        let mut props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
        };
        props.insert(*pw::keys::APP_NAME, "lgx-server");
        props.insert(*pw::keys::MEDIA_NAME, "lgx audio monitor");
        props.insert(*pw::keys::APP_PROCESS_BINARY, "lgx-server");
        props.insert(*pw::keys::NODE_NAME, "lgx-server");
        props.insert(*pw::keys::NODE_DESCRIPTION, "lgx audio monitor");
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
        // Keep capturing while the sink is idle, so the first sound after a
        // pause is delivered without a suspend/resume gap.
        props.insert(*pw::keys::NODE_ALWAYS_PROCESS, "true");
        props.insert(*pw::keys::NODE_PAUSE_ON_IDLE, "false");
        props.insert("node.suspend-on-idle", "false");
        props.insert(*pw::keys::NODE_LATENCY, "512/48000");

        let stream = pw::stream::StreamBox::new(&core, "lgx audio capture", props)
            .map_err(|e| format!("stream: {e}"))?;
        let _listener = stream
            .add_local_listener_with_user_data(capture)
            .process(|stream, cap| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let (offset, size) = {
                    let chunk = data.chunk();
                    (chunk.offset() as usize, chunk.size() as usize)
                };
                if size == 0 {
                    return;
                }
                let Some(slice) = data.data() else {
                    return;
                };
                let Some(pcm_bytes) = slice.get(offset..offset + size) else {
                    return;
                };
                let mut out = Vec::with_capacity(pcm_bytes.len() / 2);
                let target = f32::from_bits(cap.gain.load(Ordering::Relaxed));
                let from = cap.last_gain;
                cap.last_gain = target;
                f32_to_s16_dithered(pcm_bytes, &mut out, &mut cap.rng, from, target);
                let ts_us = crate::time::monotonic_us();
                if cap.pcm.try_send((ts_us, out)).is_err() {
                    cap.dropped.fetch_add(1, Ordering::Relaxed);
                }
            })
            .register()
            .map_err(|e| format!("listener: {e}"))?;

        let mut audio_info = spa::param::audio::AudioInfoRaw::new();
        audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
        audio_info.set_rate(RATE);
        audio_info.set_channels(CHANNELS);
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
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(|e| format!("connect: {e}"))?;

        let _ = ready.send(Ok(()));
        let _quit = quit.attach(mainloop.loop_(), {
            let mainloop = mainloop.clone();
            move |_| mainloop.quit()
        });
        mainloop.run();
        Ok(())
    })();

    if let Err(e) = result {
        let _ = ready.send(Err(e));
    }
}

/// A running PipeWire capture stream plus its fan-out thread. Created when the
/// first client authenticates and dropped with the last one, so an idle server
/// keeps no capture stream (and no extra node in the guest mixer).
struct CaptureHandle {
    quit: pw::channel::Sender<()>,
    join: std::thread::JoinHandle<()>,
    fanout: std::thread::JoinHandle<()>,
}

fn start_capture(clients: &Arc<Mutex<Vec<Client>>>) -> Result<CaptureHandle, String> {
    let (pcm_tx, pcm_rx) = mpsc::sync_channel::<(u64, Vec<u8>)>(4);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let dropped = Arc::new(AtomicU64::new(0));
    let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    let capture = Capture {
        pcm: pcm_tx,
        rng: 0x9E37_79B9_7F4A_7C15,
        dropped: dropped.clone(),
        gain,
        last_gain: 1.0,
    };
    let join = std::thread::Builder::new()
        .name("lgx-pw-capture".into())
        .spawn(move || capture_thread(capture, ready_tx, quit_rx))
        .map_err(|e| format!("PipeWire capture thread: {e}"))?;
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("PipeWire capture: {e}")),
        Err(_) => return Err("PipeWire capture thread exited".into()),
    }

    let listeners = clients.clone();
    let fanout_dropped = dropped.clone();
    let fanout = std::thread::Builder::new()
        .name("lgx-audio-fanout".into())
        .spawn(move || {
            while let Ok((ts_us, chunk)) = pcm_rx.recv() {
                let lost = fanout_dropped.swap(0, Ordering::Relaxed);
                if lost > 0 {
                    // Audible as a short gap; loud enough to notice in tests.
                    eprintln!("lgx-server: audio: dropped {lost} chunk(s), client behind");
                }
                let mut list = listeners.lock().unwrap();
                list.retain(|c| match c.tx.try_send((ts_us, chunk.clone())) {
                    Ok(()) | Err(TrySendError::Full(_)) => true,
                    Err(TrySendError::Disconnected(_)) => false,
                });
            }
        })
        .map_err(|e| format!("audio fanout thread: {e}"))?;

    eprintln!(
        "lgx-server: audio capture started (PipeWire monitor, float32 -> s16le dithered)"
    );
    Ok(CaptureHandle { quit: quit_tx, join, fanout })
}

fn stop_capture(handle: CaptureHandle) {
    let _ = handle.quit.send(());
    let _ = handle.join.join();
    // The fan-out ends when the capture thread drops its sender.
    let _ = handle.fanout.join();
    eprintln!("lgx-server: audio capture stopped (no clients)");
}

pub fn serve(addr: &str) -> io::Result<()> {
    let clients: Arc<Mutex<Vec<Client>>> = Arc::new(Mutex::new(Vec::new()));
    let capture: Arc<Mutex<Option<CaptureHandle>>> = Arc::new(Mutex::new(None));

    let listener = TcpListener::bind(addr)?;
    eprintln!("lgx-server: audio on {addr}");

    // Revocation must also stop audio for an already-connected client.
    {
        let clients = clients.clone();
        std::thread::Builder::new()
            .name("lgx-audio-revoke".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(1));
                let Ok(list) = crate::clients::load() else { continue };
                let valid: HashSet<String> = list.into_iter().map(|e| e.id).collect();
                let mut clients = clients.lock().unwrap();
                clients.retain(|c| {
                    if valid.contains(&c.id) {
                        true
                    } else {
                        eprintln!("lgx-server: revoking audio client {}", c.id);
                        c.secure.close();
                        false
                    }
                });
            })
            .ok();
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if !clients.lock().unwrap().is_empty() {
            eprintln!("lgx-server: audio: too many clients");
            continue;
        }
        let clients = clients.clone();
        let capture = capture.clone();
        std::thread::Builder::new()
            .name("lgx-audio-client".into())
            .spawn(move || {
                if let Err(e) = handle(stream, clients, capture) {
                    eprintln!("lgx-server: audio client: {e}");
                }
            })
            .ok();
    }
    Ok(())
}

fn handle(
    mut stream: TcpStream,
    clients: Arc<Mutex<Vec<Client>>>,
    capture: Arc<Mutex<Option<CaptureHandle>>>,
) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let client_id = secure::server_read_id(&mut stream)?;
    let token = match clients::token_for(&client_id)? {
        Some(t) => t,
        None => {
            eprintln!("lgx-server: audio: unknown client id {client_id}");
            return Ok(());
        }
    };
    let secure = Arc::new(secure::server_handshake(stream, &token)?);
    let tok = secure.recv_bytes()?;
    let got = String::from_utf8_lossy(&tok).trim().to_string();
    if !lgx_proto::auth::eq(&got, &token) {
        eprintln!("lgx-server: audio: authentication failed");
        secure.close();
        return Ok(());
    }

    let (tx, rx) = std::sync::mpsc::sync_channel::<(u64, Vec<u8>)>(4);
    {
        // Start capturing only now: an idle server must not hold a PipeWire
        // stream (and the guest must not keep its sink awake for nobody).
        let started = {
            let mut slot = capture.lock().unwrap();
            if slot.is_none() {
                match start_capture(&clients) {
                    Ok(handle) => {
                        *slot = Some(handle);
                        true
                    }
                    Err(e) => {
                        eprintln!("lgx-server: audio: capture unavailable: {e}");
                        false
                    }
                }
            } else {
                true
            }
        };
        if !started {
            secure.close();
            return Ok(());
        }
        clients.lock().unwrap().push(Client {
            id: client_id,
            tx,
            secure: secure.clone(),
        });
    }
    // The client opts in to LGXA timestamps with one AUDIO_SYNC message right
    // after the token. Reading it on a separate thread keeps the first PCM
    // chunk from waiting; an old client sends nothing and the thread simply
    // ends with the connection.
    let timestamped = Arc::new(AtomicBool::new(false));
    {
        let reader = secure.clone();
        let flag = timestamped.clone();
        std::thread::Builder::new()
            .name("lgx-audio-sync".into())
            .spawn(move || {
                if let Ok((m, _)) = reader.recv_msg() {
                    if m.typ == msg::AUDIO_SYNC {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
            })
            .ok();
    }
    while let Ok((ts_us, chunk)) = rx.recv() {
        let framed;
        let bytes: &[u8] = if timestamped.load(Ordering::Relaxed) {
            framed = lgxa_record(ts_us, &chunk);
            &framed
        } else {
            &chunk
        };
        if secure.send_bytes(bytes).is_err() {
            break;
        }
    }
    secure.close();
    // Unregister, then stop capturing if this was the last listener.
    let empty = {
        let mut list = clients.lock().unwrap();
        list.retain(|c| !Arc::ptr_eq(&c.secure, &secure));
        list.is_empty()
    };
    if empty {
        let handle = capture.lock().unwrap().take();
        if let Some(handle) = handle {
            stop_capture(handle);
        }
    }
    Ok(())
}

/// `LGXA` + u64 LE timestamp + u32 LE PCM length + PCM, the framing the
/// client's A/V sync parser already understands.
fn lgxa_record(ts_us: u64, pcm: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + pcm.len());
    out.extend_from_slice(b"LGXA");
    out.extend_from_slice(&ts_us.to_le_bytes());
    out.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32_le(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn s16_at(bytes: &[u8], i: usize) -> i16 {
        i16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]])
    }

    #[test]
    fn lgxa_framing_has_the_documented_layout() {
        let pcm = [1u8, 2, 3, 4];
        let rec = super::lgxa_record(0x0102_0304_0506, &pcm);
        assert_eq!(&rec[..4], b"LGXA");
        assert_eq!(
            u64::from_le_bytes(rec[4..12].try_into().unwrap()),
            0x0102_0304_0506
        );
        assert_eq!(u32::from_le_bytes(rec[12..16].try_into().unwrap()), 4);
        assert_eq!(&rec[16..], &pcm);
    }

    #[test]
    fn conversion_keeps_length_and_full_scale() {
        let mut rng = 1;
        let input = f32_le(&[1.0, -1.0, 0.5, 0.0]);
        let mut out = Vec::new();
        f32_to_s16_dithered(&input, &mut out, &mut rng, 1.0, 1.0);
        assert_eq!(out.len(), 8);
        assert_eq!(s16_at(&out, 0), 32767);
        assert_eq!(s16_at(&out, 1), -32768);
        // 0.5 maps to exactly 16384; dither of <1 LSB keeps it within a step.
        assert!((s16_at(&out, 2) as i32 - 16384).abs() <= 1);
    }

    #[test]
    fn dither_is_bounded_and_centred() {
        let mut rng = 42;
        let silence = vec![0u8; 4 * 20_000];
        let mut out = Vec::new();
        f32_to_s16_dithered(&silence, &mut out, &mut rng, 1.0, 1.0);
        let sum: i64 = (0..20_000).map(|i| s16_at(&out, i) as i64).sum();
        // Silent input still toggles the LSBs, but must not drift DC.
        assert!(sum.abs() < 20_000 / 2, "dither DC offset {sum}");
        let peak = (0..20_000).map(|i| s16_at(&out, i).abs()).max().unwrap();
        assert!(peak <= 2, "dither peak {peak} too large");
    }

    #[test]
    fn gain_scales_the_samples() {
        let mut rng = 9;
        let input = f32_le(&[1.0, -1.0, 0.5]);
        let mut out = Vec::new();
        f32_to_s16_dithered(&input, &mut out, &mut rng, 0.5, 0.5);
        assert!((s16_at(&out, 0) as i32 - 16384).abs() <= 1);
        assert!((s16_at(&out, 1) as i32 + 16384).abs() <= 1);
        assert!((s16_at(&out, 2) as i32 - 8192).abs() <= 1);
    }

    #[test]
    fn gain_ramps_across_one_chunk() {
        let mut rng = 11;
        let input = f32_le(&[1.0, 1.0, 1.0, 1.0]);
        let mut out = Vec::new();
        f32_to_s16_dithered(&input, &mut out, &mut rng, 1.0, 0.0);
        // The first sample keeps the previous gain, then ramps to the target.
        for (i, expected) in [32767, 24576, 16384, 8192].into_iter().enumerate() {
            assert!(
                (s16_at(&out, i) as i32 - expected).abs() <= 1,
                "sample {i}: {} vs {expected}",
                s16_at(&out, i)
            );
        }
    }

    #[test]
    fn sink_name_is_read_from_metadata_json() {
        assert_eq!(
            metadata_sink_name(r#"{"name":"alsa_output.pci.analog-stereo"}"#).as_deref(),
            Some("alsa_output.pci.analog-stereo")
        );
        assert_eq!(
            metadata_sink_name(r#"{ "name" : "easyeffects_sink" }"#).as_deref(),
            Some("easyeffects_sink")
        );
        assert_eq!(metadata_sink_name("null"), None);
        assert_eq!(metadata_sink_name(r#"{"id":5}"#), None);
    }

    #[test]
    fn clamps_out_of_range_float() {
        let mut rng = 7;
        let input = f32_le(&[2.0, -2.0, f32::NAN]);
        let mut out = Vec::new();
        f32_to_s16_dithered(&input, &mut out, &mut rng, 1.0, 1.0);
        assert_eq!(s16_at(&out, 0), 32767);
        assert_eq!(s16_at(&out, 1), -32768);
    }
}
