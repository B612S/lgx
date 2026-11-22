//! Child-process Qt dialog for a display-mode change. The SDL/Vulkan client
//! never links Qt; `lgx-resize-qt` shows the system-styled question and exits
//! with 0 (change), 1 (change and save) or 2 (cancel).

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

pub struct Dialog {
    child: Child,
}

fn helper_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("lgx-resize-qt");
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("lgx-resize-qt");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

impl Dialog {
    pub fn spawn(name: &str, old: &str, new: &str) -> Option<Self> {
        let helper = match helper_path() {
            Some(p) => p,
            None => {
                eprintln!("lgx-client: display dialog not found (lgx-resize-qt)");
                return None;
            }
        };
        let child = match Command::new(&helper)
            .arg("--name")
            .arg(name)
            .arg("--old")
            .arg(old)
            .arg("--new")
            .arg(new)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "lgx-client: cannot start display dialog {}: {e}",
                    helper.display()
                );
                return None;
            }
        };
        Some(Self { child })
    }

    /// Non-blocking: `Some(code)` once the user answered (0/1 change, 2 cancel).
    pub fn poll(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(2)),
            Ok(None) => None,
            Err(_) => Some(2),
        }
    }
}

impl Drop for Dialog {
    fn drop(&mut self) {
        let _ = self.child.try_wait();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
