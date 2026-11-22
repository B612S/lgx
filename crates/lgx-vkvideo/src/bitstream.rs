//! Minimal H.265 Annex-B bitstream reader and parameter-set parser.
//!
//! Only the syntax needed by the Vulkan Video session (SPS up to the POC
//! length and the PTL, PPS identifiers) is parsed so far. The rest of the
//! SPS/PPS and the slice header follow in later steps.

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Eof,
    Invalid(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Eof => write!(f, "end of bitstream"),
            Error::Invalid(what) => write!(f, "invalid {what}"),
        }
    }
}

impl std::error::Error for Error {}

pub struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    pub fn bits_left(&self) -> usize {
        self.data.len() * 8 - self.bit
    }

    pub fn pos(&self) -> usize {
        self.bit
    }

    pub fn u(&mut self, n: u32) -> Result<u32, Error> {
        if n > 32 {
            return Err(Error::Invalid("read width"));
        }
        if self.bits_left() < n as usize {
            return Err(Error::Eof);
        }
        let mut value = 0u32;
        for _ in 0..n {
            let byte = self.data[self.bit / 8];
            let shift = 7 - (self.bit % 8);
            value = (value << 1) | u32::from((byte >> shift) & 1);
            self.bit += 1;
        }
        Ok(value)
    }

    pub fn flag(&mut self) -> Result<bool, Error> {
        Ok(self.u(1)? != 0)
    }

    pub fn skip(&mut self, n: usize) -> Result<(), Error> {
        if self.bits_left() < n {
            return Err(Error::Eof);
        }
        self.bit += n;
        Ok(())
    }

    /// Unsigned Exp-Golomb (H.265 9.2).
    pub fn ue(&mut self) -> Result<u32, Error> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::Invalid("ue"));
            }
        }
        let suffix = if zeros == 0 { 0 } else { self.u(zeros)? };
        Ok(((1u64 << zeros) - 1 + u64::from(suffix)) as u32)
    }

    pub fn se(&mut self) -> Result<i32, Error> {
        let value = self.ue()?;
        Ok(if value & 1 == 1 {
            (value / 2 + 1) as i32
        } else {
            -((value / 2) as i32)
        })
    }
}

/// Removes H.265 emulation-prevention bytes from an EBSP.
pub fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &byte in data {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Splits an Annex-B byte stream into NAL units (without start codes).
pub fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut positions = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                positions.push((i, 3));
                i += 3;
                continue;
            }
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                positions.push((i, 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    let mut nals = Vec::with_capacity(positions.len());
    for (n, &(start, code)) in positions.iter().enumerate() {
        let begin = start + code;
        let end = positions
            .get(n + 1)
            .map(|&(next, _)| next)
            .unwrap_or(data.len());
        let mut slice = &data[begin..end];
        while let Some((&last, rest)) = slice.split_last() {
            if last == 0 {
                slice = rest;
            } else {
                break;
            }
        }
        if !slice.is_empty() {
            nals.push(slice);
        }
    }
    nals
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| (b >> 1) & 0x3f).unwrap_or(u8::MAX)
}

/// Fields of the SPS that the Vulkan session needs early.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SpsInfo {
    pub vps_id: u8,
    pub max_sub_layers: u8,
    pub temporal_id_nesting: bool,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub sps_id: u8,
    pub chroma_format_idc: u8,
    pub separate_colour_plane: bool,
    pub width: u32,
    pub height: u32,
    pub conf_win_left: u32,
    pub conf_win_right: u32,
    pub conf_win_top: u32,
    pub conf_win_bottom: u32,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub log2_max_poc_lsb: u8,
    pub max_dec_pic_buffering_minus1: u8,
    pub max_num_reorder_pics: u8,
    pub max_latency_increase_plus1: u32,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_luma_transform_block_size_minus2: u8,
    pub log2_diff_max_min_luma_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub scaling_list_enabled: bool,
    pub amp_enabled: bool,
    pub sao_enabled: bool,
    pub pcm_enabled: bool,
    pub num_short_term_ref_pic_sets: u8,
    pub long_term_ref_pics_present: bool,
    pub temporal_mvp_enabled: bool,
    pub strong_intra_smoothing: bool,
    pub vui_present: bool,
}

