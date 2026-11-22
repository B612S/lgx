//! lgx-client: hardware HEVC decoding and zero-copy presentation.
//!
//! The decoder runs in-process. VAAPI frames are exported as dma-bufs and
//! imported into the GL context; CUDA/NVDEC frames are exported too when the
//! driver allows it, otherwise they are downloaded as NV12 and uploaded.
//! Only the newest frame is kept, so a slow display never builds a queue.

mod audio;
mod clip;
mod config;
mod dec;
mod vk;
mod hid;
mod inhibit;
mod instance;
mod net;
mod pad;
mod pair;
mod osd;
mod protocols;
mod reconnect_ui;
mod resize_ui;

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use lgx_proto::{evdev, msg, Msg};
use sdl3::event::Event;
use sdl3::mouse::{Cursor, SystemCursor};
use sdl3::pixels::PixelFormat;
use sdl3::surface::Surface;

use dec::Backend;

struct Args {
    name: String,
    connect: Option<String>,
    no_window: bool,
    fullscreen: Option<bool>,
    vsync: Option<bool>,
    present_hz: Option<String>,
    screenshot: Option<String>,
    bench: Option<f64>,
    size: Option<(u32, u32)>,
    token: Option<String>,
    client_id: Option<String>,
    audio: Option<bool>,
    audio_connect: Option<String>,
    decoder: Option<String>,
    cursor_grab: Option<bool>,
    gamepads: Option<bool>,
    connections: bool,
    list_profiles: bool,
    check_profile: bool,
    clipboard: Option<bool>,
    clipboard_rich: Option<bool>,
    clipboard_max: Option<usize>,
    mute_unfocused: Option<bool>,
    frames: Option<u64>,
    pair: Option<String>,
    pair_code: Option<String>,
}

struct Settings {
    name: String,
    server: String,
    video_port: u16,
    audio_port: u16,
    width: Option<u32>,
    height: Option<u32>,
    fullscreen: bool,
    vsync: bool,
    /// `0`/`off` = present every decoded frame, `auto` = display refresh.
    present_hz: String,
    audio: bool,
    audio_connect: Option<String>,
    token: String,
    client_id: String,
    decoder: String,
    cursor_grab: bool,
    gamepads: bool,
    clipboard: bool,
    clipboard_rich: bool,
    clipboard_max_bytes: usize,
    mute_unfocused: bool,
    reconnect: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        name: String::new(),
        connect: None,
        no_window: false,
        fullscreen: None,
        vsync: None,
        present_hz: None,
        screenshot: None,
        bench: None,
        size: None,
        token: None,
        client_id: None,
        audio: None,
        audio_connect: None,
        decoder: None,
        cursor_grab: None,
        gamepads: None,
        connections: true,
        list_profiles: false,
        check_profile: false,
        clipboard: None,
        clipboard_rich: None,
        clipboard_max: None,
        mute_unfocused: None,
        frames: None,
        pair: None,
        pair_code: None,
    };
    let mut profile: Option<String> = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--connect" => a.connect = Some(it.next().ok_or("--connect")?),
            "--pair" => a.pair = Some(it.next().ok_or("--pair")?),
            "--pair-code" => a.pair_code = Some(it.next().ok_or("--pair-code")?),
            "--no-window" => a.no_window = true,
            "--fullscreen" => a.fullscreen = Some(true),
            "--windowed" => a.fullscreen = Some(false),
            "--vsync" => a.vsync = Some(true),
            "--present-hz" => a.present_hz = Some(it.next().ok_or("--present-hz")?),
            "--no-vsync" => a.vsync = Some(false),
            "--audio" => a.audio = Some(true),
            "--no-audio" => a.audio = Some(false),
            "--audio-connect" => a.audio_connect = Some(it.next().ok_or("--audio-connect")?),
            "--gamepads" => a.gamepads = Some(true),
            "--no-gamepads" => a.gamepads = Some(false),
            "--no-reconnect" => a.connections = false,
            "--list" => a.list_profiles = true,
            "--check" => a.check_profile = true,
            "--no-clipboard" => a.clipboard = Some(false),
            "--no-rich-clipboard" => a.clipboard_rich = Some(false),
            "--mute-unfocused" => a.mute_unfocused = Some(true),
            "--no-mute-unfocused" => a.mute_unfocused = Some(false),
            "--clipboard-max-bytes" => {
                a.clipboard_max = Some(
                    it.next()
                        .ok_or("--clipboard-max-bytes")?
                        .parse()
                        .map_err(|_| "--clipboard-max-bytes")?,
                )
            }
            "--screenshot" => a.screenshot = Some(it.next().ok_or("--screenshot")?),
            "--token" => a.token = Some(it.next().ok_or("--token")?),
            "--client-id" => a.client_id = Some(it.next().ok_or("--client-id")?),
            "--decoder" => a.decoder = Some(it.next().ok_or("--decoder")?),
            "--bench" => a.bench = Some(it.next().ok_or("--bench")?.parse().map_err(|_| "--bench")?),
            "--frames" => a.frames = Some(it.next().ok_or("--frames")?.parse().map_err(|_| "--frames")?),
            "--cursor-grab" => a.cursor_grab = Some(true),
            "--no-cursor-grab" => a.cursor_grab = Some(false),
            "--size" => {
                let v = it.next().ok_or("--size")?;
                let (w, h) = v.split_once('x').ok_or("--size WxH")?;
                a.size = Some((w.parse().map_err(|_| "--size")?, h.parse().map_err(|_| "--size")?));
            }
            "-h" | "--help" => {
                println!(
                    "lgx-client [NAME] [--connect HOST:PORT] [--token T] [--size WxH]\n\
                     \x20          NAME selects the [NAME] section in client.conf\n\
                     \x20          [--decoder auto|cuda|vaapi|vulkan|software]\n\
                     \x20          [--cursor-grab|--no-cursor-grab] [--fullscreen|--windowed]\n\
                     \x20          [--vsync] [--audio] [--audio-connect ADDR]\n\
                     \x20          [--gamepads|--no-gamepads] [--no-reconnect]\n\
                     \x20          [--list] [--check]\n\
                     \x20          [--no-clipboard] [--no-rich-clipboard]\n\
                     \x20          [--mute-unfocused|--no-mute-unfocused]\n\
                     \x20          [--no-window] [--bench SECONDS] [--frames N]\n\
                     \x20          [--screenshot FILE]\n\
                     \x20          [--pair HOST:PAIRPORT --pair-code CODE] [--client-id ID]"
                );
                std::process::exit(0);
            }
            other if !other.starts_with('-') => {
                if profile.is_some() {
                    return Err("only one profile name may be given".into());
                }
                profile = Some(other.to_string());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if let Some(name) = profile {
        a.name = name;
    }
    if a.name.is_empty() && !a.list_profiles {
        return Err("usage: lgx-client NAME [options]".into());
    }
    if !a.name.is_empty() {
        config::validate_name(&a.name).map_err(|e| e.to_string())?;
    }
    Ok(a)
}

struct Keyboard {
    state: xkbcommon::xkb::State,
}

impl Keyboard {
    fn new() -> Option<(Self, Vec<u8>)> {
        use xkbcommon::xkb;
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap =
            xkb::Keymap::new_from_names(&ctx, "", "", "", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)?;
        let mut bytes = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1).into_bytes();
        bytes.push(0);
        let state = xkb::State::new(&keymap);
        Some((Self { state }, bytes))
    }

    fn update(&mut self, code: u32, down: bool) -> (u32, u32, u32, u32) {
        use xkbcommon::xkb;
        let dir = if down { xkb::KeyDirection::Down } else { xkb::KeyDirection::Up };
        self.state.update_key(code.into(), dir);
        (
            self.state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            self.state.serialize_mods(xkb::STATE_MODS_LATCHED),
            self.state.serialize_mods(xkb::STATE_MODS_LOCKED),
            self.state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        )
    }
}

fn write_ppm_nv12(
    path: &str,
    w: u32,
    h: u32,
    y: &[u8],
    y_stride: usize,
    uv: &[u8],
    uv_stride: usize,
) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    write!(f, "P6\n{w} {h}\n255\n")?;
    let mut row = Vec::with_capacity(w as usize * 3);
    for j in 0..h as usize {
        row.clear();
        for i in 0..w as usize {
            let yy = y[j * y_stride + i] as f32;
            let ci = (j / 2) * uv_stride + (i / 2) * 2;
            let u = uv[ci] as f32 - 128.0;
            let v = uv[ci + 1] as f32 - 128.0;
            let r = (yy + 1.5748 * v).clamp(0.0, 255.0);
            let g = (yy - 0.1873 * u - 0.4681 * v).clamp(0.0, 255.0);
            let b = (yy + 1.8556 * u).clamp(0.0, 255.0);
            row.push(r as u8);
            row.push(g as u8);
            row.push(b as u8);
        }
        f.write_all(&row)?;
    }
    Ok(())
}


