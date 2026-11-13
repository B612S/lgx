//! Guest clipboard through wl-clipboard, including file lists.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use lgx_proto::files;

#[derive(Clone, Debug)]
pub enum Clip {
    Text(String),
    Files(Vec<PathBuf>),
}

pub fn signature(c: &Clip) -> String {
    match c {
        Clip::Text(t) => format!("t:{t}"),
        Clip::Files(p) => files::signature(p),
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

pub fn set_files(paths: &[PathBuf]) -> io::Result<()> {
    let list = files::uri_list(paths);
    let mut child = Command::new("timeout")
        .args(["5", "wl-copy", "--type", "text/uri-list"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(list.as_bytes())?;
    }
    child.wait()?;
    Ok(())
}

pub fn get() -> io::Result<Clip> {
    let types = Command::new("timeout")
        .args(["3", "wl-paste", "--list-types"])
        .stderr(Stdio::null())
        .output()?;
    let types = String::from_utf8_lossy(&types.stdout);
    if types.lines().any(|t| t.trim() == "text/uri-list") {
        let out = Command::new("timeout")
            .args(["3", "wl-paste", "--type", "text/uri-list", "--no-newline"])
            .stderr(Stdio::null())
            .output()?;
        let list = files::parse_uri_list(&String::from_utf8_lossy(&out.stdout));
        if !list.is_empty() {
            return Ok(Clip::Files(list));
        }
    }
    let out = Command::new("timeout")
        .args(["3", "wl-paste", "--no-newline"])
        .stderr(Stdio::null())
        .output()?;
    Ok(Clip::Text(String::from_utf8_lossy(&out.stdout).into_owned()))
}