fn ptl_profile(r: &mut BitReader<'_>) -> Result<u8, Error> {
    let _profile_space = r.u(2)?;
    let _tier_flag = r.flag()?;
    let mut profile_idc = r.u(5)?;
    let mut compat = [false; 32];
    for (i, slot) in compat.iter_mut().enumerate() {
        *slot = r.flag()?;
        if profile_idc == 0 && i > 0 && *slot {
            profile_idc = i as u32;
        }
    }
    let _progressive = r.flag()?;
    let _interlaced = r.flag()?;
    let _non_packed = r.flag()?;
    let _frame_only = r.flag()?;
    let has = |idc: usize| profile_idc == idc as u32 || compat[idc];
    if has(4) || has(5) || has(6) || has(7) || has(8) || has(9) || has(10) {
        for _ in 0..9 {
            r.flag()?;
        }
        if has(5) || has(9) || has(10) {
            r.flag()?;
            r.skip(33)?;
        } else {
            r.skip(34)?;
        }
    } else if has(2) {
        r.skip(7)?;
        r.flag()?;
        r.skip(35)?;
    } else {
        r.skip(43)?;
    }
    if has(1) || has(2) || has(3) || has(4) || has(5) || has(9) {
        r.flag()?;
    } else {
        r.skip(1)?;
    }
    Ok(profile_idc as u8)
}

fn parse_ptl(r: &mut BitReader<'_>, max_sub_layers: u8) -> Result<(u8, u8), Error> {
    let profile_idc = ptl_profile(r)?;
    let level_idc = r.u(8)? as u8;
    let count = max_sub_layers.saturating_sub(1) as usize;
    let mut profile_present = [false; 7];
    let mut level_present = [false; 7];
    for i in 0..count {
        profile_present[i] = r.flag()?;
        level_present[i] = r.flag()?;
    }
    if count > 0 {
        for _ in count..8 {
            r.skip(2)?;
        }
    }
    for i in 0..count {
        if profile_present[i] {
            ptl_profile(r)?;
        }
        if level_present[i] {
            r.u(8)?;
        }
    }
    Ok((profile_idc, level_idc))
}

