//! One-time pairing: hand the long-term token to a new client over an
//! encrypted channel protected by a high-entropy code printed on the server.
//!
//! The code is never sent over the network. A client that does not know it
//! cannot complete the handshake, and the token is only sent inside the
//! encrypted record after that handshake. Pairing accepts one client and
//! expires after three minutes.

use std::io;
use std::net::TcpListener;
use std::time::{Duration, Instant};

use lgx_proto::secure;

use lgx_proto::auth;

use crate::clients;

pub fn run(addr: &str, name: Option<&str>) -> io::Result<()> {
    clients::ensure()?;
    let id = auth::generate();
    let token = auth::generate();
    let name = name
        .map(clients::clean_name)
        .unwrap_or_else(|| format!("client-{}", &id[..8]));
    let code = auth::generate();
    let listener = TcpListener::bind(addr).map_err(|e| io::Error::other(format!("bind {addr}: {e}")))?;
    listener.set_nonblocking(true)?;
    eprintln!("lgx-server: pairing code: {code}");
    let port = addr.rsplit_once(':').map(|(_, p)| p).unwrap_or("5904");
    eprintln!("lgx-server: on the client run:");
    eprintln!("  lgx-client --pair <server-ip>:{port} --pair-code {code}");
    eprintln!("lgx-server: waiting up to 3 minutes for one client...");
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut failures = 0u32;
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::other("pairing timed out"));
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                let _ = stream.set_nodelay(true);
                eprintln!("lgx-server: pairing client from {peer}");
                match pair_client(stream, &code, &id, &token) {
                    Ok(()) => {
                        clients::add(clients::Entry {
                            id: id.clone(),
                            name: name.clone(),
                            token: token.clone(),
                            created: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs(),
                        })?;
                        eprintln!(
                            "lgx-server: paired client {peer}: id={id} name={name}; token delivered"
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        failures += 1;
                        eprintln!("lgx-server: pairing failed: {e}");
                        if failures >= 5 {
                            return Err(io::Error::other("too many failed pairing attempts"));
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

fn pair_client(
    stream: std::net::TcpStream,
    code: &str,
    id: &str,
    token: &str,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let secure = secure::server_handshake(stream, code)?;
    secure.send_bytes(format!("{id}\n{token}").as_bytes())?;
    let ack = secure.recv_bytes()?;
    if ack != b"ok" {
        secure.close();
        return Err(io::Error::other("client did not acknowledge the token"));
    }
    secure.close();
    Ok(())
}
