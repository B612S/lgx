//! File-clipboard helpers: URI lists, a compact offer format, and the
//! receiver-side state machine shared by both ends.

use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const KIND_FILE: u8 = 0;
pub const KIND_DIR: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: u8,
    pub path: String,
    pub size: u64,
}

/// Local source of one offered entry.
pub struct LocalEntry {
    pub entry: Entry,
    pub source: PathBuf,
}

pub fn encode_offer(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        out.push(e.kind);
        out.extend_from_slice(&(e.path.len() as u32).to_le_bytes());
        out.extend_from_slice(e.path.as_bytes());
        out.extend_from_slice(&e.size.to_le_bytes());
    }
    out
}

pub fn decode_offer(bytes: &[u8]) -> Option<Vec<Entry>> {
    let mut pos = 0usize;
    let take = |pos: &mut usize, n: usize| -> Option<&[u8]> {
        if *pos + n > bytes.len() {
            return None;
        }
        let s = &bytes[*pos..*pos + n];
        *pos += n;
        Some(s)
    };
    let count = u32::from_le_bytes(take(&mut pos, 4)?.try_into().ok()?) as usize;
    if count > 100_000 {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let kind = take(&mut pos, 1)?[0];
        let len = u32::from_le_bytes(take(&mut pos, 4)?.try_into().ok()?) as usize;
        if len == 0 || len > 4096 {
            return None;
        }
        let path = String::from_utf8(take(&mut pos, len)?.to_vec()).ok()?;
        let size = u64::from_le_bytes(take(&mut pos, 8)?.try_into().ok()?);
        out.push(Entry { kind, path, size });
    }
    Some(out)
}

/// Collects regular files and directories below `paths`. Top-level names are
/// made unique, relative paths use `/`, symlinks are skipped.
pub fn collect(paths: &[PathBuf]) -> io::Result<Vec<LocalEntry>> {
    let mut out = Vec::new();
    let mut used = std::collections::HashSet::new();
    for path in paths {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let base = match path.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => continue,
        };
        let mut name = base.clone();
        let mut n = 2;
        while !used.insert(name.clone()) {
            name = format!("{base} ({n})");
            n += 1;
        }
        let is_dir = meta.is_dir();
        out.push(LocalEntry {
            entry: Entry {
                kind: if is_dir { KIND_DIR } else { KIND_FILE },
                path: name.clone(),
                size: if is_dir { 0 } else { meta.len() },
            },
            source: path.clone(),
        });
        if is_dir {
            walk(path, &name, &mut out)?;
        }
    }
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<LocalEntry>) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for de in entries {
        let ft = match de.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_symlink() {
            continue;
        }
        let name = de.file_name().to_string_lossy().into_owned();
        let rel = format!("{prefix}/{name}");
        let meta = match de.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        out.push(LocalEntry {
            entry: Entry {
                kind: if meta.is_dir() { KIND_DIR } else { KIND_FILE },
                path: rel.clone(),
                size: if meta.is_dir() { 0 } else { meta.len() },
            },
            source: de.path(),
        });
        if meta.is_dir() {
            walk(&de.path(), &rel, out)?;
        }
    }
    Ok(())
}

/// Normalizes an offer path: relative components only, no `..`, `/` separator.
pub fn sanitize_rel(path: &str) -> Option<String> {
    let p = Path::new(path);
    if p.is_absolute() {
        return None;
    }
    let mut parts = Vec::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => {
                let s = s.to_str()?;
                if s.is_empty() || s == "." || s == ".." {
                    return None;
                }
                parts.push(s.to_string());
            }
            _ => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

pub fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub fn percent_encode_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~' | b':');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `text/uri-list` for absolute local paths.
pub fn uri_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for p in paths {
        out.push_str("file://");
        out.push_str(&percent_encode_path(p));
        out.push_str("\r\n");
    }
    out
}

pub fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("file://") else { continue };
        let path_part = if let Some(slash) = rest.find('/') {
            let host = &rest[..slash];
            if !host.is_empty() && host != "localhost" {
                continue;
            }
            &rest[slash..]
        } else {
            continue;
        };
        if let Some(p) = percent_decode(path_part) {
            if !p.is_empty() {
                out.push(PathBuf::from(p));
            }
        }
    }
    out
}