impl SpsInfo {
    pub fn parse(nal: &[u8]) -> Result<Self, Error> {
        if nal_type(nal) != 33 || nal.len() < 3 {
            return Err(Error::Invalid("not an SPS"));
        }
        let rbsp = unescape(&nal[2..]);
        let mut r = BitReader::new(&rbsp);
        let mut sps = SpsInfo {
            vps_id: r.u(4)? as u8,
            max_sub_layers: r.u(3)? as u8 + 1,
            temporal_id_nesting: r.flag()?,
            ..Default::default()
        };
        let (profile_idc, level_idc) = parse_ptl(&mut r, sps.max_sub_layers)?;
        sps.profile_idc = profile_idc;
        sps.level_idc = level_idc;
        sps.sps_id = r.ue()? as u8;
        sps.chroma_format_idc = r.ue()? as u8;
        if sps.chroma_format_idc > 3 {
            return Err(Error::Invalid("chroma_format_idc"));
        }
        if sps.chroma_format_idc == 3 {
            sps.separate_colour_plane = r.flag()?;
        }
        if sps.separate_colour_plane {
            sps.chroma_format_idc = 0;
        }
        sps.width = r.ue()?;
        sps.height = r.ue()?;
        if r.flag()? {
            sps.conf_win_left = r.ue()?;
            sps.conf_win_right = r.ue()?;
            sps.conf_win_top = r.ue()?;
            sps.conf_win_bottom = r.ue()?;
        }
        sps.bit_depth_luma = r.ue()? as u8 + 8;
        sps.bit_depth_chroma = r.ue()? as u8 + 8;
        sps.log2_max_poc_lsb = r.ue()? as u8 + 4;
        if sps.log2_max_poc_lsb > 16 {
            return Err(Error::Invalid("log2_max_poc_lsb"));
        }
        let ordering_info = r.flag()?;
        let start = if ordering_info {
            0
        } else {
            sps.max_sub_layers.saturating_sub(1) as usize
        };
        for _ in start..sps.max_sub_layers as usize {
            sps.max_dec_pic_buffering_minus1 = r.ue()? as u8;
            sps.max_num_reorder_pics = r.ue()? as u8;
            sps.max_latency_increase_plus1 = r.ue()?;
        }
        sps.log2_min_luma_coding_block_size_minus3 = r.ue()? as u8;
        sps.log2_diff_max_min_luma_coding_block_size = r.ue()? as u8;
        sps.log2_min_luma_transform_block_size_minus2 = r.ue()? as u8;
        sps.log2_diff_max_min_luma_transform_block_size = r.ue()? as u8;
        sps.max_transform_hierarchy_depth_inter = r.ue()? as u8;
        sps.max_transform_hierarchy_depth_intra = r.ue()? as u8;
        sps.scaling_list_enabled = r.flag()?;
        if sps.scaling_list_enabled {
            // Scaling-list data is not needed by the server stream yet.
            return Err(Error::Invalid("scaling_list_data"));
        }
        sps.amp_enabled = r.flag()?;
        sps.sao_enabled = r.flag()?;
        sps.pcm_enabled = r.flag()?;
        if sps.pcm_enabled {
            r.u(4)?; // pcm_sample_bit_depth_luma_minus1
            r.u(4)?; // pcm_sample_bit_depth_chroma_minus1
            r.ue()?; // log2_min_pcm_luma_coding_block_size_minus3
            r.ue()?; // log2_diff_max_min_pcm_luma_coding_block_size
            r.flag()?; // pcm_loop_filter_disabled_flag
        }
        sps.num_short_term_ref_pic_sets = r.ue()? as u8;
        // NVENC writes SPS-level short-term reference picture sets (VAAPI
        // usually leaves the list empty and puts the RPS in the slice header).
        // The decoder keeps its own DPB and does not need the POC values, but
        // the bits must be consumed to reach the fields after them.
        if sps.num_short_term_ref_pic_sets > 64 {
            return Err(Error::Invalid("num_short_term_ref_pic_sets"));
        }
        let mut delta_pocs: Vec<u32> = Vec::with_capacity(usize::from(sps.num_short_term_ref_pic_sets));
        for i in 0..usize::from(sps.num_short_term_ref_pic_sets) {
            let mut count = 0u32;
            if i != 0 && r.flag()? {
                // inter_ref_pic_set_prediction_flag; delta_idx_minus1 is not
                // present in the SPS, so the reference set is the previous one.
                let _delta_rps_sign = r.flag()?;
                let _abs_delta_rps_minus1 = r.ue()?;
                let ref_count = delta_pocs.get(i - 1).copied().unwrap_or(0);
                for _ in 0..=ref_count {
                    let used = r.flag()?;
                    let use_delta = if used { true } else { r.flag()? };
                    if used || use_delta {
                        count += 1;
                    }
                }
            } else {
                let negative = r.ue()?;
                let positive = r.ue()?;
                if negative + positive > 64 {
                    return Err(Error::Invalid("short-term RPS size"));
                }
                for _ in 0..negative {
                    let _delta_poc_s0_minus1 = r.ue()?;
                    let _used_by_curr_pic_s0 = r.flag()?;
                }
                for _ in 0..positive {
                    let _delta_poc_s1_minus1 = r.ue()?;
                    let _used_by_curr_pic_s1 = r.flag()?;
                }
                count = negative + positive;
            }
            delta_pocs.push(count);
        }
        sps.long_term_ref_pics_present = r.flag()?;
        if sps.long_term_ref_pics_present {
            let count = r.ue()?;
            for _ in 0..count {
                r.u(u32::from(sps.log2_max_poc_lsb))?;
                r.flag()?;
            }
        }
        sps.temporal_mvp_enabled = r.flag()?;
        sps.strong_intra_smoothing = r.flag()?;
        sps.vui_present = r.flag()?;
        Ok(sps)
    }
}

