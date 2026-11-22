//! VAAPI capabilities, probed through `libva` loaded at runtime.
//!
//! libva is dlopen'ed on purpose: a client may be built and run on a machine
//! without any VAAPI driver and must still fall back to software decoding, and
//! the server should be able to report "VAAPI unavailable" instead of failing
//! to start. Nothing here links against libva at build time.

use std::ffi::{CStr, CString};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::drm::DrmNode;

// VAAPI constants from va/va.h. They are part of the ABI and never change.
const VA_PROFILE_HEVC_MAIN: i32 = 17;
const VA_PROFILE_HEVC_MAIN10: i32 = 18;
const VA_PROFILE_NONE: i32 = -1;
const VA_ENTRYPOINT_VLD: i32 = 1;
const VA_ENTRYPOINT_ENC_SLICE: i32 = 6;
const VA_ENTRYPOINT_ENC_SLICE_LP: i32 = 8;
const VA_ENTRYPOINT_VIDEO_PROC: i32 = 10;

const MAX_PROFILES: usize = 128;
const MAX_ENTRYPOINTS: usize = 64;

type VaDisplay = *mut libc::c_void;

type VaGetDisplayDrm = unsafe extern "C" fn(libc::c_int) -> VaDisplay;
type VaInitialize = unsafe extern "C" fn(VaDisplay, *mut libc::c_int, *mut libc::c_int) -> libc::c_int;
type VaTerminate = unsafe extern "C" fn(VaDisplay) -> libc::c_int;
type VaQueryVendor = unsafe extern "C" fn(VaDisplay) -> *const libc::c_char;
type VaQueryProfiles =
    unsafe extern "C" fn(VaDisplay, *mut libc::c_int, *mut libc::c_int) -> libc::c_int;
type VaQueryEntrypoints =
    unsafe extern "C" fn(VaDisplay, libc::c_int, *mut libc::c_int, *mut libc::c_int) -> libc::c_int;

struct LibVa {
    _va: *mut libc::c_void,
    _va_drm: *mut libc::c_void,
    get_display_drm: VaGetDisplayDrm,
    initialize: VaInitialize,
    terminate: VaTerminate,
    vendor: VaQueryVendor,
    profiles: VaQueryProfiles,
    entrypoints: VaQueryEntrypoints,
}

// The dlopen handles and function pointers stay valid for the process; the
// probes themselves are serialized by the caller.
unsafe impl Send for LibVa {}
unsafe impl Sync for LibVa {}

impl LibVa {
    fn load() -> Option<Self> {
        unsafe {
            let va = load_library("libva.so.2")?;
            let va_drm = load_library("libva-drm.so.2")?;
            Some(Self {
                get_display_drm: symbol(va_drm, "vaGetDisplayDRM")?,
                initialize: symbol(va, "vaInitialize")?,
                terminate: symbol(va, "vaTerminate")?,
                vendor: symbol(va, "vaQueryVendorString")?,
                profiles: symbol(va, "vaQueryConfigProfiles")?,
                entrypoints: symbol(va, "vaQueryConfigEntrypoints")?,
                _va: va,
                _va_drm: va_drm,
            })
        }
    }
}

unsafe fn load_library(name: &str) -> Option<*mut libc::c_void> {
    let cname = CString::new(name).ok()?;
    let handle = libc::dlopen(cname.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
    if handle.is_null() {
        None
    } else {
        Some(handle)
    }
}

unsafe fn symbol<T: Copy>(handle: *mut libc::c_void, name: &str) -> Option<T> {
    let cname = CString::new(name).ok()?;
    let ptr = libc::dlsym(handle, cname.as_ptr());
    if ptr.is_null() {
        return None;
    }
    debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut libc::c_void>());
    Some(std::mem::transmute_copy(&ptr))
}

fn libva() -> Option<&'static LibVa> {
    static LIB: OnceLock<Option<LibVa>> = OnceLock::new();
    LIB.get_or_init(LibVa::load).as_ref()
}

/// Capabilities of one VAAPI device (one render node).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaCaps {
    pub node: PathBuf,
    /// `vaQueryVendorString`: the VA driver, not the GPU vendor.
    pub vendor: String,
    /// HEVC Main (8-bit) decode in hardware.
    pub hevc_decode: bool,
    /// HEVC Main 10-bit decode in hardware.
    pub hevc_decode_10: bool,
    /// HEVC Main encode in hardware (regular slice or low-power VDENC).
    pub hevc_encode: bool,
    /// HEVC Main low-power encode entry point (Intel VDENC). Encoding also
    /// works without it; the flag only tells the helper which mode to request.
    pub hevc_encode_low_power: bool,
    /// A video-processing entry point exists somewhere in this driver.
    /// Whether a specific RGB format can be imported still has to be tried.
    pub vpp: bool,
}

