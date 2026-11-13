//! Per-client token registry.
//!
//! Each paired client gets its own random id and token. The running server
//! looks the token up by id for every handshake and watches this file so a
//! revoked id also loses its active session.
//!
//! Format (tab separated, mode 0600):
//!   id<TAB>name<TAB>token<TAB>created-unix

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use lgx_proto::auth;
use ring::aead;
use ring::rand::{SecureRandom, SystemRandom};

pub const DEFAULT_ID: &str = "default";

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub token: String,
    pub created: u64,
}

pub fn path() -> PathBuf {
    auth::config_path().with_file_name("clients")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn clean_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "client".to_string()
    } else {
        cleaned
    }
}

fn parse(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(4, '\t');
        let (Some(id), Some(name), Some(token)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let created = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        if id.is_empty() || name.is_empty() || token.is_empty() {
            continue;
        }
        out.push(Entry {
            id: id.to_string(),
            name: name.to_string(),
            token: token.to_string(),
            created,
        });
    }
    out
}

fn master_path() -> PathBuf {
    path().with_file_name("master.key")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    for i in (0..b.len()).step_by(2) {
        let hi = (b[i] as char).to_digit(16)? as u8;
        let lo = (b[i + 1] as char).to_digit(16)? as u8;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn master_key() -> io::Result<[u8; 32]> {
    let p = master_path();
    match fs::read(&p) {
        Ok(v) if v.len() == 32 => {
            let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
            Ok(v.try_into().expect("32 bytes"))
        }
        Ok(_) => Err(io::Error::other("master.key has the wrong length")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut key = [0u8; 32];
            SystemRandom::new()
                .fill(&mut key)
                .map_err(|_| io::Error::other("system CSPRNG unavailable"))?;
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&p)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    f.write_all(&key)?;
                    f.sync_all()?;
                    if let Some(dir) = p.parent() {
                        if let Ok(d) = File::open(dir) {
                            let _ = d.sync_all();
                        }
                    }
                    Ok(key)
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => master_key(),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

fn seal_token(master: &[u8; 32], token: &str) -> io::Result<String> {
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, master)
        .map_err(|_| io::Error::other("token key"))?;
    let key = aead::LessSafeKey::new(key);
    let mut nonce = [0u8; 12];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| io::Error::other("system CSPRNG unavailable"))?;
    let mut ct = token.as_bytes().to_vec();
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::from(b"lgx-client-token-v1"),
        &mut ct,
    )
    .map_err(|_| io::Error::other("token encryption"))?;
    Ok(format!("enc:{}:{}", hex(&nonce), hex(&ct)))
}

fn open_token(master: &[u8; 32], stored: &str) -> io::Result<String> {
    let rest = stored
        .strip_prefix("enc:")
        .ok_or_else(|| io::Error::other("legacy plaintext token"))?;
    let (nhex, chex) = rest
        .split_once(':')
        .ok_or_else(|| io::Error::other("malformed encrypted token"))?;
    let nonce = unhex(nhex).ok_or_else(|| io::Error::other("malformed token nonce"))?;
    let mut ct = unhex(chex).ok_or_else(|| io::Error::other("malformed token ciphertext"))?;
    if nonce.len() != 12 {
        return Err(io::Error::other("malformed token nonce"));
    }
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, master)
        .map_err(|_| io::Error::other("token key"))?;
    let key = aead::LessSafeKey::new(key);
    let nonce: [u8; 12] = nonce.try_into().expect("12 bytes");
    let pt = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(b"lgx-client-token-v1"),
            &mut ct,
        )
        .map_err(|_| io::Error::other("token decryption failed (master.key changed?)"))?;
    String::from_utf8(pt.to_vec()).map_err(|_| io::Error::other("token is not UTF-8"))
}

fn lock() -> io::Result<File> {
    let p = path().with_extension("lock");
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir)?;
    }
    let f = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&p)?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(f)
}