impl SpsInfo {
    /// Width after the conformance window (coded size minus crop).
    pub fn display_width(&self) -> u32 {
        let crop = if self.chroma_format_idc == 1 || self.chroma_format_idc == 2 {
            2
        } else {
            1
        };
        self.width
            .saturating_sub(crop * (self.conf_win_left + self.conf_win_right))
    }

    /// Height after the conformance window (coded size minus crop).
    pub fn display_height(&self) -> u32 {
        let crop = if self.chroma_format_idc == 1 || self.chroma_format_idc == 2 {
            2
        } else {
            1
        };
        self.height
            .saturating_sub(crop * (self.conf_win_top + self.conf_win_bottom))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // First SPS of the 1280x720 HEVC test stream.
    const SPS: &[u8] = &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x78, 0xa0, 0x02, 0x80, 0x80, 0x2d, 0x16, 0x59, 0x59, 0x52, 0x93, 0x0b, 0xc0,
        0x5a, 0x02, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x78, 0x10,
    ];

    // SPS of the live 1920x1080@160 server stream (VAAPI, Main, level 5.1).
    const SERVER_SPS: &[u8] = &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x99, 0xa0, 0x03, 0xc0, 0x80, 0x10, 0xe5, 0x89, 0x2e, 0x49, 0x1b, 0x6a, 0x9b,
        0x80, 0x80, 0x80, 0x82, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x01, 0x41, 0xaf,
        0x84, 0x02, 0x08,
    ];

    // SPS of an NVENC stream (Main, 1280x720) with one SPS-level RPS.
    const NVENC_SPS: &[u8] = &[
        0x42, 0x01, 0x01, 0x21, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x96, 0xa0, 0x02, 0x80, 0x80, 0x2e, 0x1f, 0x13, 0x96, 0x5d, 0x29, 0x08, 0x46,
        0x45, 0xd5, 0x0c, 0x05, 0xb8, 0x08, 0x08, 0x08, 0x20, 0x00, 0x00, 0x03, 0x00, 0x20, 0x00,
        0x00, 0x07, 0x8c, 0x00, 0xbb, 0xca, 0x20, 0x00, 0x02, 0x62, 0x5a, 0x00, 0x00, 0x03, 0x00,
        0x98, 0x96, 0x80, 0x20, 0x00,
    ];

    #[test]
    fn parses_nvenc_sps_with_sps_level_rps() {
        let sps = SpsInfo::parse(NVENC_SPS).expect("NVENC SPS");
        // NVENC codes 1280x736 and crops 16 lines with the conformance window.
        assert_eq!((sps.width, sps.height), (1280, 736));
        assert_eq!(sps.display_width(), 1280);
        assert_eq!(sps.display_height(), 720);
        assert_eq!(sps.profile_idc, 1);
        assert!(sps.num_short_term_ref_pic_sets > 0);
    }

    #[test]
    fn parses_server_sps() {
        let sps = SpsInfo::parse(SERVER_SPS).expect("server SPS");
        assert_eq!((sps.width, sps.height), (1920, 1080));
        assert_eq!(sps.profile_idc, 1, "Main profile");
        assert_eq!(sps.level_idc, 153, "level 5.1");
        assert_eq!(sps.chroma_format_idc, 1);
        assert_eq!(sps.bit_depth_luma, 8);
        assert_eq!(sps.bit_depth_chroma, 8);
        assert_eq!(sps.max_sub_layers, 1);
        assert!(sps.temporal_id_nesting);
        assert_eq!(sps.max_dec_pic_buffering_minus1, 1);
        assert_eq!(sps.max_num_reorder_pics, 0);
        assert_eq!(sps.log2_min_luma_coding_block_size_minus3, 0);
        assert_eq!(sps.log2_diff_max_min_luma_coding_block_size, 3);
        assert!(!sps.scaling_list_enabled);
        assert!(sps.amp_enabled);
        assert!(sps.sao_enabled);
        assert!(!sps.pcm_enabled);
        assert_eq!(sps.num_short_term_ref_pic_sets, 0);
    }

    #[test]
    fn parses_sample_sps() {
        let sps = SpsInfo::parse(SPS).expect("SPS");
        assert_eq!(sps.width, 1280);
        assert_eq!(sps.height, 720);
        assert_eq!(sps.profile_idc, 1, "Main profile");
        assert_eq!(sps.chroma_format_idc, 1);
        assert_eq!(sps.bit_depth_luma, 8);
        assert_eq!(sps.bit_depth_chroma, 8);
        assert_eq!(sps.max_sub_layers, 1);
        assert!(sps.temporal_id_nesting);
        assert_eq!(sps.log2_max_poc_lsb, 8);
    }

    #[test]
    fn unescapes_emulation_prevention() {
        assert_eq!(unescape(&[0, 0, 3, 0, 0, 3, 1]), vec![0, 0, 0, 0, 1]);
    }

    #[test]
    fn splits_annex_b() {
        let stream = [
            0, 0, 0, 1, 0x40, 1, 2, 0, 0, 1, 0x42, 1, 3, 0, 0, 0, 1, 0x44, 1, 4,
        ];
        let nals = annexb_nals(&stream);
        assert_eq!(nals.len(), 3);
        assert_eq!(nal_type(nals[0]), 32);
        assert_eq!(nal_type(nals[1]), 33);
        assert_eq!(nal_type(nals[2]), 34);
    }
}

