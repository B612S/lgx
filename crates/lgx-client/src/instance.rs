//! One client instance per server host.
//!
//! The first process takes an exclusive `flock` on a lock file in
//! `$XDG_RUNTIME_DIR` (falling back to the temp directory). A second launch
//! for the same host finds the lock held and exits before connecting or
//! creating a window. Different hosts get different lock files.

use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

pub struct InstanceLock {
    _file: File,
}

fn key(host: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    host.to_ascii_lowercase().hash(&mut h);
    format!("{:016x}", h.finish())
}

pub fn acquire(host: &str) -> io::Result<Option<InstanceLock>> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let uid = unsafe { libc::geteuid() };
    let path = base.join(format!("lgx-client-{uid}-{}.lock", key(host)));
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(InstanceLock { _file: file }));
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(None)
    } else {
        Err(err)
    }
}
