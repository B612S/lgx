//! CUDA/NVENC availability, probed through the driver's `libcuda.so.1`.
//!
//! The client only needs to know whether an NVIDIA decode device exists; the
//! FFmpeg shim reports the real hwaccel errors. The server uses the same probe
//! to decide whether starting `lgx-nvenc` is worth it before the helper has
//! printed its own capabilities.

use std::ffi::CString;
use std::sync::OnceLock;

const CUDA_SUCCESS: i32 = 0;

type CuInit = unsafe extern "C" fn(u32) -> i32;
type CuDeviceGetCount = unsafe extern "C" fn(*mut i32) -> i32;
type CuDeviceGetName = unsafe extern "C" fn(*mut libc::c_char, i32, i32) -> i32;

struct LibCuda {
    _handle: *mut libc::c_void,
    init: CuInit,
    count: CuDeviceGetCount,
    name: Option<CuDeviceGetName>,
}

unsafe impl Send for LibCuda {}
unsafe impl Sync for LibCuda {}

impl LibCuda {
    fn load() -> Option<Self> {
        unsafe {
            let handle = libc::dlopen(
                CString::new("libcuda.so.1").ok()?.as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            );
            if handle.is_null() {
                return None;
            }
            let init: CuInit = symbol(handle, "cuInit")?;
            let count: CuDeviceGetCount = symbol(handle, "cuDeviceGetCount")?;
            let name = symbol_opt(handle, "cuDeviceGetName");
            Some(Self { _handle: handle, init, count, name })
        }
    }
}

unsafe fn symbol<T: Copy>(handle: *mut libc::c_void, name: &str) -> Option<T> {
    let ptr = libc::dlsym(handle, CString::new(name).ok()?.as_ptr());
    if ptr.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&ptr))
    }
}

unsafe fn symbol_opt<T: Copy>(handle: *mut libc::c_void, name: &str) -> Option<T> {
    symbol(handle, name)
}

fn libcuda() -> Option<&'static LibCuda> {
    static LIB: OnceLock<Option<LibCuda>> = OnceLock::new();
    LIB.get_or_init(LibCuda::load).as_ref()
}

/// True when the CUDA driver initializes and reports at least one device.
pub fn available() -> bool {
    device_names().is_some_and(|d| !d.is_empty())
}

/// Names of the CUDA devices, or `None` when the runtime is unusable.
pub fn device_names() -> Option<Vec<String>> {
    let lib = libcuda()?;
    unsafe {
        if (lib.init)(0) != CUDA_SUCCESS {
            return None;
        }
        let mut count = 0i32;
        if (lib.count)(&mut count) != CUDA_SUCCESS || count <= 0 {
            return Some(Vec::new());
        }
        let mut names = Vec::new();
        for i in 0..count {
            let mut buf = [0i8; 256];
            match lib.name {
                Some(name) if (name)(buf.as_mut_ptr(), buf.len() as i32, i) == CUDA_SUCCESS => {
                    let s = std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
                    names.push(s);
                }
                _ => names.push(format!("CUDA device {i}")),
            }
        }
        Some(names)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn probe_does_not_panic() {
        let _ = super::available();
    }
}
