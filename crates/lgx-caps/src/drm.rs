//! DRM node discovery.
//!
//! The kernel driver name is the only vendor fact that is stable across
//! drivers and containers: `/sys/class/drm/<node>/device/driver` is `i915`,
//! `xe`, `amdgpu`, `radeon`, `nvidia-drm` or `virtio_gpu`, while the PCI
//! vendor id is `0x8086`, `0x1002` or `0x10de`. Both are exposed; callers
//! should prefer the driver name and use the vendor id only as a fallback.

use std::fs;
use std::path::{Path, PathBuf};

/// One DRM device node (`cardN` or `renderDN`) with the facts lgx needs to
/// decide whether it can decode, encode or import dma-bufs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrmNode {
    /// `/dev/dri/renderD128` or `/dev/dri/card0`.
    pub path: PathBuf,
    /// `renderD128` / `card0`.
    pub name: String,
    /// True for a render node (no modesetting, safe for compute/encode).
    pub render: bool,
    /// DRM major/minor from `<node>/dev`, used to match
    /// `VK_EXT_physical_device_drm` and libva devices.
    pub major: u32,
    pub minor: u32,
    /// Kernel driver name, for example `i915`, `xe`, `amdgpu`, `nvidia-drm`.
    pub driver: String,
    pub vendor: u16,
    pub device: u16,
}

impl DrmNode {
    pub fn is_nvidia(&self) -> bool {
        self.driver == "nvidia-drm" || self.vendor == 0x10de
    }

    pub fn is_intel(&self) -> bool {
        self.driver == "i915" || self.driver == "xe" || self.vendor == 0x8086
    }

    pub fn is_amd(&self) -> bool {
        self.driver == "amdgpu" || self.driver == "radeon" || self.vendor == 0x1002
    }

    /// A short vendor name for logs.
    pub fn vendor_name(&self) -> &'static str {
        if self.is_nvidia() {
            "nvidia"
        } else if self.is_intel() {
            "intel"
        } else if self.is_amd() {
            "amd"
        } else if self.driver == "virtio_gpu" {
            "virtio"
        } else {
            "other"
        }
    }
}

fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn parse_hex16(s: &str) -> Option<u16> {
    let s = s.trim().trim_start_matches("0x");
    u16::from_str_radix(s, 16).ok()
}

/// True for `card0`, `card1`, ... and for connector entries like
/// `card0-DP-1` (which must be skipped, they are not device nodes).
fn is_card_name(name: &str) -> bool {
    name.strip_prefix("card").is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

fn is_render_name(name: &str) -> bool {
    name.strip_prefix("renderD").is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Enumerates the DRM nodes. Render nodes come first, then cards, each group
/// sorted by name so `renderD128` is deterministic.
pub fn drm_nodes() -> Vec<DrmNode> {
    let mut nodes = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return nodes;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let render = is_render_name(&name);
        if !render && !is_card_name(&name) {
            continue;
        }
        let dev_dir = entry.path().join("device");
        if !dev_dir.exists() {
            continue;
        }
        // The DRM node's own `dev` attribute (major:minor), not the PCI
        // device's one: every node has its own render/card number.
        let Some(dev) = read_trim(&entry.path().join("dev")) else {
            continue;
        };
        let Some((major, minor)) = dev.split_once(':') else {
            continue;
        };
        let (Ok(major), Ok(minor)) = (major.parse::<u32>(), minor.parse::<u32>()) else {
            continue;
        };
        let driver = fs::read_link(dev_dir.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let vendor = read_trim(&dev_dir.join("vendor")).and_then(|v| parse_hex16(&v)).unwrap_or(0);
        let device = read_trim(&dev_dir.join("device")).and_then(|v| parse_hex16(&v)).unwrap_or(0);
        let path = PathBuf::from("/dev/dri").join(&name);
        if !path.exists() {
            continue;
        }
        nodes.push(DrmNode {
            path,
            name,
            render,
            major,
            minor,
            driver,
            vendor,
            device,
        });
    }
    nodes.sort_by(|a, b| {
        b.render
            .cmp(&a.render)
            .then_with(|| a.name.cmp(&b.name))
    });
    nodes
}

/// The first render node whose kernel driver matches `driver`.
pub fn render_node_for_driver<'a>(nodes: &'a [DrmNode], driver: &str) -> Option<&'a DrmNode> {
    nodes.iter().find(|n| n.render && n.driver == driver)
}

/// The render node that carries the given DRM major/minor pair.
pub fn render_node_by_id(nodes: &[DrmNode], major: u32, minor: u32) -> Option<&DrmNode> {
    nodes.iter().find(|n| n.render && n.major == major && n.minor == minor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_entries_are_not_cards() {
        assert!(is_card_name("card0"));
        assert!(is_card_name("card12"));
        assert!(!is_card_name("card0-DP-1"));
        assert!(!is_card_name("card"));
        assert!(is_render_name("renderD128"));
        assert!(!is_render_name("renderD"));
    }

    #[test]
    fn vendor_classification_prefers_driver() {
        let mut n = DrmNode {
            path: "/dev/dri/renderD128".into(),
            name: "renderD128".into(),
            render: true,
            major: 226,
            minor: 128,
            driver: "xe".into(),
            vendor: 0,
            device: 0,
        };
        assert!(n.is_intel());
        assert_eq!(n.vendor_name(), "intel");
        n.driver = "nvidia-drm".into();
        n.vendor = 0x10de;
        assert!(n.is_nvidia());
        assert_eq!(n.vendor_name(), "nvidia");
        n.driver = "amdgpu".into();
        n.vendor = 0x1002;
        assert!(n.is_amd());
        assert_eq!(n.vendor_name(), "amd");
    }
}
