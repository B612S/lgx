//! Client side of the `lgx-nvenc` helper.
//!
//! NVENC has no surface hand-out: the helper takes a complete NV12 frame in
//! system memory and uploads it to the GPU itself. That is one PCIe copy more
//! than VAAPI's dma-buf import, but it needs neither GBM nor an EGL device,
//! which is exactly what makes the NVIDIA path possible on a compositor that
//! hands us tiled buffers.
//!
//! The wire format matches `vaenc`: a frame is `'F' + u32 index + u32 length +
//! bytes`, and the answer is `u32 count` followed by `count` packets.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};

pub struct NvEnc {
    stream: UnixStream,
    _child: Child,
}

fn rx(stream: &mut UnixStream, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_packets(stream: &mut UnixStream) -> io::Result<Vec<Vec<u8>>> {
    let count = u32::from_le_bytes(rx(stream, 4)?.try_into().unwrap()) as usize;
    if count > 16 {
        return Err(io::Error::other("encoder returned too many packets"));
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let len = u32::from_le_bytes(rx(stream, 4)?.try_into().unwrap()) as usize;
        out.push(rx(stream, len)?);
    }
    Ok(out)
}

impl NvEnc {
    pub fn start(
        socket: &str,
        helper: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        async_depth: u32,
    ) -> io::Result<Self> {
        let _ = std::fs::remove_file(socket);
        let child = Command::new(helper)
            .arg(socket)
            .arg(width.to_string())
            .arg(height.to_string())
            .arg(fps.to_string())
            .arg(bitrate.to_string())
            .env("LGX_ASYNC_DEPTH", async_depth.to_string())
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
        let mut stream = stream.ok_or_else(|| io::Error::other("nvenc helper did not start"))?;
        // NVENC hands out no surfaces; the helper still sends the frame count
        // so both helpers share one handshake.
        let n = u32::from_le_bytes(rx(&mut stream, 4)?.try_into().unwrap());
        if n != 0 {
            return Err(io::Error::other(format!("nvenc: unexpected surface count {n}")));
        }
        Ok(Self { stream, _child: child })
    }

    pub fn send_cpu(&mut self, data: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        self.stream.write_all(b"F")?;
        self.stream.write_all(&0u32.to_le_bytes())?;
        self.stream.write_all(&(data.len() as u32).to_le_bytes())?;
        self.stream.write_all(data)?;
        read_packets(&mut self.stream)
    }

    pub fn force_keyframe(&mut self) -> io::Result<()> {
        self.stream.write_all(b"K")
    }
}

impl Drop for NvEnc {
    fn drop(&mut self) {
        let _ = self.stream.write_all(b"Q");
    }
}
