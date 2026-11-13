//! Linear dma-buf backed by a DRM dumb buffer.
//!
//! The compositor blits the screen into this buffer on the GPU instead of
//! doing a synchronous readback into a `wl_shm` mapping, which is what makes
//! 160 fps capture possible on the passed-through iGPU. The buffer lives in
//! system memory, so the CPU can read it directly.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};

use memmap2::MmapMut;

const DRM_IOCTL_MODE_CREATE_DUMB: libc::c_ulong = 0xc020_64b2;
const DRM_IOCTL_MODE_MAP_DUMB: libc::c_ulong = 0xc010_64b3;
const DRM_IOCTL_MODE_DESTROY_DUMB: libc::c_ulong = 0xc004_64b4;
const DRM_IOCTL_PRIME_HANDLE_TO_FD: libc::c_ulong = 0xc00c_642d;

#[repr(C)]
#[derive(Default)]
struct CreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct MapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Default)]
struct PrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

#[allow(dead_code)]
pub struct DumbBuffer {
    card: File,
    handle: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub size: usize,
    pub dmabuf_fd: RawFd,
    pub map: MmapMut,
}

impl DumbBuffer {
    pub fn new(width: u32, height: u32) -> io::Result<Self> {
        let card = File::options()
            .read(true)
            .write(true)
            .open("/dev/dri/card0")
            .map_err(|e| io::Error::other(format!("/dev/dri/card0: {e}")))?;
        let fd = card.as_raw_fd();

        let mut create = CreateDumb {
            bpp: 32,
            ..Default::default()
        };
        if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut create) } < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut map_dumb = MapDumb { handle: create.handle, ..Default::default() };
        if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut map_dumb) } < 0 {
            destroy(fd, create.handle);
            return Err(io::Error::last_os_error());
        }

        let size = create.size as usize;
        let map = unsafe {
            memmap2::MmapOptions::new()
                .offset(map_dumb.offset)
                .len(size)
                .map_mut(&card)?
        };

        let mut prime = PrimeHandle {
            handle: create.handle,
            flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
            fd: -1,
        };
        if unsafe { libc::ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut prime) } < 0 {
            destroy(fd, create.handle);
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            card,
            handle: create.handle,
            width,
            height,
            pitch: create.pitch,
            size,
            dmabuf_fd: prime.fd,
            map,
        })
    }
}

impl Drop for DumbBuffer {
    fn drop(&mut self) {
        if self.dmabuf_fd >= 0 {
            unsafe { libc::close(self.dmabuf_fd) };
        }
        destroy(self.card.as_raw_fd(), self.handle);
    }
}

fn destroy(fd: RawFd, handle: u32) {
    let mut h = handle;
    unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut h) };
}
