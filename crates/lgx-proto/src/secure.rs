//! Mandatory authenticated encryption for every TCP connection.
//!
//! The existing auth token is used as a pre-shared secret. A fresh X25519
//! exchange and two random nonces are mixed with it through HKDF-SHA256; the
//! resulting directional keys encrypt records with AES-256-GCM. The token is
//! never sent on the wire and a recorded frame cannot be decrypted without it.
//!
//! AES-GCM is hardware accelerated on x86 (AES-NI/VAES). At the stream's few
//! MB/s the cost is far below the encoder/decoder noise.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::Duration;

use ring::aead;
use ring::agreement;
use ring::hkdf;
use ring::rand::{SecureRandom, SystemRandom};

use crate::{payload_len, Msg};

const MAGIC: &[u8; 4] = b"LGX1";
const CHUNK: usize = 32 * 1024;
const MAX_RECORD: usize = 64 * 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

struct KeyLen(usize);

impl hkdf::KeyType for KeyLen {
    fn len(&self) -> usize {
        self.0
    }
}

fn crypto_err() -> io::Error {
    io::Error::other("secure channel: crypto failure")
}

fn aead_key(bytes: &[u8; 32]) -> io::Result<aead::LessSafeKey> {
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, bytes).map_err(|_| crypto_err())?;
    Ok(aead::LessSafeKey::new(key))
}

fn random32(rng: &dyn SecureRandom) -> io::Result<[u8; 32]> {
    let mut out = [0u8; 32];
    rng.fill(&mut out).map_err(|_| crypto_err())?;
    Ok(out)
}

fn derive(
    private: agreement::EphemeralPrivateKey,
    peer_public: &[u8],
    token: &str,
    client_pub: &[u8],
    server_pub: &[u8],
    client_random: &[u8],
    server_random: &[u8],
) -> io::Result<([u8; 32], [u8; 32])> {
    let peer = agreement::UnparsedPublicKey::new(&agreement::X25519, peer_public);
    let mut salt = Vec::with_capacity(64);
    salt.extend_from_slice(client_random);
    salt.extend_from_slice(server_random);
    let mut info = Vec::with_capacity(4 + 32 + 32 + 32 + 32);
    info.extend_from_slice(b"lgx transport v1");
    info.extend_from_slice(client_pub);
    info.extend_from_slice(server_pub);
    info.extend_from_slice(client_random);
    info.extend_from_slice(server_random);
    let token = token.as_bytes().to_vec();
    let keys: Result<[u8; 64], ring::error::Unspecified> =
        agreement::agree_ephemeral(private, &peer, |secret| {
        let mut ikm = Vec::with_capacity(secret.len() + token.len());
        ikm.extend_from_slice(secret);
        ikm.extend_from_slice(&token);
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &salt).extract(&ikm);
        let mut out = [0u8; 64];
        prk.expand(&[info.as_slice()], KeyLen(64))
            .and_then(|okm| okm.fill(&mut out))
            .map_err(|_| ring::error::Unspecified)?;
        Ok(out)
        })
        .map_err(|_| crypto_err())?;
    // agree_ephemeral returns Result<R> where R is the closure's return value;
    // our closure is fallible, so flatten the inner Result.
    let keys = keys.map_err(|_| crypto_err())?;
    let mut c2s = [0u8; 32];
    let mut s2c = [0u8; 32];
    c2s.copy_from_slice(&keys[..32]);
    s2c.copy_from_slice(&keys[32..]);
    Ok((c2s, s2c))
}

fn send_frame(stream: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    if data.len() > 1024 {
        return Err(io::Error::other("secure handshake frame too large"));
    }
    stream.write_all(&(data.len() as u32).to_le_bytes())?;
    stream.write_all(data)
}

fn recv_frame(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if n == 0 || n > 1024 {
        return Err(io::Error::other("bad secure handshake frame"));
    }
    let mut data = vec![0u8; n];
    stream.read_exact(&mut data)?;
    Ok(data)
}

struct SendState {
    stream: TcpStream,
    key: aead::LessSafeKey,
    nonce: u64,
}

struct RecvState {
    stream: TcpStream,
    key: aead::LessSafeKey,
    nonce: u64,
}

pub struct Secure {
    send: Mutex<SendState>,
    recv: Mutex<RecvState>,
    /// Raw clone used only for shutdown; taking the send/recv locks here
    /// would deadlock while the reader is blocked in read_exact.
    shutdown: TcpStream,
}

