//! Wire-format tests: headers, payload rules, clipboard framing and auth.

use std::os::unix::fs::PermissionsExt;

use lgx_proto::{auth, decode_rich, encode_rich, evdev, file, msg, payload_len, read_msg, write_msg, Msg, VERSION};
use lgx_tests::{write_file, EnvVar, OneByteReader, TempDir};

#[test]
fn message_header_is_explicit_little_endian() {
    let m = Msg::new(0x12, 0x3456, 0x789a_bcdeu32 as i32, -2);
    assert_eq!(
        m.to_bytes(),
        [0x12, 0x00, 0x56, 0x34, 0xde, 0xbc, 0x9a, 0x78, 0xfe, 0xff, 0xff, 0xff]
    );
    assert_eq!(Msg::from_bytes(&m.to_bytes()), m);
}

#[test]
fn header_roundtrips_boundary_values() {
    for m in [
        Msg::new(0, 0, 0, 0),
        Msg::new(255, 0xffff, i32::MIN, i32::MAX),
        Msg::new(1, 2, -1, 1),
        Msg { typ: 3, flags: 0xaa, code: 0x1234, a: 7, b: -7 },
    ] {
        assert_eq!(Msg::from_bytes(&m.to_bytes()), m);
    }
}

#[test]
fn payload_types_use_field_a() {
    let payload_types = [
        msg::AUTH,
        msg::VIDEO,
        msg::KEYMAP,
        msg::CLIP_SET,
        msg::CLIP_DATA,
        msg::CLIP_SET_HTML,
        msg::CLIP_DATA_HTML,
        msg::FILE_BEGIN,
        msg::FILE_DATA,
        msg::GAMEPAD,
        msg::CURSOR_IMAGE,
    ];
    for typ in payload_types {
        assert_eq!(payload_len(&Msg::new(typ, 0, 7, 0)), 7, "type {typ}");
        assert_eq!(payload_len(&Msg::new(typ, 0, 0, 0)), 0, "type {typ}");
        assert_eq!(payload_len(&Msg::new(typ, 0, -3, 0)), 0, "type {typ}");
    }
}

#[test]
fn control_types_ignore_field_a() {
    let control = [
        msg::HELLO,
        msg::KEY,
        msg::BTN,
        msg::MOTION,
        msg::WHEEL,
        msg::MODS,
        msg::CURSOR,
        msg::CURSOR_LOCAL,
        msg::PING,
        msg::FILE_END,
        msg::IDR,
        msg::AUDIO_SYNC,
        msg::CURSOR_SHAPE,
        msg::CURSOR_HIDDEN,
    ];
    for typ in control {
        assert_eq!(payload_len(&Msg::new(typ, 0, 1234, 0)), 0, "type {typ}");
    }
}

#[test]
fn stream_carries_multiple_messages_and_a_large_payload() {
    let big = vec![0x5au8; 1024 * 1024];
    let mut wire = Vec::new();
    write_msg(&mut wire, Msg::new(msg::HELLO, 1, 1920, 1080), &[]).unwrap();
    write_msg(&mut wire, Msg::new(msg::AUTH, 0, 5, 0), b"token").unwrap();
    write_msg(&mut wire, Msg::new(msg::VIDEO, 0, big.len() as i32, 0), &big).unwrap();

    let mut cur = wire.as_slice();
    let (hello, none) = read_msg(&mut cur).unwrap();
    assert_eq!((hello.typ, hello.code, hello.a, hello.b), (msg::HELLO, 1, 1920, 1080));
    assert!(none.is_empty());
    let (auth, token) = read_msg(&mut cur).unwrap();
    assert_eq!((auth.typ, token.as_slice()), (msg::AUTH, b"token".as_slice()));
    let (video, body) = read_msg(&mut cur).unwrap();
    assert_eq!((video.typ, body.len()), (msg::VIDEO, big.len()));
    assert_eq!(body, big);
    assert!(cur.is_empty());
}

#[test]
fn message_reader_handles_partial_reads() {
    let mut wire = Vec::new();
    write_msg(&mut wire, Msg::new(msg::KEY, 30, 1, 0), &[]).unwrap();
    write_msg(&mut wire, Msg::new(msg::CLIP_DATA, 0, 3, 0), b"abc").unwrap();
    let mut reader = OneByteReader(wire.as_slice());
    let (key, p) = read_msg(&mut reader).unwrap();
    assert_eq!((key.typ, key.code, key.a), (msg::KEY, 30, 1));
    assert!(p.is_empty());
    let (clip, body) = read_msg(&mut reader).unwrap();
    assert_eq!((clip.typ, body), (msg::CLIP_DATA, b"abc".to_vec()));
}

#[test]
fn rich_text_roundtrips_and_rejects_garbage() {
    let payload = encode_rich("<b>héllo</b>", "héllo 日本語");
    assert_eq!(decode_rich(&payload).unwrap(), ("<b>héllo</b>".to_string(), "héllo 日本語".to_string()));

    let empty = encode_rich("", "");
    assert_eq!(decode_rich(&empty).unwrap(), (String::new(), String::new()));

    assert_eq!(decode_rich(&[0, 0, 0]), None);
    assert_eq!(decode_rich(&[0, 0, 0, 9, b'a']), None);
    assert_eq!(decode_rich(&[0, 0, 0, 1, 0xff]), None);
    // Invalid UTF-8 in the plain part is rejected too.
    assert_eq!(decode_rich(&[0, 0, 0, 0, 0xff]), None);
}

