#![allow(clippy::field_reassign_with_default)]
//! Capability probe for the native Vulkan Video H.265 decoder.
//!
//! It picks a device with `VK_KHR_video_decode_h265`, queries the Main/8-bit/
//! 4:2:0 decode capabilities and formats, creates a decode session and binds
//! its memory. It does not decode anything yet.

use ash::vk;
use ash::vk::native as n;
use lgx_vkvideo::bitstream::{annexb_nals, nal_type, parse_slice_header, PpsInfo, SpsInfo, VpsInfo};
use lgx_vkvideo::dpb::Dpb;
use lgx_vkvideo::std as vkstd;
use lgx_vkvideo::{H265_MAIN_PROFILE_IDC, STREAM_CHROMA, STREAM_CHROMA_DEPTH, STREAM_LUMA_DEPTH};
use std::ffi::CStr;
use std::ptr;

fn ext_name(props: &vk::ExtensionProperties) -> &CStr {
    unsafe { CStr::from_ptr(props.extension_name.as_ptr()) }
}

// Parameter sets of the live 1920x1080@160 server stream.
const SERVER_VPS: &[u8] = &[
    0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00,
    0x03, 0x00, 0x00, 0x03, 0x00, 0x99, 0x2c, 0x0c, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00,
    0x03, 0x02, 0x83, 0xa0,
];
const SERVER_SPS: &[u8] = &[
    0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00,
    0x03, 0x00, 0x99, 0xa0, 0x03, 0xc0, 0x80, 0x10, 0xe5, 0x89, 0x2e, 0x49, 0x1b, 0x6a, 0x9b,
    0x80, 0x80, 0x80, 0x82, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x01, 0x41, 0xaf,
    0x84, 0x02, 0x08,
];
const SERVER_PPS: &[u8] = &[0x44, 0x01, 0xc0, 0x62, 0x19, 0x30, 0x22, 0x40];


fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: vk::MemoryPropertyFlags,
) -> u32 {
    (0..props.memory_type_count)
        .find(|i| bits & (1 << i) != 0 && props.memory_types[*i as usize].property_flags.contains(want))
        .unwrap_or(0)
}

unsafe fn make_video_image(
    device: &ash::Device,
    props: &vk::PhysicalDeviceMemoryProperties,
    profile_list: &vk::VideoProfileListInfoKHR<'_>,
    format: vk::Format,
    width: u32,
    height: u32,
    usage: vk::ImageUsageFlags,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView), Box<dyn std::error::Error>> {
    let mut info = vk::ImageCreateInfo::default();
    info.p_next = profile_list as *const _ as *const std::ffi::c_void;
    info.image_type = vk::ImageType::TYPE_2D;
    info.format = format;
    info.extent = vk::Extent3D { width, height, depth: 1 };
    info.mip_levels = 1;
    info.array_layers = 1;
    info.samples = vk::SampleCountFlags::TYPE_1;
    info.tiling = vk::ImageTiling::OPTIMAL;
    info.usage = usage;
    info.sharing_mode = vk::SharingMode::EXCLUSIVE;
    info.initial_layout = vk::ImageLayout::UNDEFINED;
    let image = device.create_image(&info, None)?;
    let req = device.get_image_memory_requirements(image);
    let ty = find_memory_type(props, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL);
    let mem = device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(ty),
        None,
    )?;
    device.bind_image_memory(image, mem, 0)?;
    let view = device.create_image_view(
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
    )?;
    Ok((image, mem, view))
}

