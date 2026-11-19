//! Guest clipboard through wl-clipboard-rs (text and HTML).

use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::process;

use lgx_proto::file as proto_file;
use wl_clipboard_rs::{paste, watch};


#[derive(Clone, Debug)]
pub enum Clip {
    Text(String),
    Rich { html: String, text: String },
    File { name: String, data: Vec<u8> },
}

fn fnv(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

pub fn signature(c: &Clip) -> String {
    match c {
        Clip::Text(t) => format!("t:{t}"),
        Clip::Rich { html, text } => format!("h:{html}{text}"),
        Clip::File { name, data } => {
            format!("f:{}:{}:{:016x}", name, data.len(), fnv(data))
        }
    }
}

pub fn set_text(text: &str) -> io::Result<()> {
    let mut child = Command::new("timeout")
        .args(["5", "wl-copy"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    child.wait()?;
    Ok(())
}

pub fn set_rich(html: &str, text: &str) -> io::Result<()> {
    use wl_clipboard_rs::copy::{copy_multi, MimeSource, MimeType, Options, Source};
    // Plain text must be the first text source: wl-clipboard-rs uses it
    // for the additional text MIME fallbacks. HTML comes second.
    let sources = vec![
        MimeSource {
            source: Source::Bytes(text.as_bytes().to_vec().into()),
            mime_type: MimeType::Text,
        },
        MimeSource {
            source: Source::Bytes(html.as_bytes().to_vec().into()),
            mime_type: MimeType::Specific("text/html".to_string()),
        },
    ];
    copy_multi(Options::new(), sources)
        .map_err(|e| io::Error::other(format!("wl-copy rich: {e}")))
}



fn clipboard_cache() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("lgx")
        .join("clipboard")
}

/// Publishes one regular file on the Wayland clipboard as a file URL. The
/// file manager reads the URI path, so the temp file must outlive this call.
pub fn set_file(name: &str, data: &[u8]) -> io::Result<()> {
    if data.len() as u64 > proto_file::MAX_BYTES {
        return Err(io::Error::other("file clipboard exceeds the size limit"));
    }
    let name = proto_file::sanitize_name(name)
        .ok_or_else(|| io::Error::other("invalid clipboard file name"))?;
    let root = clipboard_cache();
    fs::create_dir_all(&root)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = root.join(format!("clip-{stamp}-{}", process::id()));
    fs::create_dir_all(&dir)?;
    let path = dir.join(&name);
    fs::write(&path, data)?;
    // Drop older temp copies; the current clipboard keeps only its own dir.
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            if entry.path() != dir {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
    let uri = format!("file://{}", percent_encode(path.as_os_str().as_encoded_bytes()));
    let uri_list = format!("{uri}\r\n");
    let gnome = format!("copy\n{uri}\r\n");
    use wl_clipboard_rs::copy::{copy_multi, MimeSource, MimeType, Options, Source};
    let sources = vec![
        MimeSource {
            source: Source::Bytes(uri_list.into_bytes().into()),
            mime_type: MimeType::Specific("text/uri-list".to_string()),
        },
        MimeSource {
            source: Source::Bytes(gnome.into_bytes().into()),
            mime_type: MimeType::Specific("x-special/gnome-copied-files".to_string()),
        },
    ];
    // copy_multi serves the selection until it is replaced; do not block the
    // video reader thread.
    std::thread::spawn(move || {
        if let Err(e) = copy_multi(Options::new(), sources) {
            eprintln!("lgx: file clipboard: {e}");
        }
    });
    Ok(())
}

fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for b in bytes {
        let c = *b;
        if c.is_ascii_alphanumeric() || matches!(c, b'/' | b'-' | b'.' | b'_' | b'~') {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path = if rest.starts_with('/') {
        rest.to_string()
    } else {
        let slash = rest.find('/')?;
        format!("/{}", &rest[slash + 1..])
    };
    let bytes = percent_decode(&path)?;
    let text = String::from_utf8(bytes).ok()?;
    Some(PathBuf::from(text))
}

/// One regular file from a `text/uri-list` or GNOME copied-files payload.
fn file_from_uri_text(text: &str) -> Option<Clip> {
    let mut uris: Vec<&str> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "copy" || line == "cut" {
            continue;
        }
        if line.starts_with("file://") {
            uris.push(line);
        }
    }
    // Multiple files are intentionally not turned into one; the local
    // clipboard stays intact instead of becoming a text file.
    if uris.len() != 1 {
        return None;
    }
    let path = uri_to_path(uris[0])?;
    let metadata = fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > proto_file::MAX_BYTES {
        return None;
    }
    let data = fs::read(&path).ok()?;
    let name = path.file_name()?.to_str()?.to_string();
    Some(Clip::File { name, data })
}

fn read_offer(mime_types: &[String], offer: &mut watch::Offer<'_>) -> Option<Clip> {
    fn read(offer: &mut watch::Offer<'_>, mime: &str) -> Option<String> {
        let mut pipe = offer.receive(mime).ok()?;
        let mut text = String::new();
        pipe.read_to_string(&mut text).ok()?;
        Some(text)
    }
    // A file offer must never fall through to text: that is what turned a
    // copied file into a text file in the other direction.
    for mime in ["text/uri-list", "x-special/gnome-copied-files"] {
        if mime_types.iter().any(|m| m == mime) {
            if let Some(text) = read(offer, mime) {
                if let Some(clip) = file_from_uri_text(&text) {
                    return Some(clip);
                }
                eprintln!("lgx: file clipboard offer could not be parsed");
            }
            return None;
        }
    }
    if let Some(html_mime) = mime_types.iter().find(|m| m.starts_with("text/html")) {
        if let Some(html) = read(offer, html_mime) {
            if !html.is_empty() {
                let text = ["text/plain;charset=utf-8", "text/plain"]
                    .iter()
                    .find_map(|m| read(offer, m))
                    .unwrap_or_default();
                return Some(Clip::Rich { html, text });
            }
        }
    }
    for mime in ["text/plain;charset=utf-8", "text/plain"] {
        if let Some(text) = read(offer, mime) {
            if !text.is_empty() {
                return Some(Clip::Text(text));
            }
        }
    }
    None
}

/// Event-driven clipboard watcher. The first event may be skipped on the
/// server so an old guest clipboard cannot overwrite the host on connect.
pub fn watch(mut on_clip: impl FnMut(Clip) + Send + 'static, send_initial: bool) {
    std::thread::Builder::new()
        .name("lgx-clip-watch".into())
        .spawn(move || {
            let mut watcher = match watch::Watcher::new(
                watch::ClipboardType::Regular,
                paste::Seat::Unspecified,
            ) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("lgx: clipboard watcher: {e}");
                    return;
                }
            };
            let mut first = true;
            loop {
                match watcher.next_event() {
                    Ok(Some(watch::ClipboardEvent::Changed { mime_types, mut offer, .. })) => {
                        let initial = first;
                        first = false;
                        if initial && !send_initial {
                            continue;
                        }
                        if let Some(clip) = read_offer(&mime_types, &mut offer) {
                            on_clip(clip);
                        }
                    }
                    Ok(Some(watch::ClipboardEvent::Cleared { .. })) => {}
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("lgx: clipboard watcher: {e}");
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        })
        .ok();
}
