//! System audio capture and fan-out to the single audio client.
//!
//! The capture itself lives in the `lgx-macos-audio` helper: a Core Audio
//! process tap reads the macOS system mix, mutes the local playback while the
//! tap is being read, and writes s16le 48 kHz stereo to stdout. This module
//! owns the audio TCP listener, the lgx secure handshake, and the pipe between
//! the helper and the client.
//!
//! Capture starts with the first authenticated audio client and stops when
//! that client goes away, so an idle server neither holds a tap nor silences
//! the Mac.

use std::io::{self, ErrorKind, Read};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lgx_proto::secure;

/// One audio client at a time, like the Linux server.
struct Slot {
    /// Incremented for every client that takes the slot, so a client thread
    /// that finishes late cannot clear the state of a newer client.
    generation: u64,
    tx: Option<SyncSender<Vec<u8>>>,
}

struct Capture {
    child: Child,
    reader: JoinHandle<()>,
}

struct State {
    slot: Mutex<Slot>,
    capture: Mutex<Option<Capture>>,
    helper: String,
}

pub fn serve(addr: &str, token: String, helper: String) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    eprintln!("lgx-server-macos: audio on {addr}");
    let state = Arc::new(State {
        slot: Mutex::new(Slot {
            generation: 0,
            tx: None,
        }),
        capture: Mutex::new(None),
        helper,
    });
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("lgx-server-macos: audio accept: {e}");
                continue;
            }
        };
        let token = token.clone();
        let state = Arc::clone(&state);
        thread::Builder::new()
            .name("lgx-audio-client".into())
            .spawn(move || {
                if let Err(e) = handle(stream, &token, &state) {
                    eprintln!("lgx-server-macos: audio client: {e}");
                }
            })
            .ok();
    }
    Ok(())
}

fn handle(
    mut stream: std::net::TcpStream,
    token: &str,
    state: &Arc<State>,
) -> io::Result<()> {
    let client_id = secure::server_read_id(&mut stream)?;
    let secure = Arc::new(secure::server_handshake(stream, token)?);
    // The client re-sends the token inside the encrypted record layer; the
    // video port does not, but the audio port has always done this.
    let sent = secure.recv_bytes()?;
    let got = String::from_utf8_lossy(&sent).trim().to_string();
    if !lgx_proto::auth::eq(&got, token) {
        eprintln!("lgx-server-macos: audio: authentication failed");
        secure.close();
        return Ok(());
    }

    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(4);
    let generation = {
        let mut slot = state.slot.lock().unwrap();
        if slot.tx.is_some() {
            eprintln!("lgx-server-macos: audio: too many clients");
            secure.close();
            return Ok(());
        }
        slot.generation += 1;
        slot.tx = Some(tx);
        slot.generation
    };
    eprintln!("lgx-server-macos: audio client {client_id} connected");

    if let Err(e) = start_capture(state) {
        eprintln!("lgx-server-macos: audio capture unavailable: {e}");
        unregister(state, generation);
        secure.close();
        return Ok(());
    }

    // The helper writes paced silence continuously, so a long quiet pipe means
    // it is wedged (for example waiting for the System Audio Recording
    // permission). Drop the client instead of leaving the slot busy forever.
    while let Ok(chunk) = rx.recv_timeout(Duration::from_secs(30)) {
        if secure.send_bytes(&chunk).is_err() {
            break;
        }
    }
    secure.close();
    unregister(state, generation);
    eprintln!("lgx-server-macos: audio client {client_id} disconnected");
    Ok(())
}

fn unregister(state: &Arc<State>, generation: u64) {
    let mut slot = state.slot.lock().unwrap();
    if slot.generation == generation {
        slot.tx = None;
    }
    if slot.tx.is_some() {
        return;
    }
    let capture = state.capture.lock().unwrap().take();
    drop(slot);
    if let Some(capture) = capture {
        stop_capture(capture);
    }
}

fn start_capture(state: &Arc<State>) -> Result<(), String> {
    let mut capture = state.capture.lock().unwrap();
    if capture.is_some() {
        return Ok(());
    }
    let mut child = Command::new(&state.helper)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", state.helper))?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("audio helper has no stdout".into());
        }
    };
    let sink = Arc::clone(state);
    let reader = thread::Builder::new()
        .name("lgx-audio-read".into())
        .spawn(move || read_pcm(stdout, sink))
        .map_err(|e| format!("audio reader thread: {e}"))?;
    *capture = Some(Capture { child, reader });
    Ok(())
}

/// Reads whatever the helper wrote and hands it to the audio client. Chunks
/// are dropped, not queued, when the client falls behind: a growing backlog
/// would turn into latency, and the client already reports underruns.
fn read_pcm(mut stdout: impl Read, state: Arc<State>) {
    let mut buffer = vec![0u8; 4096];
    let mut dropped = 0u64;
    let mut last_report = Instant::now();
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                let slot = state.slot.lock().unwrap();
                if let Some(tx) = &slot.tx {
                    if tx.try_send(buffer[..n].to_vec()).is_err() {
                        dropped += 1;
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        if dropped > 0 && last_report.elapsed() >= Duration::from_secs(5) {
            eprintln!("lgx-server-macos: audio: dropped {dropped} chunk(s), client behind");
            dropped = 0;
            last_report = Instant::now();
        }
    }
    // The helper is gone (crash or normal stop). Release the client so the
    // slot is not left busy, even if the server never got to stop_capture().
    let mut slot = state.slot.lock().unwrap();
    slot.tx = None;
    drop(slot);
    eprintln!("lgx-server-macos: audio capture ended");
}

/// Stops the helper with SIGTERM so it can destroy the tap before exiting
/// (a SIGKILL would leave the system muted until the HAL notices).
fn stop_capture(capture: Capture) {
    let Capture { mut child, reader } = capture;
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    let _ = reader.join();
    eprintln!("lgx-server-macos: audio capture stopped");
}
