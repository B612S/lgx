//! Conversion of the parsed H.265 parameter sets into the Vulkan std structs.
//!
//! The SPS/VPS structs point at the caller's profile-tier-level and DPB
//! storage, so those must outlive the session parameter creation.

use ash::vk::native as n;

use crate::bitstream::{PpsInfo, SpsInfo, VpsInfo};

pub fn profile_tier_level(sps: &SpsInfo) -> n::StdVideoH265ProfileTierLevel {
    let mut ptl = unsafe { std::mem::zeroed::<n::StdVideoH265ProfileTierLevel>() };
    let mut flags = unsafe { std::mem::zeroed::<n::StdVideoH265ProfileTierLevelFlags>() };
    flags.set_general_tier_flag(0);
    flags.set_general_progressive_source_flag(1);
    flags.set_general_interlaced_source_flag(0);
    flags.set_general_non_packed_constraint_flag(0);
    flags.set_general_frame_only_constraint_flag(1);
    ptl.flags = flags;
    ptl.general_profile_idc = u32::from(sps.profile_idc);
    ptl.general_level_idc = u32::from(sps.level_idc);
    ptl
}

pub fn dpb(sps: &SpsInfo) -> n::StdVideoH265DecPicBufMgr {
    let mut dpb = unsafe { std::mem::zeroed::<n::StdVideoH265DecPicBufMgr>() };
    dpb.max_latency_increase_plus1[0] = sps.max_latency_increase_plus1;
    dpb.max_dec_pic_buffering_minus1[0] = sps.max_dec_pic_buffering_minus1;
    dpb.max_num_reorder_pics[0] = sps.max_num_reorder_pics;
    dpb
}

fn sps_flags(sps: &SpsInfo) -> n::StdVideoH265SpsFlags {
    let mut flags = unsafe { std::mem::zeroed::<n::StdVideoH265SpsFlags>() };
    flags.set_sps_temporal_id_nesting_flag(u32::from(sps.temporal_id_nesting));
    flags.set_sps_sub_layer_ordering_info_present_flag(1);
    flags.set_separate_colour_plane_flag(u32::from(sps.separate_colour_plane));
    flags.set_scaling_list_enabled_flag(u32::from(sps.scaling_list_enabled));
    flags.set_amp_enabled_flag(u32::from(sps.amp_enabled));
    flags.set_sample_adaptive_offset_enabled_flag(u32::from(sps.sao_enabled));
    flags.set_pcm_enabled_flag(u32::from(sps.pcm_enabled));
    flags.set_long_term_ref_pics_present_flag(u32::from(sps.long_term_ref_pics_present));
    flags.set_sps_temporal_mvp_enabled_flag(u32::from(sps.temporal_mvp_enabled));
    flags.set_strong_intra_smoothing_enabled_flag(u32::from(sps.strong_intra_smoothing));
    flags.set_vui_parameters_present_flag(0);
    flags
}

pub fn sps(
    info: &SpsInfo,
    ptl: &n::StdVideoH265ProfileTierLevel,
    dpb: &n::StdVideoH265DecPicBufMgr,
) -> n::StdVideoH265SequenceParameterSet {
    let mut sps = unsafe { std::mem::zeroed::<n::StdVideoH265SequenceParameterSet>() };
    sps.flags = sps_flags(info);
    sps.chroma_format_idc = u32::from(info.chroma_format_idc);
    sps.pic_width_in_luma_samples = info.width;
    sps.pic_height_in_luma_samples = info.height;
    sps.sps_video_parameter_set_id = info.vps_id;
    sps.sps_max_sub_layers_minus1 = info.max_sub_layers.saturating_sub(1);
    sps.sps_seq_parameter_set_id = info.sps_id;
    sps.bit_depth_luma_minus8 = info.bit_depth_luma - 8;
    sps.bit_depth_chroma_minus8 = info.bit_depth_chroma - 8;
    sps.log2_max_pic_order_cnt_lsb_minus4 = info.log2_max_poc_lsb.saturating_sub(4);
    sps.log2_min_luma_coding_block_size_minus3 = info.log2_min_luma_coding_block_size_minus3;
    sps.log2_diff_max_min_luma_coding_block_size = info.log2_diff_max_min_luma_coding_block_size;
    sps.log2_min_luma_transform_block_size_minus2 = info.log2_min_luma_transform_block_size_minus2;
    sps.log2_diff_max_min_luma_transform_block_size =
        info.log2_diff_max_min_luma_transform_block_size;
    sps.max_transform_hierarchy_depth_inter = info.max_transform_hierarchy_depth_inter;
    sps.max_transform_hierarchy_depth_intra = info.max_transform_hierarchy_depth_intra;
    sps.num_short_term_ref_pic_sets = info.num_short_term_ref_pic_sets;
    sps.conf_win_left_offset = info.conf_win_left;
    sps.conf_win_right_offset = info.conf_win_right;
    sps.conf_win_top_offset = info.conf_win_top;
    sps.conf_win_bottom_offset = info.conf_win_bottom;
    sps.pProfileTierLevel = ptl;
    sps.pDecPicBufMgr = dpb;
    sps
}

