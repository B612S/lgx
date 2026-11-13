//! GBM buffers for zero-copy capture: the compositor blits into a GPU-local
//! (tiled) dma-buf, which can then be fed to VAAPI without touching the CPU.

use std::ffi::c_void;
use std::io;
use std::os::fd::RawFd;

const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const GBM_BO_USE_LINEAR: u32 = 1 << 4;

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

pub struct GbmBuffer {
    dev: *mut c_void,
    bo: *mut c_void,
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
        .open("/dev/dri/renderD128")
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
        .open("/dev/dri/renderD128")
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

    fn with_flags(width: u32, height: u32, format: u32, flags: u32) -> io::Result<Self> {
        let node = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/dri/renderD128")
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
        Ok(Self { dev, bo, fd: dmabuf_fd, stride, modifier, width, height, map })
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
        .open("/dev/dri/renderD128")
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