/// Fields of the PPS that the slice header and the Vulkan std struct need.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PpsInfo {
    pub pps_id: u8,
    pub sps_id: u8,
    pub dependent_slice_segments: bool,
    pub output_flag_present: bool,
    pub extra_slice_header_bits: u8,
    pub sign_data_hiding: bool,
    pub cabac_init_present: bool,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i32,
    pub constrained_intra_pred: bool,
    pub transform_skip_enabled: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: u8,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub slice_chroma_qp_offsets_present: bool,
    pub weighted_pred: bool,
    pub weighted_bipred: bool,
    pub transquant_bypass_enabled: bool,
    pub tiles_enabled: bool,
    pub entropy_coding_sync_enabled: bool,
    pub loop_filter_across_slices_enabled: bool,
    pub deblocking_filter_control_present: bool,
    pub deblocking_filter_override_enabled: bool,
    pub deblocking_filter_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub lists_modification_present: bool,
    pub log2_parallel_merge_level_minus2: u8,
    pub slice_segment_header_extension_present: bool,
}

impl PpsInfo {
    pub fn parse(nal: &[u8]) -> Result<Self, Error> {
        if nal_type(nal) != 34 || nal.len() < 3 {
            return Err(Error::Invalid("not a PPS"));
        }
        let rbsp = unescape(&nal[2..]);
        let mut r = BitReader::new(&rbsp);
        let mut pps = PpsInfo {
            pps_id: r.ue()? as u8,
            sps_id: r.ue()? as u8,
            dependent_slice_segments: r.flag()?,
            output_flag_present: r.flag()?,
            extra_slice_header_bits: r.u(3)? as u8,
            sign_data_hiding: r.flag()?,
            cabac_init_present: r.flag()?,
            num_ref_idx_l0_default_active_minus1: r.ue()? as u8,
            num_ref_idx_l1_default_active_minus1: r.ue()? as u8,
            init_qp_minus26: r.se()?,
            constrained_intra_pred: r.flag()?,
            transform_skip_enabled: r.flag()?,
            cu_qp_delta_enabled: r.flag()?,
            ..Default::default()
        };
        if pps.cu_qp_delta_enabled {
            pps.diff_cu_qp_delta_depth = r.ue()? as u8;
        }
        pps.cb_qp_offset = r.se()?;
        pps.cr_qp_offset = r.se()?;
        pps.slice_chroma_qp_offsets_present = r.flag()?;
        pps.weighted_pred = r.flag()?;
        pps.weighted_bipred = r.flag()?;
        pps.transquant_bypass_enabled = r.flag()?;
        pps.tiles_enabled = r.flag()?;
        pps.entropy_coding_sync_enabled = r.flag()?;
        if pps.tiles_enabled {
            let columns = r.ue()?;
            let rows = r.ue()?;
            if !r.flag()? {
                for _ in 0..columns {
                    r.ue()?;
                }
                for _ in 0..rows {
                    r.ue()?;
                }
            }
            r.flag()?; // loop_filter_across_tiles_enabled_flag
        }
        pps.loop_filter_across_slices_enabled = r.flag()?;
        pps.deblocking_filter_control_present = r.flag()?;
        if pps.deblocking_filter_control_present {
            pps.deblocking_filter_override_enabled = r.flag()?;
            pps.deblocking_filter_disabled = r.flag()?;
            if !pps.deblocking_filter_disabled {
                pps.beta_offset_div2 = r.se()?;
                pps.tc_offset_div2 = r.se()?;
            }
        }
        if r.flag()? {
            // pps_scaling_list_data_present_flag
            return Err(Error::Invalid("pps scaling list"));
        }
        pps.lists_modification_present = r.flag()?;
        pps.log2_parallel_merge_level_minus2 = r.ue()? as u8;
        pps.slice_segment_header_extension_present = r.flag()?;
        Ok(pps)
    }
}

