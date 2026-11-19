//! `~/.config/lgx/macos.conf` — settings that must survive a rebuild and that
//! should not sit in the LaunchAgent (the token in particular).
//!
//! The file is plain INI so it can be edited by hand:
//!
//! ```text
//! [network]
//! listen = 192.168.10.161:5902
//! token = <64 hex chars>
//! pair_port = 5904
//! [video]
//! fps = 120
//! bitrate = 80000000
//! display = 0
//! wheel_invert = true
//! adaptive = true
//! ```
//!
//! The command line still wins over the file, so a one-off run can override
//! everything; `--rotate` writes a fresh token here *and* into the LaunchAgent
//! so the plist copy does not go stale.

use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub listen: Option<String>,
    pub token: Option<String>,
    pub pair_port: Option<u16>,
    pub fps: Option<u32>,
    pub bitrate: Option<u32>,
    pub display: Option<usize>,
    pub wheel_invert: Option<bool>,
    pub adaptive: Option<bool>,
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("lgx").join("macos.conf")
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl Config {
    pub fn load() -> Config {
        let mut cfg = Config::default();
        let Ok(text) = std::fs::read_to_string(path()) else {
            return cfg;
        };
        let mut section = String::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.trim().to_ascii_lowercase();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim().trim_matches('"');
            match (section.as_str(), key.as_str()) {
                ("network", "listen") => cfg.listen = Some(value.to_string()),
                ("network", "token") => cfg.token = Some(value.to_string()),
                ("network", "pair_port") => cfg.pair_port = value.parse().ok(),
                ("video", "fps") => cfg.fps = value.parse().ok(),
                ("video", "bitrate") => cfg.bitrate = value.parse().ok(),
                ("video", "display") => cfg.display = value.parse().ok(),
                ("video", "wheel_invert") => cfg.wheel_invert = parse_bool(value),
                ("video", "adaptive") => cfg.adaptive = parse_bool(value),
                _ => {}
            }
        }
        cfg
    }

    pub fn save(&self) -> io::Result<()> {
        let path = path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut out = String::from("# lgx-server-macos settings. Written by --rotate.\n");
        out.push_str("[network]\n");
        if let Some(v) = &self.listen {
            out.push_str(&format!("listen = {v}\n"));
        }
        if let Some(v) = &self.token {
            out.push_str(&format!("token = {v}\n"));
        }
        if let Some(v) = self.pair_port {
            out.push_str(&format!("pair_port = {v}\n"));
        }
        out.push_str("[video]\n");
        if let Some(v) = self.fps {
            out.push_str(&format!("fps = {v}\n"));
        }
        if let Some(v) = self.bitrate {
            out.push_str(&format!("bitrate = {v}\n"));
        }
        if let Some(v) = self.display {
            out.push_str(&format!("display = {v}\n"));
        }
        if let Some(v) = self.wheel_invert {
            out.push_str(&format!("wheel_invert = {}\n", if v { 1 } else { 0 }));
        }
        if let Some(v) = self.adaptive {
            out.push_str(&format!("adaptive = {}\n", if v { 1 } else { 0 }));
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        io::Write::write_all(&mut file, out.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_sections_and_booleans() {
        let dir = std::env::temp_dir().join(format!("lgx-cfg-{}", std::process::id()));
        let lgx = dir.join("lgx");
        std::fs::create_dir_all(&lgx).unwrap();
        std::fs::write(
            lgx.join("macos.conf"),
            "[network]\nlisten = 10.0.0.2:5902\ntoken = deadbeef\npair_port = 5905\n\
             [video]\nfps = 90\nbitrate = 50000000\ndisplay = 1\nwheel_invert = yes\nadaptive = 0\n",
        )
        .unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let cfg = Config::load();
        assert_eq!(cfg.listen.as_deref(), Some("10.0.0.2:5902"));
        assert_eq!(cfg.token.as_deref(), Some("deadbeef"));
        assert_eq!(cfg.pair_port, Some(5905));
        assert_eq!(cfg.fps, Some(90));
        assert_eq!(cfg.bitrate, Some(50_000_000));
        assert_eq!(cfg.display, Some(1));
        assert_eq!(cfg.wheel_invert, Some(true));
        assert_eq!(cfg.adaptive, Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
