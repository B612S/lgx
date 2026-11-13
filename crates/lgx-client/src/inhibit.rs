//! Inhibits compositor keyboard shortcuts while the lgx window is focused.
//!
//! Wayland compositors (KWin, Hyprland, ...) grab things like Alt+Tab or the
//! Super key before any client sees them. `zwp_keyboard_shortcuts_inhibit_v1`
//! lets the focused surface ask the compositor to stop doing that, which is
//! what makes a remote desktop window behave like a real session.
//!
//! SDL owns the Wayland connection and does not expose this protocol, so the
//! same `wl_display` is adopted through `Backend::from_foreign_display` and the
//! inhibitor is created for SDL's own surface.

use std::ffi::c_void;
use std::io;
use std::sync::Mutex;

use wayland_backend::client::Backend;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use crate::protocols::shortcuts_inhibit::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1 as mgr, zwp_keyboard_shortcuts_inhibitor_v1 as inh,
};

struct State {
    #[allow(dead_code)]
    inhibitor: Option<inh::ZwpKeyboardShortcutsInhibitorV1>,
    active: bool,
}

impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: <wl_registry::WlRegistry as Proxy>::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: <wl_seat::WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<mgr::ZwpKeyboardShortcutsInhibitManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &mgr::ZwpKeyboardShortcutsInhibitManagerV1,
        _: <mgr::ZwpKeyboardShortcutsInhibitManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<inh::ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &inh::ZwpKeyboardShortcutsInhibitorV1,
        event: <inh::ZwpKeyboardShortcutsInhibitorV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let inh::Event::Active = event {
            eprintln!("lgx-client: keyboard shortcuts inhibited");
            state.active = true;
        } else if let inh::Event::Inactive = event {
            eprintln!("lgx-client: keyboard shortcuts released");
            state.active = false;
        }
    }
}

pub struct Guard {
    conn: Connection,
    inhibitor: Mutex<Option<inh::ZwpKeyboardShortcutsInhibitorV1>>,
}

impl Guard {
    /// Destroys the inhibitor, giving compositor shortcuts back.
    pub fn release(self) {
        if let Some(i) = self.inhibitor.lock().unwrap().take() {
            i.destroy();
        }
        let _ = self.conn.flush();
    }
}

/// `display` and `surface` are the raw pointers from `SDL_GetWindowWMInfo`.
pub unsafe fn install(display: *mut c_void, surface: *mut c_void) -> io::Result<Guard> {
    let backend = unsafe { Backend::from_foreign_display(display as *mut _) };
    let conn = Connection::from_backend(backend);
    let (globals, mut queue) = registry_queue_init::<State>(&conn)
        .map_err(|e| io::Error::other(format!("registry: {e}")))?;
    let qh = queue.handle();

    let manager = globals
        .bind::<mgr::ZwpKeyboardShortcutsInhibitManagerV1, _, _>(&qh, 1..=1, ())
        .map_err(|e| io::Error::other(format!("shortcuts inhibit: {e}")))?;
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=7, ())
        .map_err(|e| io::Error::other(format!("seat: {e}")))?;

    let iface = wayland_client::protocol::wl_surface::WlSurface::interface();
    let id = unsafe {
        wayland_backend::client::ObjectId::from_ptr(iface, surface as *mut _)
    }
    .map_err(|e| io::Error::other(format!("surface id: {e:?}")))?;
    let wl_surface = wayland_client::protocol::wl_surface::WlSurface::from_id(&conn, id)
        .map_err(|e| io::Error::other(format!("surface: {e:?}")))?;

    let inhibitor = manager.inhibit_shortcuts(&wl_surface, &seat, &qh, ());
    let mut state = State { inhibitor: Some(inhibitor.clone()), active: false };
    queue.roundtrip(&mut state).ok();
    conn.flush().ok();

    Ok(Guard { conn, inhibitor: Mutex::new(Some(inhibitor)) })
}
