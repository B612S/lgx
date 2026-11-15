//! Audio capture and fan-out over the native PipeWire stream API.
//!
//! The guest opens a capture stream on the default sink monitor
//! (`stream.capture.sink`), so desktop audio is captured without PulseAudio's
//! `parec` compatibility layer. The stream is float32 (the graph's native
//! format, so PipeWire does no conversion), and the server converts it to
//! s16le 48 kHz stereo with TPDF dither before sending chunks to every
//! authenticated client. Doing the conversion here keeps the single
//! quantization step dithered instead of relying on PipeWire's plain rounding.

use std::collections::HashSet;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lgx_proto::secure::{self, Secure};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::pod::Pod;

use crate::clients;

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;

/// Capture state owned by the PipeWire process callback.
struct Capture {
    pcm: SyncSender<Vec<u8>>,
    rng: u64,
    /// Chunks the fanout could not take because the client is behind.
    dropped: Arc<AtomicU64>,
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
fn f32_to_s16_dithered(input: &[u8], out: &mut Vec<u8>, rng: &mut u64) {
    out.clear();
    out.reserve(input.len() / 2);
    for sample in input.chunks_exact(4) {
        let v = f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]);
        let d = next_uniform(rng) - next_uniform(rng);
        // Scale by 2^15 so -1.0 reaches -32768 exactly; +1.0 saturates.
        let s = (v * 32768.0 + d).round().clamp(-32768.0, 32767.0) as i16;
        out.extend_from_slice(&s.to_le_bytes());
    }
}

struct Client {
    id: String,
    tx: SyncSender<Vec<u8>>,
    secure: Arc<Secure>,
}

fn capture_thread(capture: Capture, ready: SyncSender<Result<(), String>>) {
    let result = (|| -> Result<(), String> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
        let context =
            pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
        let core = context.connect_rc(None).map_err(|e| format!("core: {e}"))?;

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
                f32_to_s16_dithered(pcm_bytes, &mut out, &mut cap.rng);
                if cap.pcm.try_send(out).is_err() {
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
        mainloop.run();
        Ok(())
    })();

    if let Err(e) = result {
        let _ = ready.send(Err(e));
    }
}

pub fn serve(addr: &str) -> io::Result<()> {
    let (pcm_tx, pcm_rx) = mpsc::sync_channel::<Vec<u8>>(4);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let dropped = Arc::new(AtomicU64::new(0));
    let capture = Capture {
        pcm: pcm_tx,
        rng: 0x9E37_79B9_7F4A_7C15,
        dropped: dropped.clone(),
    };
    std::thread::Builder::new()
        .name("lgx-pw-capture".into())
        .spawn(move || capture_thread(capture, ready_tx))
        .map_err(|e| io::Error::other(format!("PipeWire capture thread: {e}")))?;
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(io::Error::other(format!("PipeWire capture: {e}"))),
        Err(_) => return Err(io::Error::other("PipeWire capture thread exited")),
    }
    eprintln!(
        "lgx-server: audio capture from the default sink monitor (PipeWire, float32 -> s16le dithered)"
    );

    let clients: Arc<Mutex<Vec<Client>>> = Arc::new(Mutex::new(Vec::new()));
    let listeners = clients.clone();
    std::thread::Builder::new()
        .name("lgx-audio-fanout".into())
        .spawn(move || {
            while let Ok(chunk) = pcm_rx.recv() {
                let mut list = listeners.lock().unwrap();
                list.retain(|c| match c.tx.try_send(chunk.clone()) {
                    Ok(()) | Err(TrySendError::Full(_)) => true,
                    Err(TrySendError::Disconnected(_)) => false,
                });
            }
        })
        .ok();

    let listener = TcpListener::bind(addr)?;
    eprintln!("lgx-server: audio on {addr}");

    // Revocation must also stop audio for an already-connected client.
    {
        let clients = clients.clone();
        std::thread::Builder::new()
            .name("lgx-audio-revoke".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(1));
                let lost = dropped.swap(0, Ordering::Relaxed);
                if lost > 0 {
                    // Audible as a short gap; loud enough to notice in tests.
                    eprintln!("lgx-server: audio: dropped {lost} chunk(s), client behind");
                }
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
        if clients.lock().unwrap().len() >= 1 {
            eprintln!("lgx-server: audio: too many clients");
            continue;
        }
        let clients = clients.clone();
        std::thread::Builder::new()
            .name("lgx-audio-client".into())
            .spawn(move || {
                if let Err(e) = handle(stream, clients) {
                    eprintln!("lgx-server: audio client: {e}");
                }
            })
            .ok();
    }
    Ok(())
}

fn handle(mut stream: TcpStream, clients: Arc<Mutex<Vec<Client>>>) -> io::Result<()> {
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

    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
    clients.lock().unwrap().push(Client {
        id: client_id,
        tx,
        secure: secure.clone(),
    });
    while let Ok(chunk) = rx.recv() {
        if secure.send_bytes(&chunk).is_err() {
            break;
        }
    }
    secure.close();
    Ok(())
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
    fn conversion_keeps_length_and_full_scale() {
        let mut rng = 1;
        let input = f32_le(&[1.0, -1.0, 0.5, 0.0]);
        let mut out = Vec::new();
        f32_to_s16_dithered(&input, &mut out, &mut rng);
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
        f32_to_s16_dithered(&silence, &mut out, &mut rng);
        let sum: i64 = (0..20_000).map(|i| s16_at(&out, i) as i64).sum();
        // Silent input still toggles the LSBs, but must not drift DC.
        assert!(sum.abs() < 20_000 / 2, "dither DC offset {sum}");
        let peak = (0..20_000).map(|i| s16_at(&out, i).abs()).max().unwrap();
        assert!(peak <= 2, "dither peak {peak} too large");
    }

    #[test]
    fn clamps_out_of_range_float() {
        let mut rng = 7;
        let input = f32_le(&[2.0, -2.0, f32::NAN]);
        let mut out = Vec::new();
        f32_to_s16_dithered(&input, &mut out, &mut rng);
        assert_eq!(s16_at(&out, 0), 32767);
        assert_eq!(s16_at(&out, 1), -32768);
    }
}
