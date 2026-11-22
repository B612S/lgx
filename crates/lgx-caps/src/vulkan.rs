//! Vulkan physical-device capabilities.
//!
//! A window or surface is not needed: the client uses this before it picks a
//! decoder, and the server uses it to know whether a GPU can import the
//! captured dma-buf. `VK_EXT_physical_device_drm` is the key extension — it
//! reports the DRM render node of each Vulkan device, which is how lgx matches
//! a VAAPI/CUDA decoder to the renderer instead of guessing from the device
//! name.

use std::ffi::CStr;

use ash::vk;

/// One Vulkan physical device with the facts that decide the video path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VkDeviceInfo {
    pub index: usize,
    pub name: String,
    /// `driverName` from `VK_KHR_driver_properties` (`Intel open-source Mesa
    /// driver`, `NVIDIA`, `AMD proprietary driver`, ...).
    pub driver: String,
    pub device_type: String,
    pub vendor_id: u32,
    pub device_id: u32,
    /// DRM render node reported by `VK_EXT_physical_device_drm`.
    pub render: Option<(u32, u32)>,
    pub primary: Option<(u32, u32)>,
    /// `VK_KHR_video_queue` + `VK_KHR_video_decode_h265`.
    pub video_decode_h265: bool,
    /// `VK_KHR_video_queue` + `VK_KHR_video_encode_h265`.
    pub video_encode_h265: bool,
    /// Can import a decoder's dma-buf without a CPU round trip.
    pub dmabuf_import: bool,
    /// Can export a dma-buf (needed by a decoder/encoder that produces frames
    /// for someone else, e.g. a Vulkan Video decoder feeding the renderer).
    pub dmabuf_export: bool,
}

fn device_type(t: vk::PhysicalDeviceType) -> &'static str {
    match t {
        vk::PhysicalDeviceType::INTEGRATED_GPU => "integrated",
        vk::PhysicalDeviceType::DISCRETE_GPU => "discrete",
        vk::PhysicalDeviceType::VIRTUAL_GPU => "virtual",
        vk::PhysicalDeviceType::CPU => "cpu",
        _ => "other",
    }
}

/// Enumerates every Vulkan physical device. An empty list is a normal result
/// on a machine without a Vulkan ICD; an `Err` means the loader itself could
/// not be used.
pub fn devices() -> Result<Vec<VkDeviceInfo>, String> {
    let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("cannot load Vulkan: {e}"))?;
    let app_name = std::ffi::CString::new("lgx-caps").expect("static");
    let app = vk::ApplicationInfo::default()
        .application_name(&app_name)
        .api_version(vk::make_api_version(0, 1, 3, 0));
    let create = vk::InstanceCreateInfo::default().application_info(&app);
    let instance = unsafe { entry.create_instance(&create, None) }
        .map_err(|e| format!("vkCreateInstance: {e:?}"))?;
    let mut out = Vec::new();
    let physical = unsafe { instance.enumerate_physical_devices() }
        .map_err(|e| format!("vkEnumeratePhysicalDevices: {e:?}"))?;
    for (index, pd) in physical.into_iter().enumerate() {
        let props = unsafe { instance.get_physical_device_properties(pd) };
        let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let extensions: Vec<String> = unsafe {
            instance
                .enumerate_device_extension_properties(pd)
                .map(|exts| {
                    exts.iter()
                        .map(|e| CStr::from_ptr(e.extension_name.as_ptr()).to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default()
        };
        let has = |name: &str| extensions.iter().any(|e| e == name);
        let driver = if has("VK_KHR_driver_properties") {
            let mut driver_props = vk::PhysicalDeviceDriverProperties::default();
            let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver_props);
            unsafe { instance.get_physical_device_properties2(pd, &mut props2) };
            unsafe { CStr::from_ptr(driver_props.driver_name.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        } else {
            String::new()
        };
        // The DRM properties struct may only be chained when the extension is
        // advertised; drivers are free to reject unknown pNext structs.
        let (render, primary) = if has("VK_EXT_physical_device_drm") {
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            unsafe { instance.get_physical_device_properties2(pd, &mut props2) };
            let render = drm
                .has_render
                .eq(&vk::TRUE)
                .then_some((drm.render_major as u32, drm.render_minor as u32));
            let primary = drm
                .has_primary
                .eq(&vk::TRUE)
                .then_some((drm.primary_major as u32, drm.primary_minor as u32));
            (render, primary)
        } else {
            (None, None)
        };
        let queue = has("VK_KHR_video_queue");
        out.push(VkDeviceInfo {
            index,
            name,
            driver,
            device_type: device_type(props.device_type).to_string(),
            vendor_id: props.vendor_id,
            device_id: props.device_id,
            render,
            primary,
            video_decode_h265: queue && has("VK_KHR_video_decode_h265"),
            video_encode_h265: queue && has("VK_KHR_video_encode_h265"),
            dmabuf_import: has("VK_KHR_external_memory_fd")
                && has("VK_EXT_external_memory_dma_buf")
                && has("VK_EXT_image_drm_format_modifier"),
            dmabuf_export: has("VK_KHR_external_memory_fd") && has("VK_EXT_external_memory_dma_buf"),
        });
    }
    unsafe { instance.destroy_instance(None) };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_type_names() {
        assert_eq!(device_type(vk::PhysicalDeviceType::DISCRETE_GPU), "discrete");
        assert_eq!(device_type(vk::PhysicalDeviceType::INTEGRATED_GPU), "integrated");
    }
}