fn backends(spec: &str, gl_renderer: &str, video: bool) -> Vec<Backend> {
    match spec {
        "software" | "none" | "sw" => vec![Backend::Software],
        "vaapi" => vec![Backend::Vaapi, Backend::Software],
        "cuda" | "nvidia" | "nvdec" => vec![Backend::Cuda, Backend::Software],
        "vulkan" | "vkvideo" => vec![Backend::Vulkan, Backend::Software],
        _ => {
            let mut list = Vec::new();
            if video {
                list.push(Backend::Vulkan);
            }
            if gl_renderer.to_ascii_lowercase().contains("nvidia") {
                list.extend([Backend::Cuda, Backend::Vaapi, Backend::Software]);
            } else {
                list.extend([Backend::Vaapi, Backend::Cuda, Backend::Software]);
            }
            list
        }
    }
}

/// A `--decoder` (or profile) value names one backend explicitly; everything
/// else, including `auto` and unknown strings, goes through detection.
fn explicit_decoder(spec: &str) -> bool {
    matches!(
        spec,
        "software" | "none" | "sw" | "vaapi" | "cuda" | "nvidia" | "nvdec" | "vulkan" | "vkvideo"
    )
}

fn map_decoder(b: lgx_caps::DecodeBackend) -> Backend {
    match b {
        lgx_caps::DecodeBackend::Vulkan => Backend::Vulkan,
        lgx_caps::DecodeBackend::Vaapi => Backend::Vaapi,
        lgx_caps::DecodeBackend::Cuda => Backend::Cuda,
        lgx_caps::DecodeBackend::Software => Backend::Software,
    }
}

/// Capability-driven decoder order. `renderer_name` is `None` in headless mode,
/// where there is no Vulkan device to match against the DRM nodes.
fn detect_decoders(renderer_name: Option<&str>) -> Vec<Backend> {
    let nodes = lgx_caps::drm_nodes();
    let va: Vec<lgx_caps::VaCaps> = nodes
        .iter()
        .filter(|n| n.render)
        .filter_map(lgx_caps::probe_vaapi)
        .collect();
    let cuda = lgx_caps::cuda::available();
    let devices = lgx_caps::vulkan_devices().unwrap_or_default();
    let info = renderer_name.and_then(|n| devices.iter().find(|d| d.name == n));
    let plan: Vec<lgx_caps::DecodeBackend> = match info {
        Some(info) => {
            eprintln!(
                "lgx-client: GPU {} ({}, render {}, video decode {})",
                info.name,
                if info.driver.is_empty() { "unknown" } else { &info.driver },
                info.render
                    .map(|(a, b)| format!("{a}:{b}"))
                    .unwrap_or_else(|| "unreported".into()),
                if info.video_decode_h265 { "yes" } else { "no" },
            );
            lgx_caps::decode_plan(info, &nodes, &va, cuda)
        }
        None => {
            // No Vulkan device: prefer NVDEC on an NVIDIA node (VAAPI there is
            // NVDEC behind libva, CUDA is the native path), VAAPI elsewhere.
            let nvidia = nodes.iter().any(|n| n.render && n.is_nvidia());
            let vaapi = va.iter().any(|c| c.hevc_decode);
            let mut p = Vec::new();
            if nvidia && cuda {
                p.push(lgx_caps::DecodeBackend::Cuda);
            }
            if vaapi {
                p.push(lgx_caps::DecodeBackend::Vaapi);
            }
            if cuda && !p.contains(&lgx_caps::DecodeBackend::Cuda) {
                p.push(lgx_caps::DecodeBackend::Cuda);
            }
            p.push(lgx_caps::DecodeBackend::Software);
            p
        }
    };
    let mut order: Vec<Backend> = Vec::new();
    for b in plan {
        let b = map_decoder(b);
        if !order.contains(&b) {
            order.push(b);
        }
    }
    if order.is_empty() {
        order.push(Backend::Software);
    }
    let names: Vec<&str> = order.iter().map(|b| b.name()).collect();
    eprintln!("lgx-client: decoder plan: {}", names.join(" -> "));
    order
}

fn cursor_from_shape(shape: net::CursorShape) -> Option<Cursor> {
    let mut rgba = shape.rgba;
    let pitch = shape.width * 4;
    let surface = match Surface::from_data(
        &mut rgba,
        shape.width,
        shape.height,
        pitch,
        PixelFormat::RGBA32,
    ) {
        Ok(surface) => surface,
        Err(e) => {
            eprintln!("lgx-client: cursor surface: {e}");
            return None;
        }
    };
    match Cursor::from_surface(&surface, shape.hot_x, shape.hot_y) {
        Ok(cursor) => Some(cursor),
        Err(e) => {
            eprintln!("lgx-client: cursor: {e}");
            None
        }
    }
}

fn draw_percentiles(samples: &mut [f32]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    samples.sort_by(f32::total_cmp);
    let at = |p: f32| {
        let idx = ((samples.len() - 1) as f32 * p).round() as usize;
        samples[idx.min(samples.len() - 1)]
    };
    (at(0.5), at(0.99))
}

