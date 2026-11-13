//! Child-process Qt reconnect dialog. The SDL/Vulkan client never links Qt;
//! `lgx-reconnect-qt` shows the system-styled window and exits with 2 when
//! the user cancels.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Dialog {
    child: Child,
    stdin: Option<ChildStdin>,
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
    pub fn spawn(name: &str, attempt: u32) -> Option<Self> {
        let helper = match helper_path() {
            Some(p) => p,
            None => {
                eprintln!("lgx-client: reconnect dialog not found (lgx-reconnect-qt)");
                return None;
            }
        };
        let mut child = match Command::new(&helper)
            .arg("--name")
            .arg(name)
            .arg("--attempt")
            .arg(attempt.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "lgx-client: cannot start reconnect dialog {}: {e}",
                    helper.display()
                );
                return None;
            }
        };
        let stdin = child.stdin.take();
        Some(Self { child, stdin })
    }

    /// Update the attempt number in the already visible window.
    pub fn update(&mut self, attempt: u32) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "attempt {attempt}");
            let _ = stdin.flush();
        }
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
