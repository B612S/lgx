//! Selection policy: capability facts in, ordered plan out.
//!
//! The policy is deliberately pure and vendor-name free. The only vendor
//! knowledge lives in `DrmNode::is_*`, which keys off the kernel driver, and
//! even there the plan degrades to VAAPI/software instead of failing.

use crate::drm::DrmNode;
use crate::vaapi::VaCaps;
use crate::vulkan::VkDeviceInfo;

/// Which encoder helper the server should start for the capture GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    /// `lgx-vaenc` on the capture render node (Intel, AMD, or NVIDIA through
    /// libva-nvidia-driver, which is decode-only but harmless to try).
    Vaapi,
    /// `lgx-nvenc` on the NVIDIA GPU.
    Nvenc,
}

impl EncoderBackend {
    pub fn name(self) -> &'static str {
        match self {
            EncoderBackend::Vaapi => "VAAPI",
            EncoderBackend::Nvenc => "NVENC",
        }
    }
}

/// Which decoder the client should try, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeBackend {
    Vulkan,
    Vaapi,
    Cuda,
    Software,
}

impl DecodeBackend {
    pub fn name(self) -> &'static str {
        match self {
            DecodeBackend::Vulkan => "Vulkan Video",
            DecodeBackend::Vaapi => "VAAPI",
            DecodeBackend::Cuda => "CUDA/NVDEC",
            DecodeBackend::Software => "software",
        }
    }
}

/// The render node a Vulkan device belongs to, when the driver reports it.
pub fn node_for_vulkan<'a>(vk: &VkDeviceInfo, nodes: &'a [DrmNode]) -> Option<&'a DrmNode> {
    let (major, minor) = vk.render?;
    crate::drm::render_node_by_id(nodes, major, minor)
}

pub fn va_caps_for_node<'a>(va: &'a [VaCaps], node: &DrmNode) -> Option<&'a VaCaps> {
    va.iter().find(|c| c.node == node.path)
}

/// Encoder for a capture node. NVIDIA prefers NVENC even when libva-nvidia
/// advertises an encode entry point (it does not today), because NVENC is the
/// supported encoder on that driver; every other driver uses VAAPI.
pub fn encoder_for_node(
    node: Option<&DrmNode>,
    va: Option<&VaCaps>,
    nvenc_available: bool,
) -> Option<EncoderBackend> {
    if let Some(node) = node {
        if node.is_nvidia() && nvenc_available {
            return Some(EncoderBackend::Nvenc);
        }
        if let Some(caps) = va {
            if caps.hevc_encode {
                return Some(EncoderBackend::Vaapi);
            }
        }
        if node.is_nvidia() {
            // No NVENC (old GPU or missing driver); VAAPI on NVIDIA is
            // decode-only, so there is no hardware encoder left.
            return None;
        }
    }
    va.filter(|c| c.hevc_encode).map(|_| EncoderBackend::Vaapi)
}

/// Ordered decoder plan for the client. `cuda` is whether the CUDA runtime is
/// usable at all (the shim can still fail later and the caller falls back).
///
/// Vulkan Video is first when the driver advertises it: it is the only path
/// that keeps the decoded surface on the same GPU as the renderer without a
/// vendor library. After that the plan follows the renderer's DRM node so the
/// decoder and the renderer share one driver; VAAPI on a different node would
/// still work, but only through the CPU upload path.
pub fn decode_plan(
    vk: &VkDeviceInfo,
    nodes: &[DrmNode],
    va: &[VaCaps],
    cuda: bool,
) -> Vec<DecodeBackend> {
    let mut plan = Vec::new();
    if vk.video_decode_h265 {
        plan.push(DecodeBackend::Vulkan);
    }
    let node = node_for_vulkan(vk, nodes);
    let same_node_va = node.and_then(|n| va_caps_for_node(va, n)).is_some_and(|c| c.hevc_decode);
    let nvidia_node = node.is_some_and(|n| n.is_nvidia())
        || vk.driver.to_ascii_lowercase().contains("nvidia");
    if nvidia_node {
        if cuda {
            plan.push(DecodeBackend::Cuda);
        }
        if same_node_va {
            plan.push(DecodeBackend::Vaapi);
        }
    } else {
        if same_node_va {
            plan.push(DecodeBackend::Vaapi);
        }
        if cuda {
            plan.push(DecodeBackend::Cuda);
        }
    }
    if !plan.contains(&DecodeBackend::Vaapi) && va.iter().any(|c| c.hevc_decode) {
        plan.push(DecodeBackend::Vaapi);
    }
    plan.push(DecodeBackend::Software);
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn node(driver: &str, vendor: u16, minor: u32) -> DrmNode {
        DrmNode {
            path: PathBuf::from(format!("/dev/dri/renderD{minor}")),
            name: format!("renderD{minor}"),
            render: true,
            major: 226,
            minor,
            driver: driver.into(),
            vendor,
            device: 0,
        }
    }

    fn va(node: &DrmNode, encode: bool, decode: bool) -> VaCaps {
        VaCaps {
            node: node.path.clone(),
            vendor: "test".into(),
            hevc_decode: decode,
            hevc_decode_10: false,
            hevc_encode: encode,
            hevc_encode_low_power: false,
            vpp: true,
        }
    }

    fn vk(render: Option<(u32, u32)>, video: bool) -> VkDeviceInfo {
        VkDeviceInfo {
            index: 0,
            name: "test".into(),
            driver: "test".into(),
            device_type: "discrete".into(),
            vendor_id: 0,
            device_id: 0,
            render,
            primary: None,
            video_decode_h265: video,
            video_encode_h265: false,
            dmabuf_import: true,
            dmabuf_export: true,
        }
    }

    #[test]
    fn nvidia_prefers_nvenc_and_cuda() {
        let intel = node("xe", 0x8086, 128);
        let nv = node("nvidia-drm", 0x10de, 129);
        assert_eq!(
            encoder_for_node(Some(&nv), Some(&va(&nv, false, true)), true),
            Some(EncoderBackend::Nvenc)
        );
        let plan = decode_plan(&vk(Some((226, 129)), false), &[intel.clone(), nv], &[va(&intel, false, true)], true);
        assert_eq!(plan[0], DecodeBackend::Cuda);
        assert!(plan.contains(&DecodeBackend::Vaapi));
        assert_eq!(*plan.last().unwrap(), DecodeBackend::Software);
    }

    #[test]
    fn amd_uses_vaapi_on_the_render_node() {
        let amd = node("amdgpu", 0x1002, 128);
        assert_eq!(
            encoder_for_node(Some(&amd), Some(&va(&amd, true, true)), true),
            Some(EncoderBackend::Vaapi)
        );
        let plan = decode_plan(&vk(Some((226, 128)), false), std::slice::from_ref(&amd), &[va(&amd, true, true)], true);
        assert_eq!(&plan[..2], &[DecodeBackend::Vaapi, DecodeBackend::Cuda]);
    }

    #[test]
    fn vulkan_video_goes_first_when_advertised() {
        let intel = node("xe", 0x8086, 128);
        let plan = decode_plan(&vk(Some((226, 128)), true), std::slice::from_ref(&intel), &[va(&intel, true, true)], false);
        assert_eq!(plan, vec![DecodeBackend::Vulkan, DecodeBackend::Vaapi, DecodeBackend::Software]);
    }

    #[test]
    fn no_encoder_without_vaapi_or_nvenc() {
        let nv = node("nvidia-drm", 0x10de, 128);
        assert_eq!(encoder_for_node(Some(&nv), None, false), None);
    }
}
