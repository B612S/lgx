//! Screen capture through `org.freedesktop.portal.ScreenCast`.
//!
//! wlroots compositors expose `zwlr_screencopy_manager_v1` and keep the
//! zero-copy wlr-screencopy path (see `capture.rs`). KDE Plasma (KWin) and
//! GNOME (Mutter) do not implement that protocol; their supported capture API
//! is the desktop portal, which hands out a PipeWire stream of the selected
//! monitor. X11 is out of scope.
//!
//! The portal stream is damage-driven: an idle desktop produces no frames.
//! That is why this pipeline does not use the 5 s "no packets" watchdog of the
//! wlr path.
//!
//! The first version asks for shared-memory (MemFd) buffers and converts
//! BGRx/BGRA to NV12 on the CPU. This works with every encoder backend
//! (VAAPI on Intel/AMD, NVENC on NVIDIA) and on every compositor. A dmabuf
//! path for VPP is a separate optimisation; advertising a DRM modifier here
//! without a converter would only break the NVIDIA path.

use std::io;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::spa;

use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType, Stream as PortalStream,
};
use ashpd::desktop::{PersistMode, Session};

use crate::encoder::Encoder;
use crate::nvenc::NvEnc;
use crate::vaenc::VaEnc;
use crate::Hub;

pub struct Options {
    pub helper_va: String,
    pub helper_nv: String,
    pub bitrate: u32,
    /// `video.fps` override; the stream's own framerate wins when it reports
    /// one, otherwise this is the fallback.
    pub fps: Option<u32>,
    pub low_power: bool,
    pub async_depth: u32,
    /// Request dma-buf frames and encode them through VPP (Intel).
    pub dmabuf: bool,
}

/// Handles that must stay alive for as long as the portal session is used.
/// Dropping the D-Bus connection would end the session and stop the stream.
struct PortalSession {
    _proxy: Screencast,
    _session: Session<Screencast>,
    stream: PortalStream,
    fd: OwnedFd,
    /// The compositor embeds the pointer in the frames.
    cursor_embedded: bool,
}

fn token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("lgx").join("screencast.token"))
}