impl Secure {
    fn build(stream: TcpStream, send_key: [u8; 32], recv_key: [u8; 32]) -> io::Result<Self> {
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(None)?;
        let recv_stream = stream.try_clone()?;
        let shutdown = stream.try_clone()?;
        Ok(Self {
            send: Mutex::new(SendState {
                stream,
                key: aead_key(&send_key)?,
                nonce: 0,
            }),
            recv: Mutex::new(RecvState {
                stream: recv_stream,
                key: aead_key(&recv_key)?,
                nonce: 0,
            }),
            shutdown,
        })
    }

    pub fn send_record(&self, plain: &[u8]) -> io::Result<()> {
        if plain.len() > MAX_RECORD {
            return Err(io::Error::other("secure record too large"));
        }
        let mut st = self
            .send
            .lock()
            .map_err(|_| io::Error::other("secure send lock"))?;
        let total = plain.len() as u32;
        st.stream.write_all(&total.to_le_bytes())?;
        for (idx, chunk) in plain.chunks(CHUNK).enumerate() {
            let mut aad = [0u8; 12];
            aad[..4].copy_from_slice(&total.to_le_bytes());
            aad[4..8].copy_from_slice(&(idx as u32).to_le_bytes());
            aad[8..].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
            let mut ct = chunk.to_vec();
            let mut nonce_bytes = [0u8; 12];
            nonce_bytes[4..].copy_from_slice(&st.nonce.to_be_bytes());
            st.key
                .seal_in_place_append_tag(
                    aead::Nonce::assume_unique_for_key(nonce_bytes),
                    aead::Aad::from(&aad),
                    &mut ct,
                )
                .map_err(|_| crypto_err())?;
            st.nonce = st.nonce.checked_add(1).ok_or_else(crypto_err)?;
            st.stream.write_all(&(ct.len() as u32).to_le_bytes())?;
            st.stream.write_all(&ct)?;
        }
        Ok(())
    }

    pub fn recv_record(&self) -> io::Result<Vec<u8>> {
        let mut st = self
            .recv
            .lock()
            .map_err(|_| io::Error::other("secure recv lock"))?;
        let mut len = [0u8; 4];
        st.stream.read_exact(&mut len)?;
        let total = u32::from_le_bytes(len) as usize;
        if total > MAX_RECORD {
            return Err(io::Error::other("secure record too large"));
        }
        if total == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(total);
        let mut idx = 0u32;
        while out.len() < total {
            let mut clen = [0u8; 4];
            st.stream.read_exact(&mut clen)?;
            let ct_len = u32::from_le_bytes(clen) as usize;
            if ct_len <= 16 || ct_len > CHUNK + 16 {
                return Err(io::Error::other("bad secure chunk"));
            }
            let mut ct = vec![0u8; ct_len];
            st.stream.read_exact(&mut ct)?;
            let chunk_len = (ct_len - 16) as u32;
            let mut aad = [0u8; 12];
            aad[..4].copy_from_slice(&(total as u32).to_le_bytes());
            aad[4..8].copy_from_slice(&idx.to_le_bytes());
            aad[8..].copy_from_slice(&chunk_len.to_le_bytes());
            let mut nonce_bytes = [0u8; 12];
            nonce_bytes[4..].copy_from_slice(&st.nonce.to_be_bytes());
            let pt = st
                .key
                .open_in_place(
                    aead::Nonce::assume_unique_for_key(nonce_bytes),
                    aead::Aad::from(&aad),
                    &mut ct,
                )
                .map_err(|_| crypto_err())?;
            if out.len() + pt.len() > total {
                return Err(io::Error::other("bad secure record length"));
            }
            out.extend_from_slice(pt);
            st.nonce = st.nonce.checked_add(1).ok_or_else(crypto_err)?;
            idx += 1;
        }
        Ok(out)
    }

    pub fn send_msg(&self, m: Msg, payload: &[u8]) -> io::Result<()> {
        let mut plain = Vec::with_capacity(12 + payload.len());
        plain.extend_from_slice(&m.to_bytes());
        plain.extend_from_slice(payload);
        self.send_record(&plain)
    }

    pub fn recv_msg(&self) -> io::Result<(Msg, Vec<u8>)> {
        let plain = self.recv_record()?;
        if plain.len() < 12 {
            return Err(io::Error::other("short secure message"));
        }
        let mut header = [0u8; 12];
        header.copy_from_slice(&plain[..12]);
        let m = Msg::from_bytes(&header);
        if payload_len(&m) != plain.len() - 12 {
            return Err(io::Error::other("secure message length mismatch"));
        }
        Ok((m, plain[12..].to_vec()))
    }

