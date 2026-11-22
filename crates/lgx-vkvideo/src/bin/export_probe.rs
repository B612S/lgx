#![allow(clippy::field_reassign_with_default)]
//! Check whether a video-decode DPB image can be created with a DRM modifier
//! and exported as dma-buf for the client's existing import path.

use ash::vk;
use std::ffi::CStr;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let entry = unsafe { ash::Entry::load()? };
    let app = vk::ApplicationInfo::default().application_name(c"lgx-export-probe").api_version(vk::make_api_version(0,1,3,0));
    let instance = unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None)? };
    let phys = unsafe { instance.enumerate_physical_devices()? };
    let pd = phys.into_iter().find(|pd| {
        let exts = unsafe { instance.enumerate_device_extension_properties(*pd) }.unwrap_or_default();
        exts.iter().any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == ash::khr::video_decode_h265::NAME)
    }).ok_or("no video device")?;
    let fams = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    let qf = fams.iter().position(|f| f.queue_flags.contains(vk::QueueFlags::VIDEO_DECODE_KHR)).ok_or("no qf")? as u32;
    let prio = [1.0f32];
    let qi = [vk::DeviceQueueCreateInfo::default().queue_family_index(qf).queue_priorities(&prio)];
    let exts = [
        ash::khr::video_queue::NAME.as_ptr(),
        ash::khr::video_decode_queue::NAME.as_ptr(),
        ash::khr::video_decode_h265::NAME.as_ptr(),
        ash::khr::external_memory_fd::NAME.as_ptr(),
        ash::ext::external_memory_dma_buf::NAME.as_ptr(),
        ash::ext::image_drm_format_modifier::NAME.as_ptr(),
    ];
    let device = unsafe { instance.create_device(pd, &vk::DeviceCreateInfo::default().queue_create_infos(&qi).enabled_extension_names(&exts), None)? };
    let ext = ash::khr::external_memory_fd::Device::new(&instance, &device);
    let format = vk::Format::G8_B8R8_2PLANE_420_UNORM;

    let mut list1 = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut props1 = vk::FormatProperties2::default().push_next(&mut list1);
    unsafe { instance.get_physical_device_format_properties2(pd, format, &mut props1) };
    let count = list1.drm_format_modifier_count;
    let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); count as usize];
    let mut list2 = vk::DrmFormatModifierPropertiesListEXT::default();
    list2.drm_format_modifier_count = count;
    list2.p_drm_format_modifier_properties = mods.as_mut_ptr();
    let mut props2 = vk::FormatProperties2::default().push_next(&mut list2);
    unsafe { instance.get_physical_device_format_properties2(pd, format, &mut props2) };
    println!("{} DRM modifiers for NV12", mods.len());
    let usage = vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR | vk::ImageUsageFlags::TRANSFER_SRC;
    let mut chosen = None;
    for m in &mods {
        let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(m.drm_format_modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .push_next(&mut drm);
        let mut ip = vk::ImageFormatProperties2::default();
        let ok = unsafe { instance.get_physical_device_image_format_properties2(pd, &info, &mut ip) };
        println!("  modifier {:#x}: {:?}", m.drm_format_modifier, ok.err());
        if ok.is_ok() && chosen.is_none() {
            chosen = Some(m.drm_format_modifier);
        }
    }
    let Some(modifier) = chosen else { println!("no modifier supports video decode usage"); return Ok(()) };
    println!("chosen modifier {modifier:#x}");

    let mut ext_mem = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let modifiers = [modifier];
    let mod_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
    ext_mem.p_next = &mod_list as *const _ as *const std::ffi::c_void;
    let mut image_info = vk::ImageCreateInfo::default();
    image_info.p_next = &ext_mem as *const _ as *const std::ffi::c_void;
    image_info.image_type = vk::ImageType::TYPE_2D;
    image_info.format = format;
    image_info.extent = vk::Extent3D { width: 1920, height: 1080, depth: 1 };
    image_info.mip_levels = 1;
    image_info.array_layers = 1;
    image_info.samples = vk::SampleCountFlags::TYPE_1;
    image_info.tiling = vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT;
    image_info.usage = usage;
    image_info.sharing_mode = vk::SharingMode::EXCLUSIVE;
    image_info.initial_layout = vk::ImageLayout::UNDEFINED;
    let image = unsafe { device.create_image(&image_info, None)? };
    let mut drm_props = vk::ImageDrmFormatModifierPropertiesEXT::default();
    unsafe { ash::ext::image_drm_format_modifier::Device::new(&instance, &device).get_image_drm_format_modifier_properties(image, &mut drm_props)? };
    println!("actual modifier {:#x}", drm_props.drm_format_modifier);

    let req = unsafe { device.get_image_memory_requirements(image) };
    let props = unsafe { instance.get_physical_device_memory_properties(pd) };
    let ty = (0..props.memory_type_count).find(|i| req.memory_type_bits & (1 << i) != 0 && props.memory_types[*i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)).unwrap_or(0);
    let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    export.p_next = &mut dedicated as *mut _ as *mut std::ffi::c_void;
    let mut alloc = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty);
    alloc.p_next = &export as *const _ as *const std::ffi::c_void;
    let mem = unsafe { device.allocate_memory(&alloc, None)? };
    unsafe { device.bind_image_memory(image, mem, 0)? };
    let fd = unsafe { ext.get_memory_fd(&vk::MemoryGetFdInfoKHR::default().memory(mem).handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT))? };
    println!("exported dma-buf fd {fd}");
    for aspect in [vk::ImageAspectFlags::PLANE_0, vk::ImageAspectFlags::PLANE_1] {
        let sub = vk::ImageSubresource { aspect_mask: aspect, mip_level: 0, array_layer: 0 };
        let layout = unsafe { device.get_image_subresource_layout(image, sub) };
        println!("  {aspect:?}: offset {} pitch {} size {}", layout.offset, layout.row_pitch, layout.size);
    }
    Ok(())
}