fn main() {
    let mut args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("lgx-client: {e}");
            std::process::exit(2);
        }
    };

    if let Some(addr) = args.pair.clone() {
        let code = match args.pair_code.as_deref() {
            Some(c) => c,
            None => {
                eprintln!("lgx-client: --pair requires --pair-code");
                std::process::exit(2);
            }
        };
        let (host, pair_port) = split_host_port(&addr);
        let pair_port = pair_port.unwrap_or(5904);
        let pair_addr = join_host_port(&host, pair_port);
        match pair::run(&pair_addr, code) {
            Ok((id, token, video, audio)) => {
                if id != args.name {
                    eprintln!(
                        "lgx-client: server paired id {id}, but profile is {}",
                        args.name
                    );
                    std::process::exit(1);
                }
                let mut cfg = config::Config::load_or_create();
                cfg.set_instance(&args.name, "server", &host);
                cfg.set_instance(&args.name, "video_port", video);
                cfg.set_instance(&args.name, "audio_port", audio);
                cfg.set_instance(&args.name, "pair_port", pair_port);
                cfg.set_instance(&args.name, "token", &token);
                if let Err(e) = cfg.save() {
                    eprintln!("lgx-client: cannot save [{}]: {e}", args.name);
                    std::process::exit(1);
                }
                eprintln!(
                    "lgx-client: paired as {id}; saved [{}] server={host} video={video} audio={audio} in {}",
                    args.name,
                    config::path().display()
                );
            }
            Err(e) => {
                eprintln!("lgx-client: pairing with {pair_addr} failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.list_profiles {
        let cfg = config::Config::load_or_create();
        let profiles = cfg.instances();
        if profiles.is_empty() {
            println!("no profiles in {}", config::path().display());
        } else {
            println!("{}", config::path().display());
            for name in profiles {
                let server = cfg.instance_get(&name, "server").unwrap_or("-");
                let token = if cfg.instance_get(&name, "token").is_some_and(|t| !t.is_empty()) {
                    "token"
                } else {
                    "no-token"
                };
                println!("  {name:<20} {server:<28} {token}");
            }
        }
        return;
    }
    if args.check_profile {
        let cfg = config::Config::load();
        let name = &args.name;
        let server = cfg.instance_get(name, "server");
        let token = cfg.instance_get(name, "token").filter(|t| !t.is_empty());
        if !cfg.has_instance(name) || server.is_none() || token.is_none() {
            eprintln!(
                "lgx-client: [{name}] invalid: server={} token={}",
                if server.is_some() { "yes" } else { "no" },
                if token.is_some() { "yes" } else { "no" }
            );
            std::process::exit(2);
        }
        println!(
            "[{name}] ok: server={} token=yes config={}",
            server.unwrap(),
            config::path().display()
        );
        return;
    }

    let cfg = config::Config::load_or_create();
    let token = args
        .token
        .clone()
        .or_else(|| std::env::var("LGX_TOKEN").ok())
        .or_else(|| cfg.instance_get(&args.name, "token").map(str::to_string))
        .unwrap_or_default();
    let client_id = args.client_id.clone().unwrap_or_else(|| args.name.clone());
    let s = Settings {
        name: args.name.clone(),
        server: {
            let server = args
                .connect
                .clone()
                .or_else(|| std::env::var("LGX_SERVER").ok())
                .or_else(|| cfg.instance_get(&args.name, "server").map(str::to_string));
            match server {
                Some(s) => s,
                None => {
                    eprintln!(
                        "lgx-client: profile [{}] has no server; add server = HOST:5902",
                        args.name
                    );
                    std::process::exit(2);
                }
            }
        },
        video_port: cfg
            .instance_get(&args.name, "video_port")
            .and_then(|v| v.parse().ok())
            .unwrap_or(5902),
        audio_port: cfg
            .instance_get(&args.name, "audio_port")
            .and_then(|v| v.parse().ok())
            .unwrap_or(5903),
        width: args
            .size
            .map(|(w, _)| w)
            .or_else(|| cfg.get_u32_for(&args.name, "width")),
        height: args
            .size
            .map(|(_, h)| h)
            .or_else(|| cfg.get_u32_for(&args.name, "height")),
        fullscreen: args
            .fullscreen
            .or_else(|| cfg.get_bool_for(&args.name, "fullscreen"))
            .unwrap_or(false),
        vsync: args
            .vsync
            .or_else(|| cfg.get_bool_for(&args.name, "vsync"))
            .unwrap_or(false),
        present_hz: args
            .present_hz
            .clone()
            .or_else(|| cfg.get_for(&args.name, "present_hz").map(str::to_string))
            .unwrap_or_else(|| "0".to_string()),
        audio: args
            .audio
            .or_else(|| cfg.get_bool_for(&args.name, "audio"))
            .unwrap_or(false),
        audio_connect: args
            .audio_connect
            .clone()
            .or_else(|| {
                cfg.get_for(&args.name, "audio_connect")
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
            }),
        token,
        client_id,
        decoder: args
            .decoder
            .clone()
            .or_else(|| cfg.get_for(&args.name, "decoder").map(str::to_string))
            .unwrap_or_else(|| "auto".to_string()),
        cursor_grab: args
            .cursor_grab
            .or_else(|| cfg.get_bool_for(&args.name, "cursor_grab"))
            .unwrap_or(true),
        gamepads: args
            .gamepads
            .or_else(|| cfg.get_bool_for(&args.name, "gamepads"))
            .unwrap_or(true),
        clipboard: args
            .clipboard
            .or_else(|| cfg.get_bool_for(&args.name, "clipboard"))
            .unwrap_or(true),
        clipboard_rich: args
            .clipboard_rich
            .or_else(|| cfg.get_bool_for(&args.name, "clipboard_rich"))
            .unwrap_or(true),
        clipboard_max_bytes: args
            .clipboard_max
            .or_else(|| {
                cfg.get_u32_for(&args.name, "clipboard_max_bytes")
                    .map(|v| v as usize)
            })
            .unwrap_or(1024 * 1024),
        mute_unfocused: args
            .mute_unfocused
            .or_else(|| cfg.get_bool_for(&args.name, "mute_unfocused"))
            .unwrap_or(true),
        reconnect: args.connections,
    };
    if s.token.is_empty() {
        eprintln!("lgx-client: no token in [{}] for {}", s.name, s.server);
        eprintln!(
            "lgx-client: pair with: lgx-client {} --pair HOST:PAIRPORT --pair-code CODE",
            s.name
        );
        std::process::exit(2);
    }
    let (host, explicit_video_port) = split_host_port(&s.server);
    let video_port = explicit_video_port.unwrap_or(s.video_port);
    let audio_port = s.audio_port;
    let server = join_host_port(&host, video_port);
    let audio_addr = s
        .audio_connect
        .clone()
        .unwrap_or_else(|| join_host_port(&host, audio_port));

    let this_host = host.clone();
    let _instance = match instance::acquire(&this_host) {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            eprintln!("lgx-client: already running for {this_host}");
            return;
        }
        Err(e) => {
            eprintln!("lgx-client: cannot take the instance lock: {e}");
            None
        }
    };
    let token = s.token.clone();
    let (mut keyboard, keymap) = match Keyboard::new() {
        Some(v) => (Some(v.0), v.1),
        None => {
            eprintln!("lgx-client: could not build a keymap, keyboard may not work");
            (None, Vec::new())
        }
    };

    let mut connect_attempt = reconnect_attempt();
    let mut dialog: Option<reconnect_ui::Dialog> = None;
    let (conn, hello, video_rx) = loop {
        let (video_tx, video_rx) = std::sync::mpsc::sync_channel::<dec::Chunk>(net::VIDEO_QUEUE);
        let clip_policy = net::ClipPolicy {
            enabled: s.clipboard,
            rich: s.clipboard_rich,
            max_bytes: s.clipboard_max_bytes,
        };
        match net::connect(&server, &keymap, &s.client_id, &token, video_tx, clip_policy) {
            Ok((conn, hello)) => {
                if let Some(d) = dialog.take() {
                    d.close();
                }
                break (conn, hello, video_rx);
            }
            Err(e) => {
                if !s.reconnect || connect_attempt >= MAX_RECONNECT_ATTEMPTS {
                    eprintln!("lgx-client: cannot connect to {server}: {e}");
                    std::process::exit(1);
                }
                connect_attempt += 1;
                let delay = reconnect_delay(connect_attempt);
                eprintln!(
                    "lgx-client: cannot connect to {server}: {e}; retrying in {}s (attempt {connect_attempt}/{MAX_RECONNECT_ATTEMPTS})",
                    delay.as_secs()
                );
                if !args.no_window {
                    if dialog.is_none() {
                        dialog = reconnect_ui::Dialog::spawn(&s.name, connect_attempt);
                    }
                    if let Some(d) = dialog.as_mut() {
                        d.update(connect_attempt);
                    }
                }
                if let Some(d) = dialog.as_mut() {
                    if d.wait_or_cancel(delay) {
                        eprintln!("lgx-client: reconnect cancelled");
                        std::process::exit(0);
                    }
                } else {
                    std::thread::sleep(delay);
                }
            }
        }
    };
    // Remember the attempt count in case this session drops quickly.
    std::env::set_var(RECONNECT_ATTEMPT_ENV, connect_attempt.to_string());
    let _ = conn.send(
        Msg::new(msg::CURSOR, 1, 0, 0),
        &[],
    );
    eprintln!("lgx-client: server HEVC {}x{}", hello.width, hello.height);

    if s.clipboard {
        let conn = conn.clone();
        let rich = s.clipboard_rich;
        let max_bytes = s.clipboard_max_bytes;
        clip::watch(
            move |clip| {
                let sig = clip::signature(&clip);
                if sig == *conn.last_remote.lock().unwrap() {
                    return;
                }
                if let clip::Clip::File { name, data } = &clip {
                    if data.len() as u64 > lgx_proto::file::MAX_BYTES {
                        return;
                    }
                    let begin = lgx_proto::file::encode_begin(name, data.len() as u64);
                    if conn
                        .send(Msg::new(msg::FILE_BEGIN, 0, begin.len() as i32, 0), &begin)
                        .is_err()
                    {
                        return;
                    }
                    for chunk in data.chunks(32 * 1024) {
                        if conn
                            .send(Msg::new(msg::FILE_DATA, 0, chunk.len() as i32, 0), chunk)
                            .is_err()
                        {
                            return;
                        }
                    }
                    let _ = conn.send(Msg::new(msg::FILE_END, 0, 0, 0), &[]);
                    return;
                }
                let size = match &clip {
                    clip::Clip::Text(text) => text.len(),
                    clip::Clip::Rich { html, text } => html.len() + text.len(),
                    clip::Clip::File { .. } => 0,
                };
                if max_bytes != 0 && size > max_bytes {
                    return;
                }
                match clip {
                    clip::Clip::Rich { html, text } if !html.is_empty() && rich => {
                        let payload = lgx_proto::encode_rich(&html, &text);
                        let _ = conn.send(
                            Msg::new(msg::CLIP_SET_HTML, 0, payload.len() as i32, 0),
                            &payload,
                        );
                    }
                    clip::Clip::Rich { text, .. } if !text.is_empty() => {
                        let _ = conn.send(
                            Msg::new(msg::CLIP_SET, 0, text.len() as i32, 0),
                            text.as_bytes(),
                        );
                    }
                    clip::Clip::Text(text) if !text.is_empty() => {
                        let _ = conn.send(
                            Msg::new(msg::CLIP_SET, 0, text.len() as i32, 0),
                            text.as_bytes(),
                        );
                    }
                    _ => {}
                }
            },
            true,
        );
    } else {
        eprintln!("lgx-client: clipboard forwarding disabled");
    }

    let audio_focus = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mute_unfocused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(s.mute_unfocused));
    // Shared A/V clock; the video presenter waits for the audio timeline to
    // reach each frame's capture timestamp. It stays inactive when audio is
    // disabled or the server does not timestamp its packets.
    let av_sync = audio::Sync::new();
    if s.audio {
        let addr = audio_addr;
        let token = token.clone();
        let client_id = s.client_id.clone();
        let focus = audio_focus.clone();
        let mute = mute_unfocused.clone();
        let sync = av_sync.clone();
        std::thread::Builder::new()
            .name("lgx-audio".into())
            .spawn(move || {
                if let Err(e) = audio::run(&addr, &client_id, &token, mute, focus, sync) {
                    eprintln!("lgx-client: audio: {e}");
                }
            })
            .ok();
    }

    let session_started = Instant::now();
    let deadline = args.bench.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
    let max_frames = args.frames;

    if args.no_window {
        let sink = dec::Sink::new();
        let chosen = if explicit_decoder(&s.decoder) {
            backends(&s.decoder, "", false)
        } else {
            detect_decoders(None)
        };
        let decoder = dec::spawn(chosen, None, false, false, None, video_rx, sink.clone(), None);
        run_headless(sink, &conn, args.screenshot.take(), deadline, max_frames);
        conn.close();
        let _ = decoder.join();
        return;
    }

    let sdl = sdl3::init().expect("SDL");
    let video = sdl.video().expect("SDL video");
    // Do not set SDL_MOUSE_FOCUS_CLICKTHROUGH: the click that focuses the
    // window must only focus it. Otherwise hovering a guest button and then
    // clicking to focus looks like the remote mouse pressed by itself.
    if let Err(e) = video.vulkan_load_library_default() {
        eprintln!("lgx-client: cannot load Vulkan: {e}");
        std::process::exit(1);
    }
    let (win_w, win_h) = match (s.width, s.height) {
        (Some(w), Some(h)) => (w, h),
        _ => (
            if hello.width > 1 { hello.width } else { 1280 },
            if hello.height > 1 { hello.height } else { 720 },
        ),
    };
    let mut window = video
        .window("lgx", win_w, win_h)
        .vulkan()
        .build()
        .expect("window");
    window
        .set_title(&format!("lgx — {}", s.name))
        .ok();
    if std::env::var_os("LGX_DEBUG_FPS").is_some() {
        eprintln!("lgx-client: title {:?}", window.title());
    }
    if s.fullscreen {
        window.set_fullscreen(true).ok();
    }
    let present_hz: Option<f32> = match s.present_hz.as_str() {
        "" | "0" | "off" => None,
        "auto" => window
            .display_mode()
            .map(|m| m.refresh_rate)
            .filter(|hz| *hz > 1.0),
        v => v.parse::<f32>().ok().filter(|hz| *hz > 1.0),
    };
    if let Some(hz) = present_hz {
        eprintln!("lgx-client: presenting at most {hz:.0} Hz (present_hz)");
    }
    let mut renderer = match vk::Renderer::new(&window, s.vsync, (hello.width, hello.height)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lgx-client: Vulkan renderer: {e}");
            std::process::exit(1);
        }
    };
    if let Some((family, _)) = renderer.video_decode_queue() {
        eprintln!("lgx-client: native Vulkan Video decode queue available (family {family})");
    }
    let chosen = if explicit_decoder(&s.decoder) {
        backends(&s.decoder, renderer.device_name(), renderer.video_decode_supported())
    } else {
        detect_decoders(Some(renderer.device_name()))
    };
    let native_ok = renderer.video_decode_queue().is_some()
        && renderer.can_import_dmabuf()
        && (s.decoder.is_empty() || s.decoder == "default" || s.decoder == "vulkan")
        && args.screenshot.is_none()
        && !matches!(
            std::env::var("LGX_VULKAN_VIDEO").as_deref(),
            Ok("ffmpeg") | Ok("0")
        );
    eprintln!(
        "lgx-client: native Vulkan Video path {}",
        if native_ok {
            "enabled by default (LGX_VULKAN_VIDEO=ffmpeg forces FFmpeg)"
        } else {
            "disabled"
        }
    );
    let want_dmabuf = renderer.can_import_dmabuf();
    if want_dmabuf {
        eprintln!("lgx-client: decoder frames will be imported as dma-buf (LGX_NO_DMABUF=1 forces NV12 uploads)");
    } else {
        eprintln!("lgx-client: decoder frames are uploaded as NV12 (no dma-buf import in the Vulkan path)");
    }
    let sink = dec::Sink::new();
    let wake: Option<Box<dyn Fn() + Send>> = sdl.event().ok().and_then(|ev| unsafe {
        ev.register_event().ok().map(|ty| {
            let sender = ev.event_sender();
            Box::new(move || {
                let _ = sender.push_event(Event::User {
                    timestamp: 0,
                    window_id: 0,
                    type_: ty,
                    code: 0,
                    data1: std::ptr::null_mut(),
                    data2: std::ptr::null_mut(),
                });
            }) as Box<dyn Fn() + Send>
        })
    });
    let idr_conn = conn.clone();
    let request_idr: Option<Box<dyn Fn() + Send>> = Some(Box::new(move || {
        let _ = idr_conn.send(Msg::new(msg::IDR, 0, 0, 0), &[]);
    }));
    let decoder = dec::spawn(
        chosen,
        None,
        want_dmabuf,
        native_ok,
        request_idr,
        video_rx,
        sink.clone(),
        wake,
    );

    let gamepad_subsystem = if s.gamepads {
        sdl.gamepad().ok()
    } else {
        eprintln!("lgx-client: gamepad forwarding disabled");
        None
    };
    let mut pads = gamepad_subsystem
        .as_ref()
        .map(pad::open_all)
        .unwrap_or_default();
    for p in &pads {
        pad::send(&conn, p, true);
    }
    let mut pump = sdl.event_pump().expect("event pump");
    sdl.mouse().set_relative_mouse_mode(&window, false);
    let mut grab_active = s.cursor_grab;
    if grab_active && !window.set_mouse_grab(true) {
        eprintln!("lgx-client: pointer confinement is not available; cursor stays absolute");
    }
    // Remote-cursor style: exactly one visible cursor. While the window is
    // focused the guest cursor is composited into the video and the host
    // cursor is hidden; grab only controls pointer confinement.
    sdl.mouse().show_cursor(false);
    let heartbeat = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let hb = heartbeat.clone();
        std::thread::Builder::new()
            .name("lgx-watchdog".into())
            .spawn(move || {
                let mut last = 0u64;
                let mut stalled = 0u32;
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let now = hb.load(std::sync::atomic::Ordering::Relaxed);
                    if now == last {
                        stalled += 1;
                        if stalled >= 3 {
                            eprintln!("lgx-client: UI stalled for 3 s, closing");
                            unsafe { libc::_exit(2) };
                        }
                    } else {
                        stalled = 0;
                        last = now;
                    }
                }
            })
            .ok();
    }

    let wm = unsafe { wayland_window(&window) };
    let _inhibitor: Option<inhibit::Guard> = match wm {
        Some((display, surface)) => match unsafe { inhibit::install(display, surface) } {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("lgx-client: keyboard shortcuts inhibitor: {e}");
                None
            }
        },
        None => None,
    };

    let mut img_size = (hello.width, hello.height);
    // The size the window was sized for; a RESIZE compares against this, not
    // against `img_size` (which follows every decoded frame).
    let mut display_size = (hello.width, hello.height);
    let mut resize_dialog: Option<(resize_ui::Dialog, u32, u32)> = None;
    // Test hook: answer the display prompt without a real dialog.
    let mut test_answer: Option<(i32, u32, u32)> = None;
    // `LGX_FAKE_RESIZE=1280x800@60` injects one display change for testing.
    let mut fake_resize = std::env::var("LGX_FAKE_RESIZE").ok().and_then(|value| {
        let (size, hz) = value.split_once('@').unwrap_or((&value, "0"));
        let (w, h) = size.split_once('x')?;
        Some((
            Instant::now() + Duration::from_secs(3),
            w.parse().ok()?,
            h.parse().ok()?,
            hz.parse().ok()?,
        ))
    });
    let mut last_frame: Option<dec::Frame> = None;
    let mut last_present: Option<Instant> = None;
    let mut last_cpu: Option<(u64, Instant)> = None;
    let mut shown = 0u64;
    let mut screenshot = args.screenshot.take();
    let mut next_stats = Instant::now() + Duration::from_secs(1);
    let mut last_stat_shown = 0u64;
    let mut last_stat_decoded = sink.decoded.load(std::sync::atomic::Ordering::Relaxed);
    let mut last_stat_dropped = sink.dropped.load(std::sync::atomic::Ordering::Relaxed);
    let mut draw_samples: Vec<f32> = Vec::new();
    let started = Instant::now();
    let mut quit = false;
    let mut connection_lost = false;
    let mut redraw = true;
    let mut swallow_f_keyup = false;
    let mut swallow_m_keyup = false;
    let mut swallow_g_keyup = false;
    let mut swallow_s_keyup = false;
    // `LGX_STATS_OSD=1` keeps the statistics card visible (tests, demos).
    let mut stats_osd_until: Option<Instant> = std::env::var_os("LGX_STATS_OSD")
        .map(|_| Instant::now() + Duration::from_secs(3600));
    let mut osd_until: Option<Instant> = None;
    let mut osd_card = osd::AUDIO_ON;
    let mut fullscreen = s.fullscreen;
    if let Ok(state) = std::env::var("LGX_OSD_DEMO") {
        osd_card = if state == "off" { osd::AUDIO_OFF } else { osd::AUDIO_ON };
        osd_until = Some(Instant::now() + Duration::from_secs(10));
    }
    let mut fullscreen_hold: Option<(Instant, (u32, u32))> = None;
    let mut dst = vk::fit_rect(win_w, win_h, win_w, win_h);
    // Cursor visibility: hidden while the server composites the pointer into
    // the video, shown when the server asked for a local cursor (it captures
    // at the frame rate; the local pointer follows the host at its own rate).
    let mut window_focused = true;
    let mut cursor_shown = false;
    // Wheel deltas arrive as floats: a trackpad sends fractions of a notch, and
    // truncating them to i32 dropped most events. Whole steps are pieced
    // together here, and axis 1 is forwarded so horizontal wheels work.
    let mut wheel_acc = [0.0f32; 2];
    // The server may send the guest cursor shape because the pointer is drawn
    // locally. SDL shows that shape while the window is focused; the host
    // arrow is restored when it is not.
    let local_arrow = Cursor::from_system(SystemCursor::Arrow).ok();
    let mut remote_cursor: Option<Cursor> = None;
    // A/V sync: hold the newest video frame until the audio timeline reaches
    // its capture timestamp. A stalled audio stream must not freeze video
    // forever, hence the cap. The lead can be tuned at runtime while measuring
    // the residual offset.
    // Never turn audio buffering into hundreds of milliseconds of video lag.
    // The audio timeline is kept tight in the playback callback as well; this
    // cap only covers a transient stall.
    const AV_MAX_HOLD_US: i64 = 30_000;
    // The hold is opt-in. Holding video to the audio head made the presenter
    // miss slots and drop frames at 60 Hz; the natural timestamp offset is
    // small in practice. LGX_AV_SYNC=1 enables the hold for testing.
    let av_enabled = std::env::var("LGX_AV_SYNC").map(|v| v == "1").unwrap_or(false);
    let av_lead_us: i64 = std::env::var("LGX_AV_LEAD_US")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let av_debug = std::env::var_os("LGX_DEBUG_AV").is_some();
    let mut pending_frame: Option<dec::Frame> = None;
    let mut av_last_log = Instant::now();
    // A dead or muted audio socket must not keep the last head timestamp
    // alive and delay every video frame by the hold cap.
    let mut av_last_head = 0u64;
    let mut av_head_seen = Instant::now();

    {
        let px = window.size_in_pixels();
        renderer.clear(px);
    }

    while !quit {
        heartbeat.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some((at, w, h, hz)) = fake_resize {
            if Instant::now() >= at {
                fake_resize = None;
                conn.inject_resize(w, h, hz);
            }
        }
        if let Some((w, h, _hz)) = conn.take_resize() {
            if (w, h) != display_size && resize_dialog.is_none() {
                if let Some(code) = std::env::var("LGX_TEST_RESIZE_ANSWER")
                    .ok()
                    .and_then(|v| v.parse::<i32>().ok())
                {
                    test_answer = Some((code, w, h));
                } else {
                    let old = format!("{}x{}", display_size.0, display_size.1);
                    let new = format!("{w}x{h}");
                    match resize_ui::Dialog::spawn(&s.name, &old, &new) {
                        Some(d) => resize_dialog = Some((d, w, h)),
                        None => {
                            // No helper available: never fight the stream, but do
                            // not prompt again for this mode either.
                            display_size = (w, h);
                        }
                    }
                }
            }
        }
        let mut answer = resize_dialog
            .as_mut()
            .and_then(|(d, w, h)| d.poll().map(|code| (code, *w, *h)));
        if answer.is_none() {
            answer = test_answer.take();
        }
        if let Some((code, w, h)) = answer {
            resize_dialog = None;
            display_size = (w, h);
            eprintln!("lgx-client: display answer {code} -> {w}x{h}");
            if code != 2 {
                window.set_size(w, h).ok();
                eprintln!("lgx-client: window now {:?}", window.size());
                if code == 1 {
                    let mut cfg = config::Config::load();
                    cfg.set("width", w);
                    cfg.set("height", h);
                    if let Err(e) = cfg.save() {
                        eprintln!("lgx-client: cannot save the display size: {e}");
                    } else {
                        eprintln!("lgx-client: saved display size {w}x{h}");
                    }
                }
            }
            redraw = true;
        }
        if let Some(shape) = conn.cursor.lock().unwrap().take() {
            if let Some(cursor) = cursor_from_shape(shape) {
                if window_focused && !conn.cursor_hidden.load(std::sync::atomic::Ordering::Relaxed) {
                    cursor.set();
                }
                remote_cursor = Some(cursor);
            }
        }
        if let Some(new_frame) = sink.take() {
            // Newest frame wins while a previous one is held for sync.
            pending_frame = Some(new_frame);
        }
        let mut frame = None;
        if let Some(f) = pending_frame.take() {
            let mut hold = false;
            let head = av_sync.head_us.load(std::sync::atomic::Ordering::Relaxed);
            if head != av_last_head {
                av_last_head = head;
                av_head_seen = Instant::now();
            }
            if av_enabled
                && f.capture_us != 0
                && av_sync.active.load(std::sync::atomic::Ordering::Relaxed)
                && !av_sync.paused.load(std::sync::atomic::Ordering::Relaxed)
                && av_head_seen.elapsed() < Duration::from_millis(200)
            {
                let target = f.capture_us as i64 + av_lead_us;
                let wait = target - head as i64;
                if wait > 0 && wait <= AV_MAX_HOLD_US {
                    hold = true;
                }
                if av_debug && av_last_log.elapsed() >= Duration::from_secs(1) {
                    av_last_log = Instant::now();
                    eprintln!(
                        "lgx-client: av video={} audio={} delta={} wait={} hold={} us",
                        f.capture_us, head, head as i64 - f.capture_us as i64, wait, hold
                    );
                }
            }
            if hold {
                pending_frame = Some(f);
            } else {
                frame = Some(f);
            }
        }
        let events: Vec<Event> = if frame.is_some() {
            pump.poll_iter().collect()
        } else {
            // Cursor shapes arrive through the reader thread, not as SDL
            // events, and a static macOS capture produces no frames. Poll
            // often enough that an arrow -> I-beam/resize change is applied
            // without waiting for a video frame.
            let mut v: Vec<Event> = pump.wait_timeout_iter(10).next().into_iter().collect();
            v.extend(pump.poll_iter());
            v
        };
        if conn.closed.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("lgx-client: server closed the connection");
            connection_lost = true;
            quit = true;
        }

        for ev in events {
            match ev {
                Event::Quit { .. } => quit = true,
                // The window decoration's close button arrives as a window
                // event, not as SDL_EVENT_QUIT; without this the window goes
                // away while the process keeps its video/audio threads alive.
                Event::Window { win_event: sdl3::event::WindowEvent::CloseRequested, .. } => {
                    quit = true;
                }
                Event::Window { win_event: sdl3::event::WindowEvent::FocusLost, .. } => {
                    audio_focus.store(false, std::sync::atomic::Ordering::Relaxed);
                    window_focused = false;
                    window.set_mouse_grab(false);
                    // Unfocused: show the host cursor and exclude the guest
                    // cursor from the video, so exactly one is visible.
                    if let Some(cursor) = &local_arrow {
                        cursor.set();
                    }
                    let _ = conn.send(Msg::new(msg::CURSOR, 0, 0, 0), &[]);
                }
                Event::Window { win_event: sdl3::event::WindowEvent::FocusGained, .. } => {
                    audio_focus.store(true, std::sync::atomic::Ordering::Relaxed);
                    window_focused = true;
                    window.set_mouse_grab(grab_active);
                    // Focused: guest cursor only unless the server asked for a
                    // local one; grab state does not change the cursor.
                    if !conn
                        .cursor_hidden
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        if let Some(cursor) = &remote_cursor {
                            cursor.set();
                        } else if let Some(cursor) = &local_arrow {
                            cursor.set();
                        }
                    }
                    let _ = conn.send(Msg::new(msg::CURSOR, 1, 0, 0), &[]);
                }
                Event::Window { .. } => redraw = true,
                Event::ControllerDeviceAdded { which, .. } => {
                    if let Some(subsystem) = gamepad_subsystem.as_ref() {
                        let used: Vec<u8> = pads.iter().map(|p| p.slot).collect();
                        if let Some(slot) = (0..pad::MAX_PADS as u8).find(|s| !used.contains(s)) {
                            if let Some(p) = pad::open_one(subsystem, which, slot) {
                                if pads.iter().any(|q| q.id == p.id)
                                    || (p.path.is_some()
                                        && pads.iter().any(|q| q.path == p.path))
                                {
                                    // Already opened through the initial scan.
                                } else {
                                    eprintln!("lgx-client: gamepad {}: {}", p.slot, p.name);
                                    pad::send(&conn, &p, true);
                                    pads.push(p);
                                }
                            }
                        }
                    }
                }
                Event::ControllerDeviceRemoved { which, .. } => {
                    if let Some(pos) = pads.iter().position(|p| p.id == which) {
                        let p = pads.remove(pos);
                        pad::send_disconnected(&conn, p.slot);
                        eprintln!("lgx-client: gamepad {} disconnected", p.slot);
                    }
                }
                Event::ControllerAxisMotion { which, axis, value, .. } => {
                    if let Some(p) = pads.iter_mut().find(|p| p.id == which) {
                        p.state.set_axis(axis, value);
                        pad::send(&conn, p, true);
                    }
                }
                Event::ControllerButtonDown { which, button, .. } => {
                    if let Some(p) = pads.iter_mut().find(|p| p.id == which) {
                        p.state.set_button(button, true);
                        pad::send(&conn, p, true);
                    }
                }
                Event::ControllerButtonUp { which, button, .. } => {
                    if let Some(p) = pads.iter_mut().find(|p| p.id == which) {
                        p.state.set_button(button, false);
                        pad::send(&conn, p, true);
                    }
                }
                Event::KeyDown {
                    keycode: Some(sdl3::keyboard::Keycode::F),
                    keymod,
                    ..
                } if keymod
                    .intersects(sdl3::keyboard::Mod::LCTRLMOD | sdl3::keyboard::Mod::RCTRLMOD)
                    && keymod.intersects(
                        sdl3::keyboard::Mod::LSHIFTMOD | sdl3::keyboard::Mod::RSHIFTMOD,
                    )
                    && keymod.intersects(
                        sdl3::keyboard::Mod::LALTMOD | sdl3::keyboard::Mod::RALTMOD,
                    ) =>
                {
                    fullscreen_hold = Some((Instant::now(), renderer.size()));
                    toggle_fullscreen(&mut window);
                    fullscreen = !fullscreen;
                    osd_card = if fullscreen {
                        osd::FULLSCREEN_ON
                    } else {
                        osd::FULLSCREEN_OFF
                    };
                    osd_until = Some(Instant::now() + Duration::from_millis(1600));
                    redraw = true;
                    // The modifiers were already forwarded before we could
                    // see the F; release them. F itself is never sent.
                    for code in [29u16, 42, 56, 97, 100, 54] {
                        send_key(&conn, keyboard.as_mut(), code, false);
                    }
                    swallow_f_keyup = true;
                    eprintln!("lgx-client: fullscreen toggled (Ctrl+Shift+Alt+F)");
                }
                Event::KeyDown {
                    keycode: Some(sdl3::keyboard::Keycode::S),
                    keymod,
                    repeat,
                    ..
                } if !repeat
                    && keymod.intersects(sdl3::keyboard::Mod::LCTRLMOD | sdl3::keyboard::Mod::RCTRLMOD)
                    && keymod.intersects(sdl3::keyboard::Mod::LSHIFTMOD | sdl3::keyboard::Mod::RSHIFTMOD)
                    && keymod.intersects(sdl3::keyboard::Mod::LALTMOD | sdl3::keyboard::Mod::RALTMOD) =>
                {
                    for code in [29u16, 42, 56, 97, 100, 54] {
                        send_key(&conn, keyboard.as_mut(), code, false);
                    }
                    swallow_s_keyup = true;
                    let until = Instant::now() + Duration::from_secs(5);
                    stats_osd_until = Some(until);
                    osd_card = osd::STATS_CARD;
                    osd_until = Some(until);
                    renderer.set_osd_text("statistics...");
                    redraw = true;
                    eprintln!("lgx-client: statistics OSD (Ctrl+Shift+Alt+S)");
                }
                Event::KeyDown {
                    keycode: Some(sdl3::keyboard::Keycode::M),
                    keymod,
                    repeat,
                    ..
                } if !repeat
                    && keymod
                        .intersects(sdl3::keyboard::Mod::LCTRLMOD | sdl3::keyboard::Mod::RCTRLMOD)
                    && keymod.intersects(
                        sdl3::keyboard::Mod::LSHIFTMOD | sdl3::keyboard::Mod::RSHIFTMOD,
                    )
                    && keymod.intersects(
                        sdl3::keyboard::Mod::LALTMOD | sdl3::keyboard::Mod::RALTMOD,
                    ) =>
                {
                    let now_mute = !mute_unfocused.load(std::sync::atomic::Ordering::Relaxed);
                    mute_unfocused.store(now_mute, std::sync::atomic::Ordering::Relaxed);
                    for code in [29u16, 42, 56, 97, 100, 54] {
                        send_key(&conn, keyboard.as_mut(), code, false);
                    }
                    swallow_m_keyup = true;
                    eprintln!(
                        "lgx-client: background audio {} (Ctrl+Shift+Alt+M)",
                        if now_mute { "disabled" } else { "enabled" }
                    );
                    osd_card = if now_mute { osd::AUDIO_OFF } else { osd::AUDIO_ON };
                    osd_until = Some(Instant::now() + Duration::from_millis(1600));
                    redraw = true;
                }
                Event::KeyDown {
                    keycode: Some(sdl3::keyboard::Keycode::G),
                    keymod,
                    repeat,
                    ..
                } if !repeat
                    && keymod.intersects(sdl3::keyboard::Mod::LCTRLMOD | sdl3::keyboard::Mod::RCTRLMOD)
                    && keymod.intersects(sdl3::keyboard::Mod::LALTMOD | sdl3::keyboard::Mod::RALTMOD)
                    && keymod.intersects(
                        sdl3::keyboard::Mod::LSHIFTMOD | sdl3::keyboard::Mod::RSHIFTMOD,
                    ) =>
                {
                    grab_active = !grab_active;
                    let ok = window.set_mouse_grab(grab_active);
                    for code in [29u16, 42, 56, 97, 100, 54] {
                        send_key(&conn, keyboard.as_mut(), code, false);
                    }
                    swallow_g_keyup = true;
                    eprintln!(
                        "lgx-client: pointer {} (Ctrl+Alt+Shift+G){}",
                        if grab_active { "captured" } else { "released" },
                        if ok { "" } else { " [confinement unsupported]" }
                    );
                    osd_card = if grab_active {
                        osd::POINTER_CAPTURED
                    } else {
                        osd::POINTER_RELEASED
                    };
                    osd_until = Some(Instant::now() + Duration::from_millis(1600));
                }
                Event::KeyDown {
                    keycode: Some(sdl3::keyboard::Keycode::Q),
                    keymod,
                    ..
                } if keymod.intersects(sdl3::keyboard::Mod::LCTRLMOD | sdl3::keyboard::Mod::RCTRLMOD)
                    && keymod.intersects(sdl3::keyboard::Mod::LALTMOD | sdl3::keyboard::Mod::RALTMOD)
                    && keymod.intersects(
                        sdl3::keyboard::Mod::LSHIFTMOD | sdl3::keyboard::Mod::RSHIFTMOD,
                    ) =>
                {
                    quit = true;
                }
                Event::MouseMotion { x, y, .. } => {
                    if dst.w > 0 && dst.h > 0 {
                        let nx = (((x - dst.x as f32) / dst.w as f32) * 65535.0)
                            .clamp(0.0, 65535.0) as i32;
                        let ny = (((y - dst.y as f32) / dst.h as f32) * 65535.0)
                            .clamp(0.0, 65535.0) as i32;
                        let _ = conn.send(Msg::new(msg::MOTION, 0, nx, ny), &[]);
                    }
                }
                Event::MouseButtonDown { mouse_btn, x, y, .. } => {
                    // The server presses at the last motion it saw. Re-send
                    // the position so a click after a resize, a fullscreen
                    // toggle or a skipped motion lands where the pointer is.
                    if dst.w > 0 && dst.h > 0 {
                        let nx = (((x - dst.x as f32) / dst.w as f32) * 65535.0)
                            .clamp(0.0, 65535.0) as i32;
                        let ny = (((y - dst.y as f32) / dst.h as f32) * 65535.0)
                            .clamp(0.0, 65535.0) as i32;
                        let _ = conn.send(Msg::new(msg::MOTION, 0, nx, ny), &[]);
                    }
                    if let Some(code) = btn_code(mouse_btn) {
                        let _ = conn.send(Msg::new(msg::BTN, code, 1, 0), &[]);
                    }
                }
                Event::MouseButtonUp { mouse_btn, .. } => {
                    if let Some(code) = btn_code(mouse_btn) {
                        let _ = conn.send(Msg::new(msg::BTN, code, 0, 0), &[]);
                    }
                }
                Event::MouseWheel { x, y, .. } => {
                    wheel_acc[0] += y;
                    wheel_acc[1] += x;
                    for (axis, acc) in wheel_acc.iter_mut().enumerate() {
                        while *acc >= 1.0 {
                            *acc -= 1.0;
                            let _ = conn.send(Msg::new(msg::WHEEL, axis as u16, 1, 0), &[]);
                        }
                        while *acc <= -1.0 {
                            *acc += 1.0;
                            let _ = conn.send(Msg::new(msg::WHEEL, axis as u16, -1, 0), &[]);
                        }
                    }
                }
                Event::KeyDown { scancode: Some(sc), .. } => {
                    if let Some(code) = hid::evdev_from_scancode((sc as i32) & 0x1ff) {
                        send_key(&conn, keyboard.as_mut(), code, true);
                    }
                }
                Event::KeyUp { keycode: Some(sdl3::keyboard::Keycode::F), .. }
                    if swallow_f_keyup =>
                {
                    swallow_f_keyup = false;
                }
                Event::KeyUp { keycode: Some(sdl3::keyboard::Keycode::M), .. }
                    if swallow_m_keyup =>
                {
                    swallow_m_keyup = false;
                }
                Event::KeyUp { keycode: Some(sdl3::keyboard::Keycode::S), .. }
                    if swallow_s_keyup =>
                {
                    swallow_s_keyup = false;
                }
                Event::KeyUp { keycode: Some(sdl3::keyboard::Keycode::G), .. }
                    if swallow_g_keyup =>
                {
                    swallow_g_keyup = false;
                }
                Event::KeyUp { scancode: Some(sc), .. } => {
                    if let Some(code) = hid::evdev_from_scancode((sc as i32) & 0x1ff) {
                        send_key(&conn, keyboard.as_mut(), code, false);
                    }
                }
                _ => {}
            }
        }

        // Exactly one pointer: the host cursor when unfocused or when the
        // server does not composite one, otherwise only the guest cursor in
        // the video.
        let show_local = !window_focused
            || (conn.local_cursor.load(std::sync::atomic::Ordering::Relaxed)
                && !conn
                    .cursor_hidden
                    .load(std::sync::atomic::Ordering::Relaxed));
        if show_local != cursor_shown {
            sdl.mouse().show_cursor(show_local);
            cursor_shown = show_local;
        }

        let win_px = window.size_in_pixels();
        if let Some((since, old)) = fullscreen_hold {
            let px = window.size_in_pixels();
            if (px.0 > 0 && px.1 > 0 && px != old)
                || since.elapsed() > Duration::from_millis(1200)
            {
                fullscreen_hold = None;
            }
        }
        // `present_hz` caps uploads/presents. The frame is dropped, not
        // delayed: the mailbox holds a newer one, so this only trades one
        // interval of latency for less GPU work.
        if let (Some(hz), Some(last)) = (present_hz, last_present) {
            if last.elapsed().as_secs_f32() < 1.0 / hz {
                frame = None;
            }
        }
        let hold_draw = fullscreen_hold.is_some();
        if hold_draw {
            frame = None;
            redraw = true;
        }
        let mut drawn = false;
        if !hold_draw {
        let osd = match osd_until {
            Some(until) => {
                let now = Instant::now();
                if now >= until {
                    osd_until = None;
                    None
                } else {
                    // luma appears immediately and fades out over 150 ms.
                    let left = until.saturating_duration_since(now).as_secs_f32();
                    let alpha = (left / 0.15).clamp(0.0, 1.0);
                    Some((osd_card, alpha))
                }
            }
            None => None,
        };
        renderer.set_osd(osd);
        if let Some(frame) = frame {
            let (w, h) = (frame.width, frame.height);
            let draw_started = Instant::now();
            let drawn_ok = renderer.draw(&frame, win_px);
            let draw_ms = draw_started.elapsed().as_secs_f64() * 1000.0;
            if drawn_ok {
                last_present = Some(Instant::now());
                shown += 1;
                draw_samples.push(draw_ms as f32);
                img_size = (w, h);
                if let Some(path) = screenshot.take() {
                    save_frame(&path, &frame);
                }
                last_frame = Some(frame);
                drawn = true;
            }
        } else if redraw {
            if let Some(frame) = &last_frame {
                drawn = renderer.draw(frame, win_px);
            }
        }
        }

        if img_size.0 > 1 {
            let win = window.size();
            dst = vk::fit_rect(img_size.0, img_size.1, win.0, win.1);
        }

        if drawn {
            if let Some(path) = screenshot.take() {
                if let Some(frame) = &last_frame {
                    save_frame(&path, frame);
                }
            }
        }
        redraw = false;

        if Instant::now() >= next_stats {
            let df = shown.saturating_sub(last_stat_shown);
            last_stat_shown = shown;
            let bytes = conn.take_bytes();
            let decoded = sink.decoded.load(std::sync::atomic::Ordering::Relaxed);
            let dropped = sink.dropped.load(std::sync::atomic::Ordering::Relaxed);
            let d_dec = decoded.saturating_sub(last_stat_decoded);
            let d_drop = dropped.saturating_sub(last_stat_dropped);
            last_stat_decoded = decoded;
            last_stat_dropped = dropped;
            let (draw_p50, draw_p99) = draw_percentiles(&mut draw_samples);
            if let Some(until) = stats_osd_until {
                if Instant::now() < until {
                    let line = format!(
                        "{df} fps  {draw_p50:.1}/{draw_p99:.1} ms  dec {d_dec}  drop {d_drop}"
                    );
                    renderer.set_osd_text(&line);
                    osd_card = osd::STATS_CARD;
                    osd_until = Some(until);
                    redraw = true;
                } else {
                    stats_osd_until = None;
                }
            }
            if std::env::var_os("LGX_DEBUG_FPS").is_some() {
                let cpu = match (last_cpu, cpu_ticks()) {
                    (Some((before, since)), Some(now)) => {
                        let ticks = now.saturating_sub(before) as f64;
                        Some(ticks / clock_ticks_per_second() / since.elapsed().as_secs_f64() * 100.0)
                    }
                    _ => None,
                };
                last_cpu = cpu_ticks().map(|now| (now, Instant::now()));
                match cpu {
                    Some(cpu) => eprintln!(
                        "lgx-client: fps {df}, decoded {d_dec}, overwritten {d_drop}, draw p50/p99 {draw_p50:.2}/{draw_p99:.2} ms, cpu {cpu:.0}%, {:.1} MiB/s",
                        bytes as f64 / 1_048_576.0
                    ),
                    None => eprintln!(
                        "lgx-client: fps {df}, decoded {d_dec}, overwritten {d_drop}, draw p50/p99 {draw_p50:.2}/{draw_p99:.2} ms, {:.1} MiB/s",
                        bytes as f64 / 1_048_576.0
                    ),
                }
            }
            draw_samples.clear();
            next_stats = Instant::now() + Duration::from_secs(1);
        }
        if let Some(err) = sink.error() {
            eprintln!("lgx-client: decoder: {err}");
            quit = true;
        }
        if let Some(limit) = max_frames {
            if shown >= limit {
                break;
            }
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    if shown > 0 && elapsed > 0.0 {
        eprintln!(
            "lgx-client: {shown} frames in {elapsed:.1} s ({:.1} fps), {} dropped",
            shown as f64 / elapsed,
            sink.dropped.load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    // Stop the network reader, then the decode thread, *before* the SDL GL
    // context and the CUDA context are torn down. CUDA-EGL interop crashes if
    // a decode thread is still using its context during process exit.
    conn.close();
    let _ = decoder.join();

    if connection_lost && s.reconnect {
        // `attempt` is the number of failed connection attempts already made.
        // The restarted process increments it again when its own connection
        // fails, so it must receive the old value, not the upcoming retry
        // number; otherwise the first dialog says "attempt 2".
        let attempt = if session_started.elapsed() > Duration::from_secs(30) {
            0
        } else {
            reconnect_attempt()
        };
        let next = attempt + 1;
        if next > MAX_RECONNECT_ATTEMPTS {
            eprintln!(
                "lgx-client: giving up after {MAX_RECONNECT_ATTEMPTS} reconnect attempts"
            );
            std::process::exit(1);
        }
        reconnect_exec(attempt);
    }
}

const RECONNECT_ATTEMPT_ENV: &str = "LGX_RECONNECT_ATTEMPT";
const MAX_RECONNECT_ATTEMPTS: u32 = 10;

fn reconnect_attempt() -> u32 {
    std::env::var(RECONNECT_ATTEMPT_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// 1-based retry number shown in the dialog: the first retry waits 1 s.
fn reconnect_delay(attempt: u32) -> Duration {
    let step = attempt.saturating_sub(1).min(4);
    Duration::from_secs((1u64 << step).min(15))
}

/// `attempt` is the number of failed attempts so far. The new process will
/// increment it before showing its first reconnect dialog.
fn reconnect_exec(attempt: u32) -> ! {
    eprintln!(
        "lgx-client: restarting session for reconnect (attempt {}/{MAX_RECONNECT_ATTEMPTS})",
        attempt + 1
    );
    let exe = match std::env::current_exe() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lgx-client: cannot find own executable: {e}");
            std::process::exit(1);
        }
    };
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let err = Command::new(exe)
        .args(args)
        .env(RECONNECT_ATTEMPT_ENV, attempt.to_string())
        .exec();
    eprintln!("lgx-client: cannot restart for reconnect: {err}");
    std::process::exit(1);
}

fn toggle_fullscreen(window: &mut sdl3::video::Window) {
    let full = window.fullscreen_state();
    let now_full = full != sdl3::video::FullscreenType::Off;
    window.set_fullscreen(!now_full).ok();
}

/// Process CPU time in clock ticks (utime + stime from /proc/self/stat).
fn cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The comm field may contain spaces and parentheses; everything after the
    // last ')' is space separated, starting with the state (field 3).
    let rest = stat.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace();
    fields.next()?; // state
    let utime: u64 = fields.nth(10)?.parse().ok()?; // field 14
    let stime: u64 = fields.next()?.parse().ok()?; // field 15
    Some(utime + stime)
}

fn clock_ticks_per_second() -> f64 {
    let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if tck > 0 { tck as f64 } else { 100.0 }
}

fn split_host_port(addr: &str) -> (String, Option<u16>) {
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some((host, port)) = rest.split_once("]:") {
            if let Ok(port) = port.parse::<u16>() {
                return (host.to_string(), Some(port));
            }
        }
    } else if let Some((host, port)) = addr.rsplit_once(':') {
        if !host.contains(':') {
            if let Ok(port) = port.parse::<u16>() {
                return (host.to_string(), Some(port));
            }
        }
    }
    (addr.to_string(), None)
}

fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
fn display_host(addr: &str) -> &str {
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return &rest[..end];
        }
    }
    addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr)
}

fn save_frame(path: &str, frame: &dec::Frame) {
    match &frame.pixels {
        dec::Pixels::Nv12(n) => write_frame(path, frame, n),
        // A zero-copy frame has no CPU pixels; download it once for the file.
        dec::Pixels::Dmabuf(_) => match frame.download() {
            Ok(cpu) => match &cpu.pixels {
                dec::Pixels::Nv12(n) => write_frame(path, &cpu, n),
                dec::Pixels::Dmabuf(_) => {
                    eprintln!("lgx-client: screenshot: cannot download the frame")
                }
            },
            Err(e) => eprintln!("lgx-client: screenshot: {e}"),
        },
    }
}

fn write_frame(path: &str, frame: &dec::Frame, n: &dec::Nv12) {
    let (w, h) = (frame.width, frame.height);
    let y = unsafe { std::slice::from_raw_parts(n.y, n.y_stride as usize * h as usize) };
    let uv = unsafe { std::slice::from_raw_parts(n.uv, n.uv_stride as usize * (h as usize / 2)) };
    match write_ppm_nv12(path, w, h, y, n.y_stride as usize, uv, n.uv_stride as usize) {
        Ok(()) => eprintln!("lgx-client: saved {path} ({w}x{h})"),
        Err(e) => eprintln!("lgx-client: screenshot: {e}"),
    }
}