/// One reference picture of the slice's short-term RPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefPic {
    /// POC relative to the current picture (negative = earlier).
    pub poc_delta: i32,
    pub used_by_current: bool,
}

/// The first fields of a slice segment header, up to the RPS.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SliceInfo {
    pub first_slice: bool,
    /// B = 0, P = 1, I = 2.
    pub slice_type: u8,
    pub poc_lsb: u32,
    pub short_term_ref_pic_set_sps_flag: bool,
    /// Size of the in-slice RPS syntax in bits (0 when the SPS RPS is used).
    pub rps_bits: u16,
    pub refs: Vec<RefPic>,
}

pub fn parse_slice_header(
    nal: &[u8],
    sps: &SpsInfo,
    pps: &PpsInfo,
) -> Result<SliceInfo, Error> {
    let nal_type = nal_type(nal);
    if !(0..=31).contains(&nal_type) || nal.len() < 3 {
        return Err(Error::Invalid("not a VCL NAL"));
    }
    let rbsp = unescape(&nal[2..]);
    let mut r = BitReader::new(&rbsp);
    let first_slice = r.flag()?;
    if (16..=23).contains(&nal_type) {
        r.flag()?; // no_output_of_prior_pics_flag
    }
    let pps_id = r.ue()? as u8;
    if pps_id != pps.pps_id {
        return Err(Error::Invalid("unknown PPS"));
    }
    if !first_slice {
        // A dependent slice has no slice_type/POC; its address needs the SPS
        // coding-block sizes, which this parser does not need yet.
        return Err(Error::Invalid("secondary slice segment"));
    }
    for _ in 0..pps.extra_slice_header_bits {
        r.flag()?;
    }
    let slice_type = r.ue()? as u8;
    if pps.output_flag_present {
        r.flag()?;
    }
    if sps.separate_colour_plane {
        r.u(2)?;
    }
    let poc_lsb = if nal_type != 19 && nal_type != 20 {
        r.u(u32::from(sps.log2_max_poc_lsb))?
    } else {
        0
    };
    let mut info = SliceInfo {
        first_slice,
        slice_type,
        poc_lsb,
        ..Default::default()
    };
    if nal_type != 19 && nal_type != 20 {
        info.short_term_ref_pic_set_sps_flag = r.flag()?;
        if !info.short_term_ref_pic_set_sps_flag {
            let start = r.pos();
            let negative = r.ue()?;
            let positive = r.ue()?;
            for _ in 0..negative {
                let delta = r.ue()? as i32 + 1;
                let used = r.flag()?;
                info.refs.push(RefPic { poc_delta: -delta, used_by_current: used });
            }
            for _ in 0..positive {
                let delta = r.ue()? as i32 + 1;
                let used = r.flag()?;
                info.refs.push(RefPic { poc_delta: delta, used_by_current: used });
            }
            info.rps_bits = (r.pos() - start) as u16;
        }
    }
    Ok(info)
}

