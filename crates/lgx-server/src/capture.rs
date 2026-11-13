//! Capturing compositor output through wlr-screencopy.
//!
//! Preferred path is a linear dma-buf: the compositor blits into a DRM dumb
//! buffer with the GPU, and the CPU reads that buffer directly afterwards.
//! The fallback is a `wl_shm` buffer, which makes the compositor read the
//! framebuffer back synchronously (slower).

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd};
use std::time::{Duration, Instant};

use memmap2::MmapMut;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

use crate::dmabuf::DumbBuffer;
use lgx_server::gbm::GbmBuffer;
use crate::protocols::{linux_dmabuf, screencopy, xdg_output};

pub struct FrameInfo {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
}

struct ShmBuf {
    _file: File,
    map: MmapMut,
}

struct DmaBuf {
    _dumb: DumbBuffer,
}

pub struct CapState {
    shm: wl_shm::WlShm,
    screencopy: screencopy::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    dmabuf: Option<linux_dmabuf::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    pub use_dmabuf: bool,
    pub use_gbm: bool,
    pub gbm_linear: bool,
    outputs: Vec<wl_output::WlOutput>,
    names: Vec<String>,
    sizes: Vec<(i32, i32)>,
    refresh: Vec<u32>,
    pub refresh_hz: u32,
    chosen: Option<wl_output::WlOutput>,
    shm_bufs: [Option<(wl_buffer::WlBuffer, ShmBuf)>; 2],
    dma_bufs: [Option<(wl_buffer::WlBuffer, DmaBuf)>; 2],
    gbm_bufs: [Option<(wl_buffer::WlBuffer, GbmBuffer)>; 2],
    pub cur: usize,
    pub overlay_cursor: bool,
    pub pending_copy: bool,
    pub damage_only: bool,
    pub logged_format: bool,
    pub done: bool,
    pub failed: bool,
    pub info: Option<FrameInfo>,
    pub staged: Vec<u8>,
    pub in_flight: bool,
}

pub struct Capture {
    conn: Connection,
    queue: EventQueue<CapState>,
    pub state: CapState,
}

impl Capture {
    pub fn connect(output: Option<&str>, want_dmabuf: bool) -> io::Result<Self> {
        let conn = Connection::connect_to_env()
            .map_err(|e| io::Error::other(format!("no WAYLAND_DISPLAY: {e}")))?;
        let (globals, mut queue) = registry_queue_init::<CapState>(&conn)
            .map_err(|e| io::Error::other(format!("registry: {e}")))?;
        let qh = queue.handle();

        let shm: wl_shm::WlShm = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| io::Error::other(format!("wl_shm: {e}")))?;
        let screencopy = globals
            .bind::<screencopy::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(
                &qh,
                1..=3,
                (),
            )
            .map_err(|e| io::Error::other(format!("wlr-screencopy: {e}")))?;
        let dmabuf = globals
            .bind::<linux_dmabuf::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, _, _>(
                &qh,
                1..=3,
                (),
            )
            .ok();
        let xdg_mgr = globals
            .bind::<xdg_output::client::zxdg_output_manager_v1::ZxdgOutputManagerV1, _, _>(
                &qh,
                1..=3,
                (),
            )
            .ok();

        let mut outputs = Vec::new();
        let mut names = Vec::new();
        let mut sizes = Vec::new();
        let mut refresh = Vec::new();

        for g in globals.contents().clone_list() {
            if g.interface != "wl_output" {
                continue;
            }
            let idx = outputs.len();
            let ver = g.version.min(4);
            let out: wl_output::WlOutput =
                globals.registry().bind(g.name, ver, &qh, OutputSlot { idx });
            if let Some(mgr) = &xdg_mgr {
                let _xdg = mgr.get_xdg_output(&out, &qh, OutputSlot { idx });
            }
            outputs.push(out);
            names.push(String::new());
            sizes.push((0, 0));
            refresh.push(0);
        }