fn run_headless(
    sink: std::sync::Arc<dec::Sink>,
    conn: &std::sync::Arc<net::Conn>,
    mut screenshot: Option<String>,
    deadline: Option<Instant>,
    max_frames: Option<u64>,
) {
    let t0 = Instant::now();
    let mut frames = 0u64;
    loop {
        if let Some(f) = sink.take() {
            frames += 1;
            if let Some(path) = screenshot.take() {
                if let dec::Pixels::Nv12(n) = &f.pixels {
                    let (w, h) = (f.width, f.height);
                    let y = unsafe {
                        std::slice::from_raw_parts(n.y, n.y_stride as usize * h as usize)
                    };
                    let uv = unsafe {
                        std::slice::from_raw_parts(
                            n.uv,
                            n.uv_stride as usize * (h as usize / 2),
                        )
                    };
                    match write_ppm_nv12(
                        &path,
                        w,
                        h,
                        y,
                        n.y_stride as usize,
                        uv,
                        n.uv_stride as usize,
                    ) {
                        Ok(()) => eprintln!("lgx-client: saved {path} ({w}x{h})"),
                        Err(e) => eprintln!("lgx-client: screenshot: {e}"),
                    }
                }
            }
        }
        if conn.closed.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("lgx-client: server closed the connection");
            break;
        }
        if let Some(err) = sink.error() {
            eprintln!("lgx-client: decoder: {err}");
            break;
        }
        if let Some(limit) = max_frames {
            if frames >= limit {
                break;
            }
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let dt = t0.elapsed().as_secs_f64();
    if frames > 0 && dt > 0.0 {
        eprintln!(
            "lgx-client: {frames} frames in {dt:.1} s ({:.1} fps)",
            frames as f64 / dt
        );
    }
    }

unsafe fn wayland_window(
    window: &sdl3::video::Window,
) -> Option<(*mut std::ffi::c_void, *mut std::ffi::c_void)> {
    #[link(name = "SDL3")]
    extern "C" {
        fn SDL_GetWindowProperties(window: *mut std::ffi::c_void) -> u32;
        fn SDL_GetPointerProperty(
            props: u32,
            name: *const std::ffi::c_char,
            default: *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
    }
    let props = SDL_GetWindowProperties(window.raw() as *mut _);
    if props == 0 {
        return None;
    }
    let surface = SDL_GetPointerProperty(
        props,
        c"SDL.window.wayland.surface".as_ptr(),
        std::ptr::null_mut(),
    );
    let display = SDL_GetPointerProperty(
        props,
        c"SDL.window.wayland.display".as_ptr(),
        std::ptr::null_mut(),
    );
    if surface.is_null() || display.is_null() {
        eprintln!("lgx-client: not running on Wayland, shortcuts are not inhibited");
        None
    } else {
        Some((display, surface))
    }
}

fn send_key(conn: &net::Conn, keyboard: Option<&mut Keyboard>, code: u16, down: bool) {
    if let Some(kb) = keyboard {
        let (dep, lat, lock, group) = kb.update(code as u32, down);
        let _ = conn.send(
            Msg {
                typ: msg::MODS,
                flags: group as u8,
                code: lock as u16,
                a: dep as i32,
                b: lat as i32,
            },
            &[],
        );
    }
    let _ = conn.send(Msg::new(msg::KEY, code, down as i32, 0), &[]);
}


fn btn_code(b: sdl3::mouse::MouseButton) -> Option<u16> {
    use sdl3::mouse::MouseButton::*;
    match b {
        Left => Some(evdev::BTN_LEFT),
        Right => Some(evdev::BTN_RIGHT),
        Middle => Some(evdev::BTN_MIDDLE),
        X1 => Some(evdev::BTN_SIDE),
        X2 => Some(evdev::BTN_EXTRA),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backoff_is_bounded() {
        assert_eq!(reconnect_delay(1), Duration::from_secs(1));
        assert_eq!(reconnect_delay(2), Duration::from_secs(2));
        assert_eq!(reconnect_delay(5), Duration::from_secs(15));
        assert_eq!(reconnect_delay(99), Duration::from_secs(15));
    }

    #[test]
    fn cpu_ticks_track_real_cpu_time() {
        let before = cpu_ticks().expect("/proc/self/stat must be readable on Linux");
        let mut acc = 0u64;
        for i in 0..2_000_000u64 {
            acc = acc.wrapping_add(i);
        }
        std::hint::black_box(acc);
        let after = cpu_ticks().expect("/proc/self/stat must be readable on Linux");
        // A misparsed field (starttime, rss, ...) would be far outside this.
        assert!(before < 3600 * 100, "cpu ticks look misparsed: {before}");
        assert!(after >= before, "cpu ticks must not go backwards");
    }

    #[test]
    fn host_display_strips_ports() {
        assert_eq!(display_host("192.168.122.25:5902"), "192.168.122.25");
        assert_eq!(display_host("[::1]:5902"), "::1");
        assert_eq!(display_host("example"), "example");
    }
}
