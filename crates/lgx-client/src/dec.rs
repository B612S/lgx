//! In-process HEVC decoding with hardware surfaces.
//!
//! Frames arrive from the network, go straight into libavcodec (VAAPI when the
//! host has a usable render node) and leave either as dma-buf planes that the
//! GL renderer imports without touching the CPU, or as NV12 pixels for the
//! fallback path. No helper process, no pipes, no readback in the hardware
//! path.
//!
//! A single decoder thread owns the codec; the window thread takes the newest
//! frame from [`Sink`] and drops whatever it did not pick up, so a slow render
//! pass can never grow a queue of stale frames (latency stays bounded by one
//! frame instead of by throughput).

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

#[repr(C)]
struct LgxDec {
    _private: [u8; 0],
}

#[repr(C)]
struct FrameInfo {
    kind: c_int,
    width: c_int,
    height: c_int,
    y: *const u8,
    uv: *const u8,
    y_stride: c_int,
    uv_stride: c_int,
    nb_planes: c_int,
    fds: [c_int; 4],
    offsets: [u32; 4],
    pitches: [u32; 4],
    modifier: u64,
    fourcc: u32,
    pts: i64,
}

/// Hardware backend requested from the decoder shim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Software,
    Vaapi,
    Cuda,
}

impl Backend {
    pub fn code(self) -> c_int {
        match self {
            Backend::Software => 0,
            Backend::Vaapi => 1,
            Backend::Cuda => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Backend::Software => "software",
            Backend::Vaapi => "VAAPI",
            Backend::Cuda => "CUDA/NVDEC",
        }
    }
}

extern "C" {
    fn lgx_dec_open(
        backend: c_int,
        device: *const c_char,
        want_dmabuf: c_int,
        threads: c_int,
        err: *mut c_char,
        errlen: c_int,
    ) -> *mut LgxDec;
    fn lgx_dec_push(d: *mut LgxDec, data: *const u8, len: c_int, pts: i64) -> c_int;
    fn lgx_dec_pull(d: *mut LgxDec, out: *mut FrameInfo, handle: *mut *mut c_void) -> c_int;
    #[allow(dead_code)]
    fn lgx_dec_download_frame(
        handle: *mut c_void,
        out: *mut FrameInfo,
        cpu: *mut *mut c_void,
    ) -> c_int;
    fn lgx_dec_free_frame(handle: *mut c_void);
    fn lgx_dec_close(d: *mut LgxDec);
    fn lgx_dec_last_error(d: *const LgxDec) -> *const c_char;
    fn lgx_dec_is_hw(d: *const LgxDec) -> c_int;
}

/// One encoded chunk received from the server.
pub struct Chunk {
    pub data: Vec<u8>,
    pub at: Instant,
}

/// dma-buf planes exported from a VAAPI surface.
#[allow(dead_code)]
pub struct Dmabuf {
    pub fds: [c_int; 4],
    pub offsets: [u32; 4],
    pub pitches: [u32; 4],
    pub planes: u32,
    pub modifier: u64,
    pub fourcc: u32,
    handle: *mut c_void,
}

/// CPU-visible NV12 planes.
pub struct Nv12 {
    pub y: *const u8,
    pub uv: *const u8,
    pub y_stride: u32,
    pub uv_stride: u32,
    handle: *mut c_void,
}

#[allow(dead_code)]
pub enum Pixels {
    Dmabuf(Dmabuf),
    Nv12(Nv12),
}

pub struct Frame {
    pub width: u32,
    pub height: u32,
    #[allow(dead_code)]
    pub pts: i64,
    /// When the chunk that produced this frame arrived from the network.
    pub arrival: Instant,
    pub pixels: Pixels,
}

// The buffers are owned by the opaque handle; moving the frame between the
// decoder thread and the render thread is the whole point of this type.
unsafe impl Send for Frame {}
unsafe impl Send for Dmabuf {}
unsafe impl Send for Nv12 {}

impl Drop for Dmabuf {
    fn drop(&mut self) {
        unsafe { lgx_dec_free_frame(self.handle) };
    }
}

impl Drop for Nv12 {
    fn drop(&mut self) {
        unsafe { lgx_dec_free_frame(self.handle) };
    }
}

pub struct Decoder {
    raw: *mut LgxDec,
    hw: bool,
    pub backend: Backend,
}

