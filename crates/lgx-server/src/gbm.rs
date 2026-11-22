//! GBM buffers for zero-copy capture: the compositor blits into a GPU-local
//! (tiled) dma-buf, which can then be fed to VAAPI without touching the CPU.

use std::ffi::c_void;
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const GBM_BO_USE_LINEAR: u32 = 1 << 4;

/// i915 Y-tiled. Both the compositor's screencopy blit and iHD's VPP import
/// accept it, and it is the tiling GBM picked implicitly before mesa 26.3.
pub const I915_FORMAT_MOD_Y_TILED: u64 = 0x0100_0000_0000_0002;

#[link(name = "gbm")]
extern "C" {
    fn gbm_create_device(fd: i32) -> *mut c_void;
    fn gbm_device_destroy(dev: *mut c_void);
    fn gbm_bo_create(dev: *mut c_void, width: u32, height: u32, format: u32, flags: u32)
        -> *mut c_void;
    fn gbm_bo_create_with_modifiers(
        dev: *mut c_void,
        width: u32,
        height: u32,
        format: u32,
        modifiers: *const u64,
        count: u32,
    ) -> *mut c_void;
    fn gbm_bo_destroy(bo: *mut c_void);
    fn gbm_bo_get_fd(bo: *mut c_void) -> i32;
    fn gbm_bo_get_stride(bo: *mut c_void) -> u32;
    fn gbm_bo_get_modifier(bo: *mut c_void) -> u64;
    fn gbm_bo_get_plane_count(bo: *mut c_void) -> i32;
    fn gbm_bo_get_offset(bo: *mut c_void, plane: i32) -> u32;
    fn gbm_bo_get_stride_for_plane(bo: *mut c_void, plane: i32) -> u32;
    fn gbm_bo_get_fd_for_plane(bo: *mut c_void, plane: i32) -> i32;
}

/// Unique per allocated buffer. The VPP helper caches imported surfaces by
/// this id; an fd number would be reused by the kernel and could alias.
static NEXT_BUFFER_ID: AtomicU32 = AtomicU32::new(1);

fn next_buffer_id() -> u32 {
    let id = NEXT_BUFFER_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 { 1 } else { id }
}

/// Resolved once at startup from `--drm-node`, `video.drm_node`, the
/// `LGX_DRM_NODE` environment or auto-detection.
static DRM_NODE: OnceLock<String> = OnceLock::new();

/// Pin the render node for this process (and the encoder helper, which
/// inherits it). Called once at startup.
pub fn set_drm_node(node: &str) {
    let _ = DRM_NODE.set(node.to_string());
}

/// Path of the DRM render node to use. An explicit choice (CLI, config or
/// `LGX_DRM_NODE`) wins; otherwise the first Intel render node is preferred,
/// then the first node that exists.
pub fn drm_node() -> String {
    if let Some(node) = DRM_NODE.get() {
        return node.clone();
    }
    if let Ok(node) = std::env::var("LGX_DRM_NODE") {
        if !node.is_empty() {
            return node;
        }
    }
    detect_render_node().unwrap_or_else(|| "/dev/dri/renderD128".to_string())
}

/// Enumerate `/sys/class/drm/renderD*` and prefer the Intel GPU. The VAAPI
/// helper and GBM must agree on the device; a laptop with the dGPU as
/// renderD128 must not silently disable VPP.
pub fn detect_render_node() -> Option<String> {
    let dir = std::fs::read_dir("/sys/class/drm").ok()?;
    let mut nodes: Vec<(String, String)> = Vec::new();
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("renderD") {
            continue;
        }
        let vendor = std::fs::read_to_string(entry.path().join("device/vendor"))
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        nodes.push((name, vendor));
    }
    nodes.sort_by(|a, b| a.0.cmp(&b.0));
    nodes
        .iter()
        .find(|(_, vendor)| vendor == "0x8086")
        .map(|(name, _)| format!("/dev/dri/{name}"))
        .or_else(|| nodes.first().map(|(name, _)| format!("/dev/dri/{name}")))
}

/// PCI vendor of the GPU behind `node`, as sysfs reports it (`0x8086` Intel).
pub fn node_vendor(node: &str) -> String {
    let Some(name) = std::path::Path::new(node).file_name() else {
        return String::new();
    };
    std::fs::read_to_string(
        std::path::Path::new("/sys/class/drm").join(name).join("device/vendor"),
    )
    .unwrap_or_default()
    .trim()
    .to_ascii_lowercase()
}

