//! lgx-server-macos: HEVC streaming of a macOS desktop over the lgx protocol.
//!
//! Differences from the Linux server, all forced by the platform:
//!
//! * Capture + encode live in `lgx-vtenc` (ScreenCaptureKit -> VideoToolbox),
//!   spawned as a child. It emits `u32 length + Annex-B access unit` on
//!   stdout, which is exactly what `lgx-vaenc` feeds the Linux server.
//! * ScreenCaptureKit is change-driven: a frame is produced only when the
//!   desktop changes, so the stream is event-driven rather than a fixed frame
//!   rate. The client presents every decoded frame (`present_hz = 0`), which
//!   keeps the last image on screen between changes.
//! * Input goes through `lgx-cginput` (CGEvent) instead of kernel `uinput`,
//!   because macOS has no uinput and input injection needs the Accessibility
//!   TCC grant.
//! * Audio comes from `lgx-macos-audio` (Core Audio process tap) instead of
//!   PipeWire. The tap is read only while an audio client is connected and
//!   mutes local playback for that time, so the guest hears the Mac and the
//!   Mac itself stays quiet. The first tap also needs the "System Audio
//!   Recording" TCC grant.
//! * The clipboard (wl-clipboard) is not implemented yet.
//! * The process must run inside the logged-in GUI session (LaunchAgent):
//!   Screen Recording and System Audio Recording are denied to anything
//!   spawned from `sshd`.
//!
//! Settings live in `~/.config/lgx/macos.conf` (see `config.rs`); the command
//! line overrides the file, the file overrides the built-in defaults.
//!
//! Usage:
//!   lgx-server-macos [--listen ADDR] [--fps N] [--bitrate BPS] [--display N]
//!                    [--vt-quality Q] [--vt-max-delay N]
//!                    [--vt-keyframe-interval S]
//!                    [--no-vt-speed-over-quality]
//!                    [--wheel-invert|--no-wheel-invert] [--no-adaptive]
//!                    [--no-audio] [--audio-listen ADDR]
//!                    [--helper PATH] [--input-helper PATH] [--audio-helper PATH]
//!   lgx-server-macos --rotate        # new token, rewritten into the config
//!   lgx-server-macos --pair --pair-name NAME [--pair-port 5904]

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::thread::{self, JoinHandle};

use lgx_proto::auth;
use lgx_proto::msg;
use lgx_proto::secure::{server_handshake, server_read_id, Secure};
use lgx_proto::{codec, file, Msg};

use config::Config;

/// Frames buffered between the capture helper and the socket writer. Two or
/// three frames absorb a short scheduling hiccup without letting a slow link
/// build up a multi-second backlog.
const QUEUE_FRAMES: usize = 3;

mod audio;
mod config;

#[cfg(target_os = "macos")]
#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

#[cfg(target_os = "macos")]
extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

/// Monotonic microseconds, the same mach absolute clock `lgx-vtenc` stamps
/// every access unit with, so a `PONG` lets the client measure the true
/// capture-to-present latency. The fallback only exists so the crate links
/// when the workspace is built on Linux; it is never used there.
fn monotonic_us() -> u64 {
    #[cfg(target_os = "macos")]
    {
        use std::sync::OnceLock;
        static FACTOR: OnceLock<(u64, u64)> = OnceLock::new();
        let (numer, denom) = *FACTOR.get_or_init(|| {
            let mut info = MachTimebaseInfo { numer: 1, denom: 1 };
            unsafe { mach_timebase_info(&mut info) };
            (info.numer as u64, (info.denom as u64).max(1))
        });
        let ticks = unsafe { mach_absolute_time() } as u128;
        return (ticks * numer as u128 / denom as u128 / 1000) as u64;
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::sync::OnceLock;
        static BASE: OnceLock<std::time::Instant> = OnceLock::new();
        BASE.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64
    }
}

/// Local wall-clock time for log lines; launchd's log has no timestamps.
fn now_hms() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return "--:--:--".into();
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

macro_rules! info {
    ($($arg:tt)*) => {
        println!("{} {}", now_hms(), format!($($arg)*))
    };
}

macro_rules! warn {
    ($($arg:tt)*) => {
        eprintln!("{} {}", now_hms(), format!($($arg)*))
    };
}

/// Helpers live next to the server executable inside `LGX Server.app`, so the
/// app is self-contained: drag it into /Applications and it works from there.
fn sibling(name: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let path = dir.join(name);
    path.exists().then(|| path.to_string_lossy().into_owned())
}

struct Args {
    listen: String,
    token: String,
    /// `None` lets `lgx-vtenc` follow the display's refresh rate; the helper
    /// reports the rate it picked on its first stderr line.
    fps: Option<u32>,
    bitrate: u32,
    display: Option<usize>,
    helper: String,
    input_helper: String,
    input_socket: Option<String>,
    audio: bool,
    audio_listen: Option<String>,
    audio_helper: String,
    pb_helper: String,
    /// Flip the wheel direction passed to `lgx-cginput`.
    wheel_invert: bool,
    /// Frames VideoToolbox may hold (`LGX_VT_MAX_DELAY`); 1 drops frames the
    /// encoder cannot finish in time.
    vt_max_delay: u32,
    /// 0 disables constant quality; 0 < q <= 1 sets `Quality` and drops the
    /// bitrate target.
    vt_quality: f64,
    /// Seconds between periodic keyframes (`LGX_VT_KEYFRAME_INTERVAL`); 0
    /// leaves VideoToolbox's default.
    vt_keyframe_interval: u32,
    /// VideoToolbox's speed-over-quality hint. Turning it off spends more CPU
    /// on the encoder and usually saves bits at the same quality.
    vt_speed_over_quality: bool,
    /// Lower the bitrate when frames are dropped and raise it again when the
    /// link recovers.
    adaptive: bool,
    pair: bool,
    pair_name: String,
    pair_port: u16,
    rotate: bool,
}

