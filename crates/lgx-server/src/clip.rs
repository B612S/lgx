//! Guest clipboard through wl-clipboard-rs (text and HTML).

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use wl_clipboard_rs::{paste, watch};


#[derive(Clone, Debug)]
pub enum Clip {
    Text(String),
    Rich { html: String, text: String },
}

pub fn signature(c: &Clip) -> String {
    match c {
        Clip::Text(t) => format!("t:{t}"),
        Clip::Rich { html, text } => format!("h:{html}{text}"),
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



fn read_offer(mime_types: &[String], offer: &mut watch::Offer<'_>) -> Option<Clip> {
    if mime_types.iter().any(|m| m == "text/uri-list") {
        return None;
    }
    fn read(offer: &mut watch::Offer<'_>, mime: &str) -> Option<String> {
        let mut pipe = offer.receive(mime).ok()?;
        let mut text = String::new();
        pipe.read_to_string(&mut text).ok()?;
        Some(text)
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
