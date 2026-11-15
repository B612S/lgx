//! Pair with a server that printed a one-time pairing code.
//!
//! The code protects the X25519/AES-GCM channel used only for this exchange;
//! the long-term token and the current video/audio ports arrive encrypted and
//! are stored in the profile.

use std::io;
use std::net::TcpStream;

use lgx_proto::secure;

pub fn run(addr: &str, code: &str) -> io::Result<(String, String, u16, u16)> {
    if code.is_empty() {
        return Err(io::Error::other("--pair-code must not be empty"));
    }
    let stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    let secure = secure::client_handshake(stream, code)?;
    let raw = secure.recv_bytes()?;
    let text = String::from_utf8(raw)
        .map_err(|_| io::Error::other("bad credentials from server"))?;
    let mut lines = text.lines();
    let id = lines.next().unwrap_or("").trim();
    let token = lines.next().unwrap_or("").trim();
    let video = lines.next().and_then(|v| v.trim().parse::<u16>().ok());
    let audio = lines.next().and_then(|v| v.trim().parse::<u16>().ok());
    if id.is_empty()
        || id.len() > 128
        || id.bytes().any(|b| !(0x21..=0x7e).contains(&b))
        || token.len() < 16
        || token.len() > 256
        || !token.bytes().all(|b| b.is_ascii_hexdigit())
        || video.is_none()
        || audio.is_none()
    {
        return Err(io::Error::other("server returned invalid credentials"));
    }
    secure.send_bytes(b"ok")?;
    secure.close();
    Ok((id.to_string(), token.to_string(), video.unwrap(), audio.unwrap()))
}
