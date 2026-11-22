//! HEVC access units over UDP.
//!
//! TCP keeps authentication, input and the clipboard; video on the same stream
//! makes a key press wait behind a 30 KiB access unit (and behind every
//! retransmission of a lost one). With `video.udp` the server fragments each
//! access unit into datagrams on a separate socket instead and never
//! retransmits: a lost fragment drops that access unit, and the client asks
//! for an IDR over the control stream.
//!
//! Every datagram is sealed on its own with AES-256-GCM. The 32-byte key is
//! generated per session and sent over the authenticated TCP channel; the
//! nonce is `(frame_id, frag_idx)` inside the header, which is also the AEAD
//! associated data, so a fragment cannot be moved to another frame.
//!
//! Datagram layout (little endian):
//!
//! ```text
//! 0   4  magic "LGXU"
//! 4   1  version
//! 5   1  flags (bit 0: access unit is a keyframe)
//! 6   4  frame id, increments per access unit
//! 10  2  fragment index
//! 12  2  fragment count
//! 14  6  capture timestamp (microseconds, 48-bit)
//! 20  n  ciphertext
//! ..  16  GCM tag
//! ```

use std::io;

use ring::aead;
use ring::rand::{SecureRandom, SystemRandom};

pub const MAGIC: [u8; 4] = *b"LGXU";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 20;
pub const TAG_LEN: usize = 16;
/// Payload per datagram: 1200 bytes keeps the sealed datagram at 1236 bytes,
/// inside a 1500-byte Ethernet MTU even with an IPv4 + UDP header.
pub const MAX_PAYLOAD: usize = 1200;
pub const MAX_DATAGRAM: usize = HEADER_LEN + MAX_PAYLOAD + TAG_LEN;
pub const KEY_LEN: usize = 32;
pub const FLAG_KEYFRAME: u8 = 1;

/// Bound a single access unit and the fragment count, so a buggy or hostile
/// peer cannot make the reassembler allocate without bound.
pub const MAX_FRAGMENTS: usize = 4096;
pub const MAX_ACCESS_UNIT: usize = MAX_FRAGMENTS * MAX_PAYLOAD;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub flags: u8,
    pub frame_id: u32,
    pub frag_idx: u16,
    pub frag_count: u16,
    pub ts_us: u64,
}

impl Header {
    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&MAGIC);
        out[4] = VERSION;
        out[5] = self.flags;
        out[6..10].copy_from_slice(&self.frame_id.to_le_bytes());
        out[10..12].copy_from_slice(&self.frag_idx.to_le_bytes());
        out[12..14].copy_from_slice(&self.frag_count.to_le_bytes());
        out[14..20].copy_from_slice(&(self.ts_us & 0x0000_ffff_ffff_ffff).to_le_bytes()[..6]);
        out
    }

    /// Parse a datagram header; `None` for anything that is not ours or is
    /// inconsistent.
    pub fn decode(buf: &[u8]) -> Option<Header> {
        if buf.len() < HEADER_LEN || buf[0..4] != MAGIC || buf[4] != VERSION {
            return None;
        }
        let frame_id = u32::from_le_bytes(buf[6..10].try_into().ok()?);
        let frag_idx = u16::from_le_bytes(buf[10..12].try_into().ok()?);
        let frag_count = u16::from_le_bytes(buf[12..14].try_into().ok()?);
        if frag_count == 0 || frag_count as usize > MAX_FRAGMENTS || frag_idx >= frag_count {
            return None;
        }
        let mut ts = [0u8; 8];
        ts[..6].copy_from_slice(&buf[14..20]);
        Some(Header {
            flags: buf[5],
            frame_id,
            frag_idx,
            frag_count,
            ts_us: u64::from_le_bytes(ts),
        })
    }

    fn nonce(&self) -> aead::Nonce {
        let mut nonce = [0u8; 12];
        nonce[0..4].copy_from_slice(&self.frame_id.to_le_bytes());
        nonce[4..6].copy_from_slice(&self.frag_idx.to_le_bytes());
        aead::Nonce::assume_unique_for_key(nonce)
    }
}

fn key_from(bytes: &[u8]) -> io::Result<aead::LessSafeKey> {
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, bytes)
        .map_err(|_| io::Error::other("udp: bad key"))?;
    Ok(aead::LessSafeKey::new(key))
}

/// Fresh session key; sent to the client over the secure TCP channel.
pub fn random_key() -> io::Result<[u8; KEY_LEN]> {
    let mut key = [0u8; KEY_LEN];
    SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| io::Error::other("udp: no entropy"))?;
    Ok(key)
}

/// Server side: seals fragments into datagrams.
pub struct Tx {
    key: aead::LessSafeKey,
}

