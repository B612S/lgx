//! A synthetic PipeWire screencast source used by the portal-capture test.
//!
//! It exports one BGRx video node and pushes a moving gradient at 60 Hz. The
//! server can consume it without the desktop portal, which makes it possible
//! to test format negotiation, NV12 conversion and packet pacing without a
//! compositor dialog. Run it, copy the printed node id, and start the server
//! with `LGX_TEST_PW_NODE=<id>`.
//!
//! Debug tool only: nothing links this example into the shipped binaries.

use pipewire as pw;
use pw::spa;

const WIDTH: u32 = 1280;

fn serialize(obj: spa::pod::Object) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| format!("pod: {e}"))?
    .0
    .into_inner())
}

fn simple_prop(key: u32, value: i32) -> spa::pod::Property {
    spa::pod::Property {
        key,
        flags: spa::pod::PropertyFlags::empty(),
        value: spa::pod::Value::Int(value),
    }
}
const HEIGHT: u32 = 720;
const FPS: u32 = 60;

fn main() {
    if let Err(e) = run() {
        eprintln!("pw-screencast: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    pw::init();
    let main = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main, None)?;
    let core = context.connect_rc(None)?;

    let stream = pw::stream::StreamRc::new(
        core,
        "lgx-test-screencast",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::NODE_NAME => "lgx-test-screencast",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(|stream, _, old, new| {
            eprintln!("pw-screencast: {old:?} -> {new:?}");
            if matches!(new, pw::stream::StreamState::Paused) {
                println!("node {}", stream.node_id());
            }
        })
        .process(|stream, _| {
            use std::sync::atomic::{AtomicU32, Ordering};
            static FRAMES: AtomicU32 = AtomicU32::new(0);
            // Test hook: stop producing after N frames so the idle-keyframe
            // path of the portal consumer can be exercised.
            static FREEZE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
            let freeze = *FREEZE.get_or_init(|| {
                std::env::var("LGX_TEST_PW_FREEZE_AFTER")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            });
            let n = FRAMES.fetch_add(1, Ordering::Relaxed);
            if freeze != 0 && n >= freeze {
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let stride = WIDTH as usize * 4;
            let size = stride * HEIGHT as usize;
            let Some(mapped) = data.data() else {
                return;
            };
            if mapped.len() < size {
                return;
            }
            let frame = &mut mapped[..size];
            let phase = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                / 16) as u8;
            for y in 0..HEIGHT as usize {
                let row = &mut frame[y * stride..(y + 1) * stride];
                for x in 0..WIDTH as usize {
                    let i = x * 4;
                    let v = ((x * 255 / WIDTH as usize) as u8).wrapping_add(y as u8 / 2);
                    row[i] = v.wrapping_add(phase);
                    row[i + 1] = phase.wrapping_mul(2).wrapping_add((y / 3) as u8);
                    row[i + 2] = (255 - v).wrapping_add(phase / 2);
                    row[i + 3] = 255;
                }
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = size as u32;
        })
        .register()?;

    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Id,
            spa::param::video::VideoFormat::BGRx
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Rectangle,
            spa::utils::Rectangle {
                width: WIDTH,
                height: HEIGHT
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Fraction,
            spa::utils::Fraction { num: FPS, denom: 1 }
        ),
    );
    let mut values: Vec<Vec<u8>> = vec![serialize(obj)?];
    // A producer owns the buffer pool: without SPA_PARAM_Buffers the graph
    // negotiates a format but never asks the process callback for data.
    let stride = WIDTH as i32 * 4;
    let buffers = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            simple_prop(1, 4), // SPA_PARAM_BUFFERS_buffers
            simple_prop(2, 1), // blocks
            simple_prop(3, stride * HEIGHT as i32), // size
            simple_prop(4, stride), // stride
            simple_prop(5, 16), // align
            // dataType: MemPtr | MemFd (1 << SPA_DATA_MemPtr | 1 << SPA_DATA_MemFd)
            simple_prop(6, (1 << 1) | (1 << 2)),
        ],
    };
    values.push(serialize(buffers)?);
    let mut refs: Vec<&spa::pod::Pod> = Vec::new();
    for bytes in &values {
        refs.push(spa::pod::Pod::from_bytes(bytes).ok_or("unaligned pod")?);
    }
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut refs,
    )?;
    println!("node {}", stream.node_id());
    main.run();
    Ok(())
}
