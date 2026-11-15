//! Server configuration stored in `~/.config/lgx/server.conf`.
//!
//! Settings are grouped into sections; command-line arguments override the
//! file for the current run.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("lgx").join("server.conf")
}

fn canonical(key: &str) -> String {
    let key = key.trim();
    if key.contains('.') {
        return key.to_string();
    }
    let (section, name) = match key {
        "listen" | "video_listen" | "audio_listen" | "pair_listen" | "name"
        | "server" | "video_port" | "audio_port" | "pair_port" | "discovery_port" => {
            ("network", key)
        }
        "fps" | "bitrate" | "output" | "low_power" | "async_depth" | "vpp" => {
            ("video", key)
        }
        "audio" => ("audio", "enabled"),
        "no_input" => ("input", "enabled"),
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
        let existed = match std::fs::read_to_string(path()) {
            Ok(text) => {
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
                true
            }
            Err(_) => false,
        };
        Self { values, existed }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values
            .get(&canonical(key))
            .or_else(|| self.values.get(key))
            .map(|s| s.as_str())
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

    pub fn set(&mut self, key: &str, value: impl ToString) {
        self.values.insert(canonical(key), value.to_string());
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
        let mut out = String::from("# lgx server configuration\n");
        for section in ["network", "video", "audio", "input", "misc"] {
            if let Some(values) = sections.get(section) {
                out.push_str(&format!("\n[{section}]\n"));
                for (key, value) in values {
                    out.push_str(&format!("{key} = {value}\n"));
                }
            }
        }
        for (section, values) in &sections {
            if ["network", "video", "audio", "input", "misc"].contains(&section.as_str()) {
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

/// Loads `server.conf`, creating it with defaults when it does not exist.
pub fn load_or_create() -> Config {
    let cfg = Config::load();
    if cfg.existed {
        return cfg;
    }
    let mut cfg = Config::default();
    cfg.set("network.server", "0.0.0.0");
    cfg.set("network.video_port", 5902u32);
    cfg.set("network.audio_port", 5903u32);
    cfg.set("network.pair_port", 5904u32);
    cfg.set("network.discovery_port", 5900u32);
    cfg.set("video.fps", "auto");
    cfg.set("video.bitrate", 40_000_000u32);
    cfg.set("video.output", "");
    cfg.set("video.low_power", "true");
    cfg.set("video.async_depth", 2u32);
    cfg.set("video.vpp", "true");
    cfg.set("audio.enabled", "true");
    cfg.set("input.enabled", "true");
    if let Err(e) = cfg.save() {
        eprintln!("lgx-server: cannot write {}: {e}", path().display());
    }
    cfg
}
