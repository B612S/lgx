//! End-to-end protocol sessions over the real secure transport: the video
//! port's AUTH/HELLO/keymap sequence and the audio port's token + AUDIO_SYNC
//! preamble with raw and LGXA-framed PCM.

use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use lgx_client::hid::evdev_from_scancode;
use lgx_proto::{auth, codec, decode_rich, encode_rich, file, msg, secure, Msg};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn listener() -> (TcpListener, std::net::SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

#[test]
fn video_session_negotiates_and_carries_frames_input_and_clipboard() {
    let (listener, addr) = listener();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let id = secure::server_read_id(&mut stream).unwrap();
        assert_eq!(id, "archlinux");
        let secure = secure::server_handshake(stream, TOKEN).unwrap();
        secure.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        let (auth_msg, token) = secure.recv_msg().unwrap();
        assert_eq!(auth_msg.typ, msg::AUTH);
        assert!(auth::eq(std::str::from_utf8(&token).unwrap(), TOKEN));

        secure
            .send_msg(Msg::new(msg::HELLO, codec::HEVC, 1920, 1080), &[])
            .unwrap();

        let (keymap, payload) = secure.recv_msg().unwrap();
        assert_eq!(keymap.typ, msg::KEYMAP);
        assert_eq!(&payload[..4], b"map!");

        secure.send_msg(Msg::new(msg::VIDEO, 0, 7, 0), b"frame-0").unwrap();
        let (idr, _) = secure.recv_msg().unwrap();
        assert_eq!(idr.typ, msg::IDR);
        secure.send_msg(Msg::new(msg::VIDEO, 0, 7, 0), b"frame-1").unwrap();

        let (key, _) = secure.recv_msg().unwrap();
        assert_eq!((key.typ, key.code, key.a), (msg::KEY, 30, 1));

        let (pad, snapshot) = secure.recv_msg().unwrap();
        assert_eq!(pad.typ, msg::GAMEPAD);
        assert_eq!(snapshot.len(), msg::GAMEPAD_LEN);

        let (clip, rich) = secure.recv_msg().unwrap();
        assert_eq!(clip.typ, msg::CLIP_SET_HTML);
        assert_eq!(decode_rich(&rich).unwrap(), ("<b>x</b>".to_string(), "x".to_string()));

        let (begin, head) = secure.recv_msg().unwrap();
        assert_eq!(begin.typ, msg::FILE_BEGIN);
        assert_eq!(file::decode_begin(&head), Some(("notes.txt".to_string(), 5)));
        let (data, bytes) = secure.recv_msg().unwrap();
        assert_eq!((data.typ, bytes.as_slice()), (msg::FILE_DATA, b"hello".as_slice()));
        let (end, _) = secure.recv_msg().unwrap();
        assert_eq!(end.typ, msg::FILE_END);
        secure.send_msg(Msg::new(msg::CLIP_DATA, 0, 2, 0), b"ok").unwrap();
    });

    let stream = std::net::TcpStream::connect(addr).unwrap();
    let secure = secure::client_handshake_id(stream, "archlinux", TOKEN).unwrap();
    secure.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    secure
        .send_msg(Msg::new(msg::AUTH, 0, TOKEN.len() as i32, 0), TOKEN.as_bytes())
        .unwrap();
    secure
        .send_msg(Msg::new(msg::KEYMAP, 1, 4, 0), b"map!")
        .unwrap();

    let (hello, _) = secure.recv_msg().unwrap();
    assert_eq!((hello.typ, hello.code, hello.a, hello.b), (msg::HELLO, codec::HEVC, 1920, 1080));

    let (video, first) = secure.recv_msg().unwrap();
    assert_eq!((video.typ, first.as_slice()), (msg::VIDEO, b"frame-0".as_slice()));
    secure.send_msg(Msg::new(msg::IDR, 0, 0, 0), &[]).unwrap();
    let (video, second) = secure.recv_msg().unwrap();
    assert_eq!((video.typ, second.as_slice()), (msg::VIDEO, b"frame-1".as_slice()));

    let code = evdev_from_scancode(4).expect("a is mapped");
    secure.send_msg(Msg::new(msg::KEY, code, 1, 0), &[]).unwrap();
    secure
        .send_msg(Msg::new(msg::GAMEPAD, 0, msg::GAMEPAD_LEN as i32, 0), &[0u8; 24])
        .unwrap();

    let rich = encode_rich("<b>x</b>", "x");
    secure
        .send_msg(Msg::new(msg::CLIP_SET_HTML, 0, rich.len() as i32, 0), &rich)
        .unwrap();

    let begin = file::encode_begin("a/notes.txt", 5);
    secure
        .send_msg(Msg::new(msg::FILE_BEGIN, 0, begin.len() as i32, 0), &begin)
        .unwrap();
    secure.send_msg(Msg::new(msg::FILE_DATA, 0, 5, 0), b"hello").unwrap();
    secure.send_msg(Msg::new(msg::FILE_END, 0, 0, 0), &[]).unwrap();

    let (ack, payload) = secure.recv_msg().unwrap();
    assert_eq!((ack.typ, payload.as_slice()), (msg::CLIP_DATA, b"ok".as_slice()));
    server.join().unwrap();
}

