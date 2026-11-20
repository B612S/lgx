//! End-to-end transport tests: the public client-id frame, the X25519/GCM
//! handshake and the record layer over real loopback sockets.

use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lgx_proto::secure::{self, MSG_HEADER};
use lgx_proto::{msg, Msg};
use lgx_tests::{handshake_pair, handshake_pair_tampered, handshake_pair_tokens, id_frame, raw_pair};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn handshake_with_public_id_then_both_directions() {
    let (client, server) = handshake_pair(Some("archlinux"), TOKEN);
    client.send_msg(Msg::new(msg::AUTH, 0, 5, 0), b"token").unwrap();
    let (auth, token) = server.recv_msg().unwrap();
    assert_eq!((auth.typ, token.as_slice()), (msg::AUTH, b"token".as_slice()));

    server.send_msg(Msg::new(msg::HELLO, 1, 1920, 1080), &[]).unwrap();
    let (hello, payload) = client.recv_msg().unwrap();
    assert_eq!((hello.typ, hello.code, hello.a, hello.b), (msg::HELLO, 1, 1920, 1080));
    assert!(payload.is_empty());
}

#[test]
fn handshake_without_an_id_works() {
    let (client, server) = handshake_pair(None, "token");
    server.send_bytes(b"hello").unwrap();
    assert_eq!(client.recv_bytes().unwrap(), b"hello");
    client.send_bytes(b"world").unwrap();
    assert_eq!(server.recv_bytes().unwrap(), b"world");
}

#[test]
fn a_wrong_token_is_detected_by_the_first_record() {
    let (client, server) = handshake_pair_tokens("token-a", "token-b");
    // The key exchange itself carries no authentication; the AEAD tag does.
    assert!(client.send_bytes(b"secret").is_ok());
    let err = server.recv_bytes().expect_err("wrong token must not decrypt");
    assert_eq!(err.kind(), std::io::ErrorKind::Other);
}

#[test]
fn every_record_size_roundtrips_including_chunk_boundaries() {
    let (client, server) = handshake_pair(None, TOKEN);
    // Receive on a thread: a 1 MiB record must not depend on the socket's
    // send buffer being large enough to hold it before the reader starts.
    let reader = thread::spawn(move || {
        for size in [0usize, 1, 255, 4096, 32 * 1024 - 1, 32 * 1024, 32 * 1024 + 1, 100_000, 1024 * 1024] {
            let expected: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let got = server.recv_record().unwrap();
            assert_eq!(got.len(), size, "size {size}");
            assert_eq!(got, expected, "size {size}");
        }
    });
    for size in [0usize, 1, 255, 4096, 32 * 1024 - 1, 32 * 1024, 32 * 1024 + 1, 100_000, 1024 * 1024] {
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        client.send_record(&data).unwrap();
    }
    reader.join().unwrap();
}

#[test]
fn message_payloads_survive_the_chunked_path() {
    let (client, server) = handshake_pair(None, TOKEN);
    let payload = vec![0xa5u8; 200 * 1024];
    client.send_msg(Msg::new(msg::CLIP_DATA, 0, payload.len() as i32, 0), &payload).unwrap();
    let (m, body) = server.recv_msg().unwrap();
    assert_eq!((m.typ, body.len()), (msg::CLIP_DATA, payload.len()));
    assert_eq!(body, payload);
}

#[test]
fn recv_msg_raw_keeps_the_header_in_front() {
    let (client, server) = handshake_pair(None, TOKEN);
    client.send_msg(Msg::new(msg::VIDEO, 0, 4, 0), b"data").unwrap();
    let (m, raw) = server.recv_msg_raw().unwrap();
    assert_eq!((m.typ, m.a), (msg::VIDEO, 4));
    assert_eq!(raw.len(), MSG_HEADER + 4);
    assert_eq!(&raw[..MSG_HEADER], &m.to_bytes()[..]);
    assert_eq!(&raw[MSG_HEADER..], b"data");
}

#[test]
fn both_directions_can_run_concurrently() {
    let (client, server) = handshake_pair(None, TOKEN);
    let client = Arc::new(client);
    let server = Arc::new(server);
    let c2s = {
        let client = Arc::clone(&client);
        let server = Arc::clone(&server);
        thread::spawn(move || {
            for i in 0..100i32 {
                client.send_msg(Msg::new(msg::KEY, i as u16, i, 0), &[]).unwrap();
                let (m, _) = server.recv_msg().unwrap();
                assert_eq!((m.typ, m.a), (msg::KEY, i));
            }
        })
    };
    let s2c = {
        let server = Arc::clone(&server);
        let client = Arc::clone(&client);
        thread::spawn(move || {
            for i in 0..100i32 {
                server.send_msg(Msg::new(msg::MOTION, 0, i, -i), &[]).unwrap();
                let (m, _) = client.recv_msg().unwrap();
                assert_eq!((m.typ, m.a, m.b), (msg::MOTION, i, -i));
            }
        })
    };
    c2s.join().unwrap();
    s2c.join().unwrap();
}