        let mut state = CapState {
            shm,
            screencopy,
            dmabuf: dmabuf.clone(),
            use_dmabuf: dmabuf.is_some() && want_dmabuf,
            use_gbm: false,
            gbm_linear: false,
            outputs,
            names,
            sizes,
            refresh,
            refresh_hz: 0,
            chosen: None,
            shm_bufs: [None, None],
            dma_bufs: [None, None],
            gbm_bufs: [None, None],
            cur: 0,
            overlay_cursor: true,
            pending_copy: false,
            damage_only: false,
            logged_format: false,
            done: false,
            failed: false,
            info: None,
            staged: Vec::new(),
            in_flight: false,
        };

        queue.roundtrip(&mut state).ok();
        queue.roundtrip(&mut state).ok();

        let pick = if let Some(want) = output {
            state.names.iter().position(|n| n == want)
        } else {
            Some(0)
        };
        let idx = pick.ok_or_else(|| {
            io::Error::other(format!(
                "no output {}. Available: {:?}",
                output.unwrap_or(""),
                state.names
            ))
        })?;
        state.chosen = Some(state.outputs[idx].clone());
        state.refresh_hz = state.refresh[idx];
        let (w, h) = state.sizes[idx];
        eprintln!("lgx-server: output {} ({}x{})", state.names[idx], w, h);

