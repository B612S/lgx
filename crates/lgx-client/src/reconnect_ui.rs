//! Child-process Qt reconnect dialog. The SDL/Vulkan client never links Qt;
//! `lgx-reconnect-qt` shows the system-styled window and exits with 2 when
//! the user cancels.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Dialog {
    child: Child,
}

fn helper_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name("lgx-reconnect-qt");
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("lgx-reconnect-qt");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

impl Dialog {
    pub fn spawn(server: &str, attempt: u32) -> Option<Self> {
        let helper = helper_path()?;
        let child = Command::new(helper)
            .arg("--server")
            .arg(server)
            .arg("--attempt")
            .arg(attempt.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        Some(Self { child })
    }

    /// Waits for the backoff delay. Returns true if the user cancelled or the
    /// helper exited unexpectedly, in which case reconnect must stop.
    pub fn wait_or_cancel(&mut self, delay: Duration) -> bool {
        let deadline = Instant::now() + delay;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {}
                Err(_) => return true,
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn close(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Dialog {
    fn drop(&mut self) {
        let _ = self.child.try_wait();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