#[test]
fn read_timeout_interrupts_a_blocked_receiver() {
    let (client, server) = handshake_pair(None, TOKEN);
    client.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let err = client.recv_bytes().expect_err("no data was sent");
    assert!(matches!(
        err.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    drop(server);
}

#[test]
fn close_unblocks_the_peer_receiver() {
    let (client, server) = handshake_pair(None, TOKEN);
    server.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    client.close();
    let err = server.recv_bytes().expect_err("peer closed");
    assert!(matches!(
        err.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
    ));
}

#[test]
fn records_above_the_limit_are_refused_locally() {
    let (client, _server) = handshake_pair(None, TOKEN);
    let oversized = vec![0u8; 64 * 1024 * 1024 + 1];
    let err = client.send_record(&oversized).expect_err("too large");
    assert_eq!(err.kind(), std::io::ErrorKind::Other);
}

#[test]
fn client_ids_are_checked_before_the_handshake() {
    let long = "a".repeat(129);
    for bad in ["", "with space", "line\nbreak", "é", long.as_str()] {
        let (stream, _server) = raw_pair();
        let err = match secure::client_handshake_id(stream, bad, TOKEN) {
            Ok(_) => panic!("id {bad:?} was accepted"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::Other, "id {bad:?}");
    }
}

#[test]
fn server_read_id_accepts_only_printable_short_ids() {
    let (mut client, mut server) = raw_pair();
    client.write_all(&id_frame(b"archlinux")).unwrap();
    assert_eq!(secure::server_read_id(&mut server).unwrap(), "archlinux");

    for bad in [b"".as_slice(), b"with space", b"line\nbreak", &[0xff, 0xfe]] {
        let (mut client, mut server) = raw_pair();
        client.write_all(&id_frame(bad)).unwrap();
        assert!(secure::server_read_id(&mut server).is_err(), "id {bad:?}");
    }

    let (mut client, mut server) = raw_pair();
    client.write_all(&id_frame(&[b'a'; 129])).unwrap();
    assert!(secure::server_read_id(&mut server).is_err());
}

#[test]
fn incomplete_handshakes_are_rejected() {
    let (mut client, server) = raw_pair();
    // A valid frame length followed by garbage, not the LGX1 magic.
    client.write_all(&72u32.to_le_bytes()).unwrap();
    client.write_all(&[0u8; 72]).unwrap();
    assert!(secure::server_handshake(server, TOKEN).is_err());

    let (mut client, server) = raw_pair();
    client.write_all(&0u32.to_le_bytes()).unwrap();
    assert!(secure::server_handshake(server, TOKEN).is_err());
}

#[test]
fn the_token_never_appears_in_the_client_hello() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let token = "the-secret-token-value";
    let client = thread::spawn(move || {
        let stream = TcpStream::connect(addr).unwrap();
        // The server reads the hello and drops the connection, so the client
        // handshake ends with an error; that is expected here.
        let _ = secure::client_handshake(stream, token);
    });
    let (mut stream, _) = listener.accept().unwrap();
    let mut len = [0u8; 4];
    std::io::Read::read_exact(&mut stream, &mut len).unwrap();
    let mut frame = vec![0u8; u32::from_le_bytes(len) as usize];
    std::io::Read::read_exact(&mut stream, &mut frame).unwrap();
    assert_eq!(&frame[..4], b"LGX1");
    assert!(!frame.windows(token.len()).any(|w| w == token.as_bytes()));
    drop(stream);
    client.join().unwrap();
}

#[test]
fn a_tampered_ciphertext_byte_is_rejected() {
    // Handshake bytes sent by the client: 4 + 68. Flip inside the first record.
    let (client, server) = handshake_pair_tampered(TOKEN, 90);
    server.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    client.send_bytes(b"this record is tampered with").unwrap();
    let err = server.recv_bytes().expect_err("AEAD must reject the record");
    assert_eq!(err.kind(), std::io::ErrorKind::Other);
}

#[test]
fn messages_with_a_bad_length_are_rejected() {
    let (client, server) = handshake_pair(None, TOKEN);
    // Header claims a 5-byte payload, but the record holds only the header.
    client.send_bytes(&Msg::new(msg::VIDEO, 0, 5, 0).to_bytes()).unwrap();
    assert!(server.recv_msg().is_err());
}

#[test]
fn records_shorter_than_a_header_are_rejected() {
    let (client, server) = handshake_pair(None, TOKEN);
    client.send_bytes(&[1, 2, 3]).unwrap();
    assert!(server.recv_msg().is_err());
}