impl Tx {
    pub fn new(key: &[u8; KEY_LEN]) -> io::Result<Self> {
        Ok(Self {
            key: key_from(key)?,
        })
    }

    /// Seal one fragment. `out` is reused across fragments so a frame does not
    /// allocate per datagram.
    pub fn seal(&self, header: &Header, payload: &[u8], out: &mut Vec<u8>) -> bool {
        if payload.len() > MAX_PAYLOAD {
            return false;
        }
        let head = header.encode();
        out.clear();
        out.extend_from_slice(payload);
        if self
            .key
            .seal_in_place_append_tag(
                header.nonce(),
                aead::Aad::from(&head),
                out,
            )
            .is_err()
        {
            return false;
        }
        // The header goes in front; only ~1.2 KiB moves.
        out.splice(0..0, head);
        true
    }
}

/// Client side: opens a received datagram in place.
pub struct Rx {
    key: aead::LessSafeKey,
}

impl Rx {
    pub fn new(key: &[u8; KEY_LEN]) -> io::Result<Self> {
        Ok(Self {
            key: key_from(key)?,
        })
    }

    /// Authenticate `datagram` and return its header plus the payload range
    /// inside the (now decrypted) buffer. `None` for a forged or truncated
    /// datagram.
    pub fn open(&self, datagram: &mut [u8]) -> Option<(Header, std::ops::Range<usize>)> {
        let header = Header::decode(datagram)?;
        if datagram.len() < HEADER_LEN + TAG_LEN {
            return None;
        }
        let mut aad = [0u8; HEADER_LEN];
        aad.copy_from_slice(&datagram[..HEADER_LEN]);
        let body = &mut datagram[HEADER_LEN..];
        let plain = self
            .key
            .open_in_place(header.nonce(), aead::Aad::from(&aad), body)
            .ok()?;
        let start = HEADER_LEN;
        let len = plain.len();
        Some((header, start..start + len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_and_opens_a_fragment() {
        let key = [7u8; KEY_LEN];
        let tx = Tx::new(&key).unwrap();
        let rx = Rx::new(&key).unwrap();
        let header = Header {
            flags: FLAG_KEYFRAME,
            frame_id: 42,
            frag_idx: 3,
            frag_count: 9,
            ts_us: 0x1234_5678_9abc,
        };
        let payload = vec![0xa5u8; 1000];
        let mut datagram = Vec::new();
        assert!(tx.seal(&header, &payload, &mut datagram));
        assert_eq!(datagram.len(), HEADER_LEN + payload.len() + TAG_LEN);
        let (got, range) = rx.open(&mut datagram).unwrap();
        assert_eq!(got, header);
        assert!(got.is_keyframe());
        assert_eq!(&datagram[range], &payload[..]);
    }

    #[test]
    fn rejects_a_tampered_datagram() {
        let key = [1u8; KEY_LEN];
        let tx = Tx::new(&key).unwrap();
        let rx = Rx::new(&key).unwrap();
        let header = Header {
            flags: 0,
            frame_id: 1,
            frag_idx: 0,
            frag_count: 1,
            ts_us: 0,
        };
        let mut datagram = Vec::new();
        tx.seal(&header, b"hello", &mut datagram);
        let last = datagram.len() - 1;
        datagram[last] ^= 1;
        assert!(rx.open(&mut datagram).is_none());
    }

    #[test]
    fn rejects_another_sessions_key_and_bad_headers() {
        let tx = Tx::new(&[3u8; KEY_LEN]).unwrap();
        let rx = Rx::new(&[4u8; KEY_LEN]).unwrap();
        let header = Header {
            flags: 0,
            frame_id: 2,
            frag_idx: 1,
            frag_count: 2,
            ts_us: 7,
        };
        let mut datagram = Vec::new();
        tx.seal(&header, b"payload", &mut datagram);
        assert!(rx.open(&mut datagram).is_none());

        let mut bad = Vec::new();
        bad.extend_from_slice(&MAGIC);
        bad.push(VERSION + 1);
        bad.resize(HEADER_LEN + TAG_LEN, 0);
        assert!(Header::decode(&bad).is_none());
    }

    #[test]
    fn fragment_header_roundtrip() {
        let header = Header {
            flags: 0,
            frame_id: u32::MAX,
            frag_idx: 4095,
            frag_count: 4096,
            ts_us: u64::MAX,
        };
        let encoded = header.encode();
        // The timestamp is 48-bit on the wire.
        let mut expected = header;
        expected.ts_us &= 0x0000_ffff_ffff_ffff;
        assert_eq!(Header::decode(&encoded).unwrap(), expected);
    }
}