#[test]
fn audio_port_authenticates_then_frames_raw_and_lgxa_pcm() {
    let (listener, addr) = listener();
    let pcm = vec![0x11u8; 3840];
    let server_pcm = pcm.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let id = secure::server_read_id(&mut stream).unwrap();
        assert_eq!(id, "macbook");
        let secure = secure::server_handshake(stream, TOKEN).unwrap();
        secure.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // The audio port repeats the token inside the record layer.
        let token = secure.recv_bytes().unwrap();
        assert_eq!(token.as_slice(), TOKEN.as_bytes());
        let (sync, _) = secure.recv_msg().unwrap();
        assert_eq!(sync.typ, msg::AUDIO_SYNC);

        secure.send_bytes(&server_pcm).unwrap();
        let ts_us = 1_700_000_000_000u64;
        let mut lgxa = Vec::new();
        lgxa.extend_from_slice(b"LGXA");
        lgxa.extend_from_slice(&ts_us.to_le_bytes());
        lgxa.extend_from_slice(&(server_pcm.len() as u32).to_le_bytes());
        lgxa.extend_from_slice(&server_pcm);
        secure.send_bytes(&lgxa).unwrap();
    });

    let stream = std::net::TcpStream::connect(addr).unwrap();
    let secure = secure::client_handshake_id(stream, "macbook", TOKEN).unwrap();
    secure.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    secure.send_bytes(TOKEN.as_bytes()).unwrap();
    secure.send_msg(Msg::new(msg::AUDIO_SYNC, 0, 0, 0), &[]).unwrap();

    assert_eq!(secure.recv_bytes().unwrap(), pcm);
    let record = secure.recv_bytes().unwrap();
    assert_eq!(&record[..4], b"LGXA");
    let ts_us = u64::from_le_bytes(record[4..12].try_into().unwrap());
    let len = u32::from_le_bytes(record[12..16].try_into().unwrap()) as usize;
    assert_eq!(ts_us, 1_700_000_000_000);
    assert_eq!(len, pcm.len());
    assert_eq!(record.len(), 16 + len);
    assert_eq!(&record[16..], pcm);
    server.join().unwrap();
}

#[test]
fn audio_port_rejects_a_wrong_token_before_any_pcm() {
    let (listener, addr) = listener();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = secure::server_read_id(&mut stream).unwrap();
        let secure = secure::server_handshake(stream, TOKEN).unwrap();
        let token = secure.recv_bytes().unwrap();
        assert!(!auth::eq(std::str::from_utf8(&token).unwrap(), TOKEN));
        // The real server closes the connection here; mimic that.
        secure.close();
    });

    let stream = std::net::TcpStream::connect(addr).unwrap();
    let secure = secure::client_handshake_id(stream, "macbook", TOKEN).unwrap();
    secure.send_bytes(b"wrong-token").unwrap();
    server.join().unwrap();
    secure.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    assert!(secure.recv_bytes().is_err());
}