#[test]
fn file_names_are_reduced_to_a_safe_base_name() {
    assert_eq!(file::sanitize_name("a/b.txt").as_deref(), Some("b.txt"));
    assert_eq!(file::sanitize_name("/tmp/x/y.bin").as_deref(), Some("y.bin"));
    assert_eq!(file::sanitize_name("dir/").as_deref(), Some("dir"));
    assert_eq!(file::sanitize_name("plain").as_deref(), Some("plain"));
    assert_eq!(file::sanitize_name(".."), None);
    assert_eq!(file::sanitize_name("."), None);
    assert_eq!(file::sanitize_name(""), None);
    assert_eq!(file::sanitize_name("a/.."), None);
    assert_eq!(file::sanitize_name("a\0b"), None);
    let long = "x".repeat(file::MAX_NAME + 1);
    assert_eq!(file::sanitize_name(&long), None);
    let max = "x".repeat(file::MAX_NAME);
    assert_eq!(file::sanitize_name(&max).as_deref(), Some(max.as_str()));
}

#[test]
fn file_begin_roundtrips_and_validates() {
    let payload = file::encode_begin("dir/photo.jpg", 4242);
    assert_eq!(file::decode_begin(&payload), Some(("photo.jpg".to_string(), 4242)));

    let fallback = file::encode_begin("..", 1);
    assert_eq!(file::decode_begin(&fallback), Some(("file".to_string(), 1)));

    assert_eq!(file::decode_begin(&[0, 0, 0]), None);
    assert_eq!(file::decode_begin(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), None);
    let mut short = payload.clone();
    short.pop();
    assert_eq!(file::decode_begin(&short), None);
    let mut trailing = payload.clone();
    trailing.push(0);
    assert_eq!(file::decode_begin(&trailing), None);
    let mut bad_utf8 = file::encode_begin("a", 0);
    bad_utf8[4] = 0xff;
    assert_eq!(file::decode_begin(&bad_utf8), None);
}

#[test]
fn file_records_roundtrip_and_enforce_the_size_limit() {
    let data = b"hello file clipboard";
    let record = file::encode_record("a/b.txt", data);
    assert_eq!(file::decode_record(&record), Some(("b.txt".to_string(), data.to_vec())));

    assert_eq!(file::decode_record(&[]), None);
    let mut truncated = record.clone();
    truncated.pop();
    assert_eq!(file::decode_record(&truncated), None);

    // A declared size above MAX_BYTES is rejected without reading it.
    let too_big = file::encode_begin("x.bin", file::MAX_BYTES + 1);
    assert_eq!(file::decode_record(&too_big), None);

    let mut wrong_size = file::encode_begin("x.bin", 3);
    wrong_size.extend_from_slice(b"abc");
    assert!(file::decode_record(&wrong_size).is_some());
    wrong_size.push(b'd');
    assert_eq!(file::decode_record(&wrong_size), None);
}

#[test]
fn protocol_constants_match_the_documented_values() {
    assert_eq!(VERSION, 2);
    assert_eq!(msg::GAMEPAD_LEN, 24);
    assert_eq!(file::MAX_BYTES, 16 * 1024 * 1024);
    assert_eq!(file::MAX_NAME, 255);
    let buttons = [
        evdev::BTN_LEFT,
        evdev::BTN_RIGHT,
        evdev::BTN_MIDDLE,
        evdev::BTN_SIDE,
        evdev::BTN_EXTRA,
        evdev::BTN_FORWARD,
        evdev::BTN_BACK,
    ];
    for (i, a) in buttons.iter().enumerate() {
        for b in &buttons[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[test]
fn tokens_and_codes_have_the_expected_shape() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        let code = auth::generate();
        let token = auth::generate_token();
        assert_eq!(code.len(), 32);
        assert_eq!(token.len(), 64);
        assert!(code.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(seen.insert(token));
    }
    assert!(auth::eq("same", "same"));
    assert!(!auth::eq("same", "samf"));
    assert!(!auth::eq("same", "same "));
    assert!(!auth::eq("", "x"));
    assert!(auth::eq("", ""));
}

#[test]
fn credentials_are_saved_private_and_load_in_both_formats() {
    let dir = TempDir::new("auth");
    let _env = EnvVar::set("XDG_CONFIG_HOME", dir.path());
    let path = dir.path().join("lgx/token");

    auth::save_credentials("archlinux", "secret-token").unwrap();
    let creds = auth::load_credentials().unwrap();
    assert_eq!((creds.id.as_str(), creds.token.as_str()), ("archlinux", "secret-token"));
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "credentials must be readable only by the owner");

    // `load` is the legacy single-token reader; a two-line credentials file is
    // only understood by `load_credentials`.
    write_file(&path, b"legacy-token\n");
    let legacy = auth::load_credentials().unwrap();
    assert_eq!((legacy.id.as_str(), legacy.token.as_str()), ("default", "legacy-token"));
    assert_eq!(auth::load().as_deref(), Some("legacy-token"));

    auth::save("another").unwrap();
    assert_eq!(auth::load().as_deref(), Some("another"));

    write_file(&path, b"a\nb\nc\n");
    let first_two = auth::load_credentials().unwrap();
    assert_eq!((first_two.id.as_str(), first_two.token.as_str()), ("a", "b"));
}
