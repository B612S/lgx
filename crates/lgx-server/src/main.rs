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
mod config;
mod dmabuf;
pub mod glconv;
mod input;
mod net;
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
}

fn parse_args() -> Result<Args, String> {
    let cfg = config::load_or_create();
    eprintln!("lgx-server: config {}", config::path().display());
    let fps = match cfg.get("video.fps") {
        None | Some("") | Some("auto") => None,
        Some(v) => Some(v.parse().map_err(|_| "video.fps must be auto or a number")?),
    };
    let mut a = Args {
        listen: cfg
            .get("network.listen")
            .unwrap_or("0.0.0.0:5902")
            .to_string(),
        fps,
        bitrate: cfg.get_u32("video.bitrate").unwrap_or(40_000_000),
        output: cfg
            .get("video.output")
            .filter(|v| !v.is_empty())
            .map(str::to_string),
        no_input: !cfg.get_bool("input.enabled").unwrap_or(true),
        audio: cfg.get_bool("audio.enabled").unwrap_or(true),
        audio_listen: cfg
            .get("network.audio_listen")
            .unwrap_or("0.0.0.0:5903")
            .to_string(),
        token: None,
        low_power: cfg.get_bool("video.low_power").unwrap_or(true),
        async_depth: cfg.get_u32("video.async_depth").unwrap_or(2),
        vpp: cfg.get_bool("video.vpp").unwrap_or(true),
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
            "-h" | "--help" => {
                println!(
                    "lgx-server [--listen ADDR] [--fps N] [--bitrate BPS] [--output NAME]\n\
                     \x20          (--fps defaults to the output refresh rate)\n\
                     \x20          [--no-low-power] [--async-depth N] [--no-input]\n\
                     \x20          [--token T] [--no-audio]\n\
                     \x20          [--audio-listen ADDR] [--vpp|--no-vpp]\n\
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
            let c = glconv::GlConv::new(g.device()).ok_or("GL conversion is unavailable")?;
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

    let hub = Hub::new();
    let input_tx = if args.no_input {
        let (tx, _) = std::sync::mpsc::channel();
        tx
    } else {
        input::spawn(None)
    };

    let token = match args.token.clone().or_else(lgx_proto::auth::load) {
        Some(t) => Arc::new(t),
        None => {
            let t = lgx_proto::auth::generate();
            match lgx_proto::auth::save(&t) {
                Ok(()) => eprintln!(
                    "lgx-server: no token configured, generated {} ({})",
                    t,
                    lgx_proto::auth::config_path().display()
                ),
                Err(e) => eprintln!("lgx-server: cannot save token: {e}"),
            }
            Arc::new(t)
        }
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
        let token = Some(token.clone());
        std::thread::Builder::new()
            .name("lgx-audio".into())
            .spawn(move || {
                if let Err(e) = audio::serve(&addr, token) {
                    eprintln!("lgx-server: audio: {e}");
                }
            })
            .ok();
    }

    {
        let hub = hub.clone();
        let last_remote = last_remote.clone();
        std::thread::Builder::new()
            .name("lgx-clip-poll".into())
            .spawn(move || {
                let mut last_sent = String::new();
                loop {
                    std::thread::sleep(Duration::from_millis(300));
                    let clip = match clip::get() {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let sig = clip::signature(&clip);
                    if let clip::Clip::Text(text) = &clip {
                        if text.is_empty() {
                            continue;
                        }
                    }
                    if sig == last_sent || sig == *last_remote.lock().unwrap() {
                        last_sent = sig;
                        continue;
                    }
                    match &clip {
                        clip::Clip::Text(text) => hub.broadcast_clip(text),
                        clip::Clip::Files(paths) => hub.broadcast_files(paths),
                    }
                    last_sent = sig;
                }
            })
            .ok();
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
    let token2 = Some(token.clone());
    let remote2 = last_remote.clone();
    std::thread::Builder::new()
        .name("lgx-listen".into())
        .spawn(move || {
            if let Err(e) = net::listen(&listen, hub2, input2, clip2, token2, remote2) {
                eprintln!("lgx-server: {e}");
            }
        })
        .expect("listen thread");

    stream.run(&hub)
}
