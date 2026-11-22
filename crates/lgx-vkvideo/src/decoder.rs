#![allow(clippy::field_reassign_with_default)]
//! Self-contained native Vulkan Video H.265 decoder.
//!
//! It owns its Vulkan instance/device/video queue, a session configured from
//! the stream's VPS/SPS/PPS and a two-slot DPB. `decode` returns CPU NV12 so
//! the existing client renderer can consume it; the zero-copy shared-image
//! variant is a later step.

use ash::ext::external_memory_dma_buf;
use ash::ext::image_drm_format_modifier;
use ash::khr::external_memory_fd;
use ash::vk;
use ash::vk::native as n;

use crate::bitstream::{nal_type, parse_slice_header, PpsInfo, SpsInfo, VpsInfo};
use crate::{dpb::Dpb, std as vkstd, H265_MAIN_PROFILE_IDC, STREAM_CHROMA, STREAM_CHROMA_DEPTH, STREAM_LUMA_DEPTH};

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub poc: i32,
    /// CPU NV12 for the fallback path (empty when `dma` is set).
    pub data: Vec<u8>,
    /// Exported DPB image for zero-copy import.
    pub dma: Option<DmaFrame>,
}

pub struct DmaFrame {
    /// dup()ed fd owned by the caller.
    pub fd: i32,
    /// Stable renderer cache key (session generation + slot).
    pub cache_key: u64,
    pub modifier: u64,
    pub y_offset: u32,
    pub y_pitch: u32,
    pub uv_offset: u32,
    pub uv_pitch: u32,
}

struct Slot {
    image: vk::Image,
    mem: vk::DeviceMemory,
    view: vk::ImageView,
    fd: i32,
    y_offset: u32,
    y_pitch: u32,
    uv_offset: u32,
    uv_pitch: u32,
}

struct Session {
    handle: vk::VideoSessionKHR,
    params: vk::VideoSessionParametersKHR,
    mems: Vec<vk::DeviceMemory>,
    slots: Vec<Slot>,
    out: Slot,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    src_buf: vk::Buffer,
    src_mem: vk::DeviceMemory,
    src_ptr: *mut u8,
    src_size: u64,
    width: u32,
    height: u32,
    slot_layout: [vk::ImageLayout; 2],
    out_layout: vk::ImageLayout,
    sps: SpsInfo,
    pps: PpsInfo,
}

struct Caps {
    max_dpb_slots: u32,
    max_active_reference_pictures: u32,
    std_header_version: vk::ExtensionProperties,
    min_bitstream_buffer_size_alignment: u64,
}

pub struct NativeDecoder {
    // Keeps the Vulkan loader alive while the instance/device are in use.
    _entry: ash::Entry,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    qf: u32,
    video_device: ash::khr::video_queue::Device,
    video_decode: ash::khr::video_decode_queue::Device,
    ext_fd: external_memory_fd::Device,
    drm_ext: image_drm_format_modifier::Device,
    modifier: u64,
    generation: u64,
    caps: Caps,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    session: Option<Session>,
    dpb: Dpb,
    format: vk::Format,
}

fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: vk::MemoryPropertyFlags,
) -> u32 {
    (0..props.memory_type_count)
        .find(|i| bits & (1 << i) != 0 && props.memory_types[*i as usize].property_flags.contains(want))
        .unwrap_or(0)
}