/// The client's profile points its audio connection at port 5903 on the same
/// host it streams video from.
fn default_audio_listen(video_listen: &str) -> String {
    match video_listen.rsplit_once(':') {
        Some((host, _)) if !host.is_empty() => format!("{host}:5903"),
        _ => "0.0.0.0:5903".into(),
    }
}

pub fn host_of(addr: &str) -> String {
    match addr.rsplit_once(':') {
        Some((host, _)) if !host.is_empty() => host.to_string(),
        _ => "0.0.0.0".into(),
    }
}

pub fn port_of(addr: &str) -> u16 {
    addr.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(5902)
}

fn launch_agent_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = std::path::PathBuf::from(home)
        .join("Library/LaunchAgents/dev.b612s.lgx.server.plist");
    path.exists().then_some(path)
}

/// Removes a legacy `--token VALUE` pair from the LaunchAgent. The token lives
/// in `macos.conf` (mode 0600); leaving it in `ProgramArguments` exposes it to
/// every `ps`/`pgrep` on the machine. Launchd caches the plist, so the agent
/// has to be restarted once before the scrubbed copy takes effect.
fn scrub_launch_agent_token() -> bool {
    let Some(plist) = launch_agent_path() else {
        return false;
    };
    let Some(plist) = plist.to_str() else {
        return false;
    };
    let print = |key: &str| -> Option<String> {
        Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", &format!("Print :ProgramArguments:{key}"), plist])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let mut found = None;
    for i in 0..32 {
        match print(&i.to_string()) {
            Some(value) if value == "--token" => {
                found = Some(i);
                break;
            }
            Some(_) => continue,
            None => break,
        }
    }
    let Some(i) = found else {
        return false;
    };
    // Delete the value first so the flag index stays valid.
    let _ = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", &format!("Delete :ProgramArguments:{}", i + 1), plist])
        .status();
    let ok = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", &format!("Delete :ProgramArguments:{i}"), plist])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        info!("removed --token from the LaunchAgent; the token is read from {}", config::path().display());
        info!("restart the LaunchAgent once so launchd stops passing the old token");
    }
    ok
}