pub struct GbmBuffer {
    dev: *mut c_void,
    bo: *mut c_void,
    /// Unique allocation id, used as the VPP helper cache key.
    pub id: u32,
    pub fd: RawFd,
    pub stride: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    /// CPU mapping, only for linear buffers.
    pub map: Option<&'static [u8]>,
}

pub fn create_raw_mod(
    width: u32,
    height: u32,
    format: u32,
    modifiers: &[u64],
) -> io::Result<(*mut c_void, *mut c_void)> {
    let node = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(drm_node())
        .map_err(|e| io::Error::other(format!("drm node: {e}")))?;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&node);
    let dev = unsafe { gbm_create_device(fd) };
    std::mem::forget(node);
    if dev.is_null() {
        return Err(io::Error::other("gbm_create_device failed"));
    }
    let bo = unsafe {
        gbm_bo_create_with_modifiers(dev, width, height, format, modifiers.as_ptr(), modifiers.len() as u32)
    };
    if bo.is_null() {
        unsafe { gbm_device_destroy(dev) };
        return Err(io::Error::other("gbm_bo_create_with_modifiers failed"));
    }
    Ok((dev, bo))
}

pub fn create_raw(width: u32, height: u32, format: u32, flags: u32) -> io::Result<(*mut c_void, *mut c_void)> {
    let node = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(drm_node())
        .map_err(|e| io::Error::other(format!("drm node: {e}")))?;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&node);
    let dev = unsafe { gbm_create_device(fd) };
    if dev.is_null() {
        return Err(io::Error::other("gbm_create_device failed"));
    }
    let bo = unsafe { gbm_bo_create(dev, width, height, format, flags) };
    std::mem::forget(node);
    if bo.is_null() {
        unsafe { gbm_device_destroy(dev) };
        return Err(io::Error::other("gbm_bo_create failed"));
    }
    Ok((dev, bo))
}

/// # Safety
/// `bo` must be a valid GBM buffer object.
pub unsafe fn plane_count(bo: *mut c_void) -> i32 {
    unsafe { gbm_bo_get_plane_count(bo) }
}

/// # Safety
/// `bo` must be a valid GBM buffer object.
pub unsafe fn plane_fd(bo: *mut c_void, plane: i32) -> i32 {
    unsafe { gbm_bo_get_fd_for_plane(bo, plane) }
}

/// # Safety
/// `bo` must be a valid GBM buffer object.
pub unsafe fn plane_offset(bo: *mut c_void, plane: i32) -> u32 {
    unsafe { gbm_bo_get_offset(bo, plane) }
}

/// # Safety
/// `bo` must be a valid GBM buffer object.
pub unsafe fn plane_stride(bo: *mut c_void, plane: i32) -> u32 {
    unsafe { gbm_bo_get_stride_for_plane(bo, plane) }
}

impl GbmBuffer {
    /// Raw gbm_device, needed to create an EGL display.
    pub fn device(&self) -> *mut c_void {
        self.dev
    }

    pub fn new(width: u32, height: u32, format: u32) -> io::Result<Self> {
        Self::with_flags(width, height, format, GBM_BO_USE_RENDERING)
    }

    pub fn linear(width: u32, height: u32, format: u32) -> io::Result<Self> {
        Self::with_flags(width, height, format, GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR)
    }

    /// Allocate with an explicit DRM modifier and no CPU mapping. Used for
    /// capture buffers: the implicit modifier from `gbm_bo_create` cannot be
    /// trusted (mesa 26.3 i915 can return one that VAAPI and EGL reject).
    pub fn with_modifier(width: u32, height: u32, format: u32, modifier: u64) -> io::Result<Self> {
        let (dev, bo) = create_raw_mod(width, height, format, &[modifier])?;
        let dmabuf_fd = unsafe { gbm_bo_get_fd(bo) };
        if dmabuf_fd < 0 {
            unsafe {
                gbm_bo_destroy(bo);
                gbm_device_destroy(dev);
            }
            return Err(io::Error::other("gbm_bo_get_fd failed"));
        }
        let stride = unsafe { gbm_bo_get_stride(bo) };
        let modifier = unsafe { gbm_bo_get_modifier(bo) };
        Ok(Self {
            dev,
            bo,
            id: next_buffer_id(),
            fd: dmabuf_fd,
            stride,
            modifier,
            width,
            height,
            map: None,
        })
    }