unsafe fn choose_modifier(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    format: vk::Format,
) -> Option<u64> {
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
    let usage = vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR;
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
        if unsafe { instance.get_physical_device_image_format_properties2(pd, &info, &mut ip) }.is_ok() {
            return Some(m.drm_format_modifier);
        }
    }
    None
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
unsafe fn make_export_image(
    device: &ash::Device,
    props: &vk::PhysicalDeviceMemoryProperties,
    ext_fd: &external_memory_fd::Device,
    drm_ext: &image_drm_format_modifier::Device,
    format: vk::Format,
    width: u32,
    height: u32,
    modifier: u64,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView, i32, u32, u32, u32, u32), Box<dyn std::error::Error>> {
    let mut ext_mem = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let modifiers = [modifier];
    let mod_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
    ext_mem.p_next = &mod_list as *const _ as *const std::ffi::c_void;
    let mut info = vk::ImageCreateInfo::default();
    info.p_next = &ext_mem as *const _ as *const std::ffi::c_void;
    info.image_type = vk::ImageType::TYPE_2D;
    info.format = format;
    info.extent = vk::Extent3D { width, height, depth: 1 };
    info.mip_levels = 1;
    info.array_layers = 1;
    info.samples = vk::SampleCountFlags::TYPE_1;
    info.tiling = vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT;
    info.usage = vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR
        | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR
        | vk::ImageUsageFlags::TRANSFER_SRC;
    info.sharing_mode = vk::SharingMode::EXCLUSIVE;
    info.initial_layout = vk::ImageLayout::UNDEFINED;
    let image = unsafe { device.create_image(&info, None)? };
    let mut drm_props = vk::ImageDrmFormatModifierPropertiesEXT::default();
    unsafe { drm_ext.get_image_drm_format_modifier_properties(image, &mut drm_props)? };
    let req = unsafe { device.get_image_memory_requirements(image) };
    let ty = find_memory_type(props, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL);
    let mut export = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    export.p_next = &mut dedicated as *mut _ as *mut std::ffi::c_void;
    let mut alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(ty);
    alloc.p_next = &export as *const _ as *const std::ffi::c_void;
    let mem = unsafe { device.allocate_memory(&alloc, None)? };
    unsafe { device.bind_image_memory(image, mem, 0)? };
    let fd = unsafe {
        ext_fd.get_memory_fd(
            &vk::MemoryGetFdInfoKHR::default()
                .memory(mem)
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
        )?
    };
    let view = unsafe {
        device.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                }),
            None,
        )?
    };
    let plane = |aspect| {
        let sub = vk::ImageSubresource { aspect_mask: aspect, mip_level: 0, array_layer: 0 };
        unsafe { device.get_image_subresource_layout(image, sub) }
    };
    let y = plane(vk::ImageAspectFlags::PLANE_0);
    let uv = plane(vk::ImageAspectFlags::PLANE_1);
    Ok((
        image,
        mem,
        view,
        fd,
        y.offset as u32,
        y.row_pitch as u32,
        uv.offset as u32,
        uv.row_pitch as u32,
    ))
}

