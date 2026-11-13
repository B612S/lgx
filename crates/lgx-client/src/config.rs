//! Client configuration stored in `~/.config/lgx/client.conf`.
//!
//! Settings are grouped into sections:
//!
//! ```ini
//! [network]
//! server = 192.168.122.25:5902
//!
//! [video]
//! width = 1920
//! height = 1080
//! fullscreen = false
//! vsync = false
//!
//! [audio]
//! enabled = true
//! ```
//!
//! Older flat files are migrated on load.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("lgx").join("client.conf")
}

fn canonical(key: &str) -> String {
    let key = key.trim();
    if key.contains('.') {
        return key.to_string();
    }
    let (section, name) = match key {
        "server" | "token" => ("network", key),
        "width" | "height" | "fullscreen" | "vsync" | "decoder" | "cursor" => {
            ("video", key)
        }
        "audio" => ("audio", "enabled"),
        "audio_connect" => ("audio", "connect"),
        _ => ("misc", key),
    };
    format!("{section}.{name}")
}

#[derive(Default, Clone)]
pub struct Config {
    values: BTreeMap<String, String>,
    pub existed: bool,
}

impl Config {
    pub fn load() -> Self {
        let mut values = BTreeMap::new();
        let mut section = String::new();
        let mut sectioned = false;
        let existed = match std::fs::read_to_string(path()) {
            Ok(text) => {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if line.starts_with('[') && line.ends_with(']') {
                        section = line[1..line.len() - 1].trim().to_string();
                        sectioned = true;
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
                true
            }
            Err(_) => false,
        };
        let cfg = Self { values, existed };
        if existed && !sectioned {
            let _ = cfg.save();
        }
        cfg
    }

    /// Loads the client config, creating a sectioned default file when it is
    /// missing. Existing files are never overwritten.
    pub fn load_or_create() -> Self {
        let cfg = Self::load();
        if cfg.existed {
            return cfg;
        }
        let mut cfg = Self::default();
        cfg.set("server", "192.168.122.25:5902");
        cfg.set("width", 1920);
        cfg.set("height", 1080);
        cfg.set("fullscreen", "false");
        cfg.set("vsync", "false");
        cfg.set("decoder", "auto");
        cfg.set("cursor", "remote");
        cfg.set("audio", "true");
        cfg.set("audio_connect", "");
        if let Err(e) = cfg.save() {
            eprintln!("lgx-client: cannot write {}: {e}", path().display());
        }
        cfg.existed = true;
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

    pub fn get_u32(&self, key: &str) -> Option<u32> {
        self.get(key).and_then(|v| v.parse().ok())
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(|v| match v {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
    }

    pub fn save(&self) -> io::Result<()> {
        let p = path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
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
        for section in ["network", "video", "audio", "misc"] {
            if let Some(values) = sections.get(section) {
                out.push_str(&format!("\n[{section}]\n"));
                for (key, value) in values {
                    out.push_str(&format!("{key} = {value}\n"));
                }
            }
        }
        for (section, values) in &sections {
            if ["network", "video", "audio", "misc"].contains(&section.as_str()) {
                continue;
            }
            out.push_str(&format!("\n[{section}]\n"));
            for (key, value) in values {
                out.push_str(&format!("{key} = {value}\n"));
            }
        }
        std::fs::write(p, out)
    }
}
