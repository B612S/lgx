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
mod encoder;
pub mod glconv;
mod input;
mod nvenc;
mod net;
mod pair;
mod portal;
pub mod protocols;
mod time;
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
    /// Skip frames whose content did not change (`video.skip_idle`).
    skip_idle: bool,
    /// Send HEVC access units over UDP instead of the TCP control stream.
    video_udp: bool,
    /// `cbr`, `vbr` or `cqp` (`video.rc_mode`/`--rc-mode`).
    rc_mode: String,
    /// Quantiser for `cqp` (`video.qp`/`--qp`).
    qp: u32,
    async_depth: u32,
    vpp: bool,
    /// Explicit DRM render node (`--drm-node`/`video.drm_node`); auto-detected
    /// when empty.
    drm_node: Option<String>,
    /// `auto`, `wlr` or `portal` (`--capture`/`video.capture`).
    capture: String,
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
    // `network.server` is the bind address written by older packages; keep it
    // working and use it when no interface is configured.
    let bind_host = bind.map(|ip| ip.to_string()).or_else(|| {
        cfg.get("network.server")
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    });
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
        skip_idle: cfg.get_bool("video.skip_idle").unwrap_or(true),
        video_udp: cfg.get_bool("video.udp").unwrap_or(false),
        rc_mode: cfg
            .get("video.rc_mode")
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "vbr".into()),
        qp: cfg.get_u32("video.qp").unwrap_or(24),
        async_depth: cfg.get_u32("video.async_depth").unwrap_or(2),
        vpp: cfg.get_bool("video.vpp").unwrap_or(true),
        capture: cfg.get("video.capture").unwrap_or("auto").to_string(),
        drm_node: cfg
            .get("video.drm_node")
            .filter(|v| !v.is_empty())
            .map(str::to_string),
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
            "--video-udp" => a.video_udp = true,
            "--rc-mode" => a.rc_mode = it.next().ok_or("--rc-mode")?.to_ascii_lowercase(),
            "--qp" => a.qp = it.next().ok_or("--qp")?.parse().map_err(|_| "--qp")?,
            "--async-depth" => {
                a.async_depth = it.next().ok_or("--async-depth")?.parse().map_err(|_| "--async-depth")?
            }
            "--audio" => a.audio = true,
            "--no-audio" => a.audio = false,
            "--audio-listen" => a.audio_listen = it.next().ok_or("--audio-listen")?,
            "--vpp" => a.vpp = true,
            "--no-vpp" => a.vpp = false,
            "--capture" => a.capture = it.next().ok_or("--capture")?,
            "--drm-node" => a.drm_node = Some(it.next().ok_or("--drm-node")?.to_string()),
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
                     \x20          [--audio-listen ADDR] [--vpp|--no-vpp] [--drm-node PATH]\n\
                     \x20          [--capture auto|wlr|portal] [--video-udp]\n\
                     \x20          [--rc-mode cbr|vbr|cqp] [--qp N]\n\
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
    if !matches!(a.rc_mode.as_str(), "cbr" | "vbr" | "cqp" | "icq" | "auto") {
        return Err(format!(
            "unknown --rc-mode {} (cbr, vbr, cqp or icq)",
            a.rc_mode
        ));
    }
    if a.qp < 1 || a.qp > 51 {
        return Err("--qp must be between 1 and 51".into());
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

/// Why `Stream::run` returned instead of streaming forever.
enum RunOutcome {
    /// The encoder produced no packets for 5 s while a client was connected;
    /// fall back to the GL conversion path.
    NoPackets,
    /// Capture failed repeatedly (the compositor or Wayland connection is
    /// gone); exit and let systemd restart the server.
    CaptureDead,
    /// The output's mode changed; rebuild the encoder at the new size and
    /// tell the clients to offer a resize.
    Resize,
    /// Constant-quality mode exceeded the configured bitrate; rebuild the
    /// encoder with a higher quantiser.
    Rate { qp: u32 },
}

/// The HEVC pipeline: capture -> GL NV12 -> VAAPI -> dma-buf packets.
struct Stream {
    cap: capture::Capture,
    conv: Option<glconv::GlConv>,
    planes: Option<glconv::Planes>,
    enc: encoder::Encoder,
    w: u32,
    h: u32,
    fps: u32,
    bitrate: u32,
    /// Quantiser the encoder was opened with (constant-quality mode).
    qp: u32,
    cqp: bool,
    cpu: bool,
    vpp: bool,
    /// Skip encoding a frame whose content matches the last one sent.
    skip_idle: bool,
    _target: Option<FdGuard>,
}

impl Stream {
    fn new(args: &Args, force_gl: bool) -> Result<Self, String> {
        let mut cap = capture::Capture::connect(args.output.as_deref(), true)
            .map_err(|e| format!("capture: {e}"))?;
        // The encoder and the conversion path follow the capture GPU's real
        // capabilities instead of a vendor guess: NVENC on NVIDIA, VAAPI
        // elsewhere, and VPP only where the driver advertises video
        // processing and the capture modifiers are known to import.
        let node_path = lgx_server::gbm::drm_node();
        let nodes = lgx_caps::drm_nodes();
        let node = nodes.iter().find(|n| n.path.to_string_lossy() == node_path);
        let va = node.and_then(lgx_caps::probe_vaapi);
        let cuda = lgx_caps::cuda::available();
        let backend = lgx_caps::encoder_for_node(node, va.as_ref(), cuda);
        let intel = node.is_some_and(|n| n.is_intel());
        let amd = node.is_some_and(|n| n.is_amd());
        if let Some(n) = node {
            eprintln!(
                "lgx-server: capture GPU {} ({}, encoder {})",
                n.name,
                n.vendor_name(),
                backend.map(|b| b.name()).unwrap_or("none"),
            );
        }
        // NVENC takes system-memory NV12, so the CPU staging path is not an
        // experiment there; on VAAPI the staging path stays opt-in.
        let nvenc = backend == Some(lgx_caps::EncoderBackend::Nvenc);
        let cpu = std::env::var_os("LGX_CPU_CONVERT").is_some() || nvenc;
        // VPP keeps the RGB->NV12 conversion on the media engine. Capture
        // buffers never use GBM's implicit modifier: mesa 26.3's i915 backend
        // returns 0xb for them, which neither iHD nor EGL can import. Ask for
        // Y-tiled instead (the compositor's fast screencopy blit), filtered
        // by the modifiers the compositor advertises, with linear as the
        // fallback. Other drivers keep the GL path (LGX_NO_VPP forces it for
        // Intel too).
        // `low_power` is the VAAPI VDENC entry point, which only Intel has.
        // Mesa's radeonsi advertises the regular encode entry point, so the
        // config default (true) must not reach an AMD driver.
        let low_power = args.low_power && va.as_ref().is_some_and(|c| c.hevc_encode_low_power);
        let vpp = args.vpp
            && !cpu
            && intel
            && va.as_ref().is_some_and(|c| c.vpp)
            && !force_gl
            && std::env::var_os("LGX_NO_VPP").is_none();
        cap.state.use_gbm = true;
        if intel {
            cap.state.gbm_mods = vec![lgx_server::gbm::I915_FORMAT_MOD_Y_TILED, 0];
        } else if amd && !cpu {
            // AMD modifiers are not part of the probe yet. A linear capture
            // buffer is the one every Mesa import path accepts; the Y-tiled
            // Intel modifier must never be requested here.
            cap.state.gbm_linear = true;
        }
        if cpu {
            // The compositor still blits once, but the RGB->NV12 conversion
            // leaves the 3D engine and runs on the CPU.
            cap.state.gbm_linear = true;
            cap.state.stage_cpu = true;
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

        // `video.rc_mode`/`--rc-mode` to the helper's VA_RC_* value.
        let rc_mode_num = match args.rc_mode.as_str() {
            "cbr" => 2,
            "cqp" => 1,
            "icq" => 4,
            "auto" => 0,
            _ => 3,
        };
        let helper =
            std::env::var("LGX_VAENC").unwrap_or_else(|_| "/usr/lib/lgx/lgx-vaenc".to_string());
        let mut target = None;
        let mut conv = None;
        let mut planes = None;
        let mut enc = if nvenc {
            let helper = std::env::var("LGX_NVENC")
                .unwrap_or_else(|_| "/usr/lib/lgx/lgx-nvenc".to_string());
            encoder::Encoder::Nvenc(
                nvenc::NvEnc::start(
                    "/tmp/lgx-nvenc.sock",
                    &helper,
                    w,
                    h,
                    fps,
                    args.bitrate,
                    args.async_depth,
                    &args.rc_mode,
                    args.qp,
                )
                .map_err(|e| format!("nvenc: {e}"))?,
            )
        } else if vpp {
            encoder::Encoder::Vaapi(
                vaenc::VaEnc::start_vpp(
                    "/tmp/lgx-vaenc.sock",
                    &helper,
                    w,
                    h,
                    fps,
                    args.bitrate,
                    low_power,
                    args.async_depth,
                    rc_mode_num,
                    args.qp,
                )
                .map_err(|e| format!("vaenc: {e}"))?,
            )
        } else if cpu {
            encoder::Encoder::Vaapi(
                vaenc::VaEnc::start_cpu(
                    "/tmp/lgx-vaenc.sock",
                    &helper,
                    w,
                    h,
                    fps,
                    args.bitrate,
                    low_power,
                    args.async_depth,
                    rc_mode_num,
                    args.qp,
                )
                .map_err(|e| format!("vaenc: {e}"))?,
            )
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
                rc_mode_num,
                args.qp,
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
            encoder::Encoder::Vaapi(e)
        };
        let _ = &mut enc;
        if vpp {
            eprintln!("lgx-server: VPP RGB->NV12 conversion (tiled capture)");
        } else if cpu {
            eprintln!("lgx-server: CPU NV12 conversion (linear capture)");
        }

        eprintln!(
            "lgx-server: HEVC ready ({w}x{h}@{fps}, {} Mbit/s, {} {}, {})",
            args.bitrate / 1_000_000,
            enc.name(),
            if cpu { "CPU NV12" } else if vpp { "VPP" } else { "GL" },
            if nvenc { "" } else if low_power { "low_power=1" } else { "low_power=0" },
        );
        Ok(Self {
            cap,
            conv,
            planes,
            enc,
            w,
            h,
            fps,
            bitrate: args.bitrate,
            qp: args.qp,
            cqp: args.rc_mode == "cqp",
            cpu,
            vpp,
            skip_idle: args.skip_idle,
            _target: target,
        })
    }

    fn run(mut self, hub: &Arc<Hub>) -> RunOutcome {
        let mut t0 = Instant::now();
        let mut frames = 0u64;
        let mut bytes = 0u64;
        let mut wait_samples: Vec<f32> = Vec::new();
        let mut render_samples: Vec<f32> = Vec::new();
        let mut encode_samples: Vec<f32> = Vec::new();
        let mut idle = true;
        let mut last_progress = Instant::now();
        let mut capture_errors = 0u32;
        let mut skipped = 0u64;
        // Constant quality has no bitrate ceiling of its own; count ticks over
        // and under the configured cap to adapt the quantiser.
        let mut over_ticks = 0u32;
        let mut under_ticks = 0u32;
        let mut damage_changed = 0u64;
        let mut damage_unknown = 0u64;
        // Digest of the picture the client currently has. Only a frame whose
        // content differs from this one has to be encoded.
        let mut sent_digest: Option<u64> = None;
        // A static desktop is the common case; skipping its frames is the
        // difference between the CBR rate and ~0 bytes/s. The digest costs a
        // full read of the capture buffer per frame, which is expensive on a
        // slow CPU, so `video.skip_idle = false` turns both the hash and the
        // skip off. The env var remains the escape hatch for bisecting.
        let skip_idle = self.skip_idle
            && std::env::var_os("LGX_NO_DAMAGE_SKIP").is_none();
        // Test hook: drop the VPP stream's packets so the watchdog exercises
        // the GL fallback. The GL stream (vpp == false) is unaffected.
        let drop_packets = std::env::var_os("LGX_TEST_NO_PACKETS").is_some() && self.vpp;

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
                skipped = 0;
                wait_samples.clear();
                render_samples.clear();
                encode_samples.clear();
                last_progress = Instant::now();
                capture_errors = 0;
            }
            let force_send = if hub.take_keyframe() {
                if std::env::var_os("LGX_DEBUG_IDR").is_some() {
                    eprintln!("lgx-server: forcing an IDR");
                }
                if let Err(e) = self.enc.force_keyframe() {
                    eprintln!("lgx-server: vaenc keyframe: {e}");
                }
                true
            } else {
                false
            };
            self.cap.state.overlay_cursor = hub.cursor_overlay();

            let t_wait = Instant::now();
            if let Err(e) = self.cap.next_frame() {
                capture_errors += 1;
                eprintln!("lgx-server: capture: {e}");
                if capture_errors >= 20 {
                    eprintln!("lgx-server: capture keeps failing; exiting for a clean restart");
                    return RunOutcome::CaptureDead;
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            capture_errors = 0;
            if let Some(info) = self.cap.state.info.as_ref() {
                if info.width != self.w || info.height != self.h {
                    let (w, h) = (info.width, info.height);
                    eprintln!(
                        "lgx-server: capture resized {}x{} -> {w}x{h}; rebuilding the stream",
                        self.w, self.h
                    );
                    return RunOutcome::Resize;
                }
            }
            // The compositor copied the same picture again: the client already
            // has it, so skip the VPP import and the encode. A forced keyframe
            // overrides this, otherwise a client that connects while the
            // desktop is idle would never receive its first IDR.
            match self.cap.state.frame_changed {
                Some(true) => damage_changed += 1,
                None => damage_unknown += 1,
                Some(false) => {}
            }
            if std::env::var_os("LGX_DEBUG_DAMAGE").is_some()
                && (damage_changed + damage_unknown + skipped).is_multiple_of(160)
            {
                eprintln!(
                    "lgx-server: damage changed={damage_changed} none={skipped} unknown={damage_unknown}"
                );
            }
            let digest = if skip_idle && self.cap.state.frame_changed != Some(false) {
                self.cap.frame_digest()
            } else {
                None
            };
            let unchanged = match digest {
                Some(digest) => sent_digest == Some(digest),
                None => self.cap.state.frame_changed == Some(false),
            };
            if skip_idle && !force_send && unchanged {
                skipped += 1;
                // The encoder is not stuck, the picture just did not change;
                // the no-packet watchdog must not fire.
                last_progress = Instant::now();
                capture::throttle(loop_start, self.fps);
                continue;
            }
            if digest.is_some() {
                sent_digest = digest;
            }
            let capture_us = time::monotonic_us();
            let wait_ms = t_wait.elapsed().as_secs_f64() * 1000.0;

            let t_work = Instant::now();
            let packets = if self.vpp {
                let (id, fd, stride, modifier, gw, gh) = match self.cap.last_gbm() {
                    Some(g) => (g.id, g.fd, g.stride, g.modifier, g.width, g.height),
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
                    .vpp_send(id, gw, gh, stride, fourcc, modifier, 0, fd)
                {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("lgx-server: vaenc vpp: {e}");
                        std::process::exit(1);
                    }
                };
                let encode_ms = t_enc.elapsed().as_secs_f64() * 1000.0;
                wait_samples.push(wait_ms as f32);
                render_samples.push(0.0);
                encode_samples.push(encode_ms as f32);
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
                wait_samples.push(wait_ms as f32);
                render_samples.push(render_ms as f32);
                encode_samples.push(encode_ms as f32);
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
                wait_samples.push(wait_ms as f32);
                render_samples.push(render_ms as f32);
                encode_samples.push(encode_ms as f32);
                frames += 1;
                packets
            };
            let mut produced = false;
            for data in packets {
                if !data.is_empty() && !drop_packets {
                    bytes += data.len() as u64;
                    produced = true;
                    hub.broadcast_video(data, capture_us);
                }
            }
            if produced {
                last_progress = Instant::now();
            }
            let period = if std::env::var_os("LGX_DEBUG_FPS").is_some() { 1 } else { 5 };
            if t0.elapsed() >= Duration::from_secs(period) {
                let (wait_p50, wait_p99) = percentiles(&mut wait_samples);
                let (render_p50, render_p99) = percentiles(&mut render_samples);
                let (encode_p50, encode_p99) = percentiles(&mut encode_samples);
                let clients = hub.client_count();
                let dropped = hub.take_dropped();
                let fps = frames as f64 / t0.elapsed().as_secs_f64();
                let dt = t0.elapsed().as_secs_f64();
                let mib_s = bytes as f64 / dt / 1_048_576.0;
                eprintln!(
                    "lgx-server: HEVC {fps:.1} fps, {mib_s:.1} MiB/s, wait p50/p99 {wait_p50:.2}/{wait_p99:.2} ms, render {render_p50:.2}/{render_p99:.2} ms, encode {encode_p50:.2}/{encode_p99:.2} ms, {clients} clients, {dropped} dropped, {skipped} skipped"
                );
                if self.cqp {
                    let rate = bytes as f64 * 8.0 / dt;
                    let cap = self.bitrate as f64;
                    if rate > cap {
                        over_ticks += 1;
                        under_ticks = 0;
                        // Jump far when the content is many times over the
                        // cap (full-screen noise costs 400 Mbit/s at qp 22).
                        if over_ticks >= 2 {
                            let step = if rate > cap * 4.0 { 8 } else { 4 };
                            let qp = (self.qp + step).min(51);
                            if qp != self.qp {
                                eprintln!(
                                    "lgx-server: CQP {:.1} Mbit/s is over the {} Mbit/s cap; raising qp {} -> {qp}",
                                    rate / 1e6,
                                    self.bitrate / 1_000_000,
                                    self.qp
                                );
                                return RunOutcome::Rate { qp };
                            }
                        }
                    } else {
                        over_ticks = 0;
                        // Recover slowly so a short calm period does not make
                        // the stream oscillate.
                        if rate < cap * 0.25 && self.qp > 18 {
                            under_ticks += 1;
                            if under_ticks >= 6 {
                                let qp = (self.qp - 2).max(18);
                                eprintln!(
                                    "lgx-server: CQP is at {:.1} Mbit/s, well under the cap; lowering qp {} -> {qp}",
                                    rate / 1e6,
                                    self.qp
                                );
                                return RunOutcome::Rate { qp };
                            }
                        } else {
                            under_ticks = 0;
                        }
                    }
                }
                let status = format!(
                    concat!(
                        "{{\"ts\":{},\"fps\":{:.1},\"mib_s\":{:.2},",
                        "\"wait_p50_ms\":{:.2},\"wait_p99_ms\":{:.2},",
                        "\"render_p50_ms\":{:.2},\"render_p99_ms\":{:.2},",
                        "\"encode_p50_ms\":{:.2},\"encode_p99_ms\":{:.2},",
                        "\"dropped\":{},\"skipped\":{},\"clients\":{},\"width\":{},\"height\":{},",
                        "\"target_fps\":{},\"bitrate\":{}}}\n"
                    ),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                    fps,
                    mib_s,
                    wait_p50,
                    wait_p99,
                    render_p50,
                    render_p99,
                    encode_p50,
                    encode_p99,
                    dropped,
                    skipped,
                    clients,
                    self.w,
                    self.h,
                    self.fps,
                    self.bitrate
                );
                write_status(&status);
                t0 = Instant::now();
                frames = 0;
                bytes = 0;
                skipped = 0;
                wait_samples.clear();
                render_samples.clear();
                encode_samples.clear();
            }
            if frames > 0 && last_progress.elapsed() >= Duration::from_secs(5) {
                eprintln!(
                    "lgx-server: no video packets for 5 s while a client is connected; the encoder or VPP import is stuck"
                );
                return RunOutcome::NoPackets;
            }
            capture::throttle(loop_start, self.fps);
        }
    }
}

fn percentiles(samples: &mut [f32]) -> (f32, f32) {
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

/// Latest stats for scripts and the macOS menu bar. Written atomically so a
/// reader never sees a half-written file.
fn write_status(text: &str) {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")));
    let Some(base) = base else {
        return;
    };
    let dir = base.join("lgx");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let tmp = dir.join("status.json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("status.json"));
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
    hub.set_video_udp(args.video_udp);
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

    // The render node must be pinned before any GBM/VAAPI call and before the
    // encoder helper is spawned (it inherits LGX_DRM_NODE). CLI/config win
    // over the environment; without either, auto-detection prefers Intel.
    let drm_node = args
        .drm_node
        .clone()
        .or_else(|| std::env::var("LGX_DRM_NODE").ok().filter(|v| !v.is_empty()))
        .or_else(lgx_server::gbm::detect_render_node)
        .unwrap_or_else(|| "/dev/dri/renderD128".to_string());
    let explicit = args.drm_node.is_some();
    lgx_server::gbm::set_drm_node(&drm_node);
    eprintln!(
        "lgx-server: DRM render node {drm_node} ({}, Intel: {})",
        if explicit { "configured" } else { "auto-detected" },
        lgx_server::gbm::node_vendor(&drm_node).eq_ignore_ascii_case("0x8086")
    );

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

    // KDE and GNOME do not implement wlr-screencopy; their supported capture
    // path is the desktop portal. wlroots compositors keep the direct
    // screencopy path, which is both faster and dialog-free.
    if !matches!(args.capture.as_str(), "auto" | "wlr" | "portal") {
        eprintln!("lgx-server: unknown --capture {} (auto, wlr or portal)", args.capture);
        std::process::exit(2);
    }
    let use_portal = match args.capture.as_str() {
        "portal" => true,
        "wlr" => false,
        _ => !capture::has_wlr_screencopy(),
    };
    if use_portal {
        eprintln!("lgx-server: capture backend: xdg-desktop-portal ScreenCast");
        let options = portal::Options {
            helper_va: std::env::var("LGX_VAENC")
                .unwrap_or_else(|_| "/usr/lib/lgx/lgx-vaenc".to_string()),
            helper_nv: std::env::var("LGX_NVENC")
                .unwrap_or_else(|_| "/usr/lib/lgx/lgx-nvenc".to_string()),
            bitrate: args.bitrate,
            fps: args.fps,
            low_power: args.low_power,
            async_depth: args.async_depth,
            dmabuf: false,
            rc_mode: match args.rc_mode.as_str() {
                "cbr" => 2,
                "cqp" => 1,
                "icq" => 4,
                "auto" => 0,
                _ => 3,
            },
            qp: args.qp,
        };
        if let Err(e) = portal::serve(hub, options) {
            if e.kind() == std::io::ErrorKind::Interrupted {
                eprintln!("lgx-server: portal capture: {e}");
                std::process::exit(0);
            }
            eprintln!("lgx-server: portal capture: {e}");
            std::process::exit(1);
        }
        eprintln!("lgx-server: portal capture ended");
        std::process::exit(1);
    }
    eprintln!("lgx-server: capture backend: wlr-screencopy");

    // A broken encoder must never leave a silent dead stream: fall back to the
    // GL conversion path and rebuild the pipeline, or exit for systemd.
    let mut force_gl = false;
    let mut last_geometry: Option<(u32, u32, u32)> = None;
    let mut args = args;
    loop {
        let stream = match Stream::new(&args, force_gl) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("lgx-server: cannot start HEVC streaming: {e}");
                std::process::exit(1);
            }
        };
        hub.set_geometry(stream.w, stream.h);
        let geometry = (stream.w, stream.h, stream.fps);
        if last_geometry.is_some_and(|old| old != geometry) {
            eprintln!(
                "lgx-server: stream geometry now {}x{}@{}; asking clients to resize",
                stream.w, stream.h, stream.fps
            );
            hub.broadcast_resize(stream.w, stream.h, stream.fps);
        }
        last_geometry = Some(geometry);
        match stream.run(&hub) {
            RunOutcome::Resize => {}
            RunOutcome::Rate { qp } => {
                args.qp = qp;
            }

            RunOutcome::NoPackets => {
                eprintln!("lgx-server: falling back to GL conversion and rebuilding the stream");
                force_gl = true;
            }
            RunOutcome::CaptureDead => {
                eprintln!("lgx-server: exiting for a clean restart");
                std::process::exit(1);
            }
        }
    }
}