impl NativeDecoder {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let entry = unsafe { ash::Entry::load()? };
        let app = vk::ApplicationInfo::default()
            .application_name(c"lgx-native-decoder")
            .api_version(vk::make_api_version(0, 1, 3, 0));
        let instance = unsafe {
            entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None)?
        };
        let video_instance = ash::khr::video_queue::Instance::new(&entry, &instance);
        let phys = unsafe { instance.enumerate_physical_devices()? };
        let pd = phys
            .into_iter()
            .find(|pd| {
                let exts = unsafe { instance.enumerate_device_extension_properties(*pd) }.unwrap_or_default();
                exts.iter().any(|e| unsafe { std::ffi::CStr::from_ptr(e.extension_name.as_ptr()) } == ash::khr::video_decode_h265::NAME)
            })
            .ok_or("no device with VK_KHR_video_decode_h265")?;
        let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        let qf = families
            .iter()
            .position(|f| f.queue_flags.contains(vk::QueueFlags::VIDEO_DECODE_KHR))
            .ok_or("no video decode queue")? as u32;

        let mut h265_profile = vk::VideoDecodeH265ProfileInfoKHR::default();
        h265_profile.std_profile_idc = H265_MAIN_PROFILE_IDC;
        let mut usage = vk::VideoDecodeUsageInfoKHR::default();
        usage.video_usage_hints = vk::VideoDecodeUsageFlagsKHR::DEFAULT;
        usage.p_next = &h265_profile as *const _ as *const std::ffi::c_void;
        let mut profile = vk::VideoProfileInfoKHR::default();
        profile.p_next = &usage as *const _ as *const std::ffi::c_void;
        profile.video_codec_operation = vk::VideoCodecOperationFlagsKHR::DECODE_H265;
        profile.chroma_subsampling = STREAM_CHROMA;
        profile.luma_bit_depth = STREAM_LUMA_DEPTH;
        profile.chroma_bit_depth = STREAM_CHROMA_DEPTH;
        let mut h265_caps = vk::VideoDecodeH265CapabilitiesKHR::default();
        let mut dec_caps = vk::VideoDecodeCapabilitiesKHR::default();
        dec_caps.p_next = &mut h265_caps as *mut _ as *mut std::ffi::c_void;
        let mut caps = vk::VideoCapabilitiesKHR::default();
        caps.p_next = &mut dec_caps as *mut _ as *mut std::ffi::c_void;
        unsafe {
            (video_instance.fp().get_physical_device_video_capabilities_khr)(pd, &profile, &mut caps).result()?
        };

        let available = unsafe { instance.enumerate_device_extension_properties(pd)? };
        let has = |name: &std::ffi::CStr| {
            available.iter().any(|e| unsafe { std::ffi::CStr::from_ptr(e.extension_name.as_ptr()) } == name)
        };
        if !has(external_memory_fd::NAME)
            || !has(external_memory_dma_buf::NAME)
            || !has(image_drm_format_modifier::NAME)
        {
            return Err("device cannot export dma-buf".into());
        }
        let priorities = [1.0f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qf)
            .queue_priorities(&priorities)];
        let exts = [
            ash::khr::video_queue::NAME.as_ptr(),
            ash::khr::video_decode_queue::NAME.as_ptr(),
            ash::khr::video_decode_h265::NAME.as_ptr(),
            external_memory_fd::NAME.as_ptr(),
            external_memory_dma_buf::NAME.as_ptr(),
            image_drm_format_modifier::NAME.as_ptr(),
        ];
        let device = unsafe {
            instance.create_device(
                pd,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_info)
                    .enabled_extension_names(&exts),
                None,
            )?
        };
        let video_device = ash::khr::video_queue::Device::new(&instance, &device);
        let video_decode = ash::khr::video_decode_queue::Device::new(&instance, &device);
        let ext_fd = external_memory_fd::Device::new(&instance, &device);
        let drm_ext = image_drm_format_modifier::Device::new(&instance, &device);
        let queue = unsafe { device.get_device_queue(qf, 0) };
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };
        Ok(Self {
            _entry: entry,
            instance,
            physical: pd,
            device,
            queue,
            qf,
            video_device,
            video_decode,
            ext_fd,
            drm_ext,
            modifier: 0,
            generation: 0,
            caps: Caps {
                max_dpb_slots: caps.max_dpb_slots,
                max_active_reference_pictures: caps.max_active_reference_pictures,
                std_header_version: caps.std_header_version,
                min_bitstream_buffer_size_alignment: caps.min_bitstream_buffer_size_alignment,
            },
            mem_props,
            session: None,
            dpb: Dpb::new(),
            format: vk::Format::G8_B8R8_2PLANE_420_UNORM,
        })
    }

    pub fn configure(
        &mut self,
        vps: &VpsInfo,
        sps: &SpsInfo,
        pps: &PpsInfo,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if sps.width < 16 || sps.height < 16 {
            return Err("bad SPS size".into());
        }
        self.close_session();
        let mut h265_profile = vk::VideoDecodeH265ProfileInfoKHR::default();
        h265_profile.std_profile_idc = H265_MAIN_PROFILE_IDC;
        let mut usage = vk::VideoDecodeUsageInfoKHR::default();
        usage.video_usage_hints = vk::VideoDecodeUsageFlagsKHR::DEFAULT;
        usage.p_next = &h265_profile as *const _ as *const std::ffi::c_void;
        let mut profile = vk::VideoProfileInfoKHR::default();
        profile.p_next = &usage as *const _ as *const std::ffi::c_void;
        profile.video_codec_operation = vk::VideoCodecOperationFlagsKHR::DECODE_H265;
        profile.chroma_subsampling = STREAM_CHROMA;
        profile.luma_bit_depth = STREAM_LUMA_DEPTH;
        profile.chroma_bit_depth = STREAM_CHROMA_DEPTH;
        let mut profile_list = vk::VideoProfileListInfoKHR::default();
        profile_list.profile_count = 1;
        profile_list.p_profiles = &profile;
        let ptl = vkstd::profile_tier_level(sps);
        let dpb_mgr = vkstd::dpb(sps);
        let std_vps = vkstd::vps(vps, &ptl, &dpb_mgr);
        let std_sps = vkstd::sps(sps, &ptl, &dpb_mgr);
        let std_pps = vkstd::pps(pps, sps);
        let extent = vk::Extent2D { width: sps.width, height: sps.height };
        let session_info = {
            let mut info = vk::VideoSessionCreateInfoKHR::default();
            info.queue_family_index = self.qf;
            info.p_video_profile = &profile;
            info.picture_format = self.format;
            info.max_coded_extent = extent;
            info.reference_picture_format = self.format;
            info.max_dpb_slots = self.caps.max_dpb_slots;
            info.max_active_reference_pictures = self.caps.max_active_reference_pictures;
            info.p_std_header_version = &self.caps.std_header_version;
            info
        };
        let mut handle = vk::VideoSessionKHR::null();
        unsafe {
            (self.video_device.fp().create_video_session_khr)(
                self.device.handle(),
                &session_info,
                std::ptr::null(),
                &mut handle,
            )
            .result()?
        };
        let mem_props = self.mem_props;
        let mut count = 0u32;
        unsafe {
            (self.video_device.fp().get_video_session_memory_requirements_khr)(
                self.device.handle(), handle, &mut count, std::ptr::null_mut(),
            ).result()?
        };
        let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); count as usize];
        unsafe {
            (self.video_device.fp().get_video_session_memory_requirements_khr)(
                self.device.handle(), handle, &mut count, reqs.as_mut_ptr(),
            ).result()?
        };
        let mut mems = Vec::new();
        for req in &reqs {
            let ty = find_memory_type(&mem_props, req.memory_requirements.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL);
            let mem = unsafe {
                self.device.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.memory_requirements.size)
                        .memory_type_index(ty),
                    None,
                )?
            };
            mems.push(mem);
        }
        let binds: Vec<_> = reqs.iter().zip(&mems).map(|(req, mem)| {
            vk::BindVideoSessionMemoryInfoKHR::default()
                .memory_bind_index(req.memory_bind_index)
                .memory(*mem)
                .memory_offset(0)
                .memory_size(req.memory_requirements.size)
        }).collect();
        unsafe {
            (self.video_device.fp().bind_video_session_memory_khr)(
                self.device.handle(), handle, binds.len() as u32, binds.as_ptr(),
            ).result()?
        };
        let mut add = vk::VideoDecodeH265SessionParametersAddInfoKHR::default();
        add.std_vps_count = 1; add.p_std_vp_ss = &std_vps;
        add.std_sps_count = 1; add.p_std_sp_ss = &std_sps;
        add.std_pps_count = 1; add.p_std_pp_ss = &std_pps;
        let mut create = vk::VideoDecodeH265SessionParametersCreateInfoKHR::default();
        create.max_std_vps_count = 1; create.max_std_sps_count = 1; create.max_std_pps_count = 1;
        create.p_parameters_add_info = &add;
        let mut params_info = vk::VideoSessionParametersCreateInfoKHR::default();
        params_info.video_session = handle;
        params_info.p_next = &create as *const _ as *const std::ffi::c_void;
        let mut params = vk::VideoSessionParametersKHR::null();
        unsafe {
            (self.video_device.fp().create_video_session_parameters_khr)(
                self.device.handle(), &params_info, std::ptr::null(), &mut params,
            ).result()?
        };
        let format = self.format;
        let make_image = |device: &ash::Device, usage, profile_list: &vk::VideoProfileListInfoKHR| -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView), Box<dyn std::error::Error>> {
            let mut info = vk::ImageCreateInfo::default();
            info.p_next = profile_list as *const _ as *const std::ffi::c_void;
            info.image_type = vk::ImageType::TYPE_2D;
            info.format = format;
            info.extent = vk::Extent3D { width: extent.width, height: extent.height, depth: 1 };
            info.mip_levels = 1; info.array_layers = 1;
            info.samples = vk::SampleCountFlags::TYPE_1;
            info.tiling = vk::ImageTiling::OPTIMAL;
            info.usage = usage;
            info.sharing_mode = vk::SharingMode::EXCLUSIVE;
            info.initial_layout = vk::ImageLayout::UNDEFINED;
            let image = unsafe { device.create_image(&info, None)? };
            let req = unsafe { device.get_image_memory_requirements(image) };
            let ty = find_memory_type(&mem_props, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL);
            let mem = unsafe { device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None)? };
            unsafe { device.bind_image_memory(image, mem, 0)? };
            let view = unsafe {
                device.create_image_view(&vk::ImageViewCreateInfo::default()
                    .image(image).view_type(vk::ImageViewType::TYPE_2D).format(format)
                    .subresource_range(vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 }), None)?
            };
            Ok((image, mem, view))
        };
        let modifier = unsafe { choose_modifier(&self.instance, self.physical, self.format) }
            .ok_or("no DRM modifier for video decode")?;
        self.modifier = modifier;
        self.generation = self.generation.wrapping_add(1);
        let mut slots = Vec::new();
        for _ in 0..2 {
            let (image, mem, view, fd, y_offset, y_pitch, uv_offset, uv_pitch) = unsafe { make_export_image(
                &self.device,
                &self.mem_props,
                &self.ext_fd,
                &self.drm_ext,
                self.format,
                extent.width,
                extent.height,
                modifier,
            ) }?;
            slots.push(Slot { image, mem, view, fd, y_offset, y_pitch, uv_offset, uv_pitch });
        }
        let (out_image, out_mem, out_view) = make_image(&self.device, vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR, &profile_list)?;
        let pool = unsafe { self.device.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(self.qf), None)? };
        let cmd = unsafe { self.device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1))? }[0];
        let fence = unsafe { self.device.create_fence(&vk::FenceCreateInfo::default(), None)? };
        let src_size = 1u64 << 20;
        let (src_buf, src_mem) = unsafe {
            make_host_buffer(
                &self.device,
                &self.mem_props,
                src_size,
                vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR,
            )?
        };
        let src_ptr = unsafe {
            self.device
                .map_memory(src_mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *mut u8
        };
        self.session = Some(Session {
            handle, params, mems, slots, out: Slot { image: out_image, mem: out_mem, view: out_view, fd: -1, y_offset: 0, y_pitch: 0, uv_offset: 0, uv_pitch: 0 },
            pool, cmd, fence,
            src_buf, src_mem, src_ptr, src_size,
            width: sps.width, height: sps.height,
            slot_layout: [vk::ImageLayout::UNDEFINED; 2],
            out_layout: vk::ImageLayout::UNDEFINED,
            sps: sps.clone(), pps: pps.clone(),
        });
        self.dpb = Dpb::new();
        Ok(())
    }

    fn close_session(&mut self) {
        if let Some(s) = self.session.take() {
            unsafe {
                (self.video_device.fp().destroy_video_session_parameters_khr)(self.device.handle(), s.params, std::ptr::null());
                (self.video_device.fp().destroy_video_session_khr)(self.device.handle(), s.handle, std::ptr::null());
                for mem in &s.mems { self.device.free_memory(*mem, None); }
                for slot in s.slots.iter().chain(std::iter::once(&s.out)) {
                    self.device.destroy_image_view(slot.view, None);
                    self.device.destroy_image(slot.image, None);
                    self.device.free_memory(slot.mem, None);
                }
                self.device.destroy_fence(s.fence, None);
                self.device.destroy_command_pool(s.pool, None);
                self.device.unmap_memory(s.src_mem);
                self.device.destroy_buffer(s.src_buf, None);
                self.device.free_memory(s.src_mem, None);
                for slot in &s.slots {
                    if slot.fd >= 0 {
                        libc::close(slot.fd);
                    }
                }
            }
        }
    }

    pub fn decode(&mut self, nal: &[u8]) -> Result<Option<Frame>, Box<dyn std::error::Error>> {
        let Some(session) = self.session.as_mut() else { return Ok(None) };
        let info = parse_slice_header(nal, &session.sps, &session.pps)?;
        let is_idr = matches!(nal_type(nal), 19 | 20);
        let poc = if is_idr { 0 } else { info.poc_lsb as i32 };
        if is_idr {
            self.dpb.reset();
        }
        let mut ref_pocs = Vec::new();
        let mut ref_slots: Vec<usize> = Vec::new();
        for r in info.refs.iter().filter(|r| r.used_by_current) {
            let rp = poc + r.poc_delta;
            if let Some(slot) = self.dpb.slot_of(rp) {
                ref_pocs.push(rp);
                ref_slots.push(slot);
            }
        }
        let cur = self.dpb.choose(&ref_pocs).ok_or("no free DPB slot")?;
        let device = &self.device;
        let queue = self.queue;
        let extent = vk::Extent2D { width: session.width, height: session.height };
        let pics: Vec<vk::VideoPictureResourceInfoKHR> = session.slots.iter().map(|s| {
            vk::VideoPictureResourceInfoKHR::default().coded_extent(extent).image_view_binding(s.view)
        }).collect();
        let out_pic = vk::VideoPictureResourceInfoKHR::default().coded_extent(extent).image_view_binding(session.out.view);
        let mut ref_std: Vec<n::StdVideoDecodeH265ReferenceInfo> = Vec::new();
        let mut ref_dpb: Vec<vk::VideoDecodeH265DpbSlotInfoKHR> = Vec::new();
        let mut ref_infos: Vec<vk::VideoReferenceSlotInfoKHR> = Vec::new();
        for (j, slot) in ref_slots.iter().enumerate() {
            let mut r = unsafe { std::mem::zeroed::<n::StdVideoDecodeH265ReferenceInfo>() };
            r.PicOrderCntVal = ref_pocs[j];
            ref_std.push(r);
            let mut d = vk::VideoDecodeH265DpbSlotInfoKHR::default();
            d.p_std_reference_info = &ref_std[j];
            ref_dpb.push(d);
            let mut s = vk::VideoReferenceSlotInfoKHR::default();
            s.slot_index = *slot as i32;
            s.p_picture_resource = &pics[*slot];
            s.p_next = &ref_dpb[j] as *const _ as *const std::ffi::c_void;
            ref_infos.push(s);
        }
        let mut cur_ref = unsafe { std::mem::zeroed::<n::StdVideoDecodeH265ReferenceInfo>() };
        cur_ref.PicOrderCntVal = poc;
        let mut cur_dpb = vk::VideoDecodeH265DpbSlotInfoKHR::default();
        cur_dpb.p_std_reference_info = &cur_ref;
        let mut setup = vk::VideoReferenceSlotInfoKHR::default();
        setup.slot_index = cur as i32;
        setup.p_picture_resource = &pics[cur];
        setup.p_next = &cur_dpb as *const _ as *const std::ffi::c_void;
        let align = self.caps.min_bitstream_buffer_size_alignment as usize;
        let len = (3 + nal.len() + align - 1) & !(align - 1);
        if len as u64 > session.src_size {
            let new_size = (len as u64).next_power_of_two().max(1 << 20);
            unsafe {
                device.unmap_memory(session.src_mem);
                device.destroy_buffer(session.src_buf, None);
                device.free_memory(session.src_mem, None);
            }
            let (src_buf, src_mem) = unsafe {
                make_host_buffer(
                    device,
                    &self.mem_props,
                    new_size,
                    vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR,
                )?
            };
            session.src_buf = src_buf;
            session.src_mem = src_mem;
            session.src_size = new_size;
            session.src_ptr = unsafe {
                device.map_memory(src_mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *mut u8
            };
        }
        let buf = session.src_buf;
        unsafe {
            std::ptr::copy_nonoverlapping([0u8, 0, 1].as_ptr(), session.src_ptr, 3);
            std::ptr::copy_nonoverlapping(nal.as_ptr(), session.src_ptr.add(3), nal.len());
        }
        let range = vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 };
        unsafe {
            device.reset_fences(&[session.fence])?;
            device.reset_command_buffer(session.cmd, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(session.cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
            if session.slot_layout[cur] != vk::ImageLayout::VIDEO_DECODE_DPB_KHR {
                let old = session.slot_layout[cur];
                let b = vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(if old == vk::ImageLayout::UNDEFINED { vk::PipelineStageFlags2::NONE } else { vk::PipelineStageFlags2::ALL_COMMANDS })
                    .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
                    .dst_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
                    .old_layout(old)
                    .new_layout(vk::ImageLayout::VIDEO_DECODE_DPB_KHR)
                    .image(session.slots[cur].image)
                    .subresource_range(range);
                device.cmd_pipeline_barrier2(session.cmd, &vk::DependencyInfo::default().image_memory_barriers(&[b]));
            }
            if session.out_layout == vk::ImageLayout::UNDEFINED {
                let b = vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::NONE)
                    .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
                    .dst_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::VIDEO_DECODE_DST_KHR)
                    .image(session.out.image)
                    .subresource_range(range);
                device.cmd_pipeline_barrier2(session.cmd, &vk::DependencyInfo::default().image_memory_barriers(&[b]));
            }
            let mut flags = std::mem::zeroed::<n::StdVideoDecodeH265PictureInfoFlags>();
            flags.set_IrapPicFlag(u32::from(is_idr));
            flags.set_IdrPicFlag(u32::from(is_idr));
            flags.set_IsReference(1);
            let mut pic = std::mem::zeroed::<n::StdVideoDecodeH265PictureInfo>();
            pic.flags = flags;
            pic.PicOrderCntVal = poc;
            pic.NumBitsForSTRefPicSetInSlice = info.rps_bits;
            pic.RefPicSetStCurrBefore = [0xff; 8];
            pic.RefPicSetStCurrAfter = [0xff; 8];
            pic.RefPicSetLtCurr = [0xff; 8];
            for (j, r) in info.refs.iter().filter(|r| r.used_by_current).enumerate() {
                if j < ref_slots.len() {
                    if r.poc_delta < 0 { pic.RefPicSetStCurrBefore[j] = ref_slots[j] as u8; }
                    else { pic.RefPicSetStCurrAfter[j] = ref_slots[j] as u8; }
                }
            }
            let mut offsets = [0u32];
            let mut h265 = vk::VideoDecodeH265PictureInfoKHR::default();
            h265.p_std_picture_info = &pic;
            h265.slice_segment_count = 1;
            h265.p_slice_segment_offsets = offsets.as_mut_ptr();
            let mut decode = vk::VideoDecodeInfoKHR::default();
            decode.p_next = &h265 as *const _ as *const std::ffi::c_void;
            decode.src_buffer = buf;
            decode.src_buffer_range = len as u64;
            decode.dst_picture_resource = out_pic;
            decode.p_setup_reference_slot = &setup;
            decode.reference_slot_count = ref_infos.len() as u32;
            decode.p_reference_slots = if ref_infos.is_empty() { std::ptr::null() } else { ref_infos.as_ptr() };
            let mut begin_slots = ref_infos.clone();
            begin_slots.push(setup);
            let mut begin = vk::VideoBeginCodingInfoKHR::default();
            begin.video_session = session.handle;
            begin.video_session_parameters = session.params;
            begin.reference_slot_count = begin_slots.len() as u32;
            begin.p_reference_slots = begin_slots.as_ptr();
            (self.video_device.fp().cmd_begin_video_coding_khr)(session.cmd, &begin);
            (self.video_decode.fp().cmd_decode_video_khr)(session.cmd, &decode);
            (self.video_device.fp().cmd_end_video_coding_khr)(session.cmd, &vk::VideoEndCodingInfoKHR::default());
            device.end_command_buffer(session.cmd)?;
            let list = [session.cmd];
            device.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&list)], session.fence)?;
            device.wait_for_fences(&[session.fence], true, u64::MAX)?;
            let fd = libc::dup(session.slots[cur].fd);
            if fd < 0 {
                return Err("cannot dup the DPB dma-buf".into());
            }
            let dma = DmaFrame {
                fd,
                cache_key: (self.generation << 8) | (cur as u64 + 1),
                modifier: self.modifier,
                y_offset: session.slots[cur].y_offset,
                y_pitch: session.slots[cur].y_pitch,
                uv_offset: session.slots[cur].uv_offset,
                uv_pitch: session.slots[cur].uv_pitch,
            };
            session.slot_layout[cur] = vk::ImageLayout::VIDEO_DECODE_DPB_KHR;
            session.out_layout = vk::ImageLayout::VIDEO_DECODE_DST_KHR;
            self.dpb.commit(cur, poc);
            Ok(Some(Frame {
                width: session.width,
                height: session.height,
                poc,
                data: Vec::new(),
                dma: Some(dma),
            }))
        }
    }
}

unsafe fn make_host_buffer(
    device: &ash::Device,
    props: &vk::PhysicalDeviceMemoryProperties,
    size: u64,
    usage: vk::BufferUsageFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory), Box<dyn std::error::Error>> {
    let buffer = unsafe { device.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(usage).sharing_mode(vk::SharingMode::EXCLUSIVE), None)? };
    let req = unsafe { device.get_buffer_memory_requirements(buffer) };
    let ty = find_memory_type(props, req.memory_type_bits, vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT);
    let mem = unsafe { device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None)? };
    unsafe { device.bind_buffer_memory(buffer, mem, 0)? };
    Ok((buffer, mem))
}

impl Drop for NativeDecoder {
    fn drop(&mut self) {
        self.close_session();
        unsafe {
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