    pub fn send_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        self.send_record(bytes)
    }

    pub fn recv_bytes(&self) -> io::Result<Vec<u8>> {
        self.recv_record()
    }

    pub fn close(&self) {
        let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
    }

    /// Set a timeout on the receive half. Used before the first AUTH/HELLO so
    /// a peer cannot hold a server thread without sending anything.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        let st = self
            .recv
            .lock()
            .map_err(|_| io::Error::other("secure recv lock"))?;
        st.stream.set_read_timeout(timeout)
    }
}

fn start_handshake(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    Ok(())
}

/// Send the public client id before the encrypted handshake. The id is not
/// secret; it selects the per-client token in the server registry.
pub fn client_handshake_id(mut stream: TcpStream, id: &str, token: &str) -> io::Result<Secure> {
    if id.is_empty() || id.len() > 128 || id.bytes().any(|b| !(0x21..=0x7e).contains(&b)) {
        return Err(io::Error::other("bad client id"));
    }
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    send_frame(&mut stream, id.as_bytes())?;
    client_handshake(stream, token)
}

pub fn server_read_id(stream: &mut TcpStream) -> io::Result<String> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let data = recv_frame(stream)?;
    let id = String::from_utf8(data).map_err(|_| io::Error::other("bad client id"))?;
    if id.is_empty() || id.len() > 128 || id.bytes().any(|b| !(0x21..=0x7e).contains(&b)) {
        return Err(io::Error::other("bad client id"));
    }
    Ok(id)
}

pub fn client_handshake(mut stream: TcpStream, token: &str) -> io::Result<Secure> {
    start_handshake(&stream)?;
    let rng = SystemRandom::new();
    let private = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng)
        .map_err(|_| crypto_err())?;
    let client_pub = private
        .compute_public_key()
        .map_err(|_| crypto_err())?
        .as_ref()
        .to_vec();
    let client_random = random32(&rng)?;
    let mut msg = Vec::with_capacity(68);
    msg.extend_from_slice(MAGIC);
    msg.extend_from_slice(&client_pub);
    msg.extend_from_slice(&client_random);
    send_frame(&mut stream, &msg)?;
    let response = recv_frame(&mut stream)?;
    if response.len() != 68 || &response[..4] != MAGIC {
        return Err(io::Error::other("bad secure handshake response"));
    }
    let server_pub = &response[4..36];
    let server_random = &response[36..68];
    let (c2s, s2c) = derive(
        private,
        server_pub,
        token,
        &client_pub,
        server_pub,
        &client_random,
        server_random,
    )?;
    Secure::build(stream, c2s, s2c)
}

pub fn server_handshake(mut stream: TcpStream, token: &str) -> io::Result<Secure> {
    start_handshake(&stream)?;
    let request = recv_frame(&mut stream)?;
    if request.len() != 68 || &request[..4] != MAGIC {
        return Err(io::Error::other("bad secure handshake request"));
    }
    let client_pub = &request[4..36];
    let client_random = &request[36..68];
    let rng = SystemRandom::new();
    let private = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng)
        .map_err(|_| crypto_err())?;
    let server_pub = private
        .compute_public_key()
        .map_err(|_| crypto_err())?
        .as_ref()
        .to_vec();
    let server_random = random32(&rng)?;
    let (c2s, s2c) = derive(
        private,
        client_pub,
        token,
        client_pub,
        &server_pub,
        client_random,
        &server_random,
    )?;
    let mut response = Vec::with_capacity(68);
    response.extend_from_slice(MAGIC);
    response.extend_from_slice(&server_pub);
    response.extend_from_slice(&server_random);
    send_frame(&mut stream, &response)?;
    Secure::build(stream, s2c, c2s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let secure = server_handshake(stream, "test-token").unwrap();
            let (m, p) = secure.recv_msg().unwrap();
            assert_eq!(m.typ, 1);
            assert_eq!(p, b"hello");
            secure.send_msg(Msg::new(3, 0, 5, 0), b"world").unwrap();
        });
        let stream = TcpStream::connect(addr).unwrap();
        let secure = client_handshake(stream, "test-token").unwrap();
        secure.send_msg(Msg::new(1, 0, 5, 0), b"hello").unwrap();
        let (m, p) = secure.recv_msg().unwrap();
        assert_eq!(m.typ, 3);
        assert_eq!(p, b"world");
        server.join().unwrap();
    }

    #[test]
    fn wrong_token_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let secure = server_handshake(stream, "server-token").unwrap();
            secure.recv_msg().is_err()
        });
        let stream = TcpStream::connect(addr).unwrap();
        let secure = client_handshake(stream, "client-token").unwrap();
        let _ = secure.send_msg(Msg::new(1, 0, 5, 0), b"hello");
        assert!(server.join().unwrap());
    }
}
