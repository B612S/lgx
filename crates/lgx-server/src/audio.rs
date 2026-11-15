//! Audio capture and fan-out over the native PipeWire stream API.
//!
//! The guest opens a capture stream on the default sink monitor
//! (`stream.capture.sink`), so desktop audio is captured without PulseAudio's
//! `parec` compatibility layer. Captured s16le 48 kHz stereo chunks are sent
//! to every authenticated client.

use std::collections::HashSet;
use std::io;
use std::net::{TcpListener, TcpStream};
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

struct Client {
    id: String,
    tx: SyncSender<Vec<u8>>,
    secure: Arc<Secure>,
}

fn capture_thread(pcm: SyncSender<Vec<u8>>, ready: SyncSender<Result<(), String>>) {
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
            .add_local_listener_with_user_data(pcm)
            .process(|stream, pcm| {
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
                let _ = pcm.try_send(pcm_bytes.to_vec());
            })
            .register()
            .map_err(|e| format!("listener: {e}"))?;

        let mut audio_info = spa::param::audio::AudioInfoRaw::new();
        audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
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
    std::thread::Builder::new()
        .name("lgx-pw-capture".into())
        .spawn(move || capture_thread(pcm_tx, ready_tx))
        .map_err(|e| io::Error::other(format!("PipeWire capture thread: {e}")))?;
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(io::Error::other(format!("PipeWire capture: {e}"))),
        Err(_) => return Err(io::Error::other("PipeWire capture thread exited")),
    }
    eprintln!("lgx-server: audio capture from the default sink monitor (PipeWire)");

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