fn read_token() -> Option<String> {
    let path = token_path()?;
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

fn write_token(token: &str) {
    let Some(path) = token_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&path, token).is_ok() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Opens a portal ScreenCast session. Blocks until the user confirms the
/// share dialog (or cancels, in which case this returns an error explaining
/// that no screen was shared). The restore token makes later starts silent.
fn open_portal() -> io::Result<PortalSession> {
    let restore = read_token();
    let result = futures_lite::future::block_on(async {
        let proxy = Screencast::new().await?;
        let session = proxy.create_session(Default::default()).await?;
        let mut sources = SelectSourcesOptions::default()
            .set_sources(SourceType::MONITOR)
            .set_multiple(false)
            .set_persist_mode(PersistMode::ExplicitlyRevoked);
        let mut cursor_embedded = true;
        if proxy.version() >= 2 {
            // GNOME does not support an embedded cursor (mode 2); KDE does.
            // Ask for what the backend advertises, preferring an embedded
            // pointer so the Linux client has a visible cursor without any
            // CURSOR_* messages.
            let modes = proxy.available_cursor_modes().await?;
            let cursor = if modes.contains(CursorMode::EMBEDDED) {
                CursorMode::EMBEDDED
            } else {
                cursor_embedded = false;
                eprintln!(
                    "lgx-server: portal: embedded cursor unavailable (modes {:#x}); the pointer will not be captured",
                    modes.bits()
                );
                if modes.contains(CursorMode::HIDDEN) {
                    CursorMode::HIDDEN
                } else {
                    CursorMode::METADATA
                }
            };
            sources = sources.set_cursor_mode(cursor);
        }
        if let Some(token) = restore.as_deref() {
            sources = sources.set_restore_token(Some(token));
        }
        proxy.select_sources(&session, sources).await?;
        let response = proxy.start(&session, None, Default::default()).await?.response()?;
        let stream = response
            .streams()
            .first()
            .cloned()
            .ok_or_else(|| ashpd::Error::NoResponse)?;
        if let Some(token) = response.restore_token() {
            write_token(token);
        }
        let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
        Ok::<_, ashpd::Error>(PortalSession {
            _proxy: proxy,
            _session: session,
            stream,
            fd,
            cursor_embedded,
        })
    });
    result.map_err(|e| match e {
        // A cancelled dialog is a user decision, not a failure: the caller
        // exits cleanly so systemd does not re-prompt in a restart loop.
        ashpd::Error::Response(err) => io::Error::new(
            io::ErrorKind::Interrupted,
            format!("the portal returned no screen ({err})"),
        ),
        other => io::Error::other(format!("ScreenCast portal: {other}")),
    })
}

/// The encoder is rebuilt in place when the compositor changes the captured
/// mode; the portal session and the PipeWire stream stay open.
fn make_encoder(options: &Options, w: u32, h: u32, fps: u32) -> io::Result<Encoder> {
    let node_path = lgx_server::gbm::drm_node();
    let nodes = lgx_caps::drm_nodes();
    let node = nodes.iter().find(|n| n.path.to_string_lossy() == node_path);
    let va = node.and_then(lgx_caps::probe_vaapi);
    let cuda = lgx_caps::cuda::available();
    let backend = lgx_caps::encoder_for_node(node, va.as_ref(), cuda)
        .ok_or_else(|| io::Error::other("no hardware HEVC encoder on this GPU"))?;
    match backend {
        lgx_caps::EncoderBackend::Nvenc => {
            eprintln!("lgx-server: portal capture: NVENC encoder");
            Ok(Encoder::Nvenc(NvEnc::start(
                "/tmp/lgx-nvenc.sock",
                &options.helper_nv,
                w,
                h,
                fps,
                options.bitrate,
                options.async_depth,
            )?))
        }
        lgx_caps::EncoderBackend::Vaapi => {
            let low_power =
                options.low_power && va.as_ref().is_some_and(|c| c.hevc_encode_low_power);
            eprintln!(
                "lgx-server: portal capture: VAAPI encoder ({})",
                if options.dmabuf { "VPP dma-buf" } else { "CPU NV12 upload" }
            );
            let start = if options.dmabuf { VaEnc::start_vpp } else { VaEnc::start_cpu };
            Ok(Encoder::Vaapi(start(
                "/tmp/lgx-vaenc.sock",
                &options.helper_va,
                w,
                h,
                fps,
                options.bitrate,
                low_power,
                options.async_depth,
            )?))
        }
    }
}

/// Encoder and last frame, shared by the frame callback and the idle-keyframe
/// timer. Both run on the PipeWire loop thread, so the mutex never contends.
struct DmabufFrame {
    fd: std::os::fd::OwnedFd,
    offset: u32,
    stride: u32,
    fourcc: u32,
    modifier: u64,
    width: u32,
    height: u32,
}

impl DmabufFrame {
    fn dup_from(
        fd: std::os::fd::RawFd,
        offset: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        use std::os::fd::FromRawFd;
        let dup = unsafe { libc::dup(fd) };
        if dup < 0 {
            return None;
        }
        Some(Self {
            fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(dup) },
            offset,
            stride,
            fourcc,
            modifier,
            width,
            height,
        })
    }
}

struct Inner {
    enc: Option<Encoder>,
    nv12: Vec<u8>,
    /// Last compositor dma-buf, kept so the idle timer can re-encode it as an
    /// IDR for a client that connects while the desktop is static.
    last_dmabuf: Option<DmabufFrame>,
    width: u32,
    height: u32,
    fps: u32,
}

