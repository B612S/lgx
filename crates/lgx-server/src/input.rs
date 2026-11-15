//! Virtual input devices inside the guest.
//!
//! Runs in a dedicated thread with its own Wayland connection: Wayland objects
//! are not `Send`, and input must be accepted from the TCP stream while
//! capture runs in its own loop.

use std::io::Write;
use std::os::fd::AsFd;
use std::sync::mpsc::{Receiver, Sender};

use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_pointer, wl_seat};
use wayland_client::{Connection, QueueHandle};

use crate::protocols::{virtual_keyboard, virtual_pointer};

struct InputState;

macro_rules! no_events {
    ($iface:ty) => {
        impl wayland_client::Dispatch<$iface, ()> for InputState {
            fn event(
                _: &mut Self,
                _: &$iface,
                _: <$iface as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<InputState>,
            ) {
            }
        }
    };
}

no_events!(wl_seat::WlSeat);
no_events!(wayland_client::protocol::wl_pointer::WlPointer);
no_events!(wayland_client::protocol::wl_keyboard::WlKeyboard);
no_events!(virtual_pointer::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
no_events!(virtual_pointer::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
no_events!(virtual_keyboard::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
no_events!(virtual_keyboard::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);

impl wayland_client::Dispatch<wayland_client::protocol::wl_registry::WlRegistry, wayland_client::globals::GlobalListContents>
    for InputState
{
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_registry::WlRegistry,
        _: <wayland_client::protocol::wl_registry::WlRegistry as wayland_client::Proxy>::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<InputState>,
    ) {
    }
}

pub enum Cmd {
    Keymap(Vec<u8>),
    Key { code: u32, state: u32, time: u32 },
    Btn { code: u32, state: u32, time: u32 },
    Motion { x: u32, y: u32, time: u32 },
    Wheel { axis: u32, steps: i32, time: u32 },
    Mods { depressed: u32, latched: u32, locked: u32, group: u32 },
    Gamepad {
        pad: u8,
        connected: bool,
        buttons: u32,
        axes: [i16; 6],
        hat: [i8; 2],
    },
}

/// Kernel-level input devices created by the `lgx-uinput` helper. They work for
/// every application and every compositor shortcut, like Sunshine's uinput
/// backend, and do not depend on Wayland protocol details.
/// Why the input loop returned.
enum Stop {
    /// The helper went away (restart or package upgrade): reconnect.
    Peer,
    /// The server is shutting down and dropped the command channel.
    Shutdown,
}

fn run_uinput(
    rx: &Receiver<Cmd>,
    mut sock: std::os::unix::net::UnixStream,
) -> std::io::Result<Stop> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::sync::mpsc::RecvTimeoutError;
    let fd = sock.as_raw_fd();
    loop {
        let cmd = match rx.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(cmd) => cmd,
            Err(RecvTimeoutError::Timeout) => {
                // Nothing to write, so check the socket itself: the helper
                // unlinks and re-binds its path when it restarts.
                if peer_gone(fd) {
                    return Ok(Stop::Peer);
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(Stop::Shutdown),
        };
        let ok = match cmd {
            Cmd::Key { code, state, .. } => {
                let mut b = [b'K', 0, 0, state as u8];
                b[1..3].copy_from_slice(&(code as u16).to_le_bytes());
                sock.write_all(&b)
            }
            Cmd::Btn { code, state, .. } => {
                let mut b = [b'B', 0, 0, state as u8];
                b[1..3].copy_from_slice(&(code as u16).to_le_bytes());
                sock.write_all(&b)
            }
            Cmd::Motion { x, y, .. } => {
                let mut b = [0u8; 9];
                b[0] = b'A';
                b[1..5].copy_from_slice(&(x as i32).to_le_bytes());
                b[5..9].copy_from_slice(&(y as i32).to_le_bytes());
                sock.write_all(&b)
            }
            Cmd::Wheel { steps, .. } => {
                let mut b = [0u8; 5];
                b[0] = b'W';
                b[1..5].copy_from_slice(&steps.to_le_bytes());
                sock.write_all(&b)
            }
            Cmd::Gamepad { pad, connected, buttons, axes, hat } => {
                if !std::path::Path::new("/run/lgx-input.gamepad").exists() {
                    static WARNED: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        eprintln!(
                            "lgx-server: gamepad ignored: update and restart lgx-input"
                        );
                    }
                    continue;
                }
                let mut b = [0u8; 21];
                b[0] = b'G';
                b[1] = pad;
                b[2] = connected as u8;
                b[3..7].copy_from_slice(&buttons.to_le_bytes());
                for (i, a) in axes.iter().enumerate() {
                    b[7 + i * 2..9 + i * 2].copy_from_slice(&a.to_le_bytes());
                }
                b[19] = hat[0] as u8;
                b[20] = hat[1] as u8;
                sock.write_all(&b)
            }
            // uinput devices track modifiers themselves
            Cmd::Mods { .. } | Cmd::Keymap(_) => Ok(()),
        };
        if ok.is_err() {
            return Ok(Stop::Peer);
        }
    }
}

fn peer_gone(fd: std::os::fd::RawFd) -> bool {
    let mut p = libc::pollfd { fd, events: 0, revents: 0 };
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n > 0 && (p.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0
}

const SOCKET: &str = "/run/lgx-input.sock";

pub fn spawn(seat_name: Option<String>) -> Sender<Cmd> {
    let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
    std::thread::Builder::new()
        .name("lgx-input".into())
        .spawn(move || serve(rx, SOCKET, seat_name))
        .expect("input thread");
    tx
}

fn serve(rx: Receiver<Cmd>, path: &str, seat_name: Option<String>) {
    // The helper unlinks and re-creates its socket when the package is
    // upgraded or the unit restarts, which closes our connection. Keep
    // reconnecting instead of losing input until the server restarts.
    let mut connected_once = false;
    let mut waiting_logged = false;
    loop {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(sock) => {
                if !connected_once {
                    eprintln!("lgx-server: input: kernel uinput devices");
                }
                connected_once = true;
                waiting_logged = false;
                match run_uinput(&rx, sock) {
                    Ok(Stop::Shutdown) => return,
                    Ok(Stop::Peer) => {}
                    Err(e) => eprintln!("lgx-server: input: {e}"),
                }
                eprintln!("lgx-server: input: helper connection lost, reconnecting");
            }
            Err(e) => {
                if !connected_once {
                    eprintln!("lgx-server: input: uinput helper not found, using Wayland virtual devices");
                    if let Err(e) = run(&rx, seat_name.clone()) {
                        eprintln!("lgx-server: input: {e}");
                    }
                    return;
                }
                if !waiting_logged {
                    eprintln!("lgx-server: input: waiting for the uinput helper ({e})");
                    waiting_logged = true;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn run(rx: &Receiver<Cmd>, _seat_name: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init::<InputState>(&conn)?;
    let qh: QueueHandle<InputState> = queue.handle();

    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ())?;
    let vptr_mgr = globals.bind::<virtual_pointer::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, _, _>(
        &qh,
        1..=2,
        (),
    )?;
    let vkb_mgr = globals.bind::<virtual_keyboard::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, _, _>(
        &qh,
        1..=1,
        (),
    )?;

    let vptr = vptr_mgr.create_virtual_pointer(Some(&seat), &qh, ());
    let vkb = vkb_mgr.create_virtual_keyboard(&seat, &qh, ());
    let mut vptr_version = 1;
    if let Some(v) = globals.contents().with_list(|list| {
        list.iter()
            .find(|g| g.interface == "zwlr_virtual_pointer_manager_v1")
            .map(|g| g.version)
    }) {
        vptr_version = v.min(2);
    }
    conn.flush()?;

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Keymap(bytes) => {
                let file = tempfile_in_shm(&bytes)?;
                let size = bytes.len() as u32;
                vkb.keymap(1, file.as_fd(), size);
            }
            Cmd::Key { code, state, time } => {
                vkb.key(time, code, state);
            }
            Cmd::Btn { code, state, time } => {
                vptr.button(time, code, wl_pointer::ButtonState::try_from(state as u32).unwrap_or(wl_pointer::ButtonState::Pressed));
                if vptr_version >= 2 {
                    vptr.frame();
                }
            }
            Cmd::Motion { x, y, time } => {
                vptr.motion_absolute(time, x, y, 65535, 65535);
                if vptr_version >= 2 {
                    vptr.frame();
                }
            }
            Cmd::Mods { depressed, latched, locked, group } => {
                vkb.modifiers(depressed, latched, locked, group);
            }
            Cmd::Wheel { axis, steps, time } => {
                let a = if axis == 1 {
                    wl_pointer::Axis::HorizontalScroll
                } else {
                    wl_pointer::Axis::VerticalScroll
                };
                vptr.axis(time, a, steps as f64 * 10.0);
                if vptr_version >= 2 {
                    vptr.frame();
                }
            }
            // The Wayland fallback has no gamepad device.
            Cmd::Gamepad { .. } => {}
        }
        // We do not need events: virtual devices do not emit any. Should the
        // compositor send something anyway, flush to be safe.
        conn.flush().ok();
    }
    Ok(())
}

/// File in /dev/shm holding the keymap; the compositor mmaps it by fd.
fn tempfile_in_shm(bytes: &[u8]) -> std::io::Result<std::fs::File> {
    let mut path = std::env::temp_dir();
    path.push(format!("lgx-keymap-{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)?;
    f.write_all(bytes)?;
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    /// The helper unlinks and re-binds its socket on every restart; the input
    /// thread must follow it instead of dying with the old connection.
    #[test]
    fn reconnects_when_the_helper_socket_drops() {
        let dir = std::env::temp_dir().join(format!("lgx-input-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let helper = std::thread::spawn(move || {
            // One packet per accepted connection, then hang up like a helper
            // that was restarted; two packets means we saw both connections.
            let mut seen = Vec::new();
            let mut accepts = 0;
            while seen.len() < 2 {
                let (mut conn, _) = listener.accept().unwrap();
                accepts += 1;
                let mut buf = [0u8; 4];
                if conn.read_exact(&mut buf).is_ok() {
                    seen.push(buf);
                }
            }
            (accepts, seen)
        });

        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let serve_path = path.to_str().unwrap().to_string();
        std::thread::spawn(move || serve(rx, &serve_path, None));

        // Keep sending: the event racing the hang-up may be dropped, the next
        // one must reach the reconnected stream.
        let mut sent = 0;
        while sent < 20 && !helper.is_finished() {
            let _ = tx.send(Cmd::Key { code: 30, state: 1, time: 0 });
            sent += 1;
            std::thread::sleep(Duration::from_millis(50));
        }

        let (accepts, seen) = helper.join().unwrap();
        assert!(accepts >= 2, "input thread never reconnected");
        for packet in &seen {
            assert_eq!(packet, &[b'K', 30, 0, 1]);
        }
        let _ = std::fs::remove_file(&path);
    }
}
