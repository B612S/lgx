//! Audio playback: raw s16le 48 kHz stereo received over TCP is piped into
//! `pacat --playback` (the Pulse protocol is served by PipeWire as well).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};

pub fn run(addr: &str, token: &str) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    let len = token.len() as u32;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(token.as_bytes())?;

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
    let mut stdin = child.stdin.take().expect("pw-cat stdin");

    let mut buf = vec![0u8; 8192];
    let mut total = 0u64;
    let mut last = std::time::Instant::now();
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        stdin.write_all(&buf[..n])?;
        total += n as u64;
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
