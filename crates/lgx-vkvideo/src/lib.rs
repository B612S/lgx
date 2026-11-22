//! Native Vulkan Video H.265 decoder for lgx.
//!
//! This crate is the experimental zero-copy path: it owns the Vulkan video
//! session, the DPB images and the H.265 bitstream parser. The client uses the
//! FFmpeg Vulkan backend (or VAAPI/CUDA/software) by default and only selects
//! this decoder with `LGX_VULKAN_VIDEO=native` once it is complete.

pub mod bitstream;
pub mod decoder;
pub mod dpb;
pub mod std;

use ash::vk;

/// H.265 Main profile, the profile the server streams.
pub const H265_MAIN_PROFILE_IDC: vk::native::StdVideoH265ProfileIdc =
    vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN;

/// 4:2:0 chroma subsampling, 8 bit per component.
pub const STREAM_CHROMA: vk::VideoChromaSubsamplingFlagsKHR =
    vk::VideoChromaSubsamplingFlagsKHR::TYPE_420;
pub const STREAM_LUMA_DEPTH: vk::VideoComponentBitDepthFlagsKHR =
    vk::VideoComponentBitDepthFlagsKHR::TYPE_8;
pub const STREAM_CHROMA_DEPTH: vk::VideoComponentBitDepthFlagsKHR =
    vk::VideoComponentBitDepthFlagsKHR::TYPE_8;