fn pps_flags(pps: &PpsInfo) -> n::StdVideoH265PpsFlags {
    let mut flags = unsafe { std::mem::zeroed::<n::StdVideoH265PpsFlags>() };
    flags.set_dependent_slice_segments_enabled_flag(u32::from(pps.dependent_slice_segments));
    flags.set_output_flag_present_flag(u32::from(pps.output_flag_present));
    flags.set_sign_data_hiding_enabled_flag(u32::from(pps.sign_data_hiding));
    flags.set_cabac_init_present_flag(u32::from(pps.cabac_init_present));
    flags.set_constrained_intra_pred_flag(u32::from(pps.constrained_intra_pred));
    flags.set_transform_skip_enabled_flag(u32::from(pps.transform_skip_enabled));
    flags.set_cu_qp_delta_enabled_flag(u32::from(pps.cu_qp_delta_enabled));
    flags.set_pps_slice_chroma_qp_offsets_present_flag(u32::from(
        pps.slice_chroma_qp_offsets_present,
    ));
    flags.set_weighted_pred_flag(u32::from(pps.weighted_pred));
    flags.set_weighted_bipred_flag(u32::from(pps.weighted_bipred));
    flags.set_transquant_bypass_enabled_flag(u32::from(pps.transquant_bypass_enabled));
    flags.set_tiles_enabled_flag(u32::from(pps.tiles_enabled));
    flags.set_entropy_coding_sync_enabled_flag(u32::from(pps.entropy_coding_sync_enabled));
    flags.set_pps_loop_filter_across_slices_enabled_flag(u32::from(
        pps.loop_filter_across_slices_enabled,
    ));
    flags.set_deblocking_filter_control_present_flag(u32::from(
        pps.deblocking_filter_control_present,
    ));
    flags.set_deblocking_filter_override_enabled_flag(u32::from(
        pps.deblocking_filter_override_enabled,
    ));
    flags.set_pps_deblocking_filter_disabled_flag(u32::from(pps.deblocking_filter_disabled));
    flags.set_lists_modification_present_flag(u32::from(pps.lists_modification_present));
    flags.set_slice_segment_header_extension_present_flag(u32::from(
        pps.slice_segment_header_extension_present,
    ));
    flags
}

pub fn pps(info: &PpsInfo, sps: &SpsInfo) -> n::StdVideoH265PictureParameterSet {
    let mut pps = unsafe { std::mem::zeroed::<n::StdVideoH265PictureParameterSet>() };
    pps.flags = pps_flags(info);
    pps.pps_pic_parameter_set_id = info.pps_id;
    pps.pps_seq_parameter_set_id = info.sps_id;
    pps.sps_video_parameter_set_id = sps.vps_id;
    pps.num_extra_slice_header_bits = info.extra_slice_header_bits;
    pps.num_ref_idx_l0_default_active_minus1 = info.num_ref_idx_l0_default_active_minus1;
    pps.num_ref_idx_l1_default_active_minus1 = info.num_ref_idx_l1_default_active_minus1;
    pps.init_qp_minus26 = info.init_qp_minus26 as i8;
    pps.diff_cu_qp_delta_depth = info.diff_cu_qp_delta_depth;
    pps.pps_cb_qp_offset = info.cb_qp_offset as i8;
    pps.pps_cr_qp_offset = info.cr_qp_offset as i8;
    pps.pps_beta_offset_div2 = info.beta_offset_div2 as i8;
    pps.pps_tc_offset_div2 = info.tc_offset_div2 as i8;
    pps.log2_parallel_merge_level_minus2 = info.log2_parallel_merge_level_minus2;
    pps
}

pub fn vps(
    info: &VpsInfo,
    ptl: &n::StdVideoH265ProfileTierLevel,
    dpb: &n::StdVideoH265DecPicBufMgr,
) -> n::StdVideoH265VideoParameterSet {
    let mut vps = unsafe { std::mem::zeroed::<n::StdVideoH265VideoParameterSet>() };
    let mut flags = unsafe { std::mem::zeroed::<n::StdVideoH265VpsFlags>() };
    flags.set_vps_temporal_id_nesting_flag(u32::from(info.temporal_id_nesting));
    flags.set_vps_sub_layer_ordering_info_present_flag(1);
    vps.flags = flags;
    vps.vps_video_parameter_set_id = info.vps_id;
    vps.vps_max_sub_layers_minus1 = info.max_sub_layers.saturating_sub(1);
    vps.pDecPicBufMgr = dpb;
    vps.pProfileTierLevel = ptl;
    vps
}
