//! Client-side pure helpers: SDL scancode mapping and client.conf parsing.

use std::os::unix::fs::PermissionsExt;

use lgx_client::config::{self, Config};
use lgx_client::hid::{evdev_from_scancode, HID_TO_EVDEV};
use lgx_tests::{EnvVar, TempDir};

#[test]
fn every_supported_scancode_maps_to_a_real_evdev_code() {
    for sc in 1..256i32 {
        match evdev_from_scancode(sc) {
            Some(code) => {
                assert!(code < 256, "scancode {sc} maps to {code}");
                assert_ne!(code, 0);
                assert_ne!(code, 240);
            }
            None => assert!(
                HID_TO_EVDEV[sc as usize] == 0 || HID_TO_EVDEV[sc as usize] == 240,
                "scancode {sc} has a table entry but no mapping"
            ),
        }
    }
}

#[test]
fn common_keys_match_the_kernel_hid_table() {
    let cases = [
        (4, 30),   // a
        (29, 44),  // z
        (40, 28),  // enter
        (42, 14),  // backspace
        (44, 57),  // space
        (41, 1),   // escape
        (224, 29), // left ctrl
        (225, 42), // left shift
        (226, 56), // left alt
        (227, 125),// left meta
        (58, 59),  // F1
        (68, 87),  // F11
        (69, 88),  // F12
        (127, 113),// mute
        (128, 115),// volume up
        (129, 114),// volume down
    ];
    for (sc, code) in cases {
        assert_eq!(evdev_from_scancode(sc), Some(code), "scancode {sc}");
    }
}

#[test]
fn media_scancodes_are_passed_through_the_consumer_page() {
    let cases = [
        (262, 207), (263, 164), (264, 167), (265, 208), (266, 168),
        (267, 163), (268, 165), (269, 166), (270, 161), (271, 164), (272, 226),
    ];
    for (sc, code) in cases {
        assert_eq!(evdev_from_scancode(sc), Some(code), "scancode {sc}");
    }
    for sc in [0, 1, 256, 300, -1, i32::MIN, i32::MAX] {
        assert_eq!(evdev_from_scancode(sc), None, "scancode {sc}");
    }
}

#[test]
fn reserved_table_slots_are_dropped() {
    for (sc, slot) in HID_TO_EVDEV.iter().enumerate() {
        if *slot == 240 {
            assert_eq!(evdev_from_scancode(sc as i32), None, "slot {sc}");
        }
    }
}

#[test]
fn client_config_defaults_profiles_and_overrides_roundtrip() {
    let dir = TempDir::new("client-config");
    let _env = EnvVar::set("XDG_CONFIG_HOME", dir.path());
    let path = config::path();
    assert_eq!(path, dir.path().join("lgx/client.conf"));

    // Missing file: load_or_create writes the defaults with mode 0600.
    let created = Config::load_or_create();
    assert!(path.exists());
    assert_eq!(created.get("width"), Some("1920"));
    assert_eq!(created.get("audio"), Some("true"));
    assert_eq!(created.get("gamepads"), Some("true"));
    assert_eq!(created.get("clipboard_max_bytes"), Some("1048576"));
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "client.conf holds tokens and must be 0600");

    // A profile can override a shared setting by bare key, qualified key or
    // the global field name.
    lgx_tests::write_file(
        &path,
        b"[video]\nvsync = false\nwidth = 2560\n\n[audio]\nenabled = true\nmute_unfocused = true\n\n\
          [archlinux]\nserver = 10.0.0.5:5902\nvsync = true\nmute_unfocused = false\n",
    );
    let cfg = Config::load();
    assert_eq!(cfg.get_for("archlinux", "video.vsync"), Some("true"));
    assert_eq!(cfg.get_for("other", "video.vsync"), Some("false"));
    assert_eq!(cfg.get_for("other", "video.width"), Some("2560"));
    assert_eq!(cfg.get_u32_for("other", "width"), Some(2560));
    assert_eq!(cfg.get_bool_for("archlinux", "audio.enabled"), Some(true));
    assert_eq!(cfg.get_for("archlinux", "audio.mute_unfocused"), Some("false"));
    assert_eq!(cfg.get_for("other", "mute_unfocused"), Some("true"));
    assert_eq!(cfg.get_for("archlinux", "server"), Some("10.0.0.5:5902"));
    assert_eq!(cfg.instance_get("archlinux", "server"), Some("10.0.0.5:5902"));
    assert_eq!(cfg.instances(), vec!["archlinux".to_string()]);
    assert!(cfg.has_instance("archlinux"));
    assert!(!cfg.has_instance("macbook"));
    assert_eq!(cfg.get_bool_for("archlinux", "vsync"), Some(true));
    assert_eq!(cfg.get_bool_for("archlinux", "not-a-bool"), None);

    // Saving keeps profiles, sections and the private mode.
    let mut cfg = cfg;
    cfg.set_instance("archlinux", "mute_unfocused", false);
    cfg.set("cursor_grab", false);
    cfg.save().unwrap();
    let reloaded = Config::load();
    assert_eq!(reloaded.get_for("archlinux", "audio.mute_unfocused"), Some("false"));
    assert_eq!(reloaded.get_bool_for("archlinux", "cursor_grab"), Some(false));
    assert_eq!(reloaded.instances(), vec!["archlinux".to_string()]);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn profile_names_are_restricted() {
    for good in ["archlinux", "client2", "a", "A-1_b.2", &"x".repeat(64)] {
        assert!(config::validate_name(good).is_ok(), "name {good:?}");
    }
    for bad in [
        "",
        "with space",
        "a/b",
        "a\\b",
        "é",
        "video",
        "audio",
        "input",
        "misc",
        "network",
        "default",
        &"x".repeat(65),
    ] {
        assert!(config::validate_name(bad).is_err(), "name {bad:?}");
    }
}
