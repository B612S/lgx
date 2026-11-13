//! Pair a client with a server that printed a one-time pairing code.
//!
//! The code protects the X25519/AES-GCM channel used only for this exchange;
//! the long-term token arrives encrypted and is stored with mode 0600.

use std::io;
use std::net::TcpStream;

use lgx_proto::{auth, secure};

pub fn run(addr: &str, code: &str) -> io::Result<()> {
    if code.is_empty() {
        return Err(io::Error::other("--pair-code must not be empty"));
    }
    let stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    let secure = secure::client_handshake(stream, code)?;
    let raw = secure.recv_bytes()?;
    let text = String::from_utf8(raw).map_err(|_| io::Error::other("bad credentials from server"))?;
    let (id, token) = text
        .split_once('\n')
        .ok_or_else(|| io::Error::other("server returned incomplete credentials"))?;
    let (id, token) = (id.trim(), token.trim());
    if id.is_empty()
        || id.len() > 128
        || id.bytes().any(|b| !(0x21..=0x7e).contains(&b))
        || token.len() < 16
        || token.len() > 256
        || !token.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(io::Error::other("server returned invalid credentials"));
    }
    auth::save_credentials(id, token)?;
    secure.send_bytes(b"ok")?;
    secure.close();
    eprintln!(
        "lgx-client: paired as {id}; credentials saved to {}",
        auth::config_path().display()
    );
    Ok(())
}
