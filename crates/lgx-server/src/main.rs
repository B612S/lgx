//! lgx-server: zero-copy HEVC streaming of a wlroots compositor session.
//!
//! Capture goes from wlr-screencopy into a GBM dma-buf, the GPU converts it to
//! NV12 in place, and VAAPI encodes HEVC into the same surface. The encoded
//! Annex-B access units are handed to the TCP writer; the CPU never touches a
//! pixel.
//!
//! Input is injected through the kernel-level `lgx-uinput` helper. Audio uses
//! a second TCP port, clipboard uses wl-copy/wl-paste.

pub mod audio;
mod cpuconv;
mod capture;
mod clip;
mod clients;
mod config;
mod dmabuf;
pub mod glconv;
mod input;
mod net;
mod pair;
pub mod protocols;
mod vaenc;

use std::sync::Arc;
use std::time::{Duration, Instant};

use net::Hub;

struct Args {
    listen: String,
    fps: Option<u32>,
    bitrate: u32,
    output: Option<String>,
    no_input: bool,
    audio: bool,
    audio_listen: String,
    token: Option<String>,
    low_power: bool,
    async_depth: u32,
    vpp: bool,
    pair: bool,
    pair_listen: String,
    pair_name: Option<String>,
    revoke: Option<String>,
    rotate: Option<String>,
    list: bool,
    video_port: u16,
    audio_port: u16,
    pair_port: u16,
}

fn interface_ip(name: &str) -> Option<std::net::IpAddr> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
        return None;
    }
    let mut out = None;
    let mut cur = ifap;
    while !cur.is_null() {
        let ifa = unsafe { &*cur };
        let addr = ifa.ifa_addr;
        if !addr.is_null()
            && unsafe { (*addr).sa_family as i32 } == libc::AF_INET
            && unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) } == cname.as_c_str()
        {
            let sin = unsafe { &*(addr as *const libc::sockaddr_in) };
            out = Some(std::net::IpAddr::V4(std::net::Ipv4Addr::from(
                u32::from_be(sin.sin_addr.s_addr),
            )));
            break;
        }
        cur = unsafe { (*cur).ifa_next };
    }
    unsafe { libc::freeifaddrs(ifap) };
    out
}

