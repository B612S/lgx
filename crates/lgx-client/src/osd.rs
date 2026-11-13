//! Spawns the transient Qt OSD for background-audio toggles.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

fn helper_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("lgx-osd-qt");
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("lgx-osd-qt");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn spawn(enabled: bool) -> Option<Child> {
    let helper = helper_path()?;
    Command::new(helper)
        .arg("--state")
        .arg(if enabled { "on" } else { "off" })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}
