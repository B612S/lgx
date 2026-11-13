//! Pre-shared token handling.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("lgx").join("token")
}

#[derive(Clone, Debug)]
pub struct Credentials {
    pub id: String,
    pub token: String,
}

/// Client credentials. One line is the legacy global token (`default`);
/// two lines are `<id>` and `<token>`.
pub fn load_credentials() -> Option<Credentials> {
    let text = std::fs::read_to_string(config_path()).ok()?;
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next()?.to_string();
    match lines.next() {
        None => Some(Credentials {
            id: "default".to_string(),
            token: first,
        }),
        Some(second) => {
            if first.is_empty() || second.is_empty() {
                None
            } else {
                Some(Credentials {
                    id: first,
                    token: second.to_string(),
                })
            }
        }
    }
}

pub fn save_credentials(id: &str, token: &str) -> io::Result<()> {
    save_two(id, token)
}

fn save_two(id: &str, token: &str) -> io::Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(format!("{id}\n{token}\n").as_bytes())?;
    f.sync_all()?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub fn load() -> Option<String> {
    std::fs::read_to_string(config_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn save(token: &str) -> io::Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(format!("{token}\n").as_bytes())?;
    f.sync_all()?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// 32 hex characters from /dev/urandom.
pub fn generate() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    } else {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        buf.copy_from_slice(&t.to_le_bytes());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time comparison.
pub fn eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}
