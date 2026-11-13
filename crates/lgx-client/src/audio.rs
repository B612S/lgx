//! Audio playback: raw s16le 48 kHz stereo received over TCP is piped into
//! `pacat --playback` (the Pulse protocol is served by PipeWire as well).
//!
//! With `mute_unfocused = true`, focus loss pauses the pacat child with
//! SIGSTOP. The network stream is still drained (and discarded), so the
//! server never blocks and resuming does not replay a large backlog.

use std::io::Write;
use std::net::TcpStream;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use lgx_proto::secure;

struct Pacat {
    child: Child,
    paused: bool,
}

impl Pacat {
    fn start() -> std::io::Result<(Self, ChildStdin)> {
        let mut child = Command::new("pacat")
            .args([
                "--playback",
                "--device=@DEFAULT_SINK@",
                "--format=s16le",
                "--rate=48000",
                "--channels=2",
                "--raw",
                "--latency-msec=20",
            ])
            .stdin(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take().expect("pacat stdin");
        Ok((Self { child, paused: false }, stdin))
    }

    fn set_paused(&mut self, pause: bool) {
        if self.paused == pause {
            return;
        }
        let signal = if pause { libc::SIGSTOP } else { libc::SIGCONT };
        if unsafe { libc::kill(self.child.id() as i32, signal) } == 0 {
            self.paused = pause;
            eprintln!(
                "lgx-client: audio {}",
                if pause {
                    "paused (window unfocused)"
                } else {
                    "resumed (window focused)"
                }
            );
        }
    }
}

impl Drop for Pacat {
    fn drop(&mut self) {
        if self.paused {
            unsafe { libc::kill(self.child.id() as i32, libc::SIGCONT) };
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn run(
    addr: &str,
    client_id: &str,
    token: &str,
    mute_unfocused: bool,
    focus: Arc<AtomicBool>,
) -> std::io::Result<()> {
    let stream = TcpStream::connect(addr)?;
    let secure = secure::client_handshake_id(stream, client_id, token)?;
    secure.send_bytes(token.as_bytes())?;

    let (mut player, mut stdin) = Pacat::start()?;

    let mut total = 0u64;
    let mut last = std::time::Instant::now();
    loop {
        let chunk = secure.recv_bytes()?;
        if chunk.is_empty() {
            break;
        }
        if mute_unfocused {
            player.set_paused(!focus.load(Ordering::Relaxed));
        }
        if player.paused {
            // Keep draining the secure stream; do not let it back up while
            // the window is unfocused.
            continue;
        }
        stdin.write_all(&chunk)?;
        total += chunk.len() as u64;
        if last.elapsed() >= std::time::Duration::from_secs(5) {
            eprintln!(
                "lgx-client: audio {:.1} KiB/s",
                total as f64 / 1024.0 / last.elapsed().as_secs_f64()
            );
            total = 0;
            last = std::time::Instant::now();
        }
    }
    Ok(())
}
