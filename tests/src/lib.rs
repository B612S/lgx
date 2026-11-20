//! Shared helpers for the lgx integration tests.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use lgx_proto::secure::{self, Secure};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// A temporary directory removed when the test ends.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "lgx-test-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Sets an environment variable and restores the previous value on drop.
pub struct EnvVar {
    key: &'static str,
    old: Option<OsString>,
}

impl EnvVar {
    pub fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let old = env::var_os(key);
        env::set_var(key, value);
        Self { key, old }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        match self.old.take() {
            Some(value) => env::set_var(self.key, value),
            None => env::remove_var(self.key),
        }
    }
}

/// An accepted client/server socket pair on loopback.
pub fn raw_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let accept = thread::spawn(move || listener.accept().expect("accept").0);
    let client = TcpStream::connect(addr).expect("connect");
    let server = accept.join().expect("accept thread");
    (client, server)
}

/// Runs the secure handshake for a client and a server over loopback.
/// With `id`, the public client-id frame is exchanged first (as on the wire).
pub fn handshake_pair(id: Option<&str>, token: &str) -> (Secure, Secure) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let token_owned = token.to_string();
    let server_token = token_owned.clone();
    let client_id = id.map(str::to_string);
    let server_id = client_id.clone();
    let server_thread = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        if let Some(expected) = server_id {
            let got = secure::server_read_id(&mut stream).expect("server read id");
            assert_eq!(got, expected);
        }
        secure::server_handshake(stream, &server_token).expect("server handshake")
    });
    let client_stream = TcpStream::connect(addr).expect("connect");
    let client = match client_id.as_deref() {
        Some(id) => {
            secure::client_handshake_id(client_stream, id, &token_owned).expect("client handshake")
        }
        None => secure::client_handshake(client_stream, &token_owned).expect("client handshake"),
    };
    let server = server_thread.join().expect("server thread");
    (client, server)
}

/// Runs the handshake with different tokens for the two ends; the exchange
/// itself succeeds (the token only enters the HKDF), and the first record then
/// fails to authenticate.
pub fn handshake_pair_tokens(client_token: &str, server_token: &str) -> (Secure, Secure) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server_token = server_token.to_string();
    let server_thread = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        secure::server_handshake(stream, &server_token).expect("server handshake")
    });
    let client_stream = TcpStream::connect(addr).expect("connect");
    let client = secure::client_handshake(client_stream, client_token).expect("client handshake");
    let server = server_thread.join().expect("server thread");
    (client, server)
}

/// Corrupts exactly one byte of the client-to-server stream after the
/// handshake and returns both ends. The byte is flipped after `flip_at` bytes
/// of the client direction, which must be past the handshake frames.
pub fn handshake_pair_tampered(token: &str, flip_at: usize) -> (Secure, Secure) {
    let real = TcpListener::bind("127.0.0.1:0").expect("real bind");
    let proxy_listener = TcpListener::bind("127.0.0.1:0").expect("proxy bind");
    let real_addr = real.local_addr().expect("real addr");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    let token_owned = token.to_string();

    let server = thread::spawn(move || {
        let (stream, _) = real.accept().expect("real accept");
        secure::server_handshake(stream, &token_owned).expect("server handshake")
    });
    thread::spawn(move || {
        let (client_side, _) = proxy_listener.accept().expect("proxy accept");
        let server_side = TcpStream::connect(real_addr).expect("proxy connect");
        let c2s = forward(client_side.try_clone().unwrap(), server_side.try_clone().unwrap(), flip_at);
        let s2c = forward(server_side, client_side, usize::MAX);
        let _ = c2s.join();
        let _ = s2c.join();
    });

    let client_stream = TcpStream::connect(proxy_addr).expect("client connect");
    let client = secure::client_handshake(client_stream, token).expect("client handshake");
    let server = server.join().expect("server thread");
    (client, server)
}

fn forward(mut from: TcpStream, mut to: TcpStream, flip_at: usize) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut offset = 0usize;
        let mut flipped = false;
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if !flipped && flip_at >= offset && flip_at - offset < n {
                buf[flip_at - offset] ^= 0x40;
                flipped = true;
            }
            offset = offset.saturating_add(n);
            if to.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = to.shutdown(std::net::Shutdown::Write);
    })
}

/// A reader that returns one byte per call, exercising `read_exact`.
pub struct OneByteReader<R>(pub R);

impl<R: Read> Read for OneByteReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.0.read(&mut buf[..1])
    }
}

/// Frame bytes for a raw client-id frame (u32 LE length + payload).
pub fn id_frame(id: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + id.len());
    out.extend_from_slice(&(id.len() as u32).to_le_bytes());
    out.extend_from_slice(id);
    out
}

/// Convenience for tests that need a writable path.
pub fn write_file(path: impl AsRef<Path>, contents: &[u8]) {
    if let Some(dir) = path.as_ref().parent() {
        fs::create_dir_all(dir).expect("mkdir");
    }
    fs::write(path, contents).expect("write");
}
