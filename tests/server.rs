//! Server-side pure helpers: the per-client token registry and the
//! float32 -> s16le audio conversion.

use std::os::unix::fs::PermissionsExt;

use lgx_server::audio::{f32_to_s16_dithered, metadata_sink_name};
use lgx_server::clients;
use lgx_tests::{EnvVar, TempDir};

fn f32_le(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

fn s16_at(bytes: &[u8], i: usize) -> i16 {
    i16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]])
}

#[test]
fn audio_conversion_preserves_length_and_full_scale() {
    let mut rng = 1;
    let input = f32_le(&[1.0, -1.0, 0.5, 0.0, -0.5]);
    let mut out = Vec::new();
    f32_to_s16_dithered(&input, &mut out, &mut rng, 1.0, 1.0);
    assert_eq!(out.len(), 10);
    assert_eq!(s16_at(&out, 0), 32767);
    assert_eq!(s16_at(&out, 1), -32768);
    assert!((s16_at(&out, 2) as i32 - 16384).abs() <= 1);
    assert!((s16_at(&out, 4) as i32 + 16384).abs() <= 1);
}

#[test]
fn audio_gain_scales_the_signal_and_zero_is_silence() {
    let input = f32_le(&[1.0, -1.0, 0.5]);
    let mut rng = 7;
    let mut half = Vec::new();
    f32_to_s16_dithered(&input, &mut half, &mut rng, 0.5, 0.5);
    assert!((s16_at(&half, 0) as i32 - 16384).abs() <= 1);
    assert!((s16_at(&half, 1) as i32 + 16384).abs() <= 1);
    assert!((s16_at(&half, 2) as i32 - 8192).abs() <= 1);

    let mut muted = Vec::new();
    f32_to_s16_dithered(&input, &mut muted, &mut rng, 0.0, 0.0);
    for i in 0..3 {
        assert!(s16_at(&muted, i).abs() <= 1, "sample {i} is not silence");
    }
}

#[test]
fn audio_gain_ramps_over_one_chunk() {
    let input = f32_le(&[1.0, 1.0, 1.0, 1.0]);
    let mut rng = 11;
    let mut out = Vec::new();
    f32_to_s16_dithered(&input, &mut out, &mut rng, 1.0, 0.0);
    // The first sample keeps the previous gain, then ramps down to the target.
    for (i, expected) in [32767, 24576, 16384, 8192].into_iter().enumerate() {
        assert!(
            (s16_at(&out, i) as i32 - expected).abs() <= 1,
            "sample {i}: {} vs {expected}",
            s16_at(&out, i)
        );
    }
}

#[test]
fn audio_clamps_out_of_range_input() {
    let input = f32_le(&[2.0, -2.0]);
    let mut rng = 3;
    let mut out = Vec::new();
    f32_to_s16_dithered(&input, &mut out, &mut rng, 1.0, 1.0);
    assert_eq!(s16_at(&out, 0), 32767);
    assert_eq!(s16_at(&out, 1), -32768);
}

#[test]
fn audio_dither_is_bounded_and_has_no_dc_offset() {
    let mut rng = 42;
    let silence = vec![0u8; 4 * 20_000];
    let mut out = Vec::new();
    f32_to_s16_dithered(&silence, &mut out, &mut rng, 1.0, 1.0);
    let sum: i64 = (0..20_000).map(|i| s16_at(&out, i) as i64).sum();
    assert!(sum.abs() < 20_000 / 2, "dither DC offset {sum}");
    let peak = (0..20_000).map(|i| s16_at(&out, i).abs()).max().unwrap();
    assert!(peak <= 2, "dither peak {peak} too large");
}

#[test]
fn audio_timestamp_framing_is_empty_input_safe() {
    let mut rng = 1;
    let mut out = vec![1u8; 8];
    f32_to_s16_dithered(&[], &mut out, &mut rng, 1.0, 1.0);
    assert!(out.is_empty());
}

#[test]
fn sink_metadata_names_are_parsed_from_json() {
    assert_eq!(
        metadata_sink_name(r#"{"name":"alsa_output.pci-0000_00_1b.0.analog-stereo"}"#).as_deref(),
        Some("alsa_output.pci-0000_00_1b.0.analog-stereo")
    );
    assert_eq!(
        metadata_sink_name(r#"{ "name" : "easyeffects_sink" }"#).as_deref(),
        Some("easyeffects_sink")
    );
    assert_eq!(metadata_sink_name(r#"{"name":""}"#).as_deref(), Some(""));
    assert_eq!(metadata_sink_name("null"), None);
    assert_eq!(metadata_sink_name(r#"{"id":5}"#), None);
    assert_eq!(metadata_sink_name(r#"{"name":}"#), None);
    assert_eq!(metadata_sink_name(""), None);
}

#[test]
fn registry_is_private_encrypts_tokens_and_supports_lifecycle() {
    let dir = TempDir::new("registry");
    let _env = EnvVar::set("XDG_CONFIG_HOME", dir.path());

    clients::ensure().unwrap();
    assert!(clients::path().exists());
    let mode = std::fs::metadata(clients::path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "clients registry must be 0600");
    let master_mode = std::fs::metadata(dir.path().join("lgx/master.key"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(master_mode, 0o600, "master key must be 0600");

    clients::add(clients::Entry {
        id: "archlinux".into(),
        name: "arch".into(),
        token: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        created: 65,
    })
    .unwrap();
    clients::add(clients::Entry {
        id: "macbook".into(),
        name: "mac".into(),
        token: "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210".into(),
        created: 66,
    })
    .unwrap();

    let token = clients::token_for("archlinux").unwrap().unwrap();
    assert_eq!(token.len(), 64);
    let trusted = clients::valid_tokens().unwrap();
    assert_eq!(trusted.len(), 3, "ensure() seeds the default entry");
    assert_eq!(
        trusted.get("archlinux").copied(),
        Some(clients::token_fingerprint(&token))
    );
    assert_ne!(
        clients::token_fingerprint(&token),
        clients::token_fingerprint("another")
    );

    // The on-disk registry never contains the plaintext token.
    let raw = std::fs::read_to_string(clients::path()).unwrap();
    assert!(raw.contains("# lgx clients"));
    assert!(!raw.contains(&token), "token must be sealed on disk");

    let rotated = clients::rotate("arch").unwrap().unwrap();
    assert_ne!(rotated, token);
    assert_eq!(clients::token_for("archlinux").unwrap().as_deref(), Some(rotated.as_str()));
    assert!(clients::rotate("missing").unwrap().is_none());

    assert_eq!(clients::revoke("arch").unwrap(), 1);
    assert!(clients::token_for("archlinux").unwrap().is_none());
    assert_eq!(clients::revoke("arch").unwrap(), 0);
    assert_eq!(clients::revoke("nobody").unwrap(), 0);
}

#[test]
fn registry_names_are_reduced_to_safe_values() {
    assert_eq!(clients::clean_name("arch linux"), "archlinux");
    assert_eq!(clients::clean_name("my/client\tid!"), "myclientid");
    assert_eq!(clients::clean_name("..."), "...");
    assert_eq!(clients::clean_name(""), "client");
    assert_eq!(clients::clean_name("!!!"), "client");
    assert_eq!(clients::clean_name(&"a".repeat(100)).len(), 64);
}