fn parse_args() -> Args {
    let cfg = Config::load();
    let mut a = Args {
        listen: cfg.listen.clone().unwrap_or_else(|| "0.0.0.0:5902".into()),
        token: cfg.token.clone().unwrap_or_default(),
        fps: cfg.fps,
        bitrate: cfg.bitrate.unwrap_or(20_000_000),
        display: cfg.display,
        helper: std::env::var("LGX_HELPER")
            .ok()
            .or_else(|| sibling("lgx-vtenc"))
            .unwrap_or_else(|| "lgx-vtenc".into()),
        input_helper: std::env::var("LGX_INPUT_HELPER")
            .ok()
            .or_else(|| sibling("lgx-cginput"))
            .unwrap_or_else(|| "lgx-cginput".into()),
        input_socket: std::env::var("LGX_INPUT_SOCKET").ok(),
        audio: cfg_audio_enabled(),
        audio_listen: std::env::var("LGX_AUDIO_LISTEN").ok(),
        audio_helper: std::env::var("LGX_AUDIO_HELPER")
            .ok()
            .or_else(|| sibling("lgx-macos-audio"))
            .unwrap_or_else(|| "lgx-macos-audio".into()),
        pb_helper: std::env::var("LGX_PB_HELPER")
            .ok()
            .or_else(|| sibling("lgx-pb"))
            .unwrap_or_else(|| "lgx-pb".into()),
        wheel_invert: cfg.wheel_invert.unwrap_or(false),
        vt_max_delay: std::env::var("LGX_VT_MAX_DELAY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1),
        vt_quality: std::env::var("LGX_VT_QUALITY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0),
        vt_keyframe_interval: std::env::var("LGX_VT_KEYFRAME_INTERVAL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
        vt_speed_over_quality: std::env::var("LGX_VT_SPEED_OVER_QUALITY")
            .ok()
            .map(|v| v != "0")
            .unwrap_or(true),
        adaptive: cfg.adaptive.unwrap_or(true),
        pair: false,
        pair_name: "macbook".into(),
        pair_port: cfg.pair_port.unwrap_or(5904),
        rotate: false,
    };
    // Environment first (manual runs, tests), then the command line.
    if let Ok(v) = std::env::var("LGX_LISTEN") {
        a.listen = v;
    }
    if let Ok(v) = std::env::var("LGX_TOKEN") {
        a.token = v;
    }
    if let Ok(v) = std::env::var("LGX_FPS") {
        a.fps = v.parse().ok();
    }
    if let Ok(v) = std::env::var("LGX_BITRATE") {
        if let Ok(v) = v.parse() {
            a.bitrate = v;
        }
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let next = |i: &mut usize| -> String {
            *i += 1;
            argv.get(*i).cloned().unwrap_or_default()
        };
        match argv[i].as_str() {
            "--listen" => a.listen = next(&mut i),
            "--token" => {
                warn!("--token is visible in ps; put it in {} or LGX_TOKEN instead", config::path().display());
                a.token = next(&mut i);
            }
            "--fps" => a.fps = Some(next(&mut i).parse().unwrap_or(60)),
            "--bitrate" => a.bitrate = next(&mut i).parse().unwrap_or(20_000_000),
            "--vt-max-delay" => a.vt_max_delay = next(&mut i).parse().unwrap_or(1),
            "--vt-quality" => a.vt_quality = next(&mut i).parse().unwrap_or(0.0),
            "--vt-keyframe-interval" => {
                a.vt_keyframe_interval = next(&mut i).parse().unwrap_or(10)
            }
            "--vt-speed-over-quality" => {
                a.vt_speed_over_quality = next(&mut i) != "0"
            }
            "--no-vt-speed-over-quality" => a.vt_speed_over_quality = false,
            "--display" => a.display = next(&mut i).parse().ok(),
            "--helper" => a.helper = next(&mut i),
            "--input-helper" => a.input_helper = next(&mut i),
            "--input-socket" => a.input_socket = Some(next(&mut i)),
            "--no-audio" => a.audio = false,
            "--audio" => a.audio = true,
            "--audio-listen" => a.audio_listen = Some(next(&mut i)),
            "--audio-helper" => a.audio_helper = next(&mut i),
            "--pb-helper" => a.pb_helper = next(&mut i),
            "--wheel-invert" => a.wheel_invert = true,
            "--no-wheel-invert" => a.wheel_invert = false,
            "--no-adaptive" => a.adaptive = false,
            "--adaptive" => a.adaptive = true,
            "--pair" => a.pair = true,
            "--pair-name" => a.pair_name = next(&mut i),
            "--pair-port" => a.pair_port = next(&mut i).parse().unwrap_or(5904),
            "--rotate" => a.rotate = true,
            other => warn!("unknown argument {other}"),
        }
        i += 1;
    }
    a
}

fn cfg_audio_enabled() -> bool {
    std::env::var("LGX_NO_AUDIO").is_err()
}

/// Generate a fresh token and store it in the config. A legacy `--token` in
/// the LaunchAgent is removed so the secret does not appear in `ps`.
fn rotate(args: &Args) -> std::io::Result<()> {
    let token = auth::generate_token();
    let mut cfg = Config::load();
    cfg.listen = Some(args.listen.clone());
    cfg.token = Some(token.clone());
    cfg.pair_port = Some(args.pair_port);
    cfg.fps = args.fps;
    cfg.bitrate = Some(args.bitrate);
    cfg.display = args.display;
    cfg.wheel_invert = Some(args.wheel_invert);
    cfg.adaptive = Some(args.adaptive);
    cfg.save()?;
    // The token must never live in the LaunchAgent: drop a legacy copy so the
    // plist and `ps` stop carrying it.
    scrub_launch_agent_token();
    info!("new token written to {}", config::path().display());
    info!("restart the server, then re-pair the client so its profile gets the new token");
    Ok(())
}

/// Serve one pairing request: the client proves it knows the code, and gets the
/// long-term token plus the ports to write into its profile.
fn pair_client(
    stream: std::net::TcpStream,
    code: &str,
    name: &str,
    token: &str,
    video_port: u16,
    audio_port: u16,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let secure = server_handshake(stream, code)?;
    secure.send_bytes(format!("{name}\n{token}\n{video_port}\n{audio_port}").as_bytes())?;
    let ack = secure.recv_bytes()?;
    if ack != b"ok" {
        secure.close();
        return Err(std::io::Error::other("client did not acknowledge the token"));
    }
    secure.close();
    Ok(())
}

/// One-shot pairing endpoint, mirroring `lgx-server --pair`.
fn run_pair(args: &Args) -> std::io::Result<()> {
    if args.token.is_empty() {
        return Err(std::io::Error::other("no token; run --rotate first"));
    }
    let host = host_of(&args.listen);
    let video_port = port_of(&args.listen);
    let audio_port = args.audio_listen.as_deref().map(port_of).unwrap_or(video_port + 1);
    let addr = format!("{host}:{}", args.pair_port);
    let listener = TcpListener::bind(&addr)
        .map_err(|e| std::io::Error::other(format!("bind {addr}: {e}")))?;
    listener.set_nonblocking(true)?;
    let code = auth::generate();
    info!("pairing code: {code}");
    info!("on the client run:");
    info!(
        "  lgx-client {} --pair {host}:{} --pair-code {code}",
        args.pair_name, args.pair_port
    );
    info!("waiting up to 3 minutes for one client...");
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut failures = 0u32;
    loop {
        if Instant::now() >= deadline {
            return Err(std::io::Error::other("pairing timed out"));
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                // On macOS an accepted socket inherits O_NONBLOCK from the
                // listener, which turns every read into EAGAIN. Pairing needs
                // ordinary blocking reads with a timeout.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_nodelay(true);
                info!("pairing client from {peer}");
                match pair_client(stream, &code, &args.pair_name, &args.token, video_port, audio_port)
                {
                    Ok(()) => {
                        info!(
                            "paired {peer} as {}; video={video_port} audio={audio_port}",
                            args.pair_name
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        failures += 1;
                        warn!("pairing failed: {e}");
                        if failures >= 5 {
                            return Err(std::io::Error::other("too many failed pairing attempts"));
                        }
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Spawn the capture helper and read its startup line to learn the geometry
/// and the frame rate it settled on.
/// `lgx-vtenc: capture 1920x1200@60, display 1 1920x1200 60.0 Hz`
fn spawn_capture(args: &Args) -> std::io::Result<(Child, u32, u32, u32, Option<u32>)> {
    let mut child_cmd = Command::new(&args.helper);
    // Only pass --fps when the caller pinned one; otherwise the helper follows
    // the display refresh rate instead of assuming 60 Hz.
    if let Some(fps) = args.fps {
        child_cmd.arg("--fps").arg(fps.to_string());
    }
    if let Some(display) = args.display {
        child_cmd.arg("--display").arg(display.to_string());
    }
    let mut child = child_cmd
        .arg("--bitrate")
        .arg(args.bitrate.to_string())
        .env("LGX_VT_MAX_DELAY", args.vt_max_delay.to_string())
        .env("LGX_VT_QUALITY", format!("{:.3}", args.vt_quality))
        .env(
            "LGX_VT_KEYFRAME_INTERVAL",
            args.vt_keyframe_interval.to_string(),
        )
        .env(
            "LGX_VT_SPEED_OVER_QUALITY",
            if args.vt_speed_over_quality { "1" } else { "0" },
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stderr = child.stderr.take().expect("stderr piped");
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    let (mut w, mut h, mut fps) = (1920u32, 1200u32, args.fps.unwrap_or(60));
    let mut display_id = None;
    if reader.read_line(&mut line).is_ok() {
        warn!("{}", line.trim_end());
        // `lgx-vtenc` reports the ScreenCaptureKit display id. Input must use
        // the same display, not the main one, when --display selects another.
        if let Some(idx) = line.find(" id=") {
            let digits: String = line[idx + 4..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            display_id = digits.parse::<u32>().ok();
        }
        if let Some(idx) = line.find("capture ") {
            let rest = &line[idx + 8..];
            if let Some(at) = rest.find('@') {
                let dims = &rest[..at];
                if let Some((sw, sh)) = dims.split_once('x') {
                    w = sw.parse().unwrap_or(w);
                    h = sh.parse().unwrap_or(h);
                }
                let digits: String =
                    rest[at + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(v) = digits.parse::<u32>() {
                    if v > 0 {
                        fps = v;
                    }
                }
            }
        }
    }
    // Keep draining stderr so the child never blocks on a full pipe.
    thread::spawn(move || {
        let mut sink = String::new();
        loop {
            sink.clear();
            match reader.read_line(&mut sink) {
                Ok(0) | Err(_) => break,
                Ok(_) => warn!("{}", sink.trim_end()),
            }
        }
    });
    Ok((child, w, h, fps, display_id))
}

fn spawn_pb(path: &str) -> Option<Child> {
    match Command::new(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => {
            info!("clipboard helper started ({path})");
            Some(child)
        }
        Err(e) => {
            warn!("cannot start clipboard helper {path}: {e}");
            None
        }
    }
}

/// Forwards cursor-shape frames from the helper to the client. The helper
/// writes `u32 BE length` followed by the CURSOR_IMAGE payload.
fn spawn_cursor(
    path: &str,
    secure: Arc<Secure>,
    alive: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    last: Arc<Mutex<Option<Vec<u8>>>>,
    hidden: Arc<AtomicBool>,
) -> Option<Child> {
    let mut cmd = Command::new(path);
    match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(stdout) = child.stdout.take() {
                thread::Builder::new()
                    .name("lgx-cursor".into())
                    .spawn(move || {
                        let mut reader = BufReader::new(stdout);
                        loop {
                            if !alive.load(Ordering::Relaxed) {
                                break;
                            }
                            let mut len = [0u8; 4];
                            if reader.read_exact(&mut len).is_err() {
                                break;
                            }
                            let n = u32::from_be_bytes(len) as usize;
                            if n == 0 {
                                // Zero-length frame: the guest cursor is hidden.
                                hidden.store(true, Ordering::Relaxed);
                                if enabled.load(Ordering::Relaxed)
                                    && secure
                                        .send_msg(Msg::new(msg::CURSOR_HIDDEN, 0, 0, 0), &[])
                                        .is_err()
                                {
                                    break;
                                }
                                continue;
                            }
                            if !(16..=256 * 1024).contains(&n) {
                                break;
                            }
                            let mut payload = vec![0u8; n];
                            if reader.read_exact(&mut payload).is_err() {
                                break;
                            }
                            hidden.store(false, Ordering::Relaxed);
                            *last.lock().unwrap() = Some(payload.clone());
                            if enabled.load(Ordering::Relaxed)
                                && secure
                                    .send_msg(
                                        Msg::new(msg::CURSOR_IMAGE, 0, n as i32, 0),
                                        &payload,
                                    )
                                    .is_err()
                            {
                                break;
                            }
                        }
                    })
                    .ok();
            }
            info!("cursor helper started ({path})");
            Some(child)
        }
        Err(e) => {
            warn!("cannot start cursor helper {path}: {e}");
            None
        }
    }
}

/// True when the access unit carries an HEVC IDR slice (NAL type 19 or 20).
fn is_idr(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 {
            if au[i + 2] == 1 {
                let t = (au[i + 3] >> 1) & 0x3f;
                if t == 19 || t == 20 {
                    return true;
                }
                i += 3;
                continue;
            }
            if au[i + 2] == 0 && au[i + 3] == 1 {
                let t = (au[i + 4] >> 1) & 0x3f;
                if t == 19 || t == 20 {
                    return true;
                }
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    false
}

fn spawn_input(path: &str, wheel_invert: bool, display_id: Option<u32>) -> Option<Child> {
    let mut cmd = Command::new(path);
    if wheel_invert {
        cmd.env("LGX_WHEEL_INVERT", "1");
    }
    if let Some(id) = display_id {
        cmd.env("LGX_DISPLAY_ID", id.to_string());
    }
    match cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => {
            info!("input helper started ({path})");
            Some(child)
        }
        Err(e) => {
            warn!("cannot start input helper {path}: {e}");
            None
        }
    }
}

fn send_helper(helper: &Mutex<Option<std::process::ChildStdin>>, line: &str) {
    if let Some(stdin) = helper.lock().unwrap().as_mut() {
        let _ = writeln!(stdin, "{line}");
        let _ = stdin.flush();
    }
}

fn main() {
    let mut args = parse_args();
    // Existing installs may still pass the token through ProgramArguments.
    scrub_launch_agent_token();

    if args.rotate {
        match rotate(&args) {
            Ok(()) => return,
            Err(e) => {
                warn!("rotate: {e}");
                std::process::exit(1);
            }
        }
    }

    if args.token.is_empty() {
        // Fresh install: mint a token so the server can start, and let the
        // client pick it up through --pair instead of a hand-edited plist.
        let token = auth::generate_token();
        let mut cfg = Config::load();
        cfg.listen = Some(args.listen.clone());
        cfg.token = Some(token.clone());
        cfg.pair_port = Some(args.pair_port);
        if let Err(e) = cfg.save() {
            warn!("cannot write {}: {e}", config::path().display());
        } else {
            info!("no token configured; generated one in {}", config::path().display());
        }
        args.token = token;
    }

    if args.pair {
        if let Err(e) = run_pair(&args) {
            warn!("pairing: {e}");
            std::process::exit(1);
        }
        return;
    }

    let args = Arc::new(args);

    let listener = match TcpListener::bind(&args.listen) {
        Ok(l) => l,
        Err(e) => {
            warn!("bind {}: {e}", args.listen);
            std::process::exit(1);
        }
    };
    info!("listening on {}", args.listen);

    if args.audio {
        let addr = args
            .audio_listen
            .clone()
            .unwrap_or_else(|| default_audio_listen(&args.listen));
        let token = args.token.clone();
        let helper = args.audio_helper.clone();
        thread::Builder::new()
            .name("lgx-audio".into())
            .spawn(move || {
                if let Err(e) = audio::serve(&addr, token, helper) {
                    warn!("audio: {e}");
                }
            })
            .ok();
    }

    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(e) => {
                warn!("accept: {e}");
                continue;
            }
        };
        let args = Arc::clone(&args);
        // One client at a time, like the Linux server.
        let secure = {
            let mut stream = stream;
            let id = match server_read_id(&mut stream) {
                Ok(id) => id,
                Err(e) => {
                    warn!("client id: {e}");
                    continue;
                }
            };
            match server_handshake(stream, &args.token) {
                Ok(s) => {
                    info!("client {id} authenticated");
                    Arc::new(s)
                }
                Err(e) => {
                    warn!("handshake for {id}: {e}");
                    continue;
                }
            }
        };

        if let Err(e) = serve(&args, secure) {
            warn!("session ended: {e}");
        }
        info!("client disconnected, waiting for the next one");
    }
}

fn serve(args: &Args, secure: Arc<Secure>) -> std::io::Result<()> {
    let (mut capture, w, h, fps, display_id) = spawn_capture(args)?;
    // The capture helper's stdin asks for an IDR and carries bitrate changes.
    let capture_stdin: Arc<Mutex<Option<std::process::ChildStdin>>> =
        Arc::new(Mutex::new(capture.stdin.take()));
    let input = if args.input_socket.is_some() {
        None
    } else {
        spawn_input(&args.input_helper, args.wheel_invert, display_id)
    };
    let mut pb_child = spawn_pb(&args.pb_helper);
    let pb_stdin: Arc<Mutex<Option<std::process::ChildStdin>>> =
        Arc::new(Mutex::new(pb_child.as_mut().and_then(|c| c.stdin.take())));

    // HELLO carries the geometry the client sizes its window from.
    secure.send_msg(Msg::new(msg::HELLO, codec::HEVC, w as i32, h as i32), &[])?;
    // The client draws its own pointer, which follows the local mouse at the
    // display rate. The flag travels in `a`: the client reads `m.a`, so putting
    // it in `code` left the local cursor disabled and exposed the cursor that
    // ScreenCaptureKit composites into the video, which trails by the whole
    // capture/encode/network pipeline.
    secure.send_msg(Msg::new(msg::CURSOR_LOCAL, 0, 1, 0), &[])?;
    info!(
        "streaming {w}x{h}, fps<= {fps}, {} bps, wheel {}, adaptive {}",
        args.bitrate,
        if args.wheel_invert { "inverted" } else { "normal" },
        if args.adaptive { "on" } else { "off" }
    );

    // The session dies as soon as either side notices the other is gone; a lost
    // client must never keep the single-client server busy.
    let alive = Arc::new(AtomicBool::new(true));
    let need_idr = Arc::new(AtomicBool::new(false));
    let cursor_enabled = Arc::new(AtomicBool::new(false));
    let cursor_last: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let cursor_hidden = Arc::new(AtomicBool::new(false));
    let cursor_child = sibling("lgx-macos-cursor").and_then(|path| {
        spawn_cursor(
            &path,
            Arc::clone(&secure),
            Arc::clone(&alive),
            Arc::clone(&cursor_enabled),
            Arc::clone(&cursor_last),
            Arc::clone(&cursor_hidden),
        )
    });

    // Reader thread: client -> input helper.
    // Clipboard changes made on the Mac are pushed to the client.
    if let Some(stdout) = pb_child.as_mut().and_then(|c| c.stdout.take()) {
        let sec = Arc::clone(&secure);
        let pb_alive = Arc::clone(&alive);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                if !pb_alive.load(Ordering::Relaxed) {
                    break;
                }
                let mut len = [0u8; 4];
                if reader.read_exact(&mut len).is_err() {
                    break;
                }
                let n = u32::from_be_bytes(len) as usize;
                if n == 0 || n > 32 * 1024 * 1024 {
                    break;
                }
                let mut kind = [0u8; 1];
                if reader.read_exact(&mut kind).is_err() {
                    break;
                }
                let mut payload = vec![0u8; n];
                if reader.read_exact(&mut payload).is_err() {
                    break;
                }
                if kind[0] == 3 {
                    if let Some((name, data)) = file::decode_record(&payload) {
                        let begin = file::encode_begin(&name, data.len() as u64);
                        if sec
                            .send_msg(Msg::new(msg::FILE_BEGIN, 0, begin.len() as i32, 0), &begin)
                            .is_err()
                        {
                            break;
                        }
                        let mut ok = true;
                        for chunk in data.chunks(32 * 1024) {
                            if sec
                                .send_msg(Msg::new(msg::FILE_DATA, 0, chunk.len() as i32, 0), chunk)
                                .is_err()
                            {
                                ok = false;
                                break;
                            }
                        }
                        if !ok || sec.send_msg(Msg::new(msg::FILE_END, 0, 0, 0), &[]).is_err() {
                            break;
                        }
                    }
                    continue;
                }
                let typ = if kind[0] == 2 { msg::CLIP_DATA_HTML } else { msg::CLIP_DATA };
                if sec.send_msg(Msg::new(typ, 0, payload.len() as i32, 0), &payload).is_err() {
                    break;
                }
            }
        });
    }

    let sec_reader = Arc::clone(&secure);
    let reader_alive = Arc::clone(&alive);
    let reader_idr = Arc::clone(&need_idr);
    let mut input_child = input;
    let input_stdin = input_child.as_mut().map(|c| {
        let stdin = c.stdin.take().expect("stdin piped");
        std::io::BufWriter::new(stdin)
    });
    let input_handle: Option<JoinHandle<()>> = input_stdin.map(|mut cmd| {
        let pb = Arc::clone(&pb_stdin);
        let cursor_enabled = Arc::clone(&cursor_enabled);
        let cursor_last = Arc::clone(&cursor_last);
        let cursor_hidden = Arc::clone(&cursor_hidden);
        thread::spawn(move || {
            // A read timeout lets the thread notice a dead peer instead of
            // blocking in recv_msg forever.
            let _ = sec_reader.set_read_timeout(Some(Duration::from_millis(500)));
            let _ = write_controls(
                &sec_reader,
                &mut cmd,
                &reader_alive,
                &pb,
                &reader_idr,
                &cursor_enabled,
                &cursor_last,
                &cursor_hidden,
            );
        })
    });

    if let Some(path) = args.input_socket.clone() {
        let sec = Arc::clone(&secure);
        let a = Arc::clone(&alive);
        let pb = Arc::clone(&pb_stdin);
        let idr = Arc::clone(&need_idr);
        let cursor_enabled = Arc::clone(&cursor_enabled);
        let cursor_last = Arc::clone(&cursor_last);
        let cursor_hidden = Arc::clone(&cursor_hidden);
        thread::spawn(move || match std::os::unix::net::UnixStream::connect(&path) {
            Ok(mut stream) => {
                info!("input via launcher socket {path}");
                let _ = write_controls(
                    &sec,
                    &mut stream,
                    &a,
                    &pb,
                    &idr,
                    &cursor_enabled,
                    &cursor_last,
                    &cursor_hidden,
                );
            }
            Err(e) => warn!("input socket {path}: {e}"),
        });
    }

    // Capture thread feeds a bounded channel: a client (or a link) that cannot
    // keep up must drop frames instead of growing an unbounded backlog, which
    // only turns low bandwidth into seconds of latency. Every dropped frame
    // breaks the HEVC reference chain, so the main loop asks for an IDR.
    let stdout = capture.stdout.take().expect("stdout piped");
    let (tx, rx) = mpsc::sync_channel::<(Vec<u8>, u64)>(QUEUE_FRAMES);
    let dropped = Arc::new(AtomicUsize::new(0));
    let cap_alive = Arc::clone(&alive);
    let cap_dropped = Arc::clone(&dropped);
    let cap_idr = Arc::clone(&need_idr);
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut len = [0u8; 4];
        loop {
            if !cap_alive.load(Ordering::Relaxed) {
                break;
            }
            if reader.read_exact(&mut len).is_err() {
                break;
            }
            let n = u32::from_be_bytes(len) as usize;
            if n == 0 || n > 32 * 1024 * 1024 {
                break;
            }
            let mut ts = [0u8; 8];
            if reader.read_exact(&mut ts).is_err() {
                break;
            }
            let capture_us = u64::from_be_bytes(ts);
            let mut au = vec![0u8; n];
            if reader.read_exact(&mut au).is_err() {
                break;
            }
            match tx.try_send((au, capture_us)) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    cap_dropped.fetch_add(1, Ordering::Relaxed);
                    cap_idr.store(true, Ordering::Relaxed);
                }
                Err(mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
        cap_alive.store(false, Ordering::Relaxed);
    });

    let mut last_ping = Instant::now();
    let mut last_idr: Option<Instant> = None;
    let (mut frames, mut bytes) = (0u64, 0u64);
    // Keyframes are the expensive part of constant-quality video; counting
    // them separately shows whether the keyframe interval dominates traffic.
    let (mut keyframes, mut key_bytes) = (0u64, 0u64);
    let mut send_secs = 0.0f64;
    // A frame that is already this old when it reaches the writer is not
    // worth sending: the client would present seconds-old video. Drop it and
    // let the encoder start a fresh reference chain instead.
    // ScreenCaptureKit and VideoToolbox hand frames over in bursts. Sending a
    // burst as fast as it arrives overruns the client's shallow pre-decode
    // queue, which then skips to the next keyframe; pacing the writes to the
    // capture timeline turns the burst back into a steady stream. The age check
    // is the fallback for a backlog that pacing cannot fix.
    let max_age_us = std::env::var("LGX_MAX_FRAME_AGE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(250)
        * 1000;
    let mut late = 0u64;
    let mut max_age_ms = 0u64;
    let mut bitrate = args.bitrate;
    let min_bitrate = (args.bitrate / 8).max(4_000_000);
    let mut clean_windows = 0u32;
    let mut t0 = Instant::now();
    let stats_period = if std::env::var_os("LGX_DEBUG_FPS").is_some() { 1 } else { 5 };
    let result = loop {
        if !alive.load(Ordering::Relaxed) {
            break Err(std::io::Error::new(ErrorKind::ConnectionAborted, "client went away"));
        }
        // Ask for an IDR at most twice a second: a slow client must recover,
        // but must not turn the whole stream into keyframes.
        if need_idr.load(Ordering::Relaxed)
            && last_idr.is_none_or(|t| t.elapsed() >= Duration::from_millis(500))
            && need_idr.swap(false, Ordering::Relaxed)
        {
            last_idr = Some(Instant::now());
            send_helper(&capture_stdin, "idr");
        }
        if t0.elapsed() >= Duration::from_secs(stats_period) {
            let elapsed = t0.elapsed().as_secs_f64();
            let lost = dropped.swap(0, Ordering::Relaxed);
            info!(
                "HEVC {:.1} fps, {:.1} MiB/s, send {:.2} ms, {} dropped, {} late, max age {} ms, IDR {:.1}/s {:.1} KiB/s, {} kbps",
                frames as f64 / elapsed,
                bytes as f64 / elapsed / 1_048_576.0,
                if frames > 0 { send_secs * 1000.0 / frames as f64 } else { 0.0 },
                lost,
                late,
                max_age_ms,
                keyframes as f64 / elapsed,
                key_bytes as f64 / elapsed / 1024.0,
                bitrate / 1000
            );
            late = 0;
            max_age_ms = 0;
            keyframes = 0;
            key_bytes = 0;
            // Adaptive bitrate: drops mean the pipe (or the client) cannot take
            // the current rate. Back off quickly, recover slowly.
            if args.adaptive {
                if lost > 0 {
                    clean_windows = 0;
                    let next = (bitrate * 7 / 8).max(min_bitrate);
                    if next < bitrate {
                        bitrate = next;
                        send_helper(&capture_stdin, &format!("bitrate {bitrate}"));
                        info!("bitrate lowered to {} kbps after {lost} dropped frames", bitrate / 1000);
                    }
                } else {
                    clean_windows += 1;
                    if clean_windows >= 6 && bitrate < args.bitrate {
                        bitrate = (bitrate * 9 / 8).min(args.bitrate);
                        clean_windows = 0;
                        send_helper(&capture_stdin, &format!("bitrate {bitrate}"));
                        info!("bitrate raised to {} kbps", bitrate / 1000);
                    }
                }
            }
            t0 = Instant::now();
            frames = 0;
            bytes = 0;
            send_secs = 0.0;
        }
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok((mut au, mut capture_us)) => {
                // Coalesce a burst to its newest frame. VideoToolbox hands over
                // several frames at once when it catches up; sending them all
                // overruns the client's queue and every dropped frame poisons
                // its decoder until the next keyframe. Keep the newest, ask for
                // an IDR so the reference chain is fresh, and stay at the live
                // edge.
                let mut skipped = 0u64;
                while let Ok((next_au, next_ts)) = rx.try_recv() {
                    au = next_au;
                    capture_us = next_ts;
                    skipped += 1;
                }
                if skipped > 0 {
                    late += skipped;
                    need_idr.store(true, Ordering::Relaxed);
                }
                if capture_us != 0 {
                    let age = monotonic_us().saturating_sub(capture_us);
                    max_age_ms = max_age_ms.max(age / 1000);
                    if max_age_us > 0 && age > max_age_us {
                        // Stale: sending it would add latency and the next
                        // frames reference it anyway, so start a new GOP.
                        late += 1;
                        need_idr.store(true, Ordering::Relaxed);
                        continue;
                    }
                }
                // Pace to the capture timeline: a frame that was captured one
                // interval after the previous one goes out one interval later.
                // A producer that is already slower than the pacer never
                // waits, so a steady stream is not delayed.
                let n = au.len();
                let send_started = Instant::now();
                // `code`/`b` carry a 48-bit capture timestamp in microseconds;
                // zero means unknown. The client uses it to align video with
                // the audio stream, which carries the same server clock.
                let code = (capture_us >> 32) as u16;
                let b = capture_us as u32 as i32;
                let sent = secure.send_msg(Msg::new(msg::VIDEO, code, n as i32, b), &au);
                send_secs += send_started.elapsed().as_secs_f64();
                if let Err(e) = sent {
                    break Err(e);
                }
                if is_idr(&au) {
                    keyframes += 1;
                    key_bytes += n as u64;
                }
                frames += 1;
                bytes += n as u64;

            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Keepalive: a write to a closed socket fails immediately, which
                // is what we need to notice a client that vanished silently.
                if last_ping.elapsed() >= Duration::from_secs(1) {
                    last_ping = Instant::now();
                    if let Err(e) = secure.send_msg(Msg::new(msg::PING, 0, 0, 0), &[]) {
                        break Err(e);
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break Err(std::io::Error::new(ErrorKind::BrokenPipe, "capture ended"));
            }
        }
    };

    alive.store(false, Ordering::Relaxed);
    let _ = capture.kill();
    let _ = capture.wait();
    if let Some(mut child) = input_child {
        let _ = child.kill();
        let _ = child.wait();
    }
    if let Some(mut child) = pb_child {
        let _ = child.kill();
        let _ = child.wait();
    }
    if let Some(mut child) = cursor_child {
        let _ = child.kill();
        let _ = child.wait();
    }
    if let Some(h) = input_handle {
        let _ = h.join();
    }
    result
}

fn write_clip(out: &mut impl Write, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    out.write_all(&(payload.len() as u32).to_be_bytes())?;
    out.write_all(&[kind])?;
    out.write_all(payload)?;
    out.flush()
}

/// Forward input messages to `lgx-cginput` as plain text lines.
// The cursor state is three independent Arcs shared with the cursor thread;
// grouping them would only add a wrapper used by this function and spawn_cursor.
#[allow(clippy::too_many_arguments)]
fn write_controls(
    secure: &Secure,
    out: &mut impl Write,
    alive: &Arc<AtomicBool>,
    pb: &Mutex<Option<std::process::ChildStdin>>,
    need_idr: &Arc<AtomicBool>,
    cursor_enabled: &Arc<AtomicBool>,
    cursor_last: &Arc<Mutex<Option<Vec<u8>>>>,
    cursor_hidden: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    let mut seen: u64 = 0;
    let mut incoming_file: Option<(String, u64, Vec<u8>)> = None;
    loop {
        let (m, payload) = match secure.recv_msg() {
            Ok(v) => v,
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                if !alive.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(());
                }
                continue;
            }
            Err(_) => {
                alive.store(false, std::sync::atomic::Ordering::Relaxed);
                return Ok(());
            }
        };
        let line = match m.typ {
            msg::KEY => format!("key {} {}\n", m.code, if m.a != 0 { 1 } else { 0 }),
            msg::BTN => format!("btn {} {}\n", m.code, if m.a != 0 { 1 } else { 0 }),
            msg::MOTION => format!("motion {} {}\n", m.a, m.b),
            msg::WHEEL => format!("wheel {} {}\n", m.code, m.a),
            // ScreenCaptureKit cannot compose the pointer without lagging it
            // by the capture pipeline, so the video never carries a cursor and
            // the client's own (instant) pointer is the only one on screen.
            msg::CURSOR => continue,
            // Clock probe: reply on the same clock the video timestamps use.
            msg::PING => {
                let now = monotonic_us();
                let _ = secure.send_msg(Msg::new(msg::PONG, 0, 8, 0), &now.to_le_bytes());
                continue;
            }
            // The client had to drop packets and wants a keyframe now.
            msg::IDR => {
                need_idr.store(true, std::sync::atomic::Ordering::Relaxed);
                continue;
            }
            msg::CLIP_SET => {
                if let Some(stdin) = pb.lock().unwrap().as_mut() {
                    let _ = write_clip(stdin, 1, &payload);
                }
                continue;
            }
            msg::CLIP_SET_HTML => {
                if let Some(stdin) = pb.lock().unwrap().as_mut() {
                    let _ = write_clip(stdin, 2, &payload);
                }
                continue;
            }
            msg::FILE_BEGIN => {
                incoming_file = match file::decode_begin(&payload) {
                    Some((name, size)) if size <= file::MAX_BYTES => {
                        Some((name, size, Vec::new()))
                    }
                    _ => None,
                };
                continue;
            }
            msg::FILE_DATA => {
                if let Some((_, size, data)) = incoming_file.as_mut() {
                    if data.len() as u64 + payload.len() as u64 <= *size {
                        data.extend_from_slice(&payload);
                    } else {
                        incoming_file = None;
                    }
                }
                continue;
            }
            msg::FILE_END => {
                if let Some((name, size, data)) = incoming_file.take() {
                    if data.len() as u64 == size && data.len() as u64 <= file::MAX_BYTES {
                        let record = file::encode_record(&name, &data);
                        if let Some(stdin) = pb.lock().unwrap().as_mut() {
                            let _ = write_clip(stdin, 3, &record);
                        }
                    }
                }
                continue;
            }
            msg::CURSOR_SHAPE => {
                cursor_enabled.store(true, Ordering::Relaxed);
                if cursor_hidden.load(Ordering::Relaxed) {
                    let _ = secure.send_msg(Msg::new(msg::CURSOR_HIDDEN, 0, 0, 0), &[]);
                } else if let Some(payload) = cursor_last.lock().unwrap().clone() {
                    let _ = secure.send_msg(
                        Msg::new(msg::CURSOR_IMAGE, 0, payload.len() as i32, 0),
                        &payload,
                    );
                }
                continue;
            }
            // KEYMAP/MODS and gamepads are not handled by this server yet.
            _ => continue,
        };
        seen += 1;
        if seen <= 5 || seen.is_multiple_of(60) {
            info!("input #{seen} -> {line}");
        }
        out.write_all(line.as_bytes())?;
        out.flush()?;
    }
}