pub fn signature(paths: &[PathBuf]) -> String {
    let mut s = String::from("f:");
    for (i, p) in paths.iter().enumerate() {
        if i > 0 {
            s.push('\u{1f}');
        }
        s.push_str(&p.to_string_lossy());
    }
    s
}

pub fn cache_root() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("lgx").join("clipboard")
}

/// Receives one offer into a fresh directory under the cache root.
pub struct Receiver {
    root: PathBuf,
    entries: Vec<Entry>,
    current: usize,
    written: u64,
    file: Option<File>,
    top: Vec<String>,
}

impl Receiver {
    pub fn begin(entries: Vec<Entry>) -> io::Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = cache_root().join(format!("{}-{stamp}", std::process::id()));
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let mut top = Vec::new();
        let mut s = Self {
            root,
            entries,
            current: 0,
            written: 0,
            file: None,
            top: Vec::new(),
        };
        for e in &s.entries {
            let Some(rel) = sanitize_rel(&e.path) else {
                continue;
            };
            let first = rel.split('/').next().unwrap_or(&rel).to_string();
            if !top.contains(&first) {
                top.push(first);
            }
        }
        s.top = top;
        s.advance()?;
        Ok(s)
    }

    fn advance(&mut self) -> io::Result<()> {
        self.written = 0;
        while self.current < self.entries.len() {
            let e = self.entries[self.current].clone();
            let Some(rel) = sanitize_rel(&e.path) else {
                self.current += 1;
                continue;
            };
            let path = self.root.join(&rel);
            if e.kind == KIND_DIR {
                fs::create_dir_all(&path)?;
                self.current += 1;
                continue;
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut opts = OpenOptions::new();
            opts.create(true).truncate(true).write(true);
            #[cfg(unix)]
            opts.mode(0o600);
            self.file = Some(opts.open(&path)?);
            return Ok(());
        }
        self.file = None;
        Ok(())
    }

    pub fn data(&mut self, mut chunk: &[u8]) -> io::Result<()> {
        while !chunk.is_empty() {
            if self.file.is_none() {
                if self.current >= self.entries.len() {
                    return Ok(());
                }
                self.advance()?;
                if self.file.is_none() {
                    continue;
                }
            }
            let size = self.entries[self.current].size;
            let left = size.saturating_sub(self.written);
            let n = (chunk.len() as u64).min(left) as usize;
            if n > 0 {
                if let Some(f) = self.file.as_mut() {
                    f.write_all(&chunk[..n])?;
                }
                self.written += n as u64;
                chunk = &chunk[n..];
            }
            if self.written >= size {
                self.file = None;
                self.current += 1;
                self.advance()?;
            }
        }
        Ok(())
    }

    pub fn finish(&mut self) -> io::Result<Vec<PathBuf>> {
        if let Some(f) = self.file.take() {
            f.sync_all()?;
        }
        let mut out = Vec::new();
        for name in &self.top {
            let p = self.root.join(name);
            if p.exists() {
                out.push(p);
            }
        }
        Ok(out)
    }
}

/// Reads a file in 64 KiB chunks and passes each chunk to `send`.
pub fn stream_file<F>(path: &Path, send: &mut F) -> io::Result<()>
where
    F: FnMut(&[u8]) -> io::Result<()>,
{
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        send(&buf[..n])?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_roundtrip() {
        let e = vec![
            Entry { kind: KIND_DIR, path: "dir".into(), size: 0 },
            Entry { kind: KIND_FILE, path: "dir/a b.txt".into(), size: 42 },
        ];
        let enc = encode_offer(&e);
        assert_eq!(decode_offer(&enc).unwrap(), e);
    }

    #[test]
    fn uri_roundtrip() {
        let p = PathBuf::from("/tmp/a b/файл.txt");
        let list = uri_list(&[p.clone()]);
        assert!(list.starts_with("file:///tmp/a%20b/"));
        assert_eq!(parse_uri_list(&list), vec![p]);
    }

    #[test]
    fn reject_traversal() {
        assert!(sanitize_rel("../etc/passwd").is_none());
        assert!(sanitize_rel("/etc/passwd").is_none());
        assert_eq!(sanitize_rel("a/b.txt").as_deref(), Some("a/b.txt"));
    }
}
