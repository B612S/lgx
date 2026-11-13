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
mod protocols;

use std::io::Write;
use std::time::{Duration, Instant};

use lgx_proto::{evdev, msg, Msg};
use sdl3::event::Event;

use dec::Backend;

struct Args {
    connect: Option<String>,
    no_window: bool,
    fullscreen: Option<bool>,
    vsync: Option<bool>,
    screenshot: Option<String>,
    bench: Option<f64>,
    size: Option<(u32, u32)>,
    token: Option<String>,
    client_id: Option<String>,
    audio: Option<bool>,
    audio_connect: Option<String>,
    decoder: Option<String>,
    cursor: Option<bool>,
    frames: Option<u64>,
    pair: Option<String>,
    pair_code: Option<String>,
}

struct Settings {
    server: String,
    width: Option<u32>,
    height: Option<u32>,
    fullscreen: bool,
    vsync: bool,
    audio: bool,
    audio_connect: Option<String>,
    token: String,
    client_id: String,
    decoder: String,
    cursor_remote: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        connect: None,
        no_window: false,
        fullscreen: None,
        vsync: None,
        screenshot: None,
        bench: None,
        size: None,
        token: None,
        client_id: None,
        audio: None,
        audio_connect: None,
        decoder: None,
        cursor: None,
        frames: None,
        pair: None,
        pair_code: None,
    };
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
            "--no-vsync" => a.vsync = Some(false),
            "--audio" => a.audio = Some(true),
            "--no-audio" => a.audio = Some(false),
            "--audio-connect" => a.audio_connect = Some(it.next().ok_or("--audio-connect")?),
            "--screenshot" => a.screenshot = Some(it.next().ok_or("--screenshot")?),
            "--token" => a.token = Some(it.next().ok_or("--token")?),
            "--client-id" => a.client_id = Some(it.next().ok_or("--client-id")?),
            "--decoder" => a.decoder = Some(it.next().ok_or("--decoder")?),
            "--bench" => a.bench = Some(it.next().ok_or("--bench")?.parse().map_err(|_| "--bench")?),
            "--frames" => a.frames = Some(it.next().ok_or("--frames")?.parse().map_err(|_| "--frames")?),
            "--cursor" => {
                a.cursor = Some(match it.next().ok_or("--cursor")?.as_str() {
                    "local" => false,
                    "remote" => true,
                    other => return Err(format!("--cursor must be local or remote, not {other}")),
                })
            }
            "--size" => {
                let v = it.next().ok_or("--size")?;
                let (w, h) = v.split_once('x').ok_or("--size WxH")?;
                a.size = Some((w.parse().map_err(|_| "--size")?, h.parse().map_err(|_| "--size")?));
            }
            "-h" | "--help" => {
                println!(
                    "lgx-client [--connect HOST:PORT] [--token T] [--size WxH]\n\
                     \x20          [--decoder auto|cuda|vaapi|software]\n\
                     \x20          [--cursor local|remote] [--fullscreen|--windowed]\n\
                     \x20          [--vsync] [--audio] [--audio-connect ADDR]\n\
                     \x20          [--no-window] [--bench SECONDS] [--frames N]\n\
                     \x20          [--screenshot FILE]\n\
                     \x20          [--pair HOST:PORT --pair-code CODE] [--client-id ID]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
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


fn backends(spec: &str, gl_renderer: &str) -> Vec<Backend> {
    match spec {
        "software" | "none" | "sw" => vec![Backend::Software],
        "vaapi" => vec![Backend::Vaapi, Backend::Software],
        "cuda" | "nvidia" | "nvdec" => vec![Backend::Cuda, Backend::Software],
        _ => {
            if gl_renderer.to_ascii_lowercase().contains("nvidia") {
                vec![Backend::Cuda, Backend::Vaapi, Backend::Software]
            } else {
                vec![Backend::Vaapi, Backend::Cuda, Backend::Software]
            }
        }
    }
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
        if let Err(e) = pair::run(&addr, code) {
            eprintln!("lgx-client: pairing with {addr} failed: {e}");
            std::process::exit(1);
        }
        return;
    }

    let cfg = config::Config::load_or_create();
    let creds = lgx_proto::auth::load_credentials();
    let token = args
        .token
        .clone()
        .or_else(|| std::env::var("LGX_TOKEN").ok())
        .or_else(|| cfg.get("token").map(str::to_string))
        .or_else(|| creds.as_ref().map(|c| c.token.clone()))
        .unwrap_or_default();
    let client_id = args
        .client_id
        .clone()
        .or_else(|| creds.as_ref().map(|c| c.id.clone()))
        .unwrap_or_else(|| "default".to_string());
    let s = Settings {
        server: args
            .connect
            .clone()
            .or_else(|| std::env::var("LGX_SERVER").ok())
            .or_else(|| cfg.get("server").map(str::to_string))
            .unwrap_or_else(|| "127.0.0.1:5902".to_string()),
        width: args
            .size
            .map(|(w, _)| w)
            .or_else(|| cfg.get_u32("width")),
        height: args
            .size
            .map(|(_, h)| h)
            .or_else(|| cfg.get_u32("height")),
        fullscreen: args
            .fullscreen
            .or_else(|| cfg.get_bool("fullscreen"))
            .unwrap_or(false),
        vsync: args.vsync.or_else(|| cfg.get_bool("vsync")).unwrap_or(false),
        audio: args.audio.or_else(|| cfg.get_bool("audio")).unwrap_or(false),
        audio_connect: args
            .audio_connect
            .clone()
            .or_else(|| {
                cfg.get("audio_connect")
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
            }),
        token,
        client_id,
        decoder: args
            .decoder
            .clone()
            .or_else(|| cfg.get("decoder").map(str::to_string))
            .unwrap_or_else(|| "auto".to_string()),
        cursor_remote: args
            .cursor
            .or_else(|| cfg.get("cursor").map(|v| v != "local"))
            .unwrap_or(true),
    };
    if s.token.is_empty() {
        eprintln!("lgx-client: no token for {}", s.server);
        eprintln!("lgx-client: pair a client with --pair HOST:PAIRPORT --pair-code CODE");
        std::process::exit(2);
    }
    let this_host = display_host(&s.server).to_string();
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
    let (server, token) = (s.server.clone(), s.token.clone());
    let (mut keyboard, keymap) = match Keyboard::new() {
        Some(v) => (Some(v.0), v.1),
        None => {
            eprintln!("lgx-client: could not build a keymap, keyboard may not work");
            (None, Vec::new())
        }
    };

    let (video_tx, video_rx) = std::sync::mpsc::channel::<dec::Chunk>();
    let (conn, hello) = match net::connect(&server, &keymap, &s.client_id, &token, video_tx) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lgx-client: cannot connect to {server}: {e}");
            std::process::exit(1);
        }
    };
    let _ = conn.send(
        Msg::new(msg::CURSOR, s.cursor_remote as u16, 0, 0),
        &[],
    );
    eprintln!("lgx-client: server HEVC {}x{}", hello.width, hello.height);

    {
        let conn = conn.clone();
        clip::watch(
            move |clip| {
                let sig = clip::signature(&clip);
                if sig == *conn.last_remote.lock().unwrap() {
                    return;
                }
                match clip {
                    clip::Clip::Rich { html, text } if !html.is_empty() => {
                        let payload = lgx_proto::encode_rich(&html, &text);
                        let _ = conn.send(
                            Msg::new(msg::CLIP_SET_HTML, 0, payload.len() as i32, 0),
                            &payload,
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
    }

    if s.audio {
        let addr = s
            .audio_connect
            .clone()
            .unwrap_or_else(|| match server.rsplit_once(':') {
                Some((host, _)) => format!("{host}:5903"),
                None => format!("{server}:5903"),
            });
        let token = token.clone();
        let client_id = s.client_id.clone();
        std::thread::Builder::new()
            .name("lgx-audio".into())
            .spawn(move || {
                if let Err(e) = audio::run(&addr, &client_id, &token) {
                    eprintln!("lgx-client: audio: {e}");
                }
            })
            .ok();
    }

    let deadline = args.bench.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
    let max_frames = args.frames;

    if args.no_window {
        let sink = dec::Sink::new();
        let chosen = {
            let mut b = backends(&s.decoder, "");
            if b.is_empty() {
                b.push(Backend::Software);
            }
            b
        };
        let decoder = dec::spawn(chosen, None, false, video_rx, sink.clone(), None);
        run_headless(sink, &conn, args.screenshot.take(), deadline, max_frames);
        conn.close();
        let _ = decoder.join();
        return;
    }

    let sdl = sdl3::init().expect("SDL");
    let video = sdl.video().expect("SDL video");
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
    let auto_size = s.width.is_none() || s.height.is_none();
    let mut window = video
        .window("lgx", win_w, win_h)
        .vulkan()
        .build()
        .expect("window");
    let display_host = display_host(&s.server);
    window
        .set_title(&format!("lgx — {display_host}"))
        .ok();
    if std::env::var_os("LGX_DEBUG_FPS").is_some() {
        eprintln!("lgx-client: title {:?}", window.title());
    }
    if s.fullscreen {
        window.set_fullscreen(true).ok();
    }
    let mut renderer = match vk::Renderer::new(&window, s.vsync, (hello.width, hello.height)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lgx-client: Vulkan renderer: {e}");
            std::process::exit(1);
        }
    };
    let chosen = backends(&s.decoder, renderer.device_name());
    let want_dmabuf = renderer.can_import_dmabuf();
    if !want_dmabuf {
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
    let decoder = dec::spawn(chosen, None, want_dmabuf, video_rx, sink.clone(), wake);

    let gamepad_subsystem = sdl.gamepad().ok();
    let mut pads = gamepad_subsystem
        .as_ref()
        .map(pad::open_all)
        .unwrap_or_default();
    for p in &pads {
        pad::send(&conn, p, true);
    }
    let mut pump = sdl.event_pump().expect("event pump");
    sdl.mouse().show_cursor(!s.cursor_remote);
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
    let mut inhibitor: Option<inhibit::Guard> = match wm {
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
    let mut last_frame: Option<dec::Frame> = None;
    let mut shown = 0u64;
    let mut screenshot = args.screenshot.take();
    let mut next_stats = Instant::now() + Duration::from_secs(1);
    let mut last_stat_shown = 0u64;
    let mut last_stat_decoded = sink.decoded.load(std::sync::atomic::Ordering::Relaxed);
    let mut last_stat_dropped = sink.dropped.load(std::sync::atomic::Ordering::Relaxed);
    let mut draw_ms_sum = 0.0f64;
    let mut draw_ms_max = 0.0f64;
    let mut draw_count = 0u64;
    let started = Instant::now();
    let mut quit = false;
    let mut redraw = true;
    let mut swallow_f_keyup = false;
    let mut fullscreen_hold: Option<(Instant, (u32, u32))> = None;
    let mut dst = vk::fit_rect(win_w, win_h, win_w, win_h);
    let mut config_saved = s.width.is_some() && s.height.is_some();

    {
        let px = window.size_in_pixels();
        renderer.clear(px);
    }

    while !quit {
        heartbeat.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut frame = sink.take();
        let events: Vec<Event> = if frame.is_some() {
            pump.poll_iter().collect()
        } else {
            let mut v: Vec<Event> = pump.wait_timeout_iter(100).next().into_iter().collect();
            v.extend(pump.poll_iter());
            v
        };
        if frame.is_none() {
            frame = sink.take();
        }
        if conn.closed.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("lgx-client: server closed the connection");
            quit = true;
        }

        for ev in events {
            match ev {
                Event::Quit { .. } => quit = true,
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
                    keycode: Some(sdl3::keyboard::Keycode::G),
                    keymod,
                    ..
                } if keymod.intersects(sdl3::keyboard::Mod::LCTRLMOD | sdl3::keyboard::Mod::RCTRLMOD)
                    && keymod.intersects(sdl3::keyboard::Mod::LALTMOD | sdl3::keyboard::Mod::RALTMOD) =>
                {
                    match inhibitor.take() {
                        Some(g) => {
                            g.release();
                            eprintln!("lgx-client: shortcuts released");
                        }
                        None => {
                            if let Some((display, surface)) = wm {
                                if let Ok(g) = unsafe { inhibit::install(display, surface) } {
                                    inhibitor = Some(g);
                                    eprintln!("lgx-client: shortcuts inhibited");
                                }
                            }
                        }
                    }
                }
                Event::KeyDown {
                    keycode: Some(sdl3::keyboard::Keycode::Q),
                    keymod,
                    ..
                } if keymod.intersects(sdl3::keyboard::Mod::LCTRLMOD | sdl3::keyboard::Mod::RCTRLMOD)
                    && keymod.intersects(sdl3::keyboard::Mod::LALTMOD | sdl3::keyboard::Mod::RALTMOD) =>
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
                Event::MouseButtonDown { mouse_btn, .. } => {
                    if let Some(code) = btn_code(mouse_btn) {
                        let _ = conn.send(Msg::new(msg::BTN, code, 1, 0), &[]);
                    }
                }
                Event::MouseButtonUp { mouse_btn, .. } => {
                    if let Some(code) = btn_code(mouse_btn) {
                        let _ = conn.send(Msg::new(msg::BTN, code, 0, 0), &[]);
                    }
                }
                Event::MouseWheel { y, .. } => {
                    if y != 0.0 {
                        let _ = conn.send(Msg::new(msg::WHEEL, 0, y as i32, 0), &[]);
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
                Event::KeyUp { scancode: Some(sc), .. } => {
                    if let Some(code) = hid::evdev_from_scancode((sc as i32) & 0x1ff) {
                        send_key(&conn, keyboard.as_mut(), code, false);
                    }
                }
                _ => {}
            }
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
        let hold_draw = fullscreen_hold.is_some();
        if hold_draw {
            frame = None;
            redraw = true;
        }
        let mut drawn = false;
        if !hold_draw {
        if let Some(frame) = frame {
            let (w, h) = (frame.width, frame.height);
            if auto_size && (w, h) != img_size {
                window.set_size(w, h).ok();
            }
            if auto_size && !config_saved && w > 1 {
                config_saved = true;
                let mut cfg = config::Config::load();
                cfg.set("width", w);
                cfg.set("height", h);
                let _ = cfg.save();
            }
            let draw_started = Instant::now();
            let drawn_ok = renderer.draw(&frame, win_px);
            let draw_ms = draw_started.elapsed().as_secs_f64() * 1000.0;
            if drawn_ok {
                shown += 1;
                draw_ms_sum += draw_ms;
                draw_ms_max = draw_ms_max.max(draw_ms);
                draw_count += 1;
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
            let avg_draw = if draw_count > 0 { draw_ms_sum / draw_count as f64 } else { 0.0 };
            if std::env::var_os("LGX_DEBUG_FPS").is_some() {
                eprintln!(
                    "lgx-client: fps {df}, decoded {d_dec}, overwritten {d_drop}, draw {avg_draw:.2}/{draw_ms_max:.2} ms, {:.1} MiB/s",
                    bytes as f64 / 1_048_576.0
                );
            }
            draw_ms_sum = 0.0;
            draw_ms_max = 0.0;
            draw_count = 0;
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
}

fn toggle_fullscreen(window: &mut sdl3::video::Window) {
    let full = window.fullscreen_state();
    let now_full = full != sdl3::video::FullscreenType::Off;
    window.set_fullscreen(!now_full).ok();
}

fn display_host(addr: &str) -> &str {
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return &rest[..end];
        }
    }
    addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr)
}

fn save_frame(path: &str, frame: &dec::Frame) {
    if let dec::Pixels::Nv12(n) = &frame.pixels {
        let (w, h) = (frame.width, frame.height);
        let y = unsafe { std::slice::from_raw_parts(n.y, n.y_stride as usize * h as usize) };
        let uv = unsafe {
            std::slice::from_raw_parts(n.uv, n.uv_stride as usize * (h as usize / 2))
        };
        match write_ppm_nv12(&path, w, h, y, n.y_stride as usize, uv, n.uv_stride as usize) {
            Ok(()) => eprintln!("lgx-client: saved {path} ({w}x{h})"),
            Err(e) => eprintln!("lgx-client: screenshot: {e}"),
        }
    } else {
        eprintln!("lgx-client: screenshot needs a CPU NV12 frame");
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