pub fn load() -> io::Result<Vec<Entry>> {
    match fs::read_to_string(path()) {
        Ok(text) => {
            let _ = fs::set_permissions(path(), fs::Permissions::from_mode(0o600));
            let mut entries = parse(&text);
            let master = master_key()?;
            for e in &mut entries {
                if e.token.starts_with("enc:") {
                    e.token = open_token(&master, &e.token)?;
                }
            }
            Ok(entries)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

pub fn save(entries: &[Entry]) -> io::Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("tmp");
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    writeln!(f, "# lgx clients: id\tname\ttoken\tcreated")?;
    let master = master_key()?;
    let mut sorted: Vec<_> = entries.to_vec();
    sorted.sort_by_key(|e| e.created);
    for e in &sorted {
        let sealed = seal_token(&master, &e.token)?;
        writeln!(f, "{}\t{}\t{}\t{}", e.id, e.name, sealed, e.created)?;
    }
    f.sync_all()?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, &p)?;
    fs::set_permissions(&p, fs::Permissions::from_mode(0o600))?;
    if let Some(dir) = p.parent() {
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Seed the registry on first use. A pre-registry install has one global
/// token; keep it as the `default` client so existing clients still work.
pub fn ensure() -> io::Result<()> {
    let _guard = lock()?;
    if path().exists() {
        // Re-save so legacy plaintext tokens are moved under master.key.
        let list = load()?;
        save(&list)?;
        return Ok(());
    }
    let token = match auth::load() {
        Some(t) => t,
        None => {
            let t = auth::generate_token();
            let _ = auth::save(&t);
            t
        }
    };
    let e = Entry {
        id: DEFAULT_ID.to_string(),
        name: "default".to_string(),
        token,
        created: now(),
    };
    save(&[e])?;
    eprintln!(
        "lgx-server: initialized client registry at {}",
        path().display()
    );
    Ok(())
}

pub fn set_default(token: &str) -> io::Result<()> {
    let _guard = lock()?;
    let mut list = load()?;
    list.retain(|e| e.id != DEFAULT_ID);
    list.push(Entry {
        id: DEFAULT_ID.to_string(),
        name: "default".to_string(),
        token: token.to_string(),
        created: now(),
    });
    save(&list)
}

pub fn token_for(id: &str) -> io::Result<Option<String>> {
    Ok(load()?.into_iter().find(|e| e.id == id).map(|e| e.token))
}

pub fn add(entry: Entry) -> io::Result<()> {
    let _guard = lock()?;
    let mut list = load()?;
    list.retain(|e| e.id != entry.id);
    list.push(entry);
    save(&list)
}

pub fn revoke(key: &str) -> io::Result<usize> {
    let _guard = lock()?;
    let mut list = load()?;
    let before = list.len();
    list.retain(|e| e.id != key && e.name != key);
    let removed = before - list.len();
    if removed > 0 {
        save(&list)?;
    }
    Ok(removed)
}

pub fn print_list() {
    match load() {
        Ok(list) if list.is_empty() => println!("no clients"),
        Ok(list) => {
            println!("{:<34} {:<20} CREATED", "ID", "NAME");
            for e in list {
                let when = if e.created == 0 {
                    "-".to_string()
                } else {
                    e.created.to_string()
                };
                println!("{:<34} {:<20} {}", e.id, e.name, when);
            }
        }
        Err(e) => eprintln!("lgx-server: clients: {e}"),
    }
}

pub fn token_fingerprint(token: &str) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// Registry id -> token fingerprint. The running server compares this against
/// active clients so a rotated token also terminates the old session.
pub fn valid_tokens() -> Option<HashMap<String, [u8; 32]>> {
    load().ok().map(|list| {
        list.into_iter()
            .map(|e| (e.id, token_fingerprint(&e.token)))
            .collect()
    })
}

/// Replaces the token of an existing client (matched by id or name). Returns
/// the new token for the administrator to put into the client profile.
pub fn rotate(key: &str) -> io::Result<Option<String>> {
    let _guard = lock()?;
    let mut list = load()?;
    let Some(pos) = list.iter().position(|e| e.id == key || e.name == key) else {
        return Ok(None);
    };
    let token = auth::generate_token();
    list[pos].token = token.clone();
    save(&list)?;
    Ok(Some(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_seal_roundtrip_and_wrong_key() {
        let master = [7u8; 32];
        let sealed = seal_token(&master, "0123456789abcdef").unwrap();
        assert!(sealed.starts_with("enc:"));
        assert!(!sealed.contains("0123456789abcdef"));
        assert_eq!(open_token(&master, &sealed).unwrap(), "0123456789abcdef");
        let other = [8u8; 32];
        assert!(open_token(&other, &sealed).is_err());
        assert!(open_token(&master, "plaintext").is_err());
        assert!(open_token(&master, "enc:zz:zz").is_err());
    }

    #[test]
    fn fingerprints_are_stable_and_distinct() {
        assert_eq!(token_fingerprint("abc"), token_fingerprint("abc"));
        assert_ne!(token_fingerprint("abc"), token_fingerprint("abd"));
        assert_eq!(token_fingerprint("abc").len(), 32);
    }

    #[test]
    fn names_are_restricted() {
        assert_eq!(clean_name("arch linux"), "archlinux");
        assert_eq!(clean_name("../x"), "..x");
        assert_eq!(clean_name(""), "client");
        assert!(clean_name(&"a".repeat(100)).len() <= 64);
    }
}