        Ok(Self { conn, queue, state })
    }

    pub fn begin(&mut self) -> io::Result<()> {
        eprintln!(
            "lgx-server: capture path: {}",
            if self.state.use_gbm {
                if self.state.gbm_linear { "gbm linear" } else { "gbm dmabuf" }
            } else if self.state.use_dmabuf {
                "dmabuf"
            } else {
                "wl_shm"
            }
        );
        self.request()?;
        self.finish()
    }

    pub fn next_frame(&mut self) -> io::Result<()> {
        self.finish()?;
        self.request()?;
        Ok(())
    }

    fn request(&mut self) -> io::Result<()> {
        if self.state.in_flight {
            return Ok(());
        }
        // The pointer is part of the frame; the client hides its own, so there is
        // always exactly one cursor (set `cursor = local` in client.conf to draw
        // it client side instead).
        let frame = self.state.screencopy.capture_output(
            self.state.overlay_cursor as i32,
            self.state.chosen.as_ref().unwrap(),
            &self.queue.handle(),
            (),
        );
        let cur = self.state.cur;
        let buffer = if self.state.use_gbm {
            self.state.gbm_bufs[cur].as_ref().map(|(b, _)| b)
        } else if self.state.use_dmabuf {
            self.state.dma_bufs[cur].as_ref().map(|(b, _)| b)
        } else {
            self.state.shm_bufs[cur].as_ref().map(|(b, _)| b)
        };
        match buffer {
            Some(b) => {
                if self.state.damage_only {
                    frame.copy_with_damage(b)
                } else {
                    frame.copy(b)
                }
            }
            None => self.state.pending_copy = true,
        }
        self.state.in_flight = true;
        // Send the copy request now, not on the next blocking dispatch. That
        // lets the compositor blit frame N+1 while this process converts and
        // encodes frame N.
        let _ = self.conn.flush();
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        if !self.state.in_flight {
            return Ok(());
        }
        while !self.state.done {
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|e| io::Error::other(format!("dispatch: {e}")))?;
        }
        self.state.done = false;
        self.state.in_flight = false;
        if self.state.failed {
            self.state.failed = false;
            return Err(io::Error::other("frame was not copied"));
        }

        let info = self.state.info.as_ref().expect("frame info");
        let need = (info.stride * info.height) as usize;
        let done_slot = self.state.cur;
        let next_slot = 1 - done_slot;

        self.ensure_buffers(next_slot)?;
        self.state.cur = next_slot;
        self.request()?;

        if self.state.staged.len() != need {
            self.state.staged.resize(need, 0);
        }
        if self.state.use_gbm {
            if self.state.gbm_linear {
                // CPU-visible linear buffer: stage it like a shm frame
                if let Some((_, gbm)) = self.state.gbm_bufs[done_slot].as_ref() {
                    match gbm.map {
                        Some(map) if map.len() >= need => {
                            if self.state.staged.len() != need {
                                self.state.staged.resize(need, 0);
                            }
                            self.state.staged.copy_from_slice(&map[..need]);
                        }
                        _ => self.state.staged.clear(),
                    }
                }
            } else {
                // GPU-local buffer: nothing to stage, the encoder reads the dma-buf
                self.state.staged.clear();
            }
            return Ok(());
        }
        let src: &[u8] = if self.state.use_dmabuf {
            match self.state.dma_bufs[done_slot].as_ref() {
                Some((_, d)) => &d._dumb.map[..need],
                None => return Err(io::Error::other("dmabuf buffer missing")),
            }
        } else {
            match self.state.shm_bufs[done_slot].as_ref() {
                Some((_, s)) => &s.map[..need],
                None => return Err(io::Error::other("shm buffer missing")),
            }
        };
        self.state.staged.copy_from_slice(src);
        Ok(())
    }

    fn ensure_buffers(&mut self, slot: usize) -> io::Result<()> {
        let info = match self.state.info.as_ref() {
            Some(i) => i,
            None => return Ok(()),
        };
        let (w, h, stride, format) = (info.width, info.height, info.stride, info.format);
        if self.state.use_gbm {
            if self.state.gbm_bufs[slot].is_none() {
                let (buffer, gbm) =
                    create_gbm_buffer(self.state.dmabuf.as_ref().unwrap(), &self.queue.handle(), w, h, format, self.state.gbm_linear)?;
                if let Some(i) = self.state.info.as_mut() {
                    i.stride = gbm.stride;
                }
                self.state.gbm_bufs[slot] = Some((buffer, gbm));
            }
        } else if self.state.use_dmabuf {
            if self.state.dma_bufs[slot].is_none() {
                let (buffer, dumb) = create_dmabuf_buffer(
                    self.state.dmabuf.as_ref().unwrap(),
                    &self.queue.handle(),
                    w,
                    h,
                    format,
                )?;
                let pitch = dumb.pitch;
                self.state.dma_bufs[slot] = Some((buffer, DmaBuf { _dumb: dumb }));
                if let Some(i) = self.state.info.as_mut() {
                    i.stride = pitch;
                }
            }
        } else if self.state.shm_bufs[slot].is_none() {
            let (buffer, file, map) =
                create_shm_buffer(&self.state.shm, &self.queue.handle(), w, h, stride, format)?;
            self.state.shm_bufs[slot] = Some((buffer, ShmBuf { _file: file, map }));
        }
        Ok(())
    }

    pub fn refresh_hz(&self) -> u32 {
        self.state.refresh_hz
    }

    /// The GBM buffer holding the frame returned by the last `next_frame`.
    pub fn last_gbm(&self) -> Option<&GbmBuffer> {
        let done = 1 - self.state.cur;
        self.state.gbm_bufs[done].as_ref().map(|(_, g)| g)
    }


}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.conn.flush();
    }
}

