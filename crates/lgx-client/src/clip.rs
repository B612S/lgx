//! Host clipboard through wl-clipboard, including file lists.

use std::io::{self, Write};
use std::process::{Command, Stdio};


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
    let mut child = Command::new("wl-copy")
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


pub fn get() -> io::Result<Clip> {
    let types = Command::new("wl-paste")
        .arg("--list-types")
        .stderr(Stdio::null())
        .output()?;
    let types = String::from_utf8_lossy(&types.stdout);
    if types.lines().any(|t| t.trim().starts_with("text/html")) {
        let html = Command::new("wl-paste")
            .args(["--type", "text/html", "--no-newline"])
            .stderr(Stdio::null())
            .output()?;
        let html = String::from_utf8_lossy(&html.stdout).into_owned();
        if !html.is_empty() {
            let plain = Command::new("wl-paste")
                .arg("--no-newline")
                .stderr(Stdio::null())
                .output()?;
            let text = String::from_utf8_lossy(&plain.stdout).into_owned();
            return Ok(Clip::Rich { html, text });
        }
    }
    // File clipboard is intentionally not synchronized.
    if types.lines().any(|t| t.trim() == "text/uri-list") {
        return Ok(Clip::Text(String::new()));
    }
    let out = Command::new("wl-paste")
        .arg("--no-newline")
        .stderr(Stdio::null())
        .output()?;
    Ok(Clip::Text(String::from_utf8_lossy(&out.stdout).into_owned()))
}
