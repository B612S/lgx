//! One encoder interface for the two hardware backends.
//!
//! The stream code should not care whether packets come from VAAPI (which
//! owns its surfaces and can VPP-import a capture dma-buf) or NVENC (which
//! takes CPU NV12). `Encoder` keeps that split explicit: `vpp_send` and
//! `encode` exist only on VAAPI and return a clear error otherwise.

use std::io;

use crate::nvenc::NvEnc;
use crate::vaenc::VaEnc;

pub enum Encoder {
    Vaapi(VaEnc),
    Nvenc(NvEnc),
}

impl Encoder {
    pub fn name(&self) -> &'static str {
        match self {
            Encoder::Vaapi(_) => "VAAPI",
            Encoder::Nvenc(_) => "NVENC",
        }
    }

    pub fn force_keyframe(&mut self) -> io::Result<()> {
        match self {
            Encoder::Vaapi(e) => e.force_keyframe(),
            Encoder::Nvenc(e) => e.force_keyframe(),
        }
    }

    /// CPU NV12 upload. Works on both backends and is the NVIDIA path.
    pub fn send_cpu(&mut self, data: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Encoder::Vaapi(e) => e.send_cpu(data),
            Encoder::Nvenc(e) => e.send_cpu(data),
        }
    }

    /// VPP RGB -> NV12 import, VAAPI only.
    #[allow(clippy::too_many_arguments)]
    pub fn vpp_send(
        &mut self,
        id: u32,
        width: u32,
        height: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
        offset: u32,
        fd: std::os::fd::RawFd,
    ) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Encoder::Vaapi(e) => e.vpp_send(id, width, height, stride, fourcc, modifier, offset, fd),
            Encoder::Nvenc(_) => Err(io::Error::other("NVENC has no VPP import")),
        }
    }

    /// Encode the surface the caller rendered into, VAAPI only.
    pub fn encode(&mut self, idx: u32) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Encoder::Vaapi(e) => e.encode(idx),
            Encoder::Nvenc(_) => Err(io::Error::other("NVENC has no imported surfaces")),
        }
    }
}