fn parse_args() -> Result<Args, String> {
    let cfg = config::load_or_create();
    eprintln!("lgx-server: config {}", config::path().display());
    let fps = match cfg.get("video.fps") {
        None | Some("") | Some("auto") => None,
        Some(v) => Some(v.parse().map_err(|_| "video.fps must be auto or a number")?),
    };
    let interface = cfg.get("network.interface").unwrap_or("");
    let video_port = cfg.get_u32("network.video_port").unwrap_or(5902) as u16;
    let audio_port = cfg.get_u32("network.audio_port").unwrap_or(5903) as u16;
    let pair_port = cfg.get_u32("network.pair_port").unwrap_or(5904) as u16;
    let bind = if interface.is_empty() {
        None
    } else {
        Some(
            interface_ip(interface)
                .ok_or_else(|| format!("network.interface {interface} has no IPv4 address"))?,
        )
    };
    let bind_host = bind.map(|ip| ip.to_string());
    let make_addr = |old_keys: &[&str], port: u16, label: &str| -> Result<String, String> {
        for key in old_keys {
            if let Some(v) = cfg.get(key) {
                if !v.is_empty() {
                    return Ok(v.to_string());
                }
            }
        }
        bind_host
            .as_ref()
            .map(|host| format!("{host}:{port}"))
            .ok_or_else(|| format!("set network.interface (or an explicit {label} address)"))
    };
    let listen = make_addr(&["network.video_listen", "network.listen"], video_port, "video")?;
    let audio_listen = make_addr(&["network.audio_listen"], audio_port, "audio")?;
    let pair_listen = make_addr(&["network.pair_listen"], pair_port, "pair")?;
    let mut a = Args {
        listen,
        fps,
        bitrate: cfg.get_u32("video.bitrate").unwrap_or(40_000_000),
        output: cfg
            .get("video.output")
            .filter(|v| !v.is_empty())
            .map(str::to_string),
        no_input: !cfg.get_bool("input.enabled").unwrap_or(true),
        audio: cfg.get_bool("audio.enabled").unwrap_or(true),
        audio_listen,
        token: None,
        low_power: cfg.get_bool("video.low_power").unwrap_or(true),
        async_depth: cfg.get_u32("video.async_depth").unwrap_or(2),
        vpp: cfg.get_bool("video.vpp").unwrap_or(true),
        pair: false,
        pair_listen,
        pair_name: None,
        revoke: None,
        rotate: None,
        list: false,
        video_port,
        audio_port,
        pair_port,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--listen" => a.listen = it.next().ok_or("--listen requires an address")?,
            "--fps" => {
                a.fps = Some(it.next().ok_or("--fps")?.parse().map_err(|_| "--fps")?)
            }
            "--bitrate" => {
                a.bitrate = it.next().ok_or("--bitrate")?.parse().map_err(|_| "--bitrate")?
            }
            "--output" => a.output = Some(it.next().ok_or("--output")?),
            "--no-input" => a.no_input = true,
            "--no-low-power" => a.low_power = false,
            "--async-depth" => {
                a.async_depth = it.next().ok_or("--async-depth")?.parse().map_err(|_| "--async-depth")?
            }
            "--audio" => a.audio = true,
            "--no-audio" => a.audio = false,
            "--audio-listen" => a.audio_listen = it.next().ok_or("--audio-listen")?,
            "--vpp" => a.vpp = true,
            "--no-vpp" => a.vpp = false,
            "--token" => a.token = Some(it.next().ok_or("--token")?),
            "--pair" => a.pair = true,
            "--pair-listen" => a.pair_listen = it.next().ok_or("--pair-listen")?,
            "--pair-name" => a.pair_name = Some(it.next().ok_or("--pair-name")?),
            "--revoke" => a.revoke = Some(it.next().ok_or("--revoke")?),
            "--rotate" => a.rotate = Some(it.next().ok_or("--rotate")?),
            "--list" => a.list = true,
            "-h" | "--help" => {
                println!(
                    "lgx-server [--listen ADDR] [--fps N] [--bitrate BPS] [--output NAME]\n\
                     \x20          (--fps defaults to the output refresh rate)\n\
                     \x20          [--no-low-power] [--async-depth N] [--no-input]\n\
                     \x20          [--token T] [--no-audio]\n\
                     \x20          [--audio-listen ADDR] [--vpp|--no-vpp]\n\
                     \x20          [--pair] [--pair-listen ADDR] [--pair-name NAME]\n\
                     \x20          [--list] [--revoke ID|NAME] [--rotate ID|NAME]\n\
                     \x20          settings: ~/.config/lgx/server.conf"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if let Some(fps) = a.fps {
        if !(1..=1000).contains(&fps) {
            return Err("--fps must be within 1..1000".into());
        }
    }
    if a.bitrate < 100_000 {
        return Err("--bitrate must be at least 100000 bit/s".into());
    }
    if !(1..=4).contains(&a.async_depth) {
        return Err("--async-depth must be within 1..4".into());
    }
    Ok(a)
}

struct FdGuard(i32);

impl Drop for FdGuard {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe { libc::close(self.0) };
        }
    }
}

/// The HEVC pipeline: capture -> GL NV12 -> VAAPI -> dma-buf packets.
struct Stream {
    cap: capture::Capture,
    conv: Option<glconv::GlConv>,
    planes: Option<glconv::Planes>,
    enc: vaenc::VaEnc,
    w: u32,
    h: u32,
    fps: u32,
    cpu: bool,
    vpp: bool,
    _target: Option<FdGuard>,
}

impl Stream {
    fn new(args: &Args) -> Result<Self, String> {
        let mut cap = capture::Capture::connect(args.output.as_deref(), true)
            .map_err(|e| format!("capture: {e}"))?;
        // The Intel render node supports VPP import of the captured tiled
        // RGB dma-buf, which keeps the conversion off the 3D engine. Other
        // drivers keep the GL path (LGX_NO_VPP forces it).
        let intel = std::fs::read_to_string("/sys/class/drm/renderD128/device/vendor")
            .map(|v| v.trim().eq_ignore_ascii_case("0x8086"))
            .unwrap_or(false);
        let cpu = std::env::var_os("LGX_CPU_CONVERT").is_some();
        let vpp = args.vpp && !cpu && intel && std::env::var_os("LGX_NO_VPP").is_none();
        cap.state.use_gbm = true;
        if cpu {
            // Linear GPU buffer, mapped for the CPU: the compositor still
            // blits once, but the RGB->NV12 conversion leaves the 3D engine.
            cap.state.gbm_linear = true;
        }
        cap.begin().map_err(|e| format!("capture begin: {e}"))?;
        let (w, h) = {
            let info = cap.state.info.as_ref().ok_or("capture returned no frame info")?;
            (info.width, info.height)
        };
        if w < 16 || h < 16 {
            return Err(format!("unexpected capture size {w}x{h}"));
        }
        // Default to the panel's refresh rate; --fps is only an override.
        let fps = args.fps.unwrap_or_else(|| {
            let hz = cap.refresh_hz();
            if hz > 0 { hz } else { 160 }
        });

        let helper =
            std::env::var("LGX_VAENC").unwrap_or_else(|_| "/usr/lib/lgx/lgx-vaenc".to_string());
        let mut target = None;
        let mut conv = None;
        let mut planes = None;
        let mut enc = if vpp {
            vaenc::VaEnc::start_vpp(
                "/tmp/lgx-vaenc.sock",
                &helper,
                w,
                h,
                fps,
                args.bitrate,
                args.low_power,
                args.async_depth,
            )
            .map_err(|e| format!("vaenc: {e}"))?
        } else if cpu {
            vaenc::VaEnc::start_cpu(
                "/tmp/lgx-vaenc.sock",
                &helper,
                w,
                h,
                fps,
                args.bitrate,
                args.low_power,
                args.async_depth,
            )
            .map_err(|e| format!("vaenc: {e}"))?
        } else {
            let y_pitch = match lgx_server::gbm::alloc_linear_fmt(w, h, 0x2020_3852) {
                Ok((_, fd, pitch, _)) => {
                    unsafe { libc::close(fd) };
                    pitch
                }
                Err(e) => return Err(format!("cannot probe the NV12 pitch: {e}")),
            };
            let uv_off = y_pitch * h;
            let total = uv_off + y_pitch * (h / 2);
            let target_h = total.div_ceil(y_pitch);
            let (_dy, y_fd, y_pitch, _ys) =
                lgx_server::gbm::alloc_linear_fmt(w, target_h, 0x2020_3852)
                    .map_err(|e| format!("cannot allocate the NV12 target: {e}"))?;
            target = Some(FdGuard(y_fd));
            let g = cap.last_gbm().ok_or("no GBM device for GL conversion")?;
            let c = unsafe { glconv::GlConv::new(g.device()) }.ok_or("GL conversion is unavailable")?;
            let mut e = vaenc::VaEnc::start_import(
                "/tmp/lgx-vaenc.sock",
                &helper,
                w,
                h,
                fps,
                args.bitrate,
                args.low_power,
                args.async_depth,
            )
            .map_err(|e| format!("vaenc: {e}"))?;
            if !e
                .import_surface(
                    target.as_ref().unwrap().0,
                    [y_pitch, y_pitch, uv_off, y_pitch * (h / 2)],
                )
                .unwrap_or(false)
            {
                return Err("VAAPI refused the NV12 surface".into());
            }
            planes = Some(unsafe {
                c.prepare_planes(target.as_ref().unwrap().0, uv_off, y_pitch, w, h)
            }
            .ok_or("cannot import the NV12 planes into GL")?);
            conv = Some(c);
            e
        };
        let _ = &mut enc;
        if vpp {
            eprintln!("lgx-server: VPP RGB->NV12 conversion (tiled capture)");
        } else if cpu {
            eprintln!("lgx-server: CPU NV12 conversion (linear capture)");
        }

        eprintln!(
            "lgx-server: HEVC ready ({w}x{h}@{fps}, {} Mbit/s, low_power={}{})",
            args.bitrate / 1_000_000,
            args.low_power,
            if cpu { ", CPU NV12" } else { "" }
        );
        Ok(Self {
            cap,
            conv,
            planes,
            enc,
            w,
            h,
            fps,
            cpu,
            vpp,
            _target: target,
        })
    }

    fn run(mut self, hub: &Arc<Hub>) -> ! {
        let mut t0 = Instant::now();
        let mut frames = 0u64;
        let mut bytes = 0u64;
        let mut sum_wait = 0.0f64;
        let mut sum_render = 0.0f64;
        let mut sum_encode = 0.0f64;
        let mut idle = true;

        loop {
            let loop_start = Instant::now();
            if hub.client_count() == 0 {
                idle = true;
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            if idle {
                idle = false;
                t0 = Instant::now();
                frames = 0;
                bytes = 0;
                sum_wait = 0.0;
                sum_render = 0.0;
                sum_encode = 0.0;
            }
            if hub.take_keyframe() {
                if let Err(e) = self.enc.force_keyframe() {
                    eprintln!("lgx-server: vaenc keyframe: {e}");
                }
            }
            self.cap.state.overlay_cursor = hub.cursor_overlay();

            let t_wait = Instant::now();
            if let Err(e) = self.cap.next_frame() {
                eprintln!("lgx-server: capture: {e}");
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let wait_ms = t_wait.elapsed().as_secs_f64() * 1000.0;

            let t_work = Instant::now();
            let packets = if self.vpp {
                let (fd, stride, modifier, gw, gh) = match self.cap.last_gbm() {
                    Some(g) => (g.fd, g.stride, g.modifier, g.width, g.height),
                    None => continue,
                };
                let fourcc = self
                    .cap
                    .state
                    .info
                    .as_ref()
                    .map(|i| i.format)
                    .unwrap_or(0x3432_5241);
                let t_enc = Instant::now();
                let packets = match self
                    .enc
                    .vpp_send(fd as u32, gw, gh, stride, fourcc, modifier, fd)
                {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("lgx-server: vaenc vpp: {e}");
                        std::process::exit(1);
                    }
                };
                let encode_ms = t_enc.elapsed().as_secs_f64() * 1000.0;
                sum_wait += wait_ms;
                sum_render += 0.0;
                sum_encode += encode_ms;
                frames += 1;
                packets
            } else if self.cpu {
                let info = match self.cap.state.info.as_ref() {
                    Some(i) => i,
                    None => continue,
                };
                let stride = info.stride as usize;
                let need = stride * self.h as usize;
                if self.cap.state.staged.len() < need {
                    continue;
                }
                let mut nv12 = Vec::new();
                cpuconv::xrgb_to_nv12(
                    &self.cap.state.staged[..need],
                    self.w as usize,
                    self.h as usize,
                    stride,
                    &mut nv12,
                );
                let render_ms = t_work.elapsed().as_secs_f64() * 1000.0;
                let t_enc = Instant::now();
                let packets = match self.enc.send_cpu(&nv12) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("lgx-server: vaenc: {e}");
                        std::process::exit(1);
                    }
                };
                let encode_ms = t_enc.elapsed().as_secs_f64() * 1000.0;
                sum_wait += wait_ms;
                sum_render += render_ms;
                sum_encode += encode_ms;
                frames += 1;
                packets
            } else {
                let tex = {
                    let g = match self.cap.last_gbm() {
                        Some(g) => g,
                        None => continue,
                    };
                    match unsafe { self.conv.as_ref().unwrap().import_rgb(g.fd, g.stride, g.modifier, self.w, self.h) } {
                        Some(t) => t,
                        None => continue,
                    }
                };
                unsafe { self.conv.as_ref().unwrap().draw_planes(self.planes.as_ref().unwrap(), tex, self.w, self.h) };
                let render_ms = t_work.elapsed().as_secs_f64() * 1000.0;
                let t_enc = Instant::now();
                let packets = match self.enc.encode(0) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("lgx-server: vaenc: {e}");
                        std::process::exit(1);
                    }
                };
                let encode_ms = t_enc.elapsed().as_secs_f64() * 1000.0;
                sum_wait += wait_ms;
                sum_render += render_ms;
                sum_encode += encode_ms;
                frames += 1;
                packets
            };
            for data in packets {
                if !data.is_empty() {
                    bytes += data.len() as u64;
                    hub.broadcast_video(data);
                }
            }
            let period = if std::env::var_os("LGX_DEBUG_FPS").is_some() { 1 } else { 5 };
            if t0.elapsed() >= Duration::from_secs(period) {
                let n = frames.max(1) as f64;
                eprintln!(
                    "lgx-server: HEVC {:.1} fps, {:.1} MiB/s, wait {:.2} ms, render {:.2} ms, encode {:.2} ms, {} clients, {} dropped",
                    frames as f64 / t0.elapsed().as_secs_f64(),
                    bytes as f64 / t0.elapsed().as_secs_f64() / 1_048_576.0,
                    sum_wait / n,
                    sum_render / n,
                    sum_encode / n,
                    hub.client_count(),
                    hub.take_dropped()
                );
                t0 = Instant::now();
                frames = 0;
                bytes = 0;
                sum_wait = 0.0;
                sum_render = 0.0;
                sum_encode = 0.0;
            }
            capture::throttle(loop_start, self.fps);
        }
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("lgx-server: {e}");
            std::process::exit(2);
        }
    };

    if let Err(e) = clients::ensure() {
        eprintln!("lgx-server: cannot initialize client registry: {e}");
        std::process::exit(1);
    }
    if let Some(token) = args.token.clone() {
        if let Err(e) = clients::set_default(&token) {
            eprintln!("lgx-server: cannot update default token: {e}");
            std::process::exit(1);
        }
    }
    if args.list {
        clients::print_list();
        return;
    }
    if let Some(key) = args.revoke.clone() {
        match clients::revoke(&key) {
            Ok(0) => eprintln!("lgx-server: no client matching {key}"),
            Ok(n) => eprintln!("lgx-server: revoked {n} client(s) matching {key}"),
            Err(e) => eprintln!("lgx-server: revoke: {e}"),
        }
        return;
    }
    if let Some(key) = args.rotate.clone() {
        match clients::rotate(&key) {
            Ok(Some(token)) => {
                println!("{token}");
                eprintln!(
                    "lgx-server: rotated {key}; put this token into [{key}] in client.conf"
                );
            }
            Ok(None) => eprintln!("lgx-server: no client matching {key}"),
            Err(e) => eprintln!("lgx-server: rotate: {e}"),
        }
        return;
    }
    if args.pair {
        if let Err(e) = pair::run(&args.pair_listen, args.pair_name.as_deref(), args.video_port, args.audio_port, args.pair_port) {
            eprintln!("lgx-server: pairing: {e}");
            std::process::exit(1);
        }
        return;
    }

    let hub = Hub::new();
    {
        let hub = hub.clone();
        std::thread::Builder::new()
            .name("lgx-revoke-watch".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(1));
                if let Some(valid) = clients::valid_tokens() {
                    hub.revoke_missing(&valid);
                }
            })
            .ok();
    }
    let input_tx = if args.no_input {
        let (tx, _) = std::sync::mpsc::channel();
        tx
    } else {
        input::spawn(None)
    };

    let last_remote = Arc::new(std::sync::Mutex::new(String::new()));
    let clip_set: Arc<dyn Fn(&str) + Send + Sync> = {
        let last = last_remote.clone();
        Arc::new(move |text: &str| {
            *last.lock().unwrap() =
                clip::signature(&clip::Clip::Text(text.to_string()));
            if let Err(e) = clip::set_text(text) {
                eprintln!("lgx-server: wl-copy: {e}");
            }
        })
    };

    if args.audio {
        let addr = args.audio_listen.clone();
        std::thread::Builder::new()
            .name("lgx-audio".into())
            .spawn(move || {
                if let Err(e) = audio::serve(&addr) {
                    eprintln!("lgx-server: audio: {e}");
                }
            })
            .ok();
    }

    {
        let hub = hub.clone();
        let last_remote = last_remote.clone();
        clip::watch(
            move |clip| {
                let sig = clip::signature(&clip);
                if sig == *last_remote.lock().unwrap() {
                    return;
                }
                match clip {
                    clip::Clip::Rich { html, text } if !html.is_empty() => {
                        hub.broadcast_rich(&html, &text);
                    }
                    clip::Clip::Text(text) if !text.is_empty() => {
                        hub.broadcast_clip(&text);
                    }
                    clip::Clip::File { name, data } => {
                        eprintln!("lgx-server: file clipboard {} ({} bytes)", name, data.len());
                        hub.broadcast_file(&name, &data);
                    }
                    _ => {}
                }
            },
            false,
        );
    }

    let stream = match Stream::new(&args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lgx-server: cannot start HEVC streaming: {e}");
            std::process::exit(1);
        }
    };
    hub.set_geometry(stream.w, stream.h);

    let listen = args.listen.clone();
    let hub2 = hub.clone();
    let input2 = input_tx.clone();
    let clip2 = clip_set.clone();
    let remote2 = last_remote.clone();
    std::thread::Builder::new()
        .name("lgx-listen".into())
        .spawn(move || {
            if let Err(e) = net::listen(&listen, hub2, input2, clip2, remote2) {
                eprintln!("lgx-server: {e}");
            }
        })
        .expect("listen thread");

    stream.run(&hub)
}