#[cfg(test)]
mod slice_tests {
    use super::*;

    const SERVER_SPS: &[u8] = &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x99, 0xa0, 0x03, 0xc0, 0x80, 0x10, 0xe5, 0x89, 0x2e, 0x49, 0x1b, 0x6a, 0x9b,
        0x80, 0x80, 0x80, 0x82, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x01, 0x41, 0xaf,
        0x84, 0x02, 0x08,
    ];
    const SERVER_PPS: &[u8] = &[0x44, 0x01, 0xc0, 0x62, 0x19, 0x30, 0x22, 0x40];
    // First 48 bytes of the live stream's IDR (type 19) and first 64 bytes
    // of the following B slice.
    const IDR_PREFIX: &[u8] = &[
        0x26, 0x01, 0xaf, 0xa0, 0x65, 0xc6, 0x24, 0x1d, 0xf0, 0x3c, 0xee, 0xc9, 0x37, 0x99, 0x0a,
        0x07, 0xbc, 0x38, 0x45, 0x07, 0xd6, 0x50, 0x8e, 0x72, 0x76, 0xe2, 0x51, 0xa0, 0xac, 0xab,
        0x36, 0x03, 0xe6, 0xe2, 0x73, 0xe0, 0xaf, 0x67, 0xcc, 0xc1, 0xe7, 0xb5, 0xf3, 0xba, 0xf5,
        0xce, 0x89, 0x91,
    ];
    const B_PREFIX: &[u8] = &[
        0x02, 0x01, 0xe0, 0x02, 0x5f, 0x9d, 0xfa, 0xae, 0x70, 0x3f, 0xc9, 0x1c, 0xc8, 0x7c, 0x82,
        0xb8, 0x38, 0x73, 0x99, 0x88, 0xef, 0x68, 0x78, 0x33, 0xd8, 0xf7, 0x79, 0xad, 0x7d, 0xd0,
        0x06, 0xe6, 0x48, 0xd5, 0x07, 0xc0, 0xeb, 0xb5, 0x37, 0x8e, 0x83, 0x27, 0xad, 0x68, 0xa1,
        0x54, 0xf7, 0x26, 0xff, 0x8b, 0x79, 0x15, 0xd8, 0x65, 0x85, 0xb7, 0x1b, 0x4b, 0xa7, 0x78,
        0xb2, 0x2c, 0xea, 0x2d,
    ];

    #[test]
    fn parses_server_pps() {
        let pps = PpsInfo::parse(SERVER_PPS).expect("PPS");
        assert_eq!(pps.pps_id, 0);
        assert_eq!(pps.sps_id, 0);
        assert!(!pps.dependent_slice_segments);
        assert!(!pps.output_flag_present);
        assert_eq!(pps.extra_slice_header_bits, 0);
        assert!(!pps.tiles_enabled);
        assert!(!pps.entropy_coding_sync_enabled);
        assert!(!pps.weighted_pred);
        assert!(!pps.weighted_bipred);
        assert!(!pps.transquant_bypass_enabled);
        assert_eq!(pps.init_qp_minus26, 4);
        assert!(pps.transform_skip_enabled);
        assert!(pps.cu_qp_delta_enabled);
        assert_eq!(pps.diff_cu_qp_delta_depth, 3);
        assert_eq!(pps.cb_qp_offset, 0);
        assert!(!pps.slice_chroma_qp_offsets_present);
        assert!(pps.loop_filter_across_slices_enabled);
        assert!(!pps.deblocking_filter_control_present);
        assert!(!pps.lists_modification_present);
    }

    #[test]
    fn parses_idr_and_b_slice_headers() {
        let sps = SpsInfo::parse(SERVER_SPS).expect("SPS");
        let pps = PpsInfo::parse(SERVER_PPS).expect("PPS");
        let idr = parse_slice_header(IDR_PREFIX, &sps, &pps).expect("IDR slice");
        assert!(idr.first_slice);
        assert_eq!(idr.slice_type, 2, "I slice");
        assert_eq!(idr.poc_lsb, 0, "IDR has no POC lsb");
        assert!(idr.refs.is_empty());
        assert_eq!(idr.rps_bits, 0);

        let b = parse_slice_header(B_PREFIX, &sps, &pps).expect("B slice");
        assert!(b.first_slice);
        assert_eq!(b.slice_type, 0, "B slice");
        assert_eq!(b.poc_lsb, 1);
        assert!(!b.short_term_ref_pic_set_sps_flag);
        assert_eq!(b.refs, vec![RefPic { poc_delta: -1, used_by_current: true }]);
        assert!(b.rps_bits > 0);
    }
}