impl VaCaps {
    pub fn is_intel(&self) -> bool {
        let v = self.vendor.to_ascii_lowercase();
        v.contains("intel") || v.contains("i965") || v.contains("ihd")
    }

    pub fn is_mesa(&self) -> bool {
        self.vendor.to_ascii_lowercase().contains("mesa") || self.is_amd()
    }

    pub fn is_amd(&self) -> bool {
        let v = self.vendor.to_ascii_lowercase();
        v.contains("radeon") || v.contains("amd") || v.contains("mesa")
    }

    pub fn is_nvidia(&self) -> bool {
        // libva-nvidia-driver reports "VA-API NVDEC driver", not "NVIDIA".
        let v = self.vendor.to_ascii_lowercase();
        v.contains("nvidia") || v.contains("nvdec")
    }
}

/// Probes `node` for VAAPI. Returns `None` when libva is missing, the node is
/// not a DRM render node, or the driver refuses to initialize.
pub fn probe(node: &DrmNode) -> Option<VaCaps> {
    let lib = libva()?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(&node.path)
        .ok()?;
    unsafe {
        let dpy = (lib.get_display_drm)(file.as_raw_fd());
        if dpy.is_null() {
            return None;
        }
        let mut major = 0;
        let mut minor = 0;
        if (lib.initialize)(dpy, &mut major, &mut minor) != 0 {
            return None;
        }
        let caps = read_caps(lib, dpy, &node.path);
        (lib.terminate)(dpy);
        caps
    }
}

unsafe fn read_caps(lib: &LibVa, dpy: VaDisplay, path: &std::path::Path) -> Option<VaCaps> {
    let vendor = {
        let ptr = (lib.vendor)(dpy);
        if ptr.is_null() {
            String::new()
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };
    let mut profiles = [0i32; MAX_PROFILES];
    let mut num = 0i32;
    if (lib.profiles)(dpy, profiles.as_mut_ptr(), &mut num) != 0 {
        return None;
    }
    let num = (num as usize).min(MAX_PROFILES);
    let mut caps = VaCaps {
        node: path.to_path_buf(),
        vendor,
        hevc_decode: false,
        hevc_decode_10: false,
        hevc_encode: false,
        hevc_encode_low_power: false,
        vpp: false,
    };
    for &profile in &profiles[..num] {
        let entrypoints = entrypoints_for(lib, dpy, profile);
        let has = |e: i32| entrypoints.contains(&e);
        match profile {
            VA_PROFILE_HEVC_MAIN => {
                caps.hevc_decode |= has(VA_ENTRYPOINT_VLD);
                caps.hevc_encode |= has(VA_ENTRYPOINT_ENC_SLICE);
                caps.hevc_encode_low_power |= has(VA_ENTRYPOINT_ENC_SLICE_LP);
            }
            VA_PROFILE_HEVC_MAIN10 => {
                caps.hevc_decode_10 |= has(VA_ENTRYPOINT_VLD);
            }
            _ => {}
        }
        caps.vpp |= has(VA_ENTRYPOINT_VIDEO_PROC);
    }
    // VPP is usually advertised under `VAProfileNone`, which some drivers do
    // not list in `vaQueryConfigProfiles`. Query it explicitly.
    if !caps.vpp {
        caps.vpp = entrypoints_for(lib, dpy, VA_PROFILE_NONE).contains(&VA_ENTRYPOINT_VIDEO_PROC);
    }
    Some(caps)
}

unsafe fn entrypoints_for(lib: &LibVa, dpy: VaDisplay, profile: i32) -> Vec<i32> {
    let mut eps = [0i32; MAX_ENTRYPOINTS];
    let mut num = 0i32;
    if (lib.entrypoints)(dpy, profile, eps.as_mut_ptr(), &mut num) != 0 {
        return Vec::new();
    }
    let n = (num as usize).min(MAX_ENTRYPOINTS);
    eps[..n].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_predicates() {
        let mk = |v: &str| VaCaps {
            node: PathBuf::from("/dev/dri/renderD128"),
            vendor: v.to_string(),
            hevc_decode: true,
            hevc_decode_10: false,
            hevc_encode: true,
            hevc_encode_low_power: true,
            vpp: true,
        };
        assert!(mk("Intel iHD driver for Intel(R) Gen Graphics - 24.3.4").is_intel());
        assert!(mk("Mesa Gallium driver 24.2.0 for AMD Radeon RX 7800 XT").is_amd());
        assert!(mk("VA-API NVDEC driver [direct backend]").is_nvidia());
    }
}
