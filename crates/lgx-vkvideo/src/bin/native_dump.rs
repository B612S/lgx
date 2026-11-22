//! Decode a captured Annex-B stream with the native decoder and write every
//! picture to `OUT/frame-NNNNN.nv12` (NV12, stride = width) for comparison
//! against FFmpeg.
//!
//! Usage: native_dump STREAM.hevc OUT_DIR [MAX_FRAMES]

use lgx_vkvideo::bitstream::{annexb_nals, nal_type, PpsInfo, SpsInfo, VpsInfo};
use lgx_vkvideo::decoder::NativeDecoder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: native_dump STREAM.hevc OUT_DIR [N]")?;
    let out_dir = args.next().ok_or("usage: native_dump STREAM.hevc OUT_DIR [N]")?;
    let max_frames: usize = args
        .next()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(usize::MAX);
    let data = std::fs::read(&path)?;
    std::fs::create_dir_all(&out_dir)?;
    let mut decoder = NativeDecoder::new()?;
    let mut vps: Option<VpsInfo> = None;
    let mut sps: Option<SpsInfo> = None;
    let mut pps: Option<PpsInfo> = None;
    let mut configured = false;
    let mut frames = 0usize;
    for nal in annexb_nals(&data) {
        match nal_type(nal) {
            32 => vps = Some(VpsInfo::parse(nal)?),
            33 => sps = Some(SpsInfo::parse(nal)?),
            34 => pps = Some(PpsInfo::parse(nal)?),
            0..=31 => {
                if !configured {
                    decoder.configure(
                        vps.as_ref().ok_or("no VPS")?,
                        sps.as_ref().ok_or("no SPS")?,
                        pps.as_ref().ok_or("no PPS")?,
                    )?;
                    configured = true;
                }
                if let Some(frame) = decoder.decode(nal)? {
                    // The CPU fallback already carries the picture; the
                    // zero-copy path has to read it back.
                    let nv12 = if frame.data.is_empty() {
                        decoder.readback()?
                    } else {
                        frame.data
                    };
                    let file = format!("{out_dir}/frame-{frames:05}.nv12");
                    std::fs::write(&file, &nv12)?;
                    frames += 1;
                    if frames >= max_frames {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    println!("wrote {frames} frames to {out_dir}");
    Ok(())
}