impl Decoder {
    /// `device`: `None` forces software decoding, `Some("")` lets FFmpeg pick a
    /// VAAPI render node, `Some(path)` pins one. `want_dmabuf` must be false
    /// when there is no GL context to import the planes into.
    pub fn open(backend: Backend, device: Option<&str>, want_dmabuf: bool) -> io::Result<Self> {
        let cdev = match device {
            Some(s) => Some(CString::new(s).map_err(|e| io::Error::other(e.to_string()))?),
            None => None,
        };
        let mut err = [0i8; 256];
        let raw = unsafe {
            lgx_dec_open(
                backend.code(),
                cdev.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                want_dmabuf as c_int,
                0,
                err.as_mut_ptr(),
                err.len() as c_int,
            )
        };
        if raw.is_null() {
            let msg = unsafe { CStr::from_ptr(err.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(io::Error::other(if msg.is_empty() {
                "decoder failed to open".to_string()
            } else {
                msg
            }));
        }
        let hw = unsafe { lgx_dec_is_hw(raw) != 0 };
        Ok(Self { raw, hw, backend })
    }

    pub fn hardware(&self) -> bool {
        self.hw
    }

    fn last_error(&self) -> String {
        unsafe { CStr::from_ptr(lgx_dec_last_error(self.raw)) }
            .to_string_lossy()
            .into_owned()
    }

    /// `Ok(false)` means the decoder wants its frames drained before the packet
    /// can be accepted again.
    pub fn push(&mut self, data: &[u8], pts: i64) -> io::Result<bool> {
        let r = unsafe { lgx_dec_push(self.raw, data.as_ptr(), data.len() as c_int, pts) };
        match r {
            1 => Ok(true),
            2 => Ok(false),
            _ => {
                let head: Vec<String> = data.iter().take(8).map(|b| format!("{b:02x}")).collect();
                eprintln!(
                    "lgx-client: decoder rejected a {} byte chunk [{}{}]...",
                    data.len(),
                    head.join(" "),
                    if data.len() > 8 { ", " } else { "" }
                );
                Err(io::Error::other(self.last_error()))
            }
        }
    }

    pub fn pull(&mut self, arrival: Instant) -> io::Result<Option<Frame>> {
        let mut info: FrameInfo = unsafe { std::mem::zeroed() };
        let mut handle: *mut c_void = std::ptr::null_mut();
        let r = unsafe { lgx_dec_pull(self.raw, &mut info, &mut handle) };
        if r == 0 {
            return Ok(None);
        }
        if r < 0 || handle.is_null() {
            return Err(io::Error::other(self.last_error()));
        }
        Ok(Some(frame_from_info(info, handle, arrival)?))
    }
}

#[allow(dead_code)]
impl Frame {
    /// Copies a dma-buf frame into CPU NV12. This is the escape hatch for a
    /// host whose EGL cannot import the decoder's surfaces; the GL renderer
    /// calls it once and then stops trying dma-buf imports.
    pub fn download(&self) -> io::Result<Frame> {
        let Pixels::Dmabuf(d) = &self.pixels else {
            return Err(io::Error::other("frame is already on the CPU"));
        };
        let mut info: FrameInfo = unsafe { std::mem::zeroed() };
        let mut handle: *mut c_void = std::ptr::null_mut();
        let r = unsafe { lgx_dec_download_frame(d.handle, &mut info, &mut handle) };
        if r != 1 || handle.is_null() {
            return Err(io::Error::other("cannot download the decoded surface"));
        }
        frame_from_info(info, handle, self.arrival)
    }
}

fn frame_from_info(
    info: FrameInfo,
    handle: *mut c_void,
    arrival: Instant,
) -> io::Result<Frame> {
    let pixels = if info.kind == 1 {
        let d = Dmabuf {
            fds: info.fds,
            offsets: info.offsets,
            pitches: info.pitches,
            planes: info.nb_planes as u32,
            modifier: info.modifier,
            fourcc: info.fourcc,
            handle,
        };
        if d.planes != 2 {
            unsafe { lgx_dec_free_frame(handle) };
            return Err(io::Error::other("decoder exported an unsupported plane count"));
        }
        for fd in d.fds.iter().take(d.planes as usize) {
            if *fd < 0 {
                unsafe { lgx_dec_free_frame(handle) };
                return Err(io::Error::other("decoder exported a closed dma-buf"));
            }
        }
        Pixels::Dmabuf(d)
    } else if info.kind == 2 {
        Pixels::Nv12(Nv12 {
            y: info.y,
            uv: info.uv,
            y_stride: info.y_stride as u32,
            uv_stride: info.uv_stride as u32,
            handle,
        })
    } else {
        unsafe { lgx_dec_free_frame(handle) };
        return Err(io::Error::other("decoder returned an unknown frame kind"));
    };
    Ok(Frame {
        width: info.width as u32,
        height: info.height as u32,
        pts: info.pts,
        arrival,
        pixels,
    })
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { lgx_dec_close(self.raw) };
    }
}

/// Newest-frame mailbox between the decoder and the window.
pub struct Sink {
    slot: Mutex<Option<Frame>>,
    wake: Condvar,
    pub decoded: AtomicU64,
    pub dropped: AtomicU64,
    pub stopped: AtomicBool,
    pub error: Mutex<Option<String>>,
    pub hw: AtomicBool,
}

impl Sink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(None),
            wake: Condvar::new(),
            decoded: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            error: Mutex::new(None),
            hw: AtomicBool::new(false),
        })
    }

    pub fn publish(&self, frame: Frame) {
        let mut slot = self.slot.lock().unwrap();
        if slot.is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        *slot = Some(frame);
        drop(slot);
        self.wake.notify_all();
    }

    pub fn take(&self) -> Option<Frame> {
        self.slot.lock().unwrap().take()
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

/// Decoder thread. `wake` is invoked for every published frame so the window
/// loop can sleep in SDL_WaitEvent instead of spinning.
pub fn spawn(
    backends: Vec<Backend>,
    device: Option<String>,
    want_dmabuf: bool,
    rx: Receiver<Chunk>,
    sink: Arc<Sink>,
    wake: Option<Box<dyn Fn() + Send>>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("lgx-decode".into())
        .spawn(move || {
            let mut opened = None;
            for backend in backends {
                match Decoder::open(backend, device.as_deref(), want_dmabuf) {
                    Ok(d) => {
                        opened = Some(d);
                        break;
                    }
                    Err(e) => eprintln!("lgx-client: {} decoder unavailable: {e}", backend.name()),
                }
            }
            let mut dec = match opened {
                Some(d) => d,
                None => {
                    *sink.error.lock().unwrap() = Some("no HEVC decoder available".into());
                    sink.stopped.store(true, Ordering::Relaxed);
                    return;
                }
            };
            sink.hw.store(dec.hardware(), Ordering::Relaxed);
            eprintln!(
                "lgx-client: HEVC decoder ready ({}, {} frame)",
                if dec.hardware() { dec.backend.name() } else { "software" },
                if dec.hardware() && want_dmabuf { "zero-copy dma-buf" } else { "CPU NV12" }
            );

            let mut pts = 0i64;
            while let Ok(chunk) = rx.recv() {
                // A packet can be refused until pending frames are pulled.
                loop {
                    match dec.push(&chunk.data, pts) {
                        Ok(true) => break,
                        Ok(false) => {
                            if let Err(e) = drain(&mut dec, &sink, chunk.at, &wake) {
                                fail(&sink, e);
                                return;
                            }
                        }
                        Err(e) => {
                            fail(&sink, e);
                            return;
                        }
                    }
                }
                pts = pts.wrapping_add(1);
                if let Err(e) = drain(&mut dec, &sink, chunk.at, &wake) {
                    fail(&sink, e);
                    return;
                }
            }
            sink.stopped.store(true, Ordering::Relaxed);
        })
        .expect("decoder thread")
}

fn drain(
    dec: &mut Decoder,
    sink: &Arc<Sink>,
    arrival: Instant,
    wake: &Option<Box<dyn Fn() + Send>>,
) -> io::Result<()> {
    loop {
        match dec.pull(arrival)? {
            Some(frame) => {
                if std::env::var_os("LGX_DEBUG_LAT").is_some() {
                    let us = frame.arrival.elapsed().as_micros();
                    eprintln!("lgx-client: decode+queue {us} us ({}x{})", frame.width, frame.height);
                }
                sink.decoded.fetch_add(1, Ordering::Relaxed);
                sink.publish(frame);
                if let Some(w) = wake {
                    w();
                }
            }
            None => return Ok(()),
        }
    }
}

fn fail(sink: &Arc<Sink>, e: io::Error) {
    eprintln!("lgx-client: decoder: {e}");
    *sink.error.lock().unwrap() = Some(e.to_string());
    sink.stopped.store(true, Ordering::Relaxed);
}
