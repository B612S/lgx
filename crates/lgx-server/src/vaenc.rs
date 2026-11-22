//! Client side of the `lgx-vaenc` helper: it owns the VAAPI surfaces and the
//! HEVC encoder, we render into its dma-bufs with GL (zero copy).

// Unsafe FFI wrappers: every `unsafe fn` here forwards to the driver and its
// contract is described at the call site, so the per-function `# Safety`
// section and the argument-count heuristic are skipped deliberately.
#![allow(clippy::missing_safety_doc, clippy::too_many_arguments)]


use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};

#[allow(dead_code)]
pub struct VaFrame {
    pub fds: [RawFd; 2],
    pub offsets: [u32; 2],
    pub pitches: [u32; 2],
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
}

impl Drop for VaFrame {
    fn drop(&mut self) {
        for fd in self.fds {
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
        }
    }
}

pub struct VaEnc {
    stream: UnixStream,
    #[allow(dead_code)]
    pub frames: Vec<VaFrame>,
    _child: Child,
}

fn rx(stream: &mut UnixStream, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

fn recv_with_fds(stream: &UnixStream, len: usize) -> io::Result<(Vec<u8>, Vec<RawFd>)> {
    let mut data = vec![0u8; len];
    let mut iov = libc::iovec { iov_base: data.as_mut_ptr() as *mut libc::c_void, iov_len: len };
    let mut ctrl = [0u8; 128];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = ctrl.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = ctrl.len();
    let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*cmsg).cmsg_len - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                let ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
                for i in 0..count {
                    fds.push(*ptr.add(i));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    data.truncate(n as usize);
    Ok((data, fds))
}

use std::os::fd::AsRawFd;

impl VaEnc {
    pub fn start_cpu(
        socket: &str,
        helper: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        low_power: bool,
        async_depth: u32,
    ) -> io::Result<Self> {
        Self::start_with(
            socket, helper, false, false, width, height, fps, bitrate, low_power, async_depth,
        )
    }

    pub fn start_vpp(
        socket: &str,
        helper: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        low_power: bool,
        async_depth: u32,
    ) -> io::Result<Self> {
        Self::start_with(
            socket, helper, false, true, width, height, fps, bitrate, low_power, async_depth,
        )
    }

    pub fn start_import(
        socket: &str,
        helper: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        low_power: bool,
        async_depth: u32,
    ) -> io::Result<Self> {
        Self::start_with(
            socket,
            helper,
            true,
            false,
            width,
            height,
            fps,
            bitrate,
            low_power,
            async_depth,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with(
        socket: &str,
        helper: &str,
        import: bool,
        vpp: bool,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        low_power: bool,
        async_depth: u32,
    ) -> io::Result<Self> {
        let _ = std::fs::remove_file(socket);
        let mut cmd = Command::new(helper);
        cmd.arg(socket)
            .arg(width.to_string())
            .arg(height.to_string())
            .arg(fps.to_string())
            .arg(bitrate.to_string());
        if import {
            cmd.arg("import");
        }
        cmd.env("LGX_LOW_POWER", if low_power { "1" } else { "0" });
        // The helper must open the same render node as capture/GBM.
        cmd.env("LGX_DRM_NODE", lgx_server::gbm::drm_node());
        cmd.env("LGX_ASYNC_DEPTH", async_depth.to_string());
        if vpp {
            cmd.env("LGX_VPP", "1");
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut stream = None;
        for _ in 0..50 {
            match UnixStream::connect(socket) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }
        let mut stream = stream.ok_or_else(|| io::Error::other("vaenc helper did not start"))?;

        let n = u32::from_le_bytes(rx(&mut stream, 4)?.try_into().unwrap()) as usize;
        let mut frames = Vec::with_capacity(n);
        for _ in 0..n {
            let modifier = u64::from_le_bytes(rx(&mut stream, 8)?.try_into().unwrap());
            let hdr = rx(&mut stream, 16)?;
            let width = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
            let height = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
            let fourcc = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
            let layers = u32::from_le_bytes(hdr[12..16].try_into().unwrap());
            let (meta, fds) = recv_with_fds(&stream, 16)?;
            let planes = u32::from_le_bytes(rx(&mut stream, 4)?.try_into().unwrap()) as usize;
            if layers < 2 || planes < 2 || fds.len() < 2 {
                return Err(io::Error::other("vaenc: unexpected surface layout"));
            }
            frames.push(VaFrame {
                fds: [fds[0], fds[1]],
                offsets: [
                    u32::from_le_bytes(meta[0..4].try_into().unwrap()),
                    u32::from_le_bytes(meta[4..8].try_into().unwrap()),
                ],
                pitches: [
                    u32::from_le_bytes(meta[8..12].try_into().unwrap()),
                    u32::from_le_bytes(meta[12..16].try_into().unwrap()),
                ],
                modifier,
                width,
                height,
                fourcc,
            });
        }
        Ok(Self { stream, frames, _child: child })
    }

    /// Imports an external NV12 dma-buf as the encoder surface.
    pub fn import_surface(&mut self, fd: RawFd, meta: [u32; 4]) -> io::Result<bool> {
        self.stream.write_all(b"I")?;
        let mut payload = [0u8; 16];
        for (i, v) in meta.iter().enumerate() {
            payload[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        let mut iov = libc::iovec { iov_base: payload.as_mut_ptr() as *mut libc::c_void, iov_len: payload.len() };
        let mut ctrl = [0u8; 64];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = ctrl.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = ctrl.len();
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(4) as usize;
            std::ptr::copy_nonoverlapping(&fd as *const RawFd as *const u8, libc::CMSG_DATA(c), 4);
            msg.msg_controllen = libc::CMSG_SPACE(4) as usize;
            if libc::sendmsg(self.stream.as_raw_fd(), &msg, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let ok = u32::from_le_bytes(rx(&mut self.stream, 4)?.try_into().unwrap());
        Ok(ok == 1)
    }

    /// Converts one tiled/linear RGB dma-buf with VPP into the encoder's NV12
    /// surface, then encodes it. Only metadata and the fd cross the socket.
    pub fn vpp_send(
        &mut self,
        id: u32,
        width: u32,
        height: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
        offset: u32,
        fd: RawFd,
    ) -> io::Result<Vec<Vec<u8>>> {
        self.stream.write_all(b"R")?;
        let meta: [u32; 8] = [
            width,
            height,
            stride,
            fourcc,
            id,
            modifier as u32,
            (modifier >> 32) as u32,
            offset,
        ];
        let mut iov = libc::iovec {
            iov_base: meta.as_ptr() as *mut libc::c_void,
            iov_len: std::mem::size_of_val(&meta),
        };
        let mut ctrl = [0u8; 64];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = ctrl.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = ctrl.len();
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(4) as usize;
            std::ptr::copy_nonoverlapping(&fd as *const RawFd as *const u8, libc::CMSG_DATA(c), 4);
            msg.msg_controllen = libc::CMSG_SPACE(4) as usize;
            if libc::sendmsg(self.stream.as_raw_fd(), &msg, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let count = u32::from_le_bytes(rx(&mut self.stream, 4)?.try_into().unwrap()) as usize;
        if count > 16 {
            return Err(io::Error::other("encoder returned too many packets"));
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let len = u32::from_le_bytes(rx(&mut self.stream, 4)?.try_into().unwrap()) as usize;
            out.push(rx(&mut self.stream, len)?);
        }
        Ok(out)
    }

    /// Uploads one CPU NV12 frame into the helper's VAAPI surface and encodes
    /// it. The helper returns the same packet list format as `encode`.
    pub fn send_cpu(&mut self, data: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        self.stream.write_all(b"F")?;
        self.stream.write_all(&0u32.to_le_bytes())?;
        self.stream.write_all(&(data.len() as u32).to_le_bytes())?;
        self.stream.write_all(data)?;
        let count = u32::from_le_bytes(rx(&mut self.stream, 4)?.try_into().unwrap()) as usize;
        if count > 16 {
            return Err(io::Error::other("encoder returned too many packets"));
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let len = u32::from_le_bytes(rx(&mut self.stream, 4)?.try_into().unwrap()) as usize;
            out.push(rx(&mut self.stream, len)?);
        }
        Ok(out)
    }

    /// Asks the helper to make the next encoded frame an IDR, so a client that
    /// just joined can start decoding immediately instead of waiting for the
    /// next keyframe interval.
    pub fn force_keyframe(&mut self) -> io::Result<()> {
        self.stream.write_all(b"K")
    }

    /// Encodes the surface rendered by the caller. The helper may return
    /// several access units for one frame; each stays a separate packet.
    pub fn encode(&mut self, idx: u32) -> io::Result<Vec<Vec<u8>>> {
        self.stream.write_all(b"E")?;
        self.stream.write_all(&idx.to_le_bytes())?;
        let count = u32::from_le_bytes(rx(&mut self.stream, 4)?.try_into().unwrap()) as usize;
        if count > 16 {
            return Err(io::Error::other("encoder returned too many packets"));
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let len = u32::from_le_bytes(rx(&mut self.stream, 4)?.try_into().unwrap()) as usize;
            out.push(rx(&mut self.stream, len)?);
        }
        Ok(out)
    }
}

impl Drop for VaEnc {
    fn drop(&mut self) {
        let _ = self.stream.write_all(b"Q");
    }
}