unsafe fn make_host_buffer(
    device: &ash::Device,
    props: &vk::PhysicalDeviceMemoryProperties,
    size: u64,
    usage: vk::BufferUsageFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory), Box<dyn std::error::Error>> {
    let buffer = device.create_buffer(
        &vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE),
        None,
    )?;
    let req = device.get_buffer_memory_requirements(buffer);
    let ty = (0..props.memory_type_count)
        .find(|i| {
            req.memory_type_bits & (1 << i) != 0
                && props.memory_types[*i as usize].property_flags.contains(
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
        })
        .unwrap_or(0);
    let mem = device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(ty),
        None,
    )?;
    device.bind_buffer_memory(buffer, mem, 0)?;
    Ok((buffer, mem))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let entry = unsafe { ash::Entry::load()? };
    let app_name = c"lgx-vkvideo-probe";
    let app = vk::ApplicationInfo::default()
        .application_name(app_name)
        .api_version(vk::make_api_version(0, 1, 3, 0));
    // NVIDIA exposes the video device extensions without the instance-level
    // VK_KHR_video_queue; the capability query still works with a valid
    // VkVideoDecodeUsageInfoKHR -> H.265 profile chain.
    let available = unsafe { entry.enumerate_instance_extension_properties(None)? };
    let has_video_instance = available
        .iter()
        .any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == ash::khr::video_queue::NAME);
    println!("VK_KHR_video_queue instance extension: {has_video_instance}");
    let instance_exts: [*const std::ffi::c_char; 0] = [];
    let instance = unsafe {
        entry.create_instance(
            &vk::InstanceCreateInfo::default()
                .application_info(&app)
                .enabled_extension_names(&instance_exts),
            None,
        )?
    };
    let video_instance = ash::khr::video_queue::Instance::new(&entry, &instance);

    let phys_devs = unsafe { instance.enumerate_physical_devices()? };
    for pd in phys_devs {
        let props = unsafe { instance.get_physical_device_properties(pd) };
        let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) };
        let dev_exts = unsafe { instance.enumerate_device_extension_properties(pd)? };
        let has = |n: &CStr| dev_exts.iter().any(|e| ext_name(e) == n);
        if !(has(ash::khr::video_queue::NAME)
            && has(ash::khr::video_decode_queue::NAME)
            && has(ash::khr::video_decode_h265::NAME))
        {
            continue;
        }
        let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        let Some((qf, _)) = families
            .iter()
            .enumerate()
            .find(|(_, f)| f.queue_flags.contains(vk::QueueFlags::VIDEO_DECODE_KHR))
        else {
            continue;
        };
        let qf = qf as u32;
        println!("device: {:?} (queue family {qf})", name);

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
            (video_instance.fp().get_physical_device_video_capabilities_khr)(
                pd, &profile, &mut caps,
            )
            .result()?
        };
        println!(
            "  H.265 caps: min {:?}, max {:?}, maxDpbSlots {}, maxActiveRefs {}, std {:?}",
            (caps.min_coded_extent.width, caps.min_coded_extent.height),
            (caps.max_coded_extent.width, caps.max_coded_extent.height),
            caps.max_dpb_slots,
            caps.max_active_reference_pictures,
            (caps.std_header_version.spec_version, caps.std_header_version.extension_name_as_c_str()),
        );

        let mut profile_list = vk::VideoProfileListInfoKHR::default();
        profile_list.profile_count = 1;
        profile_list.p_profiles = &profile;
        let mut format_info = vk::PhysicalDeviceVideoFormatInfoKHR::default();
        format_info.image_usage =
            vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR | vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR;
        format_info.p_next = &profile_list as *const _ as *const std::ffi::c_void;
        let mut format_count = 0u32;
        unsafe {
            (video_instance.fp().get_physical_device_video_format_properties_khr)(
                pd,
                &format_info,
                &mut format_count,
                ptr::null_mut(),
            )
            .result()?
        };
        let mut formats = vec![vk::VideoFormatPropertiesKHR::default(); format_count as usize];
        unsafe {
            (video_instance.fp().get_physical_device_video_format_properties_khr)(
                pd,
                &format_info,
                &mut format_count,
                formats.as_mut_ptr(),
            )
            .result()?
        };
        formats.truncate(format_count as usize);
        let picture = formats
            .iter()
            .find(|f| f.format == vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .map(|f| f.format)
            .unwrap_or(vk::Format::G8_B8R8_2PLANE_420_UNORM);
        println!(
            "  video formats: {:?} (using {:?})",
            formats
                .iter()
                .map(|f| (f.format, f.image_tiling, f.image_usage_flags))
                .collect::<Vec<_>>(),
            picture
        );

        let queue_priorities = [1.0f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qf)
            .queue_priorities(&queue_priorities)];
        let dev_exts = [
            ash::khr::video_queue::NAME.as_ptr(),
            ash::khr::video_decode_queue::NAME.as_ptr(),
            ash::khr::video_decode_h265::NAME.as_ptr(),
        ];
        let device = unsafe {
            instance.create_device(
                pd,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_info)
                    .enabled_extension_names(&dev_exts),
                None,
            )?
        };
        let video_device = ash::khr::video_queue::Device::new(&instance, &device);
        let video_decode = ash::khr::video_decode_queue::Device::new(&instance, &device);
        let queue = unsafe { device.get_device_queue(qf, 0) };

        let extent = vk::Extent2D { width: 1920, height: 1080 };
        let mut session_info = vk::VideoSessionCreateInfoKHR::default();
        session_info.queue_family_index = qf;
        session_info.p_video_profile = &profile;
        session_info.picture_format = picture;
        session_info.max_coded_extent = extent;
        session_info.reference_picture_format = picture;
        session_info.max_dpb_slots = caps.max_dpb_slots;
        session_info.max_active_reference_pictures = caps.max_active_reference_pictures;
        session_info.p_std_header_version = &caps.std_header_version;
        let mut session = vk::VideoSessionKHR::null();
        unsafe {
            (video_device.fp().create_video_session_khr)(
                device.handle(),
                &session_info,
                ptr::null(),
                &mut session,
            )
            .result()?
        };
        println!("  session created: {session:?}");

        let mut mem_count = 0u32;
        unsafe {
            (video_device.fp().get_video_session_memory_requirements_khr)(
                device.handle(),
                session,
                &mut mem_count,
                ptr::null_mut(),
            )
            .result()?
        };
        let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); mem_count as usize];
        unsafe {
            (video_device.fp().get_video_session_memory_requirements_khr)(
                device.handle(),
                session,
                &mut mem_count,
                reqs.as_mut_ptr(),
            )
            .result()?
        };
        reqs.truncate(mem_count as usize);
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };
        let mut binds = Vec::new();
        for req in &reqs {
            let type_index = (0..mem_props.memory_type_count)
                .find(|i| {
                    req.memory_requirements.memory_type_bits & (1 << i) != 0
                        && mem_props.memory_types[*i as usize]
                            .property_flags
                            .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                })
                .unwrap_or(0);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.memory_requirements.size)
                .memory_type_index(type_index);
            let memory = unsafe { device.allocate_memory(&alloc, None)? };
            binds.push(
                vk::BindVideoSessionMemoryInfoKHR::default()
                    .memory_bind_index(req.memory_bind_index)
                    .memory(memory)
                    .memory_offset(0)
                    .memory_size(req.memory_requirements.size),
            );
        }
        unsafe {
            (video_device.fp().bind_video_session_memory_khr)(
                device.handle(),
                session,
                binds.len() as u32,
                binds.as_ptr(),
            )
            .result()?
        };
        println!("  bound {} memory objects", binds.len());

        let vps_info = VpsInfo::parse(SERVER_VPS)?;
        let sps_info = SpsInfo::parse(SERVER_SPS)?;
        let pps_info = PpsInfo::parse(SERVER_PPS)?;
        let ptl = vkstd::profile_tier_level(&sps_info);
        let dpb = vkstd::dpb(&sps_info);
        let vps = vkstd::vps(&vps_info, &ptl, &dpb);
        let sps = vkstd::sps(&sps_info, &ptl, &dpb);
        let pps = vkstd::pps(&pps_info, &sps_info);
        let mut add = vk::VideoDecodeH265SessionParametersAddInfoKHR::default();
        add.std_vps_count = 1;
        add.p_std_vp_ss = &vps;
        add.std_sps_count = 1;
        add.p_std_sp_ss = &sps;
        add.std_pps_count = 1;
        add.p_std_pp_ss = &pps;
        let mut create = vk::VideoDecodeH265SessionParametersCreateInfoKHR::default();
        create.max_std_vps_count = 1;
        create.max_std_sps_count = 1;
        create.max_std_pps_count = 1;
        create.p_parameters_add_info = &add;
        let mut params_info = vk::VideoSessionParametersCreateInfoKHR::default();
        params_info.video_session = session;
        params_info.p_next = &create as *const _ as *const std::ffi::c_void;
        let mut params = vk::VideoSessionParametersKHR::null();
        unsafe {
            (video_device.fp().create_video_session_parameters_khr)(
                device.handle(),
                &params_info,
                ptr::null(),
                &mut params,
            )
            .result()?
        };
        println!("  session parameters created: {params:?}");

        // Sequence decode: up to 10 VCL pictures with a two-slot DPB.
        unsafe {
            if let Ok(data) = std::fs::read("/tmp/lgx-server.hevc") {
                let nals = annexb_nals(&data);
                let vcl: Vec<&[u8]> = nals
                    .iter()
                    .copied()
                    .filter(|n| (0..=31).contains(&nal_type(n)))
                    .take(10)
                    .collect();
                if !vcl.is_empty() {
                    let format = picture;
                    let extent = vk::Extent2D { width: 1920, height: 1080 };
                    let mut image_profile_list = vk::VideoProfileListInfoKHR::default();
                    image_profile_list.profile_count = 1;
                    image_profile_list.p_profiles = &profile;
                    let dpb_usage = vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR
                        | vk::ImageUsageFlags::TRANSFER_SRC;
                    let dst_usage = vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR;
                    let (dpb_img0, dpb_mem0, dpb_view0) = make_video_image(&device, &mem_props, &image_profile_list, format, 1920, 1080, dpb_usage)?;
                    let (dpb_img1, dpb_mem1, dpb_view1) = make_video_image(&device, &mem_props, &image_profile_list, format, 1920, 1080, dpb_usage)?;
                    let (out_img, out_mem, out_view) = make_video_image(&device, &mem_props, &image_profile_list, format, 1920, 1080, dst_usage)?;
                    let dpb_imgs = [dpb_img0, dpb_img1];
                    let pics = [
                        vk::VideoPictureResourceInfoKHR::default().coded_extent(extent).image_view_binding(dpb_view0),
                        vk::VideoPictureResourceInfoKHR::default().coded_extent(extent).image_view_binding(dpb_view1),
                    ];
                    let out_pic = vk::VideoPictureResourceInfoKHR::default().coded_extent(extent).image_view_binding(out_view);
                    let mut dpb = Dpb::new();
                    let mut slot_layout = [vk::ImageLayout::UNDEFINED; 2];
                    let mut out_layout = vk::ImageLayout::UNDEFINED;
                    let align = caps.min_bitstream_buffer_size_alignment as usize;
                    let rb_size = (1920 * 1080 * 3 / 2) as u64;
                    let (rb, rbm) = make_host_buffer(&device, &mem_props, rb_size, vk::BufferUsageFlags::TRANSFER_DST)?;
                    let mut buf_mems = Vec::new();
                    let pool = device.create_command_pool(
                        &vk::CommandPoolCreateInfo::default()
                            .queue_family_index(qf)
                            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                        None,
                    )?;
                    let cmd = device.allocate_command_buffers(
                        &vk::CommandBufferAllocateInfo::default()
                            .command_pool(pool)
                            .level(vk::CommandBufferLevel::PRIMARY)
                            .command_buffer_count(1),
                    )?[0];
                    let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
                    let range = vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    };
                    let copy = vk::BufferImageCopy::default()
                        .image_subresource(vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 })
                        .image_extent(vk::Extent3D { width: 1920, height: 1080, depth: 1 });

                    for (i, nal) in vcl.iter().enumerate() {
                        let info = parse_slice_header(nal, &sps_info, &pps_info)?;
                        let is_idr = matches!(nal_type(nal), 19 | 20);
                        let poc = if is_idr { 0 } else { info.poc_lsb as i32 };
                        if is_idr {
                            dpb.reset();
                        }
                        // Map used references to DPB slots.
                        let mut ref_pocs = Vec::new();
                        let mut ref_slots: Vec<usize> = Vec::new();
                        for r in info.refs.iter().filter(|r| r.used_by_current) {
                            let rp = poc + r.poc_delta;
                            if let Some(slot) = dpb.slot_of(rp) {
                                ref_pocs.push(rp);
                                ref_slots.push(slot);
                            }
                        }
                        let cur = dpb.choose(&ref_pocs).ok_or("no free DPB slot")?;
                        let len = (3 + nal.len() + align - 1) & !(align - 1);
                        let (buf, bmem) = make_host_buffer(&device, &mem_props, len as u64, vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR)?;
                        let ptr = device.map_memory(bmem, 0, len as u64, vk::MemoryMapFlags::empty())? as *mut u8;
                        std::ptr::copy_nonoverlapping([0u8, 0, 1].as_ptr(), ptr, 3);
                        std::ptr::copy_nonoverlapping(nal.as_ptr(), ptr.add(3), nal.len());
                        device.unmap_memory(bmem);
                        buf_mems.push((buf, bmem));

                        // Reference slot infos.
                        let mut ref_std: Vec<n::StdVideoDecodeH265ReferenceInfo> = Vec::new();
                        let mut ref_dpb: Vec<vk::VideoDecodeH265DpbSlotInfoKHR> = Vec::new();
                        let mut ref_infos: Vec<vk::VideoReferenceSlotInfoKHR> = Vec::new();
                        for (j, slot) in ref_slots.iter().enumerate() {
                            let mut std_ref = std::mem::zeroed::<n::StdVideoDecodeH265ReferenceInfo>();
                            std_ref.PicOrderCntVal = ref_pocs[j];
                            ref_std.push(std_ref);
                            let mut dpb_ref = vk::VideoDecodeH265DpbSlotInfoKHR::default();
                            dpb_ref.p_std_reference_info = &ref_std[j];
                            ref_dpb.push(dpb_ref);
                            let mut slot_info = vk::VideoReferenceSlotInfoKHR::default();
                            slot_info.slot_index = *slot as i32;
                            slot_info.p_picture_resource = &pics[*slot];
                            slot_info.p_next = &ref_dpb[j] as *const _ as *const std::ffi::c_void;
                            ref_infos.push(slot_info);
                        }
                        let mut std_cur = std::mem::zeroed::<n::StdVideoDecodeH265ReferenceInfo>();
                        std_cur.PicOrderCntVal = poc;
                        let mut dpb_cur = vk::VideoDecodeH265DpbSlotInfoKHR::default();
                        dpb_cur.p_std_reference_info = &std_cur;
                        let mut setup = vk::VideoReferenceSlotInfoKHR::default();
                        setup.slot_index = cur as i32;
                        setup.p_picture_resource = &pics[cur];
                        setup.p_next = &dpb_cur as *const _ as *const std::ffi::c_void;

                        device.reset_fences(&[fence])?;
                        device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
                        device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
                        // Layouts for the current slot and the dst image.
                        let cur_old = slot_layout[cur];
                        if cur_old != vk::ImageLayout::VIDEO_DECODE_DPB_KHR {
                            let barrier = vk::ImageMemoryBarrier2::default()
                                .src_stage_mask(if cur_old == vk::ImageLayout::UNDEFINED { vk::PipelineStageFlags2::NONE } else { vk::PipelineStageFlags2::ALL_COMMANDS })
                                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
                                .dst_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
                                .old_layout(cur_old)
                                .new_layout(vk::ImageLayout::VIDEO_DECODE_DPB_KHR)
                                .image(dpb_imgs[cur])
                                .subresource_range(range);
                            device.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&[barrier]));
                        }
                        if out_layout == vk::ImageLayout::UNDEFINED {
                            let barrier = vk::ImageMemoryBarrier2::default()
                                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
                                .dst_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
                                .old_layout(vk::ImageLayout::UNDEFINED)
                                .new_layout(vk::ImageLayout::VIDEO_DECODE_DST_KHR)
                                .image(out_img)
                                .subresource_range(range);
                            device.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&[barrier]));
                        }
                        let mut flags = std::mem::zeroed::<n::StdVideoDecodeH265PictureInfoFlags>();
                        flags.set_IrapPicFlag(u32::from(is_idr));
                        flags.set_IdrPicFlag(u32::from(matches!(nal_type(nal), 19 | 20)));
                        flags.set_IsReference(1);
                        if !info.short_term_ref_pic_set_sps_flag {
                            flags.set_short_term_ref_pic_set_sps_flag(0);
                        }
                        let mut std_pic = std::mem::zeroed::<n::StdVideoDecodeH265PictureInfo>();
                        std_pic.flags = flags;
                        std_pic.PicOrderCntVal = poc;
                        std_pic.NumBitsForSTRefPicSetInSlice = info.rps_bits;
                        std_pic.RefPicSetStCurrBefore = [0xff; 8];
                        std_pic.RefPicSetStCurrAfter = [0xff; 8];
                        std_pic.RefPicSetLtCurr = [0xff; 8];
                        for (j, r) in info.refs.iter().filter(|r| r.used_by_current).enumerate() {
                            if j < ref_slots.len() {
                                if r.poc_delta < 0 {
                                    std_pic.RefPicSetStCurrBefore[j] = ref_slots[j] as u8;
                                } else {
                                    std_pic.RefPicSetStCurrAfter[j] = ref_slots[j] as u8;
                                }
                            }
                        }
                        let mut offs = [0u32];
                        let mut h265_pic = vk::VideoDecodeH265PictureInfoKHR::default();
                        h265_pic.p_std_picture_info = &std_pic;
                        h265_pic.slice_segment_count = 1;
                        h265_pic.p_slice_segment_offsets = offs.as_mut_ptr();
                        let mut decode = vk::VideoDecodeInfoKHR::default();
                        decode.p_next = &h265_pic as *const _ as *const std::ffi::c_void;
                        decode.src_buffer = buf;
                        decode.src_buffer_range = len as u64;
                        decode.dst_picture_resource = out_pic;
                        decode.p_setup_reference_slot = &setup;
                        decode.reference_slot_count = ref_infos.len() as u32;
                        decode.p_reference_slots = if ref_infos.is_empty() { std::ptr::null() } else { ref_infos.as_ptr() };
                        let mut begin_slots = ref_infos.clone();
                        begin_slots.push(setup);
                        let mut begin = vk::VideoBeginCodingInfoKHR::default();
                        begin.video_session = session;
                        begin.video_session_parameters = params;
                        begin.reference_slot_count = begin_slots.len() as u32;
                        begin.p_reference_slots = begin_slots.as_ptr();
                        (video_device.fp().cmd_begin_video_coding_khr)(cmd, &begin);
                        (video_decode.fp().cmd_decode_video_khr)(cmd, &decode);
                        (video_device.fp().cmd_end_video_coding_khr)(cmd, &vk::VideoEndCodingInfoKHR::default());
                        let to_src = vk::ImageMemoryBarrier2::default()
                            .src_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
                            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                            .src_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
                            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                            .old_layout(vk::ImageLayout::VIDEO_DECODE_DPB_KHR)
                            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                            .image(dpb_imgs[cur])
                            .subresource_range(range);
                        device.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&[to_src]));
                        device.cmd_copy_image_to_buffer(cmd, dpb_imgs[cur], vk::ImageLayout::TRANSFER_SRC_OPTIMAL, rb, &[copy]);
                        device.end_command_buffer(cmd)?;
                        let list = [cmd];
                        device.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&list)], fence)?;
                        device.wait_for_fences(&[fence], true, u64::MAX)?;
                        let mapped = device.map_memory(rbm, 0, rb_size, vk::MemoryMapFlags::empty())? as *const u8;
                        let y = std::slice::from_raw_parts(mapped, 1920 * 1080);
                        let sum: u64 = y.iter().map(|&v| u64::from(v)).sum();
                        println!("  frame {i}: type {} poc {poc} refs {:?} -> slot {cur}, Y mean {:.2}", nal_type(nal), ref_slots, sum as f64 / y.len() as f64);
                        std::fs::write(format!("/tmp/lgx-vk-seq-{i}.yuv"), std::slice::from_raw_parts(mapped, rb_size as usize))?;
                        device.unmap_memory(rbm);
                        dpb.commit(cur, poc);
                        slot_layout[cur] = vk::ImageLayout::TRANSFER_SRC_OPTIMAL;
                        out_layout = vk::ImageLayout::VIDEO_DECODE_DST_KHR;
                    }
                    device.destroy_fence(fence, None);
                    device.destroy_command_pool(pool, None);
                    device.destroy_buffer(rb, None);
                    device.free_memory(rbm, None);
                    for (buf, mem) in buf_mems {
                        device.destroy_buffer(buf, None);
                        device.free_memory(mem, None);
                    }
                    for (img, mem, view) in [(dpb_img0, dpb_mem0, dpb_view0), (dpb_img1, dpb_mem1, dpb_view1), (out_img, out_mem, out_view)] {
                        device.destroy_image_view(view, None);
                        device.destroy_image(img, None);
                        device.free_memory(mem, None);
                    }
                }
            }
        }

        unsafe {
            (video_device.fp().destroy_video_session_parameters_khr)(
                device.handle(),
                params,
                ptr::null(),
            );
            (video_device.fp().destroy_video_session_khr)(device.handle(), session, ptr::null());
            device.destroy_device(None);
        }
        break;
    }
    unsafe { instance.destroy_instance(None) };
    Ok(())
}