fn create_shm_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<CapState>,
    w: u32,
    h: u32,
    stride: u32,
    format: u32,
) -> io::Result<(wl_buffer::WlBuffer, File, MmapMut)> {
    let size = (stride * h) as usize;
    let fd = unsafe { libc::memfd_create(c"lgx-frame".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(size as u64)?;
    let map = unsafe { MmapMut::map_mut(&file)? };

    let wl_format = if format == 0x3432_5258 {
        wl_shm::Format::Xrgb8888
    } else {
        wl_shm::Format::Argb8888
    };
    let pool: wl_shm_pool::WlShmPool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    let buffer =
        pool.create_buffer(0, w as i32, h as i32, stride as i32, wl_format, qh, ());
    pool.destroy();
    Ok((buffer, file, map))
}

fn create_gbm_buffer(
    mgr: &linux_dmabuf::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    qh: &QueueHandle<CapState>,
    w: u32,
    h: u32,
    format: u32,
    linear: bool,
) -> io::Result<(wl_buffer::WlBuffer, GbmBuffer)> {
    let gbm = if linear {
        GbmBuffer::linear(w, h, format)?
    } else {
        GbmBuffer::new(w, h, format)?
    };
    let params = mgr.create_params(qh, ());
    let fd = unsafe { BorrowedFd::borrow_raw(gbm.fd) };
    params.add(
        fd,
        0,
        0,
        gbm.stride,
        (gbm.modifier >> 32) as u32,
        (gbm.modifier & 0xffff_ffff) as u32,
    );
    let buffer = params.create_immed(w as i32, h as i32, format, 0, qh, ());
    params.destroy();
    Ok((buffer, gbm))
}

fn create_dmabuf_buffer(
    mgr: &linux_dmabuf::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    qh: &QueueHandle<CapState>,
    w: u32,
    h: u32,
    format: u32,
) -> io::Result<(wl_buffer::WlBuffer, DumbBuffer)> {
    let dumb = DumbBuffer::new(w, h)?;
    let params = mgr.create_params(qh, ());
    let fd = unsafe { BorrowedFd::borrow_raw(dumb.dmabuf_fd) };
    params.add(fd, 0, 0, dumb.pitch, 0, 0); // modifier = LINEAR (0)
    let buffer = params.create_immed(w as i32, h as i32, format, 0, qh, ());
    params.destroy();
    Ok((buffer, dumb))
}

// ---------- Dispatch ----------

#[derive(Clone, Copy)]
struct OutputSlot {
    idx: usize,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for CapState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: <wl_registry::WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! ignore {
    ($($iface:ty),* $(,)?) => {
        $(
            impl Dispatch<$iface, ()> for CapState {
                fn event(
                    _: &mut Self,
                    _: &$iface,
                    _: <$iface as Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }
        )*
    };
}

ignore!(
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    screencopy::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    xdg_output::client::zxdg_output_manager_v1::ZxdgOutputManagerV1,
    linux_dmabuf::client::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
);

impl Dispatch<linux_dmabuf::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for CapState {
    fn event(
        _: &mut Self,
        _: &linux_dmabuf::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        _: <linux_dmabuf::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // format/modifier advertisements are not needed: a linear modifier is
        // assumed and a rejection falls back to wl_shm
    }
}

impl Dispatch<wl_output::WlOutput, OutputSlot> for CapState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: <wl_output::WlOutput as Proxy>::Event,
        data: &OutputSlot,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode { width, height, refresh, .. } = event {
            if data.idx < state.sizes.len() {
                state.sizes[data.idx] = (width, height);
                if data.idx < state.refresh.len() {
                    state.refresh[data.idx] = ((refresh.max(0) as u32) + 500) / 1000;
                }
            }
        }
    }
}

impl Dispatch<xdg_output::client::zxdg_output_v1::ZxdgOutputV1, OutputSlot> for CapState {
    fn event(
        state: &mut Self,
        _: &xdg_output::client::zxdg_output_v1::ZxdgOutputV1,
        event: <xdg_output::client::zxdg_output_v1::ZxdgOutputV1 as Proxy>::Event,
        data: &OutputSlot,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_output::client::zxdg_output_v1::Event::Name { name } = event {
            if data.idx < state.names.len() {
                state.names[data.idx] = name;
            }
        }
    }
}

impl Dispatch<screencopy::client::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()>
    for CapState
{
    fn event(
        state: &mut Self,
        proxy: &screencopy::client::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: <screencopy::client::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use screencopy::client::zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer { format, width, height, stride } => {
                if state.use_dmabuf {
                    return;
                }
                let fmt = match format.into_result() {
                    Ok(wl_shm::Format::Xrgb8888) => 0x3432_5258,
                    _ => 0x3432_5241,
                };
                if !state.logged_format {
                    state.logged_format = true;
                    eprintln!(
                        "lgx-server: shm buffer {width}x{height} stride={stride} fmt={fmt:08x}"
                    );
                }
                state.info = Some(FrameInfo { width, height, stride, format: fmt });
                let cur = state.cur;
                if state.shm_bufs[cur].is_none() {
                    match create_shm_buffer(&state.shm, qh, width, height, stride, fmt) {
                        Ok((buffer, file, map)) => {
                            state.shm_bufs[cur] = Some((buffer, ShmBuf { _file: file, map }));
                        }
                        Err(e) => {
                            eprintln!("lgx-server: buffer: {e}");
                            state.failed = true;
                            state.done = true;
                            return;
                        }
                    }
                }
                if state.pending_copy {
                    state.pending_copy = false;
                    if let Some((b, _)) = state.shm_bufs[cur].as_ref() {
                        if state.damage_only {
                            proxy.copy_with_damage(b);
                        } else {
                            proxy.copy(b);
                        }
                    }
                }
            }
            Event::LinuxDmabuf { format, width, height } => {
                if !state.use_dmabuf {
                    return;
                }
                let cur = state.cur;
                let pitch = width * 4;
                if !state.logged_format {
                    state.logged_format = true;
                    eprintln!(
                        "lgx-server: dmabuf {width}x{height} fmt={format:08x}"
                    );
                }
                state.info = Some(FrameInfo {
                    width,
                    height,
                    stride: pitch,
                    format,
                });
                if state.use_gbm && state.gbm_bufs[cur].is_none() {
                    match create_gbm_buffer(state.dmabuf.as_ref().unwrap(), qh, width, height, format, state.gbm_linear) {
                        Ok((buffer, gbm)) => {
                            if state.gbm_linear && gbm.modifier != 0 {
                                eprintln!(
                                    "lgx-server: gbm buffer is tiled (modifier {:#x}), falling back to wl_shm",
                                    gbm.modifier
                                );
                                state.use_gbm = false;
                                state.use_dmabuf = false;
                                state.pending_copy = true;
                                return;
                            }
                            if let Some(i) = state.info.as_mut() {
                                i.stride = gbm.stride;
                            }
                            state.gbm_bufs[cur] = Some((buffer, gbm));
                        }
                        Err(e) => {
                            eprintln!("lgx-server: gbm: {e}");
                            state.use_gbm = false;
                            state.use_dmabuf = false;
                            state.pending_copy = true;
                            return;
                        }
                    }
                }
                if state.use_gbm {
                    if state.pending_copy {
                        state.pending_copy = false;
                        if let Some((b, _)) = state.gbm_bufs[cur].as_ref() {
                            proxy.copy(b);
                        }
                    }
                    return;
                }
                if state.dma_bufs[cur].is_none() {
                    match create_dmabuf_buffer(state.dmabuf.as_ref().unwrap(), qh, width, height, format)
                    {
                        Ok((buffer, dumb)) => {
                            let pitch = dumb.pitch;
                            state.dma_bufs[cur] = Some((buffer, DmaBuf { _dumb: dumb }));
                            if let Some(i) = state.info.as_mut() {
                                i.stride = pitch;
                            }
                        }
                        Err(e) => {
                            eprintln!("lgx-server: dmabuf: {e}");
                            state.use_dmabuf = false;
                            state.pending_copy = true;
                            return;
                        }
                    }
                }
                if state.pending_copy {
                    state.pending_copy = false;
                    if let Some((b, _)) = state.dma_bufs[cur].as_ref() {
                        if state.damage_only {
                            proxy.copy_with_damage(b);
                        } else {
                            proxy.copy(b);
                        }
                    }
                }
            }
            Event::Flags { .. } => {}
            Event::Ready { .. } => {
                state.done = true;
            }
            Event::Failed => {
                eprintln!("lgx-server: frame copy failed");
                state.failed = true;
                state.done = true;
            }
            _ => {}
        }
    }
}

/// Small pause used to cap the frame rate.
pub fn throttle(start: Instant, fps: u32) {
    if fps == 0 {
        return;
    }
    let target = Duration::from_secs_f64(1.0 / fps as f64);
    let elapsed = start.elapsed();
    if elapsed < target {
        std::thread::sleep(target - elapsed);
    }
}
