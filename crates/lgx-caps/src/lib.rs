//! Runtime capability detection for the lgx server and client.
//!
//! lgx supports Intel, AMD and NVIDIA GPUs, wlroots and portal-based Wayland
//! compositors, and several encoder/decoder APIs. Hardcoding a vendor (or the
//! render node order) breaks on hybrid laptops and on every driver update, so
//! both sides ask this crate what the machine can actually do and then pick a
//! path from those facts:
//!
//! - [`drm`] lists the render/card nodes, their kernel driver and vendor.
//! - [`vaapi`] probes a render node for VAAPI profiles and entry points
//!   (decode, encode, video processing) by dlopen'ing libva, so a machine
//!   without libva still runs the software path.
//! - [`vulkan`] enumerates Vulkan physical devices together with their
//!   Vulkan Video extensions, dma-buf import support and — with
//!   `VK_EXT_physical_device_drm` — the DRM node they belong to. The node is
//!   what lets us match a decoder to the renderer instead of guessing.
//! - [`select`] turns those facts into an ordered plan; it is pure and unit
//!   tested, so the policy can change without touching the probes.

pub mod cuda;
pub mod drm;
pub mod select;
pub mod vaapi;
pub mod vulkan;

pub use drm::{drm_nodes, DrmNode};
pub use select::{decode_plan, encoder_for_node, DecodeBackend, EncoderBackend};
pub use vaapi::{probe as probe_vaapi, VaCaps};
pub use vulkan::{devices as vulkan_devices, VkDeviceInfo};
