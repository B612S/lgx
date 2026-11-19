//! Client configuration stored in `~/.config/lgx/client.conf`.
//!
//! Global settings are `[video]`, `[audio]` and `[input]`; every client
//! instance is a section named after the instance, e.g. `[archlinux]`:
//!
//! ```ini
//! [video]
//! width = 1920
//!
//! [audio]
//! enabled = true
//!
//! [archlinux]
//! server = 192.168.122.25:5902
//! token = <token>
//! # optional per-profile overrides of any shared setting
//! vsync = true
//! mute_unfocused = false
//! ```
//!
//! Run `lgx-client archlinux`. The section name is also the public client ID
//! used on the wire. There is no `[network]`, no `[default]` and no automatic
//! migration. The file contains tokens, so it is written mode 0600.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("lgx").join("client.conf")
}

pub fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        || matches!(name, "video" | "audio" | "input" | "misc" | "network" | "default")
    {
        return Err(io::Error::other(
            "instance name must be 1..64 chars of A-Z a-z 0-9 - _ . and not a reserved section",
        ));
    }
    Ok(())
}

fn canonical(key: &str) -> String {
    let key = key.trim();
    if key.contains('.') {
        return key.to_string();
    }
    let (section, name) = match key {
        "width" | "height" | "fullscreen" | "vsync" | "decoder" | "present_hz" => {
            ("video", key)
        }
        "audio" | "enabled" => ("audio", "enabled"),
        "mute_unfocused" => ("audio", "mute_unfocused"),
        "audio_connect" => ("audio", "connect"),
        "gamepads" => ("input", "gamepads"),
        "cursor_grab" => ("input", "cursor_grab"),
        "clipboard" => ("input", "clipboard"),
        "clipboard_rich" => ("input", "clipboard_rich"),
        "clipboard_max_bytes" => ("input", "clipboard_max_bytes"),
        _ => ("misc", key),
    };
    format!("{section}.{name}")
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[derive(Default, Clone)]
pub struct Config {
    values: BTreeMap<String, String>,
    pub existed: bool,
}

fn parse(text: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let full = if section.is_empty() {
                canonical(k)
            } else {
                format!("{section}.{k}")
            };
            values.insert(full, v.trim().to_string());
        }
    }
    values
}

impl Config {
    pub fn load() -> Self {
        match fs::read_to_string(path()) {
            Ok(text) => Self {
                values: parse(&text),
                existed: true,
            },
            Err(_) => Self::default(),
        }
    }

    /// Creates the file with video/audio defaults when missing. No migration
    /// from old configs or token files is attempted.
    pub fn load_or_create() -> Self {
        let mut cfg = Self::load();
        let mut changed = !cfg.existed;
        if !cfg.existed {
            cfg.set("width", 1920);
            cfg.set("height", 1080);
            cfg.set("fullscreen", "false");
            cfg.set("vsync", "false");
            cfg.set("present_hz", "0");
            cfg.set("decoder", "auto");
            cfg.set("cursor_grab", "true");
            cfg.set("audio", "true");
        }
        for (key, value) in [
            ("gamepads", "true"),
            ("clipboard", "true"),
            ("clipboard_rich", "true"),
            ("clipboard_max_bytes", "1048576"),
            ("mute_unfocused", "true"),
        ] {
            if cfg.get(key).is_none() {
                cfg.set(key, value);
                changed = true;
            }
        }
        if changed {
            if let Err(e) = cfg.save() {
                eprintln!("lgx-client: cannot write {}: {e}", path().display());
            }
            cfg.existed = true;
        }
        cfg
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values
            .get(&canonical(key))
            .or_else(|| self.values.get(key))
            .map(|s| s.as_str())
    }

    pub fn set(&mut self, key: &str, value: impl ToString) {
        self.values.insert(canonical(key), value.to_string());
    }

    /// Returns a shared setting for `instance`, preferring an override in the
    /// instance's own section over the global `[video]`/`[audio]`/`[input]`
    /// value.
    ///
    /// A profile may spell the override as the bare key (`vsync`), as the
    /// section-qualified key (`video.vsync`), or as the field used inside the
    /// global section (`enabled` for `[audio] enabled`).
    pub fn get_for(&self, instance: &str, key: &str) -> Option<&str> {
        let global = canonical(key);
        let field = global
            .rsplit_once('.')
            .map_or(global.as_str(), |(_, field)| field);
        for candidate in [
            format!("{instance}.{global}"),
            format!("{instance}.{}", key.trim()),
            format!("{instance}.{field}"),
        ] {
            if let Some(value) = self.values.get(&candidate) {
                return Some(value.as_str());
            }
        }
        self.get(key)
    }