/// Fields of the VPS needed by the Vulkan std struct.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VpsInfo {
    pub vps_id: u8,
    pub max_layers_minus1: u8,
    pub max_sub_layers: u8,
    pub temporal_id_nesting: bool,
    pub max_dec_pic_buffering_minus1: u8,
    pub max_num_reorder_pics: u8,
    pub max_latency_increase_plus1: u32,
    pub max_layer_id: u8,
    pub num_layer_sets_minus1: u16,
}

impl VpsInfo {
    pub fn parse(nal: &[u8]) -> Result<Self, Error> {
        if nal_type(nal) != 32 || nal.len() < 3 {
            return Err(Error::Invalid("not a VPS"));
        }
        let rbsp = unescape(&nal[2..]);
        let mut r = BitReader::new(&rbsp);
        let mut vps = VpsInfo {
            vps_id: r.u(4)? as u8,
            ..Default::default()
        };
        r.flag()?; // vps_base_layer_internal_flag
        r.flag()?; // vps_base_layer_available_flag
        vps.max_layers_minus1 = r.u(6)? as u8;
        vps.max_sub_layers = r.u(3)? as u8 + 1;
        vps.temporal_id_nesting = r.flag()?;
        r.skip(16)?; // vps_reserved_0xffff_16bits
        parse_ptl(&mut r, vps.max_sub_layers)?;
        let ordering_info = r.flag()?;
        let start = if ordering_info {
            0
        } else {
            vps.max_sub_layers.saturating_sub(1) as usize
        };
        for _ in start..vps.max_sub_layers as usize {
            vps.max_dec_pic_buffering_minus1 = r.ue()? as u8;
            vps.max_num_reorder_pics = r.ue()? as u8;
            vps.max_latency_increase_plus1 = r.ue()?;
        }
        vps.max_layer_id = r.u(6)? as u8;
        vps.num_layer_sets_minus1 = r.ue()? as u16;
        Ok(vps)
    }
}

#[cfg(test)]
mod vps_tests {
    use super::*;

    const SERVER_VPS: &[u8] = &[
        0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00,
        0x03, 0x00, 0x00, 0x03, 0x00, 0x99, 0x2c, 0x0c, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00,
        0x03, 0x02, 0x83, 0xa0,
    ];

    #[test]
    fn parses_server_vps() {
        let vps = VpsInfo::parse(SERVER_VPS).expect("VPS");
        assert_eq!(vps.vps_id, 0);
        assert_eq!(vps.max_sub_layers, 1);
        assert_eq!(vps.max_layers_minus1, 0);
        assert!(vps.temporal_id_nesting);
        assert_eq!(vps.max_dec_pic_buffering_minus1, 1);
        assert_eq!(vps.max_num_reorder_pics, 0);
        assert_eq!(vps.max_layer_id, 0);
        assert_eq!(vps.num_layer_sets_minus1, 0);
    }
}
