//! Decode the first ten pictures of a captured stream with `NativeDecoder`.

use lgx_vkvideo::bitstream::{annexb_nals, nal_type, PpsInfo, SpsInfo, VpsInfo};
use lgx_vkvideo::decoder::NativeDecoder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "/tmp/lgx-server.hevc".into());
    let data = std::fs::read(&path)?;
    let mut decoder = NativeDecoder::new()?;
    let mut vps: Option<VpsInfo> = None;
    let mut sps: Option<SpsInfo> = None;
    let mut pps: Option<PpsInfo> = None;
    let mut configured = false;
    let mut frames = 0;
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
                    match &frame.dma {
                        Some(d) => println!(
                            "frame {frames}: {}x{} poc {} dma fd {} modifier {:#x}",
                            frame.width, frame.height, frame.poc, d.fd, d.modifier
                        ),
                        None => println!(
                            "frame {frames}: {}x{} poc {} ({} bytes)",
                            frame.width, frame.height, frame.poc, frame.data.len()
                        ),
                    }
                    frames += 1;
                    if frames == 10 {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}