    pub fn get_u32_for(&self, instance: &str, key: &str) -> Option<u32> {
        self.get_for(instance, key).and_then(|v| v.parse().ok())
    }

    pub fn get_bool_for(&self, instance: &str, key: &str) -> Option<bool> {
        self.get_for(instance, key).and_then(parse_bool)
    }

    pub fn instance_get(&self, name: &str, key: &str) -> Option<&str> {
        self.values
            .get(&format!("{name}.{key}"))
            .map(|s| s.as_str())
    }

    pub fn set_instance(&mut self, name: &str, key: &str, value: impl ToString) {
        self.values
            .insert(format!("{name}.{key}"), value.to_string());
    }

    pub fn instances(&self) -> Vec<String> {
        let mut names = std::collections::BTreeSet::new();
        for key in self.values.keys() {
            if let Some((section, _)) = key.split_once('.') {
                if ["video", "audio", "input", "misc", "network", "default"]
                    .contains(&section)
                {
                    continue;
                }
                names.insert(section.to_string());
            }
        }
        names.into_iter().collect()
    }

    pub fn has_instance(&self, name: &str) -> bool {
        let prefix = format!("{name}.");
        self.values.keys().any(|k| k.starts_with(&prefix))
    }

    pub fn save(&self) -> io::Result<()> {
        let p = path();
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir)?;
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
        let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for (full, value) in &self.values {
            let (section, key) = match full.split_once('.') {
                Some((s, k)) => (s.to_string(), k.to_string()),
                None => ("misc".to_string(), full.clone()),
            };
            sections
                .entry(section)
                .or_default()
                .insert(key, value.clone());
        }
        let mut out = String::from("# lgx client configuration\n");
        for section in ["video", "audio", "input", "misc"] {
            if let Some(values) = sections.get(section) {
                out.push_str(&format!("\n[{section}]\n"));
                for (key, value) in values {
                    if section == "audio" && key == "connect" && value.is_empty() {
                        continue;
                    }
                    out.push_str(&format!("{key} = {value}\n"));
                }
            }
        }
        for (section, values) in &sections {
            if ["video", "audio", "input", "misc", "network", "default"].contains(&section.as_str()) {
                continue;
            }
            out.push_str(&format!("\n[{section}]\n"));
            for (key, value) in values {
                out.push_str(&format!("{key} = {value}\n"));
            }
        }
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&p)?;
        f.write_all(out.as_bytes())?;
        f.sync_all()?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe_sections() {
        assert!(validate_name("archlinux").is_ok());
        assert!(validate_name("home-2").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("../x").is_err());
        assert!(validate_name("video").is_err());
        assert!(validate_name("input").is_err());
        assert!(validate_name("default").is_err());
    }

    #[test]
    fn instance_values_are_namespaced() {
        let mut cfg = Config::default();
        cfg.set_instance("archlinux", "server", "192.168.122.25:5902");
        cfg.set_instance("archlinux", "token", "abc");
        cfg.set_instance("home", "server", "10.0.0.5:5902");
        assert_eq!(cfg.instance_get("archlinux", "token"), Some("abc"));
        assert_eq!(cfg.instance_get("home", "server"), Some("10.0.0.5:5902"));
        assert_eq!(cfg.instances(), vec!["archlinux", "home"]);
        assert!(cfg.has_instance("archlinux"));
        assert!(!cfg.has_instance("missing"));
    }

    #[test]
    fn shared_input_settings_are_typed() {
        let mut cfg = Config::default();
        cfg.set("gamepads", "false");
        cfg.set("clipboard", "false");
        cfg.set("clipboard_rich", "false");
        cfg.set("clipboard_max_bytes", "4096");
        cfg.set("mute_unfocused", "true");
        assert_eq!(cfg.get_bool_for("other", "gamepads"), Some(false));
        assert_eq!(cfg.get_bool_for("other", "mute_unfocused"), Some(true));
        assert_eq!(cfg.get_bool_for("other", "clipboard"), Some(false));
        assert_eq!(cfg.get_bool_for("other", "clipboard_rich"), Some(false));
        assert_eq!(cfg.get_u32_for("other", "clipboard_max_bytes"), Some(4096));
    }

    #[test]
    fn instance_settings_override_globals() {
        let mut cfg = Config::default();
        cfg.set("width", 1920);
        cfg.set("vsync", "false");
        cfg.set("audio", "true");
        cfg.set("mute_unfocused", "true");
        cfg.set("clipboard", "true");
        cfg.set("gamepads", "true");

        cfg.set_instance("macbook", "width", 2560);
        cfg.set_instance("macbook", "video.vsync", "true");
        cfg.set_instance("macbook", "enabled", "false");
        cfg.set_instance("macbook", "mute_unfocused", "false");
        cfg.set_instance("macbook", "clipboard", "false");
        cfg.set_instance("macbook", "gamepads", "off");

        assert_eq!(cfg.get_u32_for("macbook", "width"), Some(2560));
        assert_eq!(cfg.get_bool_for("macbook", "vsync"), Some(true));
        assert_eq!(cfg.get_bool_for("macbook", "audio"), Some(false));
        assert_eq!(cfg.get_bool_for("macbook", "enabled"), Some(false));
        assert_eq!(cfg.get_bool_for("macbook", "mute_unfocused"), Some(false));
        assert_eq!(cfg.get_bool_for("macbook", "clipboard"), Some(false));
        assert_eq!(cfg.get_bool_for("macbook", "gamepads"), Some(false));

        assert_eq!(cfg.get_u32_for("other", "width"), Some(1920));
        assert_eq!(cfg.get_bool_for("other", "vsync"), Some(false));
        assert_eq!(cfg.get_bool_for("other", "audio"), Some(true));
        assert_eq!(cfg.get_bool_for("other", "mute_unfocused"), Some(true));
        assert_eq!(cfg.get_bool_for("other", "clipboard"), Some(true));
        assert_eq!(cfg.get_bool_for("other", "gamepads"), Some(true));
    }

    #[test]
    fn instance_override_accepts_section_names() {
        let mut cfg = Config::default();
        cfg.set("clipboard_max_bytes", "1048576");
        cfg.set("audio_connect", "10.0.0.1:5903");
        cfg.set_instance("macbook", "input.clipboard_max_bytes", "4096");
        cfg.set_instance("macbook", "connect", "10.0.0.2:5903");
        assert_eq!(
            cfg.get_u32_for("macbook", "clipboard_max_bytes"),
            Some(4096)
        );
        assert_eq!(
            cfg.get_for("macbook", "audio_connect"),
            Some("10.0.0.2:5903")
        );
        assert_eq!(cfg.get_for("other", "audio_connect"), Some("10.0.0.1:5903"));
    }

    #[test]
    fn profile_overrides_parse_from_the_file() {
        let text = "\
[video]
width = 1280
height = 720
vsync = false

[audio]
enabled = true
mute_unfocused = false

[input]
clipboard = false
clipboard_rich = false
clipboard_max_bytes = 4096
cursor_grab = true
gamepads = true

[macbook]
audio_port = 5903
pair_port = 5904
server = 192.168.10.161
token = test-token
video_port = 5902
height = 1080
vsync = true
width = 1920
enabled = false
mute_unfocused = true
clipboard = true
clipboard_max_bytes = 1048576
clipboard_rich = true
cursor_grab = false
gamepads = false
";
        let cfg = Config {
            values: parse(text),
            existed: true,
        };
        assert_eq!(
            cfg.instance_get("macbook", "server"),
            Some("192.168.10.161")
        );
        assert_eq!(cfg.instance_get("macbook", "token"), Some("test-token"));
        assert_eq!(cfg.get_u32_for("macbook", "width"), Some(1920));
        assert_eq!(cfg.get_u32_for("macbook", "height"), Some(1080));
        assert_eq!(cfg.get_bool_for("macbook", "vsync"), Some(true));
        assert_eq!(cfg.get_bool_for("macbook", "audio"), Some(false));
        assert_eq!(cfg.get_bool_for("macbook", "mute_unfocused"), Some(true));
        assert_eq!(cfg.get_bool_for("macbook", "clipboard"), Some(true));
        assert_eq!(
            cfg.get_u32_for("macbook", "clipboard_max_bytes"),
            Some(1048576)
        );
        assert_eq!(cfg.get_bool_for("macbook", "clipboard_rich"), Some(true));
        assert_eq!(cfg.get_bool_for("macbook", "cursor_grab"), Some(false));
        assert_eq!(cfg.get_bool_for("macbook", "gamepads"), Some(false));
        assert_eq!(cfg.instances(), vec!["macbook"]);

        assert_eq!(cfg.get_u32_for("other", "width"), Some(1280));
        assert_eq!(cfg.get_bool_for("other", "audio"), Some(true));
        assert_eq!(cfg.get_bool_for("other", "clipboard"), Some(false));
    }

    #[test]
    fn instance_override_does_not_mask_missing_settings() {
        let mut cfg = Config::default();
        cfg.set("vsync", "true");
        assert_eq!(cfg.get_bool_for("macbook", "vsync"), Some(true));
        assert_eq!(cfg.get_bool_for("macbook", "present_hz"), None);
        cfg.set_instance("macbook", "present_hz", "60");
        assert_eq!(cfg.get_for("macbook", "present_hz"), Some("60"));
    }
}