    fn with_flags(width: u32, height: u32, format: u32, flags: u32) -> io::Result<Self> {
        let node = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(drm_node())
            .or_else(|_| std::fs::OpenOptions::new().read(true).write(true).open("/dev/dri/card0"))
            .map_err(|e| io::Error::other(format!("drm node: {e}")))?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&node);
        let dev = unsafe { gbm_create_device(fd) };
        if dev.is_null() {
            return Err(io::Error::other("gbm_create_device failed"));
        }
        let bo = unsafe { gbm_bo_create(dev, width, height, format, flags) };
        if bo.is_null() {
            unsafe { gbm_device_destroy(dev) };
            return Err(io::Error::other("gbm_bo_create failed"));
        }
        let dmabuf_fd = unsafe { gbm_bo_get_fd(bo) };
        if dmabuf_fd < 0 {
            unsafe {
                gbm_bo_destroy(bo);
                gbm_device_destroy(dev);
            }
            return Err(io::Error::other("gbm_bo_get_fd failed"));
        }
        let stride = unsafe { gbm_bo_get_stride(bo) };
        let modifier = unsafe { gbm_bo_get_modifier(bo) };
        let map = if flags & GBM_BO_USE_LINEAR != 0 {
            let size = (stride as usize) * (height as usize);
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    dmabuf_fd,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                None
            } else {
                Some(unsafe { std::slice::from_raw_parts(ptr as *const u8, size) })
            }
        } else {
            None
        };
        std::mem::forget(node);
        Ok(Self {
            dev,
            bo,
            id: next_buffer_id(),
            fd: dmabuf_fd,
            stride,
            modifier,
            width,
            height,
            map,
        })
    }
}

impl Drop for GbmBuffer {
    fn drop(&mut self) {
        if let Some(map) = self.map {
            unsafe {
                libc::munmap(map.as_ptr() as *mut c_void, map.len());
            }
        }
        unsafe {
            libc::close(self.fd);
            gbm_bo_destroy(self.bo);
            gbm_device_destroy(self.dev);
        }
    }
}

/// Allocates a linear, CPU-invisible buffer big enough for NV12 and returns
/// (gbm_device, fd, stride, size). Used as the shared YUV target for GL+VAAPI.
pub fn alloc_linear(width: u32, height: u32) -> io::Result<(usize, RawFd, u32, usize)> {
    alloc_linear_fmt(width, height, 0x3432_5258)
}

/// Allocates a linear buffer with an explicit DRM format (e.g. R8, GR88).
pub fn alloc_linear_fmt(width: u32, height: u32, format: u32) -> io::Result<(usize, RawFd, u32, usize)> {
    let node = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(drm_node())
        .map_err(|e| io::Error::other(format!("drm node: {e}")))?;
    let dev = unsafe { gbm_create_device(std::os::fd::AsRawFd::as_raw_fd(&node)) };
    if dev.is_null() {
        return Err(io::Error::other("gbm_create_device failed"));
    }
    let bo = unsafe { gbm_bo_create(dev, width, height, format, GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING) };
    let mut node = node;
    let _ = &mut node;
    if bo.is_null() {
        // retry with an explicit linear modifier list
        let bo2 = unsafe { gbm_bo_create_with_modifiers(dev, width, height, format, [0u64].as_ptr(), 1) };
        if bo2.is_null() {
            unsafe { gbm_device_destroy(dev) };
            return Err(io::Error::other("gbm_bo_create failed"));
        }
        let stride = unsafe { gbm_bo_get_stride(bo2) };
        let fd = unsafe { gbm_bo_get_fd(bo2) };
        let size = (stride as usize) * (height as usize);
        std::mem::forget(node);
        return Ok((dev as usize, fd, stride, size));
    }
    let stride = unsafe { gbm_bo_get_stride(bo) };
    let fd = unsafe { gbm_bo_get_fd(bo) };
    let size = (stride as usize) * (height as usize);
    // leak dev/bo on purpose: they live for the whole process
    std::mem::forget(node);
    Ok((dev as usize, fd, stride, size))
}