struct State {
    hub: Arc<Hub>,
    options: Arc<Options>,
    inner: Arc<std::sync::Mutex<Inner>>,
    format: spa::param::video::VideoInfoRaw,
    quit: Option<pw::main_loop::MainLoopRc>,
    failure: Arc<std::sync::Mutex<Option<String>>>,
    frames: u64,
    bytes: u64,
    conv_us: u64,
    logged_format: bool,
    logged_frame: bool,
    dmabuf: bool,
    /// Test hook: count delivered frames without an encoder (GNOME/DE capture
    /// checks on a VM without a hardware encoder).
    count_only: bool,
    empty_streak: u32,
    dmabuf_error: bool,
    t0: Instant,
}

fn ensure_encoder(
    inner: &mut Inner,
    options: &Options,
    hub: &Hub,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<(), String> {
    if inner.enc.is_some() && inner.width == width && inner.height == height && inner.fps == fps {
        return Ok(());
    }
    let enc = make_encoder(options, width, height, fps).map_err(|e| e.to_string())?;
    let resized = inner.width != 0 && (inner.width != width || inner.height != height);
    inner.enc = Some(enc);
    inner.width = width;
    inner.height = height;
    inner.fps = fps;
    hub.set_geometry(width, height);
    if resized {
        eprintln!("lgx-server: portal capture resized to {width}x{height}@{fps}");
        hub.broadcast_resize(width, height, fps);
    }
    eprintln!(
        "lgx-server: HEVC ready ({width}x{height}@{fps}, {} Mbit/s, portal capture)",
        options.bitrate / 1_000_000
    );
    Ok(())
}

impl State {

    fn frame(
        &mut self,
        format: &spa::param::video::VideoInfoRaw,
        bytes: Option<(&[u8], usize)>,
    ) {
        let width = format.size().width;
        let height = format.size().height;
        if width < 16 || height < 16 {
            if !self.logged_frame {
                self.logged_frame = true;
                eprintln!("lgx-server: portal: frame with missing size {width}x{height}");
            }
            return;
        }
        if self.count_only {
            self.frames += 1;
            if self.frames == 1 || self.frames.is_multiple_of(60) {
                eprintln!(
                    "lgx-server: portal test frame {} ({}x{}, no encoder)",
                    self.frames, width, height
                );
            }
            self.bytes += (width as u64) * (height as u64) * 4;
            return;
        }
        let fps = self.fps_for(format);
        let mut inner = self.inner.lock().unwrap();
        if let Err(e) = ensure_encoder(&mut inner, &self.options, &self.hub, width, height, fps) {
            drop(inner);
            *self.failure.lock().unwrap() = Some(e);
            if let Some(m) = &self.quit {
                m.quit();
            }
            return;
        }
        let Some((src, stride)) = bytes else {
            return;
        };
        if stride < width as usize * 4 || src.len() < stride * height as usize {
            return;
        }
        let Inner { enc, nv12, .. } = &mut *inner;
        let enc = enc.as_mut().expect("encoder was just created");
        if self.hub.take_keyframe() {
            if let Err(e) = enc.force_keyframe() {
                eprintln!("lgx-server: portal keyframe: {e}");
            }
        }
        let convert = std::time::Instant::now();
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(8);
        crate::cpuconv::xrgb_to_nv12_parallel(
            src,
            width as usize,
            height as usize,
            stride,
            nv12,
            threads,
        );
        self.conv_us += convert.elapsed().as_micros() as u64;
        let sent = enc.send_cpu(nv12);
        drop(inner);
        let ts = crate::time::monotonic_us();
        match sent {
            Ok(packets) => self.publish(packets, ts),
            Err(e) => eprintln!("lgx-server: portal: encoder unavailable: {e}"),
        }
        if self.t0.elapsed() >= Duration::from_secs(5) {
            let fps = self.frames as f64 / self.t0.elapsed().as_secs_f64();
            let mib_s = self.bytes as f64 / self.t0.elapsed().as_secs_f64() / 1_048_576.0;
            let conv_ms = if self.frames > 0 {
                self.conv_us as f64 / self.frames as f64 / 1000.0
            } else {
                0.0
            };
            eprintln!(
                "lgx-server: portal HEVC {fps:.1} fps, {mib_s:.1} MiB/s, convert {conv_ms:.2} ms, {} clients",
                self.hub.client_count()
            );
            self.conv_us = 0;
            self.frames = 0;
            self.bytes = 0;
            self.t0 = Instant::now();
        }
    }

    fn fps_for(&self, format: &spa::param::video::VideoInfoRaw) -> u32 {
        let fr = format.framerate();
        if fr.denom > 0 && fr.num > 0 && (fr.num / fr.denom) >= 1 {
            return fr.num / fr.denom;
        }
        // KWin reports a variable framerate (0/1) plus the panel refresh in
        // maxFramerate. Falling back to 60 would halve the motion on a
        // 120/160 Hz panel.
        let max = format.max_framerate();
        if max.denom > 0 && max.num > 0 && (max.num / max.denom) >= 1 {
            max.num / max.denom
        } else {
            self.options.fps.unwrap_or(60).max(1)
        }
    }

    fn publish(&mut self, packets: Vec<Vec<u8>>, ts: u64) {
        for data in packets {
            if data.is_empty() {
                continue;
            }
            self.bytes += data.len() as u64;
            self.frames += 1;
            self.hub.broadcast_video(data, ts);
        }
    }

    /// A dma-buf frame goes straight into VPP, with no CPU conversion.
    fn frame_dmabuf(
        &mut self,
        format: &spa::param::video::VideoInfoRaw,
        fd: std::os::fd::RawFd,
        offset: u32,
        stride: i32,
    ) {
        let width = format.size().width;
        let height = format.size().height;
        if width < 16 || height < 16 {
            return;
        }
        if self.count_only {
            self.frames += 1;
            if self.frames == 1 || self.frames.is_multiple_of(60) {
                eprintln!(
                    "lgx-server: portal test dma-buf frame {} ({}x{}, modifier {:#x})",
                    self.frames, width, height, format.modifier()
                );
            }
            return;
        }
        let fourcc = match format.format() {
            spa::param::video::VideoFormat::BGRA => 0x3432_5241, // DRM_FORMAT_ARGB8888
            spa::param::video::VideoFormat::BGRx => 0x3432_5258, // DRM_FORMAT_XRGB8888
            other => {
                if !self.dmabuf_error {
                    self.dmabuf_error = true;
                    eprintln!("lgx-server: portal: unsupported dma-buf format {other:?}");
                }
                return;
            }
        };
        let modifier = format.modifier();
        let stride = if stride > 0 { stride as u32 } else { width * 4 };
        if self.count_only {
            self.frames += 1;
            if self.frames == 1 || self.frames.is_multiple_of(60) {
                eprintln!(
                    "lgx-server: portal test frame {} ({}x{}, no encoder)",
                    self.frames, width, height
                );
            }
            self.bytes += (width as u64) * (height as u64) * 4;
            return;
        }
        let fps = self.fps_for(format);
        let mut inner = self.inner.lock().unwrap();
        if let Err(e) = ensure_encoder(&mut inner, &self.options, &self.hub, width, height, fps) {
            drop(inner);
            *self.failure.lock().unwrap() = Some(e);
            if let Some(m) = &self.quit {
                m.quit();
            }
            return;
        }
        let Inner {
            enc,
            last_dmabuf,
            ..
        } = &mut *inner;
        let Some(Encoder::Vaapi(enc)) = enc.as_mut() else {
            return;
        };
        if self.hub.take_keyframe() {
            if let Err(e) = enc.force_keyframe() {
                eprintln!("lgx-server: portal keyframe: {e}");
            }
        }
        let needs_cache = match last_dmabuf.as_ref() {
            Some(d) => {
                d.width != width
                    || d.height != height
                    || d.stride != stride
                    || d.fourcc != fourcc
                    || d.modifier != modifier
                    || d.offset != offset
            }
            None => true,
        };
        if needs_cache {
            *last_dmabuf = DmabufFrame::dup_from(fd, offset, stride, fourcc, modifier, width, height);
        }
        let sent = enc.vpp_send(
            fd as u32,
            width,
            height,
            stride,
            fourcc,
            modifier,
            offset,
            fd,
        );
        drop(inner);
        let ts = crate::time::monotonic_us();
        match sent {
            Ok(packets) if !packets.is_empty() => {
                self.empty_streak = 0;
                self.publish(packets, ts);
            }
            Ok(_) => {
                // The first frame after the helper starts can return no
                // packets because async_depth buffers it; only a long streak
                // means VPP is stuck.
                self.empty_streak += 1;
                if self.empty_streak == 50 && !self.dmabuf_error {
                    self.dmabuf_error = true;
                    eprintln!(
                        "lgx-server: portal: no VPP packets after 50 frames (modifier {modifier:#x}, stride {stride})"
                    );
                }
            }
            Err(e) => {
                if !self.dmabuf_error {
                    self.dmabuf_error = true;
                    eprintln!("lgx-server: portal: dma-buf encode failed: {e}");
                }
            }
        }
    }
}

/// Zero-copy portal capture is only enabled where the RGB dma-buf can go
/// straight into VPP: Intel VAAPI. AMD's VPP RGB import is unverified and
/// NVENC cannot import a dma-buf, so both keep the shared-memory path.
fn vpp_dmabuf_available() -> bool {
    // Opt-in while the VPP dma-buf path is being validated: a static desktop
    // produces too few frames to confirm it, and the idle-keyframe replay has
    // no NV12 cache in that mode. The verified default stays shared memory.
    if !std::env::var("LGX_PORTAL_DMABUF").is_ok_and(|v| v == "1" || v == "true") {
        return false;
    }
    let path = lgx_server::gbm::drm_node();
    let nodes = lgx_caps::drm_nodes();
    let Some(node) = nodes.iter().find(|n| n.path.to_string_lossy() == path) else {
        return false;
    };
    let Some(va) = lgx_caps::probe_vaapi(node) else {
        return false;
    };
    node.is_intel() && va.hevc_encode && va.vpp
}

fn modifier_property(modifier: i64) -> spa::pod::Property {
    spa::pod::Property {
        key: spa::param::format::FormatProperties::VideoModifier.as_raw(),
        flags: spa::pod::PropertyFlags::MANDATORY | spa::pod::PropertyFlags::DONT_FIXATE,
        value: spa::pod::Value::Choice(spa::pod::ChoiceValue::Long(spa::utils::Choice(
            spa::utils::ChoiceFlags::empty(),
            spa::utils::ChoiceEnum::Enum {
                default: modifier,
                alternatives: vec![modifier, 0],
            },
        ))),
    }
}

fn serialize(obj: spa::pod::Object) -> Result<Vec<u8>, String> {
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map(|v| v.0.into_inner())
    .map_err(|e| e.to_string())
}

/// Runs the portal pipeline. Returns when the stream ends (mode change,
/// compositor closed it, or the user stopped sharing).
pub fn serve(hub: Arc<Hub>, mut options: Options) -> io::Result<()> {
    // Test hook: consume an existing PipeWire node instead of asking the
    // portal. The examples/pw_screencast.rs producer exists for this; it lets
    // the format negotiation, conversion and packet pacing be tested without
    // a compositor dialog.
    let test_node = std::env::var("LGX_TEST_PW_NODE")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let mut _session: Option<PortalSession> = None;
    let (node, fd) = match test_node {
        Some(v) => {
            let node: u32 = v
                .trim()
                .parse()
                .map_err(|_| io::Error::other(format!("LGX_TEST_PW_NODE={v}: not a node id")))?;
            eprintln!("lgx-server: portal capture using test PipeWire node {node}");
            (node, None)
        }
        None => {
            let session = open_portal()?;
            hub.set_cursor_local(!session.cursor_embedded);
            eprintln!(
                "lgx-server: portal capture ready (node {}, {}x{} at the compositor)",
                session.stream.pipe_wire_node_id(),
                session.stream.size().map(|s| s.0).unwrap_or(0),
                session.stream.size().map(|s| s.1).unwrap_or(0),
            );
            let node = session.stream.pipe_wire_node_id();
            let fd = session.fd.try_clone()?;
            _session = Some(session);
            (node, Some(fd))
        }
    };

    let dmabuf = vpp_dmabuf_available();
    options.dmabuf = dmabuf;
    if dmabuf {
        eprintln!("lgx-server: portal capture will request dma-buf frames for VPP");
    }
    pw::init();
    let main = pw::main_loop::MainLoopRc::new(None).map_err(io::Error::other)?;
    let context = pw::context::ContextRc::new(&main, None).map_err(io::Error::other)?;
    let core = match fd {
        Some(fd) => context.connect_fd_rc(fd, None),
        None => context.connect_rc(None),
    }
    .map_err(io::Error::other)?;

    let stream = pw::stream::StreamRc::new(
        core,
        "lgx-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(io::Error::other)?;

    let quit = main.clone();
    let failure = Arc::new(std::sync::Mutex::new(None));
    let inner = Arc::new(std::sync::Mutex::new(Inner {
        enc: None,
        nv12: Vec::new(),
        last_dmabuf: None,
        width: 0,
        height: 0,
        fps: 0,
    }));
    // A portal screencast is damage-driven: an idle desktop sends no frames,
    // so a client that connects while nothing moves would never get its IDR.
    // Re-encode the last NV12 frame as an IDR when the hub asks for one.
    let timer_inner = inner.clone();
    let timer_hub = hub.clone();
    let timer = main.loop_().add_timer(move |_| {
        if !timer_hub.take_keyframe() {
            return;
        }
        let mut inner = timer_inner.lock().unwrap();
        let Inner {
            enc,
            nv12,
            last_dmabuf,
            ..
        } = &mut *inner;
        let Some(enc) = enc.as_mut() else {
            return;
        };
        if let Err(e) = enc.force_keyframe() {
            eprintln!("lgx-server: portal keyframe: {e}");
            return;
        }
        let sent = if !nv12.is_empty() {
            enc.send_cpu(nv12)
        } else if let Some(d) = last_dmabuf.as_ref() {
            match enc {
                Encoder::Vaapi(v) => v.vpp_send(
                    std::os::fd::AsRawFd::as_raw_fd(&d.fd) as u32,
                    d.width,
                    d.height,
                    d.stride,
                    d.fourcc,
                    d.modifier,
                    d.offset,
                    std::os::fd::AsRawFd::as_raw_fd(&d.fd),
                ),
                Encoder::Nvenc(_) => return,
            }
        } else {
            return;
        };
        match sent {
            Ok(packets) => {
                eprintln!("lgx-server: portal idle keyframe (no damage on screen)");
                let ts = crate::time::monotonic_us();
                for data in packets {
                    if !data.is_empty() {
                        timer_hub.broadcast_video(data, ts);
                    }
                }
            }
            Err(e) => eprintln!("lgx-server: portal idle frame: {e}"),
        }
    });
    timer
        .update_timer(
            Some(Duration::from_millis(100)),
            Some(Duration::from_millis(100)),
        )
        .into_result()
        .map_err(|e| io::Error::other(format!("portal timer: {e:?}")))?;
    let state = State {
        hub,
        options: Arc::new(options),
        inner,
        format: Default::default(),
        quit: Some(quit),
        failure: failure.clone(),
        frames: 0,
        bytes: 0,
        conv_us: 0,
        logged_format: false,
        logged_frame: false,
        dmabuf,
        count_only: std::env::var_os("LGX_TEST_NO_ENCODER").is_some(),
        empty_streak: 0,
        dmabuf_error: false,
        t0: Instant::now(),
    };

    let listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, state, old, new| {
            eprintln!("lgx-server: portal stream {old:?} -> {new:?}");
            if matches!(new, pw::stream::StreamState::Error(_)) {
                *state.failure.lock().unwrap() = Some(format!("PipeWire stream error: {new:?}"));
                if let Some(m) = &state.quit {
                    m.quit();
                }
            }
        })
        .param_changed(|_, state, id, param| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else { return };
            let Ok((media, subtype)) = spa::param::format_utils::parse_format(param) else {
                return;
            };
            if media != spa::param::format::MediaType::Video
                || subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            // Keep the fixated format in the user data so `process` can read
            // the size and framerate of the buffers it receives.
            if let Err(e) = state.format.parse(param) {
                eprintln!("lgx-server: portal: cannot parse the video format: {e}");
                return;
            }
            if !state.logged_format {
                state.logged_format = true;
                let f = state.format;
                eprintln!(
                    "lgx-server: portal format {}x{} {} fps fourcc {:#x} modifier {:#x}",
                    f.size().width,
                    f.size().height,
                    f.framerate().num,
                    f.format().as_raw(),
                    f.modifier()
                );
            }
        })
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            if state.dmabuf && data.type_() == spa::buffer::DataType::DmaBuf {
                let fd = data.fd();
                let offset = data.chunk().offset();
                let stride = data.chunk().stride();
                let format = state.format;
                state.frame_dmabuf(&format, fd, offset, stride);
                return;
            }
            let offset = data.chunk().offset() as usize;
            let chunk_stride = data.chunk().stride();
            let Some(bytes) = data.data() else {
                if !state.logged_frame {
                    state.logged_frame = true;
                    eprintln!("lgx-server: portal: buffer is not mapped");
                }
                return;
            };
            let bytes: &[u8] = bytes;
            if offset >= bytes.len() {
                return;
            }
            let format = state.format;
            let width = format.size().width as usize;
            let height = format.size().height as usize;
            let stride = if chunk_stride > 0 {
                chunk_stride as usize
            } else {
                width * 4
            };
            // A MemFd buffer may be larger than one frame; only the bytes the
            // converter needs are passed on.
            let needed = stride.saturating_mul(height);
            let end = offset.saturating_add(needed).min(bytes.len());
            if end <= offset || width < 1 || height < 1 {
                return;
            }
            state.frame(&format, Some((&bytes[offset..end], stride)));
        })
        .register()
        .map_err(io::Error::other)?;

    let obj = pw::spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::RGBA,
        ),
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 640, height: 480 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 16384, height: 16384 }
        ),
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 1000, denom: 1 }
        ),
    );
    // Offer a dma-buf format (modifier choice) first when VPP can import it,
    // and always keep the shared-memory format as a fallback. The producer
    // fixes one of the two; `process` handles both.
    let mut values: Vec<Vec<u8>> = Vec::new();
    if dmabuf {
        let mut with_modifier = obj.clone();
        with_modifier
            .properties
            .push(modifier_property(0x0100_0000_0000_0002)); // I915_FORMAT_MOD_Y_TILED
        values.push(serialize(with_modifier).map_err(io::Error::other)?);
    }
    values.push(serialize(obj).map_err(io::Error::other)?);
    // A consumer that only offers MemFd/MemPtr buffers keeps the intersection
    // with compositor nodes that have a dmabuf format as well. Without this
    // param some producers (KWin's own screencast node) report "no more input
    // formats" instead of falling back to shared memory.
    let buffers = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![spa::pod::Property {
            // SPA_PARAM_BUFFERS_dataType: MemPtr | MemFd (| DmaBuf when VPP
            // can import the buffer).
            key: 6,
            flags: spa::pod::PropertyFlags::empty(),
            value: spa::pod::Value::Int(if dmabuf {
                (1 << 1) | (1 << 2) | (1 << 3)
            } else {
                (1 << 1) | (1 << 2)
            }),
        }],
    };
    values.push(serialize(buffers).map_err(io::Error::other)?);
    let mut params: Vec<&spa::pod::Pod> = Vec::new();
    for b in &values {
        params.push(spa::pod::Pod::from_bytes(b).ok_or_else(|| io::Error::other("pod is not aligned"))?);
    }
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )
    .map_err(io::Error::other)?;

    main.run();
    drop(listener);
    if let Some(err) = failure.lock().unwrap().take() {
        return Err(io::Error::other(err));
    }
    Ok(())
}
