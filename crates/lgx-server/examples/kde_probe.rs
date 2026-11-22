//! Ask KWin for a screencast stream without the portal dialog.
//!
//! This is a diagnostic tool: it binds KWin's private
//! `zkde_screencast_unstable_v1`, requests the first output with an embedded
//! cursor, and prints the PipeWire node id. Feed that id to the server via
//! `LGX_TEST_PW_NODE=<id>` with `--capture portal` to test the KDE capture
//! path end to end. Regular deployments use the desktop portal.

use std::time::{Duration, Instant};

use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use lgx_server::protocols::kde_screencast::client::zkde_screencast_stream_unstable_v1::{
    Event as StreamEvent, ZkdeScreencastStreamUnstableV1,
};
use lgx_server::protocols::kde_screencast::client::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1;

struct State {
    outputs: Vec<wl_output::WlOutput>,
    node: Option<u32>,
    failure: Option<String>,
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

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_output::WlOutput,
        _: <wl_output::WlOutput as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZkdeScreencastUnstableV1,
        _: <ZkdeScreencastUnstableV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        event: StreamEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            StreamEvent::Created { node } => {
                println!("node {node}");
                state.node = Some(node);
            }
            StreamEvent::Serial {
                object_serial_hi,
                object_serial_low,
            } => {
                let serial = ((object_serial_hi as u64) << 32) | object_serial_low as u64;
                println!("serial {serial}");
                state.node = Some(object_serial_low);
            }
            StreamEvent::Failed { error } => {
                eprintln!("kde-probe: failed: {error}");
                state.failure = Some(error);
            }
            _ => {}
        }
    }
}

fn main() {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kde-probe: no Wayland display: {e}");
            std::process::exit(1);
        }
    };
    let (globals, mut queue) = match registry_queue_init::<State>(&conn) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("kde-probe: registry: {e}");
            std::process::exit(1);
        }
    };
    let qh = queue.handle();
    let mut state = State {
        outputs: Vec::new(),
        node: None,
        failure: None,
    };
    let screencast = match globals.bind::<ZkdeScreencastUnstableV1, _, _>(&qh, 1..=6, ()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("kde-probe: this compositor has no zkde_screencast: {e}");
            std::process::exit(2);
        }
    };
    for g in globals.contents().clone_list() {
        if g.interface != "wl_output" {
            continue;
        }
        let out: wl_output::WlOutput = globals.registry().bind(g.name, g.version.min(4), &qh, ());
        state.outputs.push(out);
    }
    queue.roundtrip(&mut state).ok();
    let Some(output) = state.outputs.first() else {
        eprintln!("kde-probe: no wl_output");
        std::process::exit(1);
    };
    let _stream = screencast.stream_output(output, 2 /* embedded cursor */, &qh, ());
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.node.is_none() && state.failure.is_none() && Instant::now() < deadline {
        queue.roundtrip(&mut state).ok();
        std::thread::sleep(Duration::from_millis(10));
    }
    if let Some(err) = state.failure {
        eprintln!("kde-probe: {err}");
        std::process::exit(1);
    }
    if state.node.is_none() {
        eprintln!("kde-probe: KWin created no stream");
        std::process::exit(1);
    }
    // Keep the KWin stream (and therefore its PipeWire node) alive while the
    // server consumes it; exiting closes the node.
    let hold = std::env::var("LGX_KDE_HOLD_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120u64);
    std::thread::sleep(Duration::from_secs(hold));
}
