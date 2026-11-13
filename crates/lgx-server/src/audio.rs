//! Audio capture and fan-out.
//!
//! The guest records the monitor of its default sink with `pw-cat` (raw
//! s16le, 48 kHz, stereo) and streams the samples to every connected client.

use std::collections::HashSet;
use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lgx_proto::secure::{self, Secure};

use crate::clients;

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;

struct Client {
    id: String,
    tx: SyncSender<Vec<u8>>,
    secure: Arc<Secure>,
}

pub fn serve(addr: &str) -> io::Result<()> {
    // @DEFAULT_MONITOR@ is the sink monitor (desktop audio), not the default
    // source (microphone). pw-cat's --target is not reliable on PipeWire and
    // auto-connected to easyeffects_source on the test VM.
    let mut child = Command::new("parec")
        .args([
            "--device=@DEFAULT_MONITOR@",
            "--format=s16le",
            "--rate=48000",
            "--channels=2",
            "--raw",
            "--latency-msec=20",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| io::Error::other(format!("parec: {e}")))?;
    std::thread::sleep(Duration::from_millis(150));
    if let Ok(Some(status)) = child.try_wait() {
        return Err(io::Error::other(format!("parec exited with {status}")));
    }
    eprintln!("lgx-server: audio capture from @DEFAULT_MONITOR@");
    let mut out = child.stdout.take().expect("parec stdout");

    let clients: Arc<Mutex<Vec<Client>>> = Arc::new(Mutex::new(Vec::new()));
    let listeners = clients.clone();
    std::thread::Builder::new()
        .name("lgx-audio-read".into())
        .spawn(move || {
            let mut buf = vec![0u8; 8192];
            loop {
                match out.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        let mut list = listeners.lock().unwrap();
                        list.retain(|c| match c.tx.try_send(chunk.clone()) {
                            Ok(()) | Err(TrySendError::Full(_)) => true,
                            Err(TrySendError::Disconnected(_)) => false,
                        });
                    }
                    Err(_) => break,
                }
            }
        })
        .ok();

    let listener = TcpListener::bind(addr)?;
    eprintln!("lgx-server: audio on {addr}");
    let _keep_child = child;

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
        if clients.lock().unwrap().len() >= 8 {
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

fn handle(
    mut stream: TcpStream,
    clients: Arc<Mutex<Vec<Client>>>,
) -> io::Result<()> {
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

    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
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
