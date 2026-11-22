//! Vulkan presentation of decoded HEVC frames.
//!
//! The preferred path imports the decoder's dma-buf planes directly
//! (`VK_EXT_image_drm_format_modifier`, `VK_EXT_external_memory_dma_buf`) and
//! samples them without a CPU transfer. When the device cannot import the
//! frame's modifier, or the import fails at runtime, the renderer falls back
//! to CPU NV12: the frame is downloaded, uploaded into two R8/R8G8 images from
//! staging buffers, and presented with the same fragment shader. The shader
//! converts full-range BT.709 to RGB either way.
//!
//! The swapchain is created from SDL's Vulkan surface; presentation is
//! immediate unless vsync is requested.

use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};

use ash::vk;

const VERT_SPV: &[u8] = include_bytes!("../shaders/video.vert.spv");
const FRAG_SPV: &[u8] = include_bytes!("../shaders/video.frag.spv");
const OSD_VERT_SPV: &[u8] = include_bytes!("../shaders/osd.vert.spv");
const OSD_FRAG_SPV: &[u8] = include_bytes!("../shaders/osd.frag.spv");

#[repr(C)]
struct OsdPush {
    rect: [f32; 4],
    uv: [f32; 4],
    alpha: f32,
}

pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

pub fn fit_rect(img_w: u32, img_h: u32, win_w: u32, win_h: u32) -> Rect {
    if img_w == 0 || img_h == 0 || win_w == 0 || win_h == 0 {
        return Rect { x: 0, y: 0, w: win_w, h: win_h };
    }
    let scale = f64::min(win_w as f64 / img_w as f64, win_h as f64 / img_h as f64);
    let w = (img_w as f64 * scale).round().max(1.0) as u32;
    let h = (img_h as f64 * scale).round().max(1.0) as u32;
    Rect {
        x: ((win_w - w) / 2) as i32,
        y: ((win_h - h) / 2) as i32,
        w,
        h,
    }
}

struct Texture {
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
    width: u32,
    height: u32,
}

struct Staging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    /// Row length in texels (bytes for R8, 2-byte pairs for R8G8) the buffer
    /// was sized for, so padded decoder rows can be copied in one memcpy.
    pitch: u32,
}

/// A dma-buf frame imported into Vulkan memory.
///
/// The decoder exports either one NV12 layer (both planes inside one dma-buf
/// object) or two layers (R8 luma and R8G8 chroma, each with its own object).
/// The first case becomes a single two-plane VkImage with one view per plane,
/// the second two one-plane images. Both end up as the same two descriptor
/// bindings the CPU path uses, so the shader sees an R8 and an R8G8 texture.
struct ImportedFrame {
    images: Vec<vk::Image>,
    memories: Vec<vk::DeviceMemory>,
    views: [vk::ImageView; 2],
    /// (image, aspect) of the luma and chroma plane, used for the layout
    /// barriers that make the imported memory visible to the shader.
    planes: [(vk::Image, vk::ImageAspectFlags); 2],
}

/// One decoder layer expressed as the Vulkan image it has to become.
struct Layer {
    name: &'static str,
    format: vk::Format,
    extent: vk::Extent2D,
    layouts: Vec<vk::SubresourceLayout>,
    /// File descriptor owned by the decoder's AVFrame; it is dup()ed before
    /// a Vulkan import because importing transfers fd ownership.
    fd: i32,
}

pub struct Renderer {
    entry: ash::Entry,
    instance: ash::Instance,
    surface_loader: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    device: ash::Device,
    physical: vk::PhysicalDevice,
    queue: vk::Queue,
    /// Video decode queue (family, queue) when the device was created with
    /// `VK_KHR_video_queue`/`VK_KHR_video_decode_queue`/`VK_KHR_video_decode_h265`.
    video_decode: Option<(u32, vk::Queue)>,
    swapchain_loader: ash::khr::swapchain::Device,
    swapchain: vk::SwapchainKHR,
    format: vk::Format,
    extent: vk::Extent2D,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    render_pass: vk::RenderPass,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    desc_layout: vk::DescriptorSetLayout,
    desc_pool: vk::DescriptorPool,
    desc_set: vk::DescriptorSet,
    sampler: vk::Sampler,
    osd_sampler: vk::Sampler,
    tex_y: Texture,
    tex_uv: Texture,
    stage_y: Staging,
    stage_uv: Staging,
    osd_tex: Texture,
    osd_stage: Staging,
    osd_ready: bool,
    /// (offset, width) of every card in the OSD atlas, and atlas height.
    osd_cards: Vec<(u32, u32)>,
    osd_height: u32,
    osd_scale: f32,
    /// Index of the runtime statistics card in `osd_cards`.
    osd_dynamic: usize,
    /// The staging buffer holds a new statistics card waiting for the next
    /// frame's command buffer.
    osd_staged: bool,
    osd_draw: Option<(usize, f32)>,
    osd_desc_layout: vk::DescriptorSetLayout,
    osd_desc_pool: vk::DescriptorPool,
    osd_desc_set: vk::DescriptorSet,
    osd_pipeline_layout: vk::PipelineLayout,
    osd_pipeline: vk::Pipeline,
    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    image_available: vk::Semaphore,
    render_done: vk::Semaphore,
    in_flight: vk::Fence,
    device_name: String,
    dmabuf_capable: bool,
    /// Device-side entry point for `vkGetMemoryFdPropertiesKHR`; present only
    /// when the external memory extensions were enabled.
    dmabuf_fd: Option<ash::khr::external_memory_fd::Device>,
    /// Frame imported for the current dma-buf frame, if any.
    dmabuf: Option<ImportedFrame>,
    /// Imported images kept per native DPB slot, so a native frame is
    /// imported once per slot instead of every frame.
    dmabuf_cache: HashMap<u64, ImportedFrame>,
    /// Cache key of the imported frame currently in `dmabuf` (0 = ephemeral).
    dmabuf_key: u64,
    /// Set after the first failed import: every later dma-buf frame is
    /// downloaded instead of retried, so a driver that cannot import this
    /// modifier still shows the picture.
    dmabuf_disabled: bool,
    dmabuf_logged: bool,
    /// Whether the descriptor set currently points at imported plane views.
    desc_dmabuf: bool,
    /// Size of the frame being presented, used for the fit rectangle.
    frame_size: (u32, u32),
    vsync: bool,
}

unsafe impl Send for Renderer {}

impl Renderer {
    pub fn new(
        window: &sdl3::video::Window,
        vsync: bool,
        initial: (u32, u32),
    ) -> Result<Self, String> {
        unsafe { Self::new_inner(window, vsync, initial) }
    }

    unsafe fn new_inner(
        window: &sdl3::video::Window,
        vsync: bool,
        initial: (u32, u32),
    ) -> Result<Self, String> {
        let entry = ash::Entry::load().map_err(|e| format!("cannot load Vulkan: {e}"))?;
        let exts = window
            .vulkan_instance_extensions()
            .map_err(|e| format!("SDL Vulkan extensions: {e}"))?;
        let ext_cstrings: Vec<CString> = exts
            .iter()
            .map(|s| CString::new(s.as_str()).unwrap())
            .collect();
        let ext_ptrs: Vec<*const c_char> = ext_cstrings.iter().map(|s| s.as_ptr()).collect();

        let app_name = CString::new("lgx").unwrap();
        let app_info = vk::ApplicationInfo::default()
            .application_name(app_name.as_c_str())
            .application_version(vk::make_api_version(0, 3, 0, 0))
            .engine_name(app_name.as_c_str())
            .engine_version(vk::make_api_version(0, 3, 0, 0))
            .api_version(vk::make_api_version(0, 1, 3, 0));
        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&ext_ptrs);
        let instance = entry
            .create_instance(&create_info, None)
            .map_err(|e| format!("vkCreateInstance: {e:?}"))?;

        let surface = window
            .vulkan_create_surface(instance.handle())
            .map_err(|e| format!("SDL Vulkan surface: {e}"))?;
        let surface_loader = ash::khr::surface::Instance::new(&entry, &instance);

        let physical = instance
            .enumerate_physical_devices()
            .map_err(|e| format!("enumerate physical devices: {e:?}"))?;
        // (physical device, score, queue family, name)
        let mut candidates: Vec<(vk::PhysicalDevice, u32, u32, String)> = Vec::new();
        for pd in physical {
            let props = instance.get_physical_device_properties(pd);
            let qfs = instance.get_physical_device_queue_family_properties(pd);
            let mut family = None;
            for (i, qf) in qfs.iter().enumerate() {
                let graphics = qf.queue_flags.contains(vk::QueueFlags::GRAPHICS);
                let present = surface_loader
                    .get_physical_device_surface_support(pd, i as u32, surface)
                    .unwrap_or(false);
                if graphics && present {
                    family = Some(i as u32);
                    break;
                }
            }
            let Some(family) = family else { continue };
            let exts = match instance.enumerate_device_extension_properties(pd) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let has_swapchain = exts.iter().any(|e| {
                CStr::from_ptr(e.extension_name.as_ptr()) == ash::khr::swapchain::NAME
            });
            if !has_swapchain {
                continue;
            }
            let name = CStr::from_ptr(props.device_name.as_ptr())
                .to_string_lossy()
                .into_owned();
            let discrete = props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU;
            let score = if discrete { 2 } else { 1 };
            candidates.push((pd, score, family, name));
        }
        let best = |list: &[(vk::PhysicalDevice, u32, u32, String)]| {
            list.iter()
                .max_by_key(|c| c.1)
                .map(|c| (c.0, c.2))
        };
        // Hybrid systems decode and render on different GPUs by default (for
        // example Intel VAAPI + the discrete card). dma-buf import only works
        // when both use the same driver, so allow pinning the Vulkan device by
        // name substring to test or prefer the decoder's GPU.
        let want_device = std::env::var("LGX_VK_DEVICE")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty());
        let pinned = want_device.as_deref().and_then(|want| {
            let matching: Vec<_> = candidates
                .iter()
                .filter(|c| c.3.to_ascii_lowercase().contains(want))
                .cloned()
                .collect();
            best(&matching)
        });
        let (pd, family) = match pinned {
            Some(choice) => choice,
            None => {
                if let Some(want) = &want_device {
                    eprintln!(
                        "lgx-client: no Vulkan device matches LGX_VK_DEVICE={want}; using the default choice"
                    );
                }
                best(&candidates).ok_or("no Vulkan device with a present queue")?
            }
        };
        let props = instance.get_physical_device_properties(pd);
        let device_name = CStr::from_ptr(props.device_name.as_ptr()).to_string_lossy().into_owned();

        // dma-buf import needs the fd handle type, the DRM modifier extension and
        // the dma-buf extension. Report honestly instead of assuming.
        let available: Vec<String> = instance
            .enumerate_device_extension_properties(pd)
            .map(|v| {
                v.iter()
                    .map(|e| CStr::from_ptr(e.extension_name.as_ptr()).to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let has = |name: &std::ffi::CStr| available.iter().any(|e| e == &name.to_string_lossy());
        let ext_mem_fd = has(ash::khr::external_memory_fd::NAME);
        let ext_dmabuf = has(ash::ext::external_memory_dma_buf::NAME);
        let ext_modifier = has(ash::ext::image_drm_format_modifier::NAME);
        let dmabuf_capable = ext_mem_fd && ext_dmabuf && ext_modifier;
        eprintln!(
            "lgx-client: dma-buf import {} (external_memory_fd={ext_mem_fd}, dma_buf={ext_dmabuf}, drm_modifier={ext_modifier})",
            if dmabuf_capable { "available" } else { "unavailable" }
        );

        let video_exts = [
            ash::khr::video_queue::NAME,
            ash::khr::video_decode_queue::NAME,
            ash::khr::video_decode_h265::NAME,
        ];
        let video_available = video_exts.iter().all(|name| has(name));
        let qfs = instance.get_physical_device_queue_family_properties(pd);
        let video_family = if video_available {
            qfs.iter()
                .position(|q| q.queue_flags.contains(vk::QueueFlags::VIDEO_DECODE_KHR))
                .map(|i| i as u32)
        } else {
            None
        };
        let priority = [1.0f32];
        let mut queue_info = vec![vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&priority)];
        if let Some(vf) = video_family {
            if vf != family {
                queue_info.push(
                    vk::DeviceQueueCreateInfo::default()
                        .queue_family_index(vf)
                        .queue_priorities(&priority),
                );
            }
        }
        let mut device_exts = vec![ash::khr::swapchain::NAME.as_ptr()];
        if dmabuf_capable {
            device_exts.push(ash::khr::external_memory_fd::NAME.as_ptr());
            device_exts.push(ash::ext::external_memory_dma_buf::NAME.as_ptr());
            device_exts.push(ash::ext::image_drm_format_modifier::NAME.as_ptr());
        }
        let mut dmabuf_capable = dmabuf_capable;
        let mut video_enabled = video_available && video_family.is_some();
        if video_enabled {
            for name in video_exts {
                device_exts.push(name.as_ptr());
            }
        }
        let device = match instance.create_device(
            pd,
            &vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_info)
                .enabled_extension_names(&device_exts),
            None,
        ) {
            Ok(device) => device,
            // Enabling the optional extensions must never cost the window: if
            // the driver refuses them, recreate the device without them.
            Err(e) if video_enabled || dmabuf_capable => {
                eprintln!(
                    "lgx-client: optional device extensions were rejected ({e:?}); using the CPU path"
                );
                dmabuf_capable = false;
                video_enabled = false;
                let minimal = [ash::khr::swapchain::NAME.as_ptr()];
                instance
                    .create_device(
                        pd,
                        &vk::DeviceCreateInfo::default()
                            .queue_create_infos(&queue_info)
                            .enabled_extension_names(&minimal),
                        None,
                    )
                    .map_err(|e| format!("vkCreateDevice: {e:?}"))?
            }
            Err(e) => return Err(format!("vkCreateDevice: {e:?}")),
        };
        let queue = device.get_device_queue(family, 0);
        let video_decode = if video_enabled {
            video_family.map(|vf| (vf, device.get_device_queue(vf, 0)))
        } else {
            None
        };
        if let Some((vf, _)) = video_decode {
            eprintln!("lgx-client: native Vulkan Video queue family {vf} available");
        }
        let swapchain_loader = ash::khr::swapchain::Device::new(&instance, &device);
        let dmabuf_fd = if dmabuf_capable {
            Some(ash::khr::external_memory_fd::Device::new(&instance, &device))
        } else {
            None
        };

        let caps = surface_loader
            .get_physical_device_surface_capabilities(pd, surface)
            .map_err(|e| format!("surface capabilities: {e:?}"))?;
        let formats = surface_loader
            .get_physical_device_surface_formats(pd, surface)
            .map_err(|e| format!("surface formats: {e:?}"))?;
        let format = formats
            .iter()
            .find(|f| {
                f.format == vk::Format::B8G8R8A8_UNORM
                    && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
            })
            .or_else(|| {
                formats.iter().find(|f| f.format == vk::Format::B8G8R8A8_SRGB)
            })
            .copied()
            .ok_or("no suitable surface format")?;
        let modes = surface_loader
            .get_physical_device_surface_present_modes(pd, surface)
            .map_err(|e| format!("present modes: {e:?}"))?;
        let requested = std::env::var("LGX_VK_PRESENT").unwrap_or_default();
        let present_mode = if vsync || requested == "fifo" {
            vk::PresentModeKHR::FIFO
        } else if requested == "immediate" && modes.contains(&vk::PresentModeKHR::IMMEDIATE) {
            vk::PresentModeKHR::IMMEDIATE
        } else if requested == "mailbox" && modes.contains(&vk::PresentModeKHR::MAILBOX) {
            vk::PresentModeKHR::MAILBOX
        } else if modes.contains(&vk::PresentModeKHR::IMMEDIATE) {
            vk::PresentModeKHR::IMMEDIATE
        } else if modes.contains(&vk::PresentModeKHR::MAILBOX) {
            vk::PresentModeKHR::MAILBOX
        } else {
            vk::PresentModeKHR::FIFO
        };
        let win_size = window.size_in_pixels();
        let extent = pick_extent(&caps, win_size);

        let swapchain_info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count((caps.min_image_count + 1).max(3))
            .image_format(format.format)
            .image_color_space(format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(present_mode)
            .clipped(true);
        let swapchain = swapchain_loader
            .create_swapchain(&swapchain_info, None)
            .map_err(|e| format!("vkCreateSwapchain: {e:?}"))?;
        let images = swapchain_loader
            .get_swapchain_images(swapchain)
            .map_err(|e| format!("swapchain images: {e:?}"))?;

        let render_pass = create_render_pass(&device, format.format)?;
        let desc_layout = create_desc_layout(&device)?;
        let sampler = create_sampler(&device)?;
        let osd_sampler = create_osd_sampler(&device)?;

        // OSD atlas: generated once from system fonts, uploaded before the
        // first frame. A missing font still leaves a blank card rather than
        // breaking video.
        let scale = window.display_scale().clamp(0.5, 4.0);
        let (osd_w, osd_h, osd_cards, osd_rgba) = match crate::osd::atlas(scale) {
            Some(atlas) => (atlas.width, atlas.height, atlas.cards, atlas.rgba),
            None => {
                eprintln!("lgx-client: OSD font not found; OSD overlay disabled");
                (2, 1, vec![(0, 1), (1, 1)], vec![0u8; 8])
            }
        };
        let osd_height = osd_h;
        let (osd_tex, osd_stage) =
            create_rgba_texture(&device, &instance, pd, osd_w, osd_h, &osd_rgba)?;
        let osd_desc_layout = create_osd_desc_layout(&device)?;
        let osd_desc_pool = device
            .create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&[vk::DescriptorPoolSize::default()
                        .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .descriptor_count(1)]),
                None,
            )
            .map_err(|e| format!("OSD descriptor pool: {e:?}"))?;
        let osd_desc_set = device
            .allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(osd_desc_pool)
                    .set_layouts(&[osd_desc_layout]),
            )
            .map_err(|e| format!("OSD descriptor set: {e:?}"))?[0];
        write_osd_descriptor(&device, osd_desc_set, &osd_tex, osd_sampler);
        let osd_push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<OsdPush>() as u32)];
        let osd_pipeline_layout = device
            .create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&[osd_desc_layout])
                    .push_constant_ranges(&osd_push),
                None,
            )
            .map_err(|e| format!("OSD pipeline layout: {e:?}"))?;
        let osd_pipeline = create_osd_pipeline(&device, render_pass, osd_pipeline_layout)?;

        let desc_pool = device
            .create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&[vk::DescriptorPoolSize::default()
                        .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .descriptor_count(2)]),
                None,
            )
            .map_err(|e| format!("descriptor pool: {e:?}"))?;
        let desc_sets = device
            .allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(desc_pool)
                    .set_layouts(&[desc_layout]),
            )
            .map_err(|e| format!("descriptor set: {e:?}"))?;
        let desc_set = desc_sets[0];

        let (tw, th) = if initial.0 > 1 && initial.1 > 1 {
            initial
        } else {
            (1280, 720)
        };
        let (tex_y, stage_y) = create_texture(&device, &instance, pd, tw, th, true)?;
        let (tex_uv, stage_uv) = create_texture(&device, &instance, pd, tw, th, false)?;
        write_descriptors(&device, desc_set, &tex_y, &tex_uv, sampler);

        let pipeline_layout = device
            .create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&[desc_layout]),
                None,
            )
            .map_err(|e| format!("pipeline layout: {e:?}"))?;
        let pipeline = create_pipeline(&device, render_pass, pipeline_layout)?;

        let views = create_views(&device, &images, format.format)?;
        let framebuffers = create_framebuffers(&device, render_pass, &views, extent)?;

        let cmd_pool = device
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(family),
                None,
            )
            .map_err(|e| format!("command pool: {e:?}"))?;
        let cmd = device
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
            .map_err(|e| format!("command buffer: {e:?}"))?[0];
        let sem = |device: &ash::Device| {
            device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
        };
        let image_available = sem(&device).map_err(|e| format!("semaphore: {e:?}"))?;
        let render_done = sem(&device).map_err(|e| format!("semaphore: {e:?}"))?;
        let in_flight = device
            .create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )
            .map_err(|e| format!("fence: {e:?}"))?;

        let api = entry
            .try_enumerate_instance_version()
            .ok()
            .flatten()
            .unwrap_or(vk::make_api_version(0, 1, 3, 0));
        eprintln!(
            "lgx-client: Vulkan {}.{}.{} ({device_name})",
            vk::api_version_major(api),
            vk::api_version_minor(api),
            vk::api_version_patch(api)
        );
        Ok(Self {
            entry,
            instance,
            surface_loader,
            surface,
            device,
            physical: pd,
            queue,
            video_decode,
            swapchain_loader,
            swapchain,
            format: format.format,
            extent,
            images,
            views,
            framebuffers,
            render_pass,
            pipeline_layout,
            pipeline,
            desc_layout,
            desc_pool,
            desc_set,
            sampler,
            osd_sampler,
            tex_y,
            tex_uv,
            stage_y,
            stage_uv,
            osd_tex,
            osd_stage,
            osd_ready: false,
            osd_cards,
            osd_height,
            osd_scale: scale,
            osd_dynamic: crate::osd::STATS_CARD,
            osd_staged: false,
            osd_draw: None,
            osd_desc_layout,
            osd_desc_pool,
            osd_desc_set,
            osd_pipeline_layout,
            osd_pipeline,
            cmd_pool,
            cmd,
            image_available,
            render_done,
            in_flight,
            device_name,
            vsync,
            dmabuf_capable,
            dmabuf_fd,
            dmabuf: None,
            dmabuf_cache: HashMap::new(),
            dmabuf_key: 0,
            dmabuf_disabled: false,
            dmabuf_logged: false,
            desc_dmabuf: false,
            frame_size: (tw, th),
        })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// True when this device can import the decoder's dma-buf planes.
    /// `LGX_NO_DMABUF=1` forces the CPU upload path without touching the rest
    /// of the renderer, which is the escape hatch if an import ever renders
    /// incorrectly on a driver.
    /// Whether this device advertises Vulkan Video H.265 decode. FFmpeg opens
    /// its own decode device, so only the physical-device extensions matter
    /// here; the decoder falls back when session creation fails.
    /// Whether the client's Vulkan device was created with the native video
    /// decode extensions and a video decode queue.
    pub fn video_decode_queue(&self) -> Option<(u32, vk::Queue)> {
        self.video_decode
    }

    pub fn video_decode_supported(&self) -> bool {
        let available: Vec<String> = unsafe {
            self.instance
                .enumerate_device_extension_properties(self.physical)
                .map(|v| {
                    v.iter()
                        .map(|e| {
                            CStr::from_ptr(e.extension_name.as_ptr())
                                .to_string_lossy()
                                .into_owned()
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let has = |name: &std::ffi::CStr| available.iter().any(|e| e == &name.to_string_lossy());
        has(ash::vk::KHR_VIDEO_QUEUE_NAME)
            && has(ash::vk::KHR_VIDEO_DECODE_QUEUE_NAME)
            && has(ash::vk::KHR_VIDEO_DECODE_H265_NAME)
    }

    pub fn can_import_dmabuf(&self) -> bool {
        self.dmabuf_capable && std::env::var_os("LGX_NO_DMABUF").is_none()
    }

    pub fn size(&self) -> (u32, u32) {
        (self.extent.width, self.extent.height)
    }

    pub fn set_osd(&mut self, osd: Option<(usize, f32)>) {
        self.osd_draw = osd;
    }

    /// Renders `text` into the reserved statistics card and stages an OSD
    /// atlas upload for the next frame. Called at most once per second while
    /// the card is visible.
    pub fn set_osd_text(&mut self, text: &str) -> bool {
        if !self.osd_ready {
            return false;
        }
        let Some(&(offset, width)) = self.osd_cards.get(self.osd_dynamic) else {
            return false;
        };
        let Some((w, h, rgba)) = crate::osd::render_card(text, self.osd_scale, width) else {
            return false;
        };
        if w != width || h != self.osd_height {
            return false;
        }
        unsafe {
            let row = (self.osd_tex.width * 4) as usize;
            let card_row = (w * 4) as usize;
            for y in 0..h as usize {
                let src = rgba.as_ptr().add(y * card_row);
                let dst = self.osd_stage.ptr.add(y * row + offset as usize * 4);
                std::ptr::copy_nonoverlapping(src, dst, card_row);
            }
        }
        // The copy is recorded into the next frame's command buffer (`present`),
        // so the presenter never has to wait for the queue.
        self.osd_staged = true;
        true
    }

    /// Records the staged OSD atlas update into the frame command buffer,
    /// right after the in-flight fence was waited (so the previous frame's
    /// sampling of the texture is complete).
    unsafe fn upload_osd_update(&mut self, cmd: vk::CommandBuffer) {
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .base_mip_level(0)
            .level_count(1)
            .base_array_layer(0)
            .layer_count(1);
        let to_dst = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.osd_tex.image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::SHADER_READ)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
        let to_read = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.osd_tex.image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ);
        let region = [vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(0)
                    .base_array_layer(0)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: self.osd_tex.width,
                height: self.osd_tex.height,
                depth: 1,
            })];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_dst],
        );
        self.device.cmd_copy_buffer_to_image(
            cmd,
            self.osd_stage.buffer,
            self.osd_tex.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &region,
        );
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_read],
        );
    }

    /// Clears the window once so it never shows undefined memory.
    pub fn clear(&mut self, win: (u32, u32)) {
        let _ = win;
        unsafe {
            self.render_black();
        }
    }

    pub fn draw(&mut self, frame: &crate::dec::Frame, win: (u32, u32)) -> bool {
        self.frame_size = (frame.width, frame.height);
        match &frame.pixels {
            crate::dec::Pixels::Nv12(nv) => unsafe { self.draw_nv12(frame, nv, win) },
            crate::dec::Pixels::Dmabuf(d) => unsafe {
                if self.dmabuf_disabled || self.dmabuf_fd.is_none() {
                    return self.draw_downloaded(frame, win);
                }
                if d.cache_key != 0 {
                    if self.dmabuf_key != d.cache_key {
                        self.drop_import();
                        if let Some(imported) = self.dmabuf_cache.remove(&d.cache_key) {
                            write_view_descriptors(
                                &self.device,
                                self.desc_set,
                                imported.views[0],
                                imported.views[1],
                                self.sampler,
                            );
                            self.desc_dmabuf = true;
                            self.dmabuf = Some(imported);
                            self.dmabuf_key = d.cache_key;
                        } else if let Err(e) = self.import_dmabuf(frame, d) {
                            eprintln!("lgx-client: dma-buf import failed: {e}");
                            eprintln!(
                                "lgx-client: falling back to CPU NV12 uploads (set LGX_NO_DMABUF=1 to skip import attempts)"
                            );
                            self.dmabuf_disabled = true;
                            self.drop_import();
                            return self.draw_downloaded(frame, win);
                        } else {
                            self.dmabuf_key = d.cache_key;
                        }
                    }
                } else if let Err(e) = self.import_dmabuf(frame, d) {
                    // A failed import must never leave a black window: log the
                    // reason once and keep presenting this and every later
                    // dma-buf frame through the CPU upload path.
                    eprintln!("lgx-client: dma-buf import failed: {e}");
                    eprintln!(
                        "lgx-client: falling back to CPU NV12 uploads (set LGX_NO_DMABUF=1 to skip import attempts)"
                    );
                    self.dmabuf_disabled = true;
                    self.drop_import();
                    return self.draw_downloaded(frame, win);
                }
                match self.present(win, false) {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!("lgx-client: Vulkan present: {e}");
                        false
                    }
                }
            },
        }
    }

    /// Last-resort path for a frame the GPU cannot import: transfer it to CPU
    /// NV12 and present it like a software-decoded frame.
    unsafe fn draw_downloaded(&mut self, frame: &crate::dec::Frame, win: (u32, u32)) -> bool {
        match frame.download() {
            Ok(cpu) => match &cpu.pixels {
                crate::dec::Pixels::Nv12(nv) => self.draw_nv12(&cpu, nv, win),
                crate::dec::Pixels::Dmabuf(_) => false,
            },
            Err(e) => {
                eprintln!("lgx-client: cannot download the dma-buf frame: {e}");
                false
            }
        }
    }

    unsafe fn draw_nv12(
        &mut self,
        frame: &crate::dec::Frame,
        nv: &crate::dec::Nv12,
        win: (u32, u32),
    ) -> bool {
        self.frame_size = (frame.width, frame.height);
        // A frame that came back as NV12 after an import (or on a CPU-only
        // device) uses the texture path; release an older imported frame and
        // point the descriptors back at the upload images.
        self.drop_import();
        if frame.width != self.tex_y.width || frame.height != self.tex_y.height {
            if let Err(e) = self.resize_textures(frame.width, frame.height) {
                eprintln!("lgx-client: Vulkan textures: {e}");
                return false;
            }
        }
        if nv.y_stride != self.stage_y.pitch || nv.uv_stride / 2 != self.stage_uv.pitch {
            if let Err(e) = self.resize_staging(frame.width, frame.height, nv.y_stride, nv.uv_stride) {
                eprintln!("lgx-client: Vulkan staging: {e}");
                return false;
            }
        }
        self.upload(frame.height, nv.y, nv.uv);
        match self.present(win, true) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("lgx-client: Vulkan present: {e}");
                false
            }
        }
    }

    unsafe fn upload(&mut self, h: u32, y: *const u8, uv: *const u8) {
        // Rows keep the decoder's stride and the copy into the image uses
        // `buffer_row_length`, so each plane is one contiguous memcpy even
        // when the decoder pads it.
        let y_bytes = (self.stage_y.pitch as usize) * (h as usize);
        std::ptr::copy_nonoverlapping(y, self.stage_y.ptr, y_bytes);
        let uv_bytes = (self.stage_uv.pitch as usize) * 2 * (h as usize / 2);
        std::ptr::copy_nonoverlapping(uv, self.stage_uv.ptr, uv_bytes);
    }

    /// Sizes the staging buffers for the decoder's current row pitch.
    unsafe fn resize_staging(
        &mut self,
        w: u32,
        h: u32,
        y_stride: u32,
        uv_stride: u32,
    ) -> Result<(), String> {
        // Only called when the pitch changed, which happens once per format.
        self.device.device_wait_idle().map_err(|e| format!("wait idle: {e:?}"))?;
        destroy_staging(&self.device, &mut self.stage_y);
        destroy_staging(&self.device, &mut self.stage_uv);
        self.stage_y = create_staging(
            &self.device,
            &self.instance,
            self.physical,
            y_stride,
            h,
            1,
        )?;
        let uv_texels = (uv_stride / 2).max(w / 2);
        self.stage_uv = create_staging(
            &self.device,
            &self.instance,
            self.physical,
            uv_texels,
            h / 2,
            2,
        )?;
        Ok(())
    }

    unsafe fn resize_textures(&mut self, w: u32, h: u32) -> Result<(), String> {
        self.device.device_wait_idle().map_err(|e| format!("wait idle: {e:?}"))?;
        destroy_texture(&self.device, &mut self.tex_y);
        destroy_texture(&self.device, &mut self.tex_uv);
        destroy_staging(&self.device, &mut self.stage_y);
        destroy_staging(&self.device, &mut self.stage_uv);
        let (ty, sy) = create_texture(&self.device, &self.instance, self.physical, w, h, true)?;
        let (tu, su) = create_texture(&self.device, &self.instance, self.physical, w, h, false)?;
        self.tex_y = ty;
        self.tex_uv = tu;
        self.stage_y = sy;
        self.stage_uv = su;
        write_descriptors(&self.device, self.desc_set, &self.tex_y, &self.tex_uv, self.sampler);
        self.desc_dmabuf = false;
        Ok(())
    }

    /// Copies the staged NV12 planes into the two upload images and makes them
    /// readable by the fragment shader.
    unsafe fn blit_nv12(&self, cmd: vk::CommandBuffer) {
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .base_mip_level(0)
            .level_count(1)
            .base_array_layer(0)
            .layer_count(1);
        let barriers = [
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.tex_y.image)
                .subresource_range(range)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE),
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.tex_uv.image)
                .subresource_range(range)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &barriers,
        );
        let y_region = [vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(self.stage_y.pitch)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(0)
                    .base_array_layer(0)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D { width: self.tex_y.width, height: self.tex_y.height, depth: 1 })];
        self.device.cmd_copy_buffer_to_image(
            cmd,
            self.stage_y.buffer,
            self.tex_y.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &y_region,
        );
        let uv_region = [vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(self.stage_uv.pitch)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(0)
                    .base_array_layer(0)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: self.tex_uv.width,
                height: self.tex_uv.height,
                depth: 1,
            })];
        self.device.cmd_copy_buffer_to_image(
            cmd,
            self.stage_uv.buffer,
            self.tex_uv.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &uv_region,
        );
        let read_barriers = [
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.tex_y.image)
                .subresource_range(range)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ),
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.tex_uv.image)
                .subresource_range(range)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ),
        ];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &read_barriers,
        );
    }

    /// Transitions the imported dma-buf planes for sampling. A fresh image is
    /// created for every frame, so this is the only transition it needs.
    unsafe fn prepare_dmabuf(&self, cmd: vk::CommandBuffer) -> Result<(), String> {
        let Some(frame) = &self.dmabuf else {
            return Err("no imported dma-buf frame to present".into());
        };
        let barriers: Vec<vk::ImageMemoryBarrier> = frame
            .planes
            .iter()
            .map(|&(image, aspect)| {
                vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(aspect)
                            .base_mip_level(0)
                            .level_count(1)
                            .base_array_layer(0)
                            .layer_count(1),
                    )
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)
            })
            .collect();
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &barriers,
        );
        Ok(())
    }

    /// Releases the imported frame and points the descriptors back at the
    /// upload textures. Safe to call when nothing is imported.
    unsafe fn drop_import(&mut self) {
        if self.dmabuf.is_none() && !self.desc_dmabuf {
            return;
        }
        // The last submission may still sample the imported images and the
        // descriptor set may still point at their views, so wait before
        // touching either.
        let _ = self
            .device
            .wait_for_fences(&[self.in_flight], true, 1_000_000_000);
        if self.desc_dmabuf {
            write_descriptors(&self.device, self.desc_set, &self.tex_y, &self.tex_uv, self.sampler);
            self.desc_dmabuf = false;
        }
        if let Some(old) = self.dmabuf.take() {
            if self.dmabuf_key != 0 {
                if self.dmabuf_cache.len() >= 4 {
                    if let Some((&key, _)) = self.dmabuf_cache.iter().next() {
                        if let Some(old) = self.dmabuf_cache.remove(&key) {
                            destroy_imported(&self.device, old);
                        }
                    }
                }
                self.dmabuf_cache.insert(self.dmabuf_key, old);
            } else {
                destroy_imported(&self.device, old);
            }
            self.dmabuf_key = 0;
        }
    }

    /// Imports one dma-buf frame. On error nothing is left bound, and the
    /// caller can still present the frame through the CPU path.
    unsafe fn import_dmabuf(
        &mut self,
        frame: &crate::dec::Frame,
        d: &crate::dec::Dmabuf,
    ) -> Result<(), String> {
        let layers = dmabuf_layers(frame, d)?;
        if !self.dmabuf_logged {
            self.dmabuf_logged = true;
            eprintln!(
                "lgx-client: dma-buf frame {}x{}, modifier {:#018x}, {} layer(s), offsets {}/{}, pitches {}/{}",
                frame.width,
                frame.height,
                d.modifier,
                layers.len(),
                d.offsets[0],
                d.offsets[1],
                d.pitches[0],
                d.pitches[1]
            );
        }
        // The previous imported image may still be in flight.
        self.device
            .wait_for_fences(&[self.in_flight], true, 1_000_000_000)
            .map_err(|e| format!("wait for the previous frame: {e:?}"))?;
        if let Some(old) = self.dmabuf.take() {
            destroy_imported(&self.device, old);
        }
        self.dmabuf_key = 0;

        let imported = self.build_import(&layers, d.modifier)?;
        write_view_descriptors(
            &self.device,
            self.desc_set,
            imported.views[0],
            imported.views[1],
            self.sampler,
        );
        self.desc_dmabuf = true;
        self.dmabuf = Some(imported);
        Ok(())
    }

    unsafe fn build_import(
        &self,
        layers: &[Layer],
        modifier: u64,
    ) -> Result<ImportedFrame, String> {
        let mut out = ImportedFrame {
            images: Vec::with_capacity(layers.len()),
            memories: Vec::with_capacity(layers.len()),
            views: [vk::ImageView::null(); 2],
            planes: [(vk::Image::null(), vk::ImageAspectFlags::empty()); 2],
        };
        if let Err(e) = self.fill_import(&mut out, layers, modifier) {
            destroy_imported(&self.device, out);
            return Err(e);
        }
        Ok(out)
    }

    unsafe fn fill_import(
        &self,
        out: &mut ImportedFrame,
        layers: &[Layer],
        modifier: u64,
    ) -> Result<(), String> {
        for layer in layers {
            let planes = self.modifier_plane_count(layer, modifier)?;
            if planes as usize != layer.layouts.len() {
                return Err(format!(
                    "{} with modifier {:#018x} has {} memory planes, the decoder exported {}",
                    layer.name,
                    modifier,
                    planes,
                    layer.layouts.len()
                ));
            }
            let (image, memory) = self.import_layer(layer, modifier)?;
            out.images.push(image);
            out.memories.push(memory);
        }

        // The NV12 form is one two-plane image; the split form is two
        // single-plane images. Both are sampled as an R8 and an R8G8 view.
        let aspects = if layers.len() == 1 {
            [
                vk::ImageAspectFlags::PLANE_0,
                vk::ImageAspectFlags::PLANE_1,
            ]
        } else {
            [vk::ImageAspectFlags::COLOR, vk::ImageAspectFlags::COLOR]
        };
        let formats = [vk::Format::R8_UNORM, vk::Format::R8G8_UNORM];
        let images = if layers.len() == 1 {
            [out.images[0], out.images[0]]
        } else {
            [out.images[0], out.images[1]]
        };
        for i in 0..2 {
            let view = self
                .device
                .create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(images[i])
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(formats[i])
                        .subresource_range(
                            vk::ImageSubresourceRange::default()
                                .aspect_mask(aspects[i])
                                .base_mip_level(0)
                                .level_count(1)
                                .base_array_layer(0)
                                .layer_count(1),
                        ),
                    None,
                )
                .map_err(|e| format!("plane {i} image view: {e:?}"))?;
            out.views[i] = view;
            out.planes[i] = (images[i], aspects[i]);
        }
        Ok(())
    }

    /// Creates one VkImage for a decoder layer and binds an import of its
    /// dma-buf. A two-plane NV12 image shares one memory, so it is bound with
    /// a single `vkBindImageMemory`; the per-plane offsets live in the
    /// explicit modifier description.
    unsafe fn import_layer(
        &self,
        layer: &Layer,
        modifier: u64,
    ) -> Result<(vk::Image, vk::DeviceMemory), String> {
        let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut external_props = vk::ExternalImageFormatProperties::default();
        let mut props = vk::ImageFormatProperties2::default().push_next(&mut external_props);
        self.instance
            .get_physical_device_image_format_properties2(
                self.physical,
                &vk::PhysicalDeviceImageFormatInfo2::default()
                    .format(layer.format)
                    .ty(vk::ImageType::TYPE_2D)
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .usage(vk::ImageUsageFlags::SAMPLED)
                    .push_next(&mut drm)
                    .push_next(&mut external),
                &mut props,
            )
            .map_err(|e| {
                format!(
                    "{} {}x{} with modifier {modifier:#018x} cannot be imported: {e:?}",
                    layer.name, layer.extent.width, layer.extent.height
                )
            })?;
        if !external_props
            .external_memory_properties
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        {
            return Err(format!(
                "{} with modifier {modifier:#018x} does not accept dma-buf handles",
                layer.name
            ));
        }

        let view_formats = [
            layer.format,
            vk::Format::R8_UNORM,
            vk::Format::R8G8_UNORM,
        ];
        let mut format_list = vk::ImageFormatListCreateInfo::default().view_formats(&view_formats);
        let plane_layouts = layer.layouts.clone();
        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&plane_layouts);
        let mut external_image = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let separate_planes = layer.layouts.len() > 1;
        let mut info = vk::ImageCreateInfo::default()
            .flags(if separate_planes {
                // Sampling the two planes of one NV12 image through R8/R8G8
                // views requires a mutable-format image and a format list.
                vk::ImageCreateFlags::MUTABLE_FORMAT
            } else {
                vk::ImageCreateFlags::empty()
            })
            .image_type(vk::ImageType::TYPE_2D)
            .format(layer.format)
            .extent(vk::Extent3D {
                width: layer.extent.width,
                height: layer.extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut modifier_info)
            .push_next(&mut external_image);
        if separate_planes {
            info = info.push_next(&mut format_list);
        }
        let image = self.device.create_image(&info, None).map_err(|e| {
            format!(
                "vkCreateImage({} {}x{}, modifier {modifier:#018x}, {} plane(s)): {e:?}",
                layer.name,
                layer.extent.width,
                layer.extent.height,
                layer.layouts.len()
            )
        })?;

        let memory = match self.import_memory(image, layer.fd) {
            Ok(memory) => memory,
            Err(e) => {
                self.device.destroy_image(image, None);
                return Err(e);
            }
        };
        if let Err(e) = self.device.bind_image_memory(image, memory, 0) {
            self.device.free_memory(memory, None);
            self.device.destroy_image(image, None);
            return Err(format!("vkBindImageMemory: {e:?}"));
        }
        Ok((image, memory))
    }

    /// Duplicates the decoder's fd and imports it as dedicated image memory.
    /// The duplicate is closed on failure; on success Vulkan owns it.
    unsafe fn import_memory(&self, image: vk::Image, raw_fd: i32) -> Result<vk::DeviceMemory, String> {
        let Some(loader) = &self.dmabuf_fd else {
            return Err("external memory fd support is not enabled".into());
        };
        let mut dedicated_req = vk::MemoryDedicatedRequirements::default();
        let mut req2 = vk::MemoryRequirements2::default().push_next(&mut dedicated_req);
        self.device.get_image_memory_requirements2(
            &vk::ImageMemoryRequirementsInfo2::default().image(image),
            &mut req2,
        );
        let req = req2.memory_requirements;

        let fd = libc::dup(raw_fd);
        if fd < 0 {
            return Err(format!(
                "cannot duplicate the dma-buf fd: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        if let Err(e) = loader.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            fd,
            &mut fd_props,
        ) {
            libc::close(fd);
            return Err(format!("vkGetMemoryFdPropertiesKHR: {e:?}"));
        }
        // Both the image and the imported file descriptor restrict the memory
        // types; the allocation has to satisfy the intersection.
        let bits = req.memory_type_bits & fd_props.memory_type_bits;
        if bits == 0 {
            libc::close(fd);
            return Err(format!(
                "the dma-buf memory types ({:#x}) do not match the image requirements ({:#x})",
                fd_props.memory_type_bits, req.memory_type_bits
            ));
        }
        let mem_type = match find_memory(
            &self.instance,
            self.physical,
            bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        ) {
            Ok(t) => t,
            Err(e) => match find_memory(
                &self.instance,
                self.physical,
                bits,
                vk::MemoryPropertyFlags::empty(),
            ) {
                Ok(t) => t,
                Err(_) => {
                    libc::close(fd);
                    return Err(e);
                }
            },
        };

        let dedicated = dedicated_req.requires_dedicated_allocation != 0
            || dedicated_req.prefers_dedicated_allocation != 0;
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(fd);
        let memory = if dedicated {
            let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(mem_type)
                .push_next(&mut dedicated_info)
                .push_next(&mut import);
            self.device.allocate_memory(&alloc, None)
        } else {
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(mem_type)
                .push_next(&mut import);
            self.device.allocate_memory(&alloc, None)
        };
        match memory {
            Ok(memory) => Ok(memory),
            Err(e) => {
                libc::close(fd);
                Err(format!("vkAllocateMemory (imported dma-buf): {e:?}"))
            }
        }
    }

    /// The plane count the device associates with `format` + `modifier`.
    /// A mismatch with the decoded layout must fail before `vkCreateImage`.
    unsafe fn modifier_plane_count(&self, layer: &Layer, modifier: u64) -> Result<u32, String> {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        self.instance
            .get_physical_device_format_properties2(self.physical, layer.format, &mut props);
        let count = list.drm_format_modifier_count as usize;
        if count == 0 {
            return Err(format!(
                "{}: the device reports no DRM format modifiers",
                layer.name
            ));
        }
        let mut entries = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut entries);
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        self.instance
            .get_physical_device_format_properties2(self.physical, layer.format, &mut props);
        if let Some(entry) = entries.iter().find(|e| e.drm_format_modifier == modifier) {
            return Ok(entry.drm_format_modifier_plane_count);
        }
        let supported: Vec<String> = entries
            .iter()
            .take(6)
            .map(|e| format!("{:#018x}", e.drm_format_modifier))
            .collect();
        Err(format!(
            "{}: modifier {modifier:#018x} is not supported (device supports {}{})",
            layer.name,
            supported.join(", "),
            if entries.len() > supported.len() { ", ..." } else { "" }
        ))
    }

    unsafe fn upload_osd(&mut self, cmd: vk::CommandBuffer) -> Result<(), String> {
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .base_mip_level(0)
            .level_count(1)
            .base_array_layer(0)
            .layer_count(1);
        let barrier = [vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.osd_tex.image)
            .subresource_range(range)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &barrier,
        );
        let region = [vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .mip_level(0)
                    .base_array_layer(0)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: self.osd_tex.width,
                height: self.osd_tex.height,
                depth: 1,
            })];
        self.device.cmd_copy_buffer_to_image(
            cmd,
            self.osd_stage.buffer,
            self.osd_tex.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &region,
        );
        let read = [vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.osd_tex.image)
            .subresource_range(range)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)];
        self.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &read,
        );
        self.osd_ready = true;
        Ok(())
    }

    unsafe fn draw_osd(&self, cmd: vk::CommandBuffer, card: usize, alpha: f32) {
        if alpha <= 0.0 {
            return;
        }
        let Some(&(offset, card_width)) = self.osd_cards.get(card) else {
            return;
        };
        let sw = self.extent.width as f32;
        let sh = self.extent.height as f32;
        let card_h = self.osd_height as f32;
        let total_w = (self.osd_cards.iter().map(|c| c.1).sum::<u32>()).max(1) as f32;
        let (card_w, u0, u1) = (
            card_width as f32,
            offset as f32 / total_w,
            (offset + card_width) as f32 / total_w,
        );
        let x0 = ((sw - card_w) / 2.0).round();
        let y0 = 40.0;
        let y1 = y0 + card_h;
        let nx0 = 2.0 * x0 / sw - 1.0;
        let nx1 = 2.0 * (x0 + card_w) / sw - 1.0;
        let ny0 = 2.0 * y0 / sh - 1.0;
        let ny1 = 2.0 * y1 / sh - 1.0;
        let uv = [u0, 0.0, u1, 1.0];
        let push = OsdPush {
            rect: [nx0, ny0, nx1, ny1],
            uv,
            alpha,
        };
        let bytes = std::slice::from_raw_parts(
            (&push as *const OsdPush).cast::<u8>(),
            std::mem::size_of::<OsdPush>(),
        );
        let viewport = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: sw,
            height: sh,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissor = [vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: self.extent,
        }];
        self.device.cmd_set_viewport(cmd, 0, &viewport);
        self.device.cmd_set_scissor(cmd, 0, &scissor);
        self.device
            .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.osd_pipeline);
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::GRAPHICS,
            self.osd_pipeline_layout,
            0,
            &[self.osd_desc_set],
            &[],
        );
        self.device.cmd_push_constants(
            cmd,
            self.osd_pipeline_layout,
            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
            0,
            bytes,
        );
        self.device.cmd_draw(cmd, 4, 1, 0, 0);
    }

    unsafe fn present(&mut self, win: (u32, u32), upload: bool) -> Result<(), String> {
        // A fullscreen transition can briefly report a zero-sized window.
        // Do not touch the swapchain until SDL has a real size again.
        if win.0 == 0 || win.1 == 0 {
            return Ok(());
        }
        let sw = self.extent.width;
        let sh = self.extent.height;
        if win.0 != sw || win.1 != sh {
            self.recreate_swapchain(win)?;
        }
        self.device
            .wait_for_fences(&[self.in_flight], true, 1_000_000_000)
            .map_err(|e| format!("fence: {e:?}"))?;
        // Acquire with a timeout: during a compositor resize/fullscreen
        // transition the Wayland surface can stop handing out buffers. The
        // old code waited forever here, which froze the whole UI.
        let (idx, _) = match self.swapchain_loader.acquire_next_image(
            self.swapchain,
            100_000_000,
            self.image_available,
            vk::Fence::null(),
        ) {
            Ok(v) => v,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.recreate_swapchain(win)?;
                return Ok(());
            }
            Err(vk::Result::TIMEOUT) | Err(vk::Result::NOT_READY) => {
                self.recreate_swapchain(win)?;
                return Ok(());
            }
            Err(e) => return Err(format!("acquire: {e:?}")),
        };
        // Reset only after a successful acquire; otherwise a timeout would
        // leave an unsignalled fence and the next frame would block on it.
        self.device
            .reset_fences(&[self.in_flight])
            .map_err(|e| format!("reset fence: {e:?}"))?;
        let cmd = self.cmd;
        self.device
            .reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
            .map_err(|e| format!("reset cmd: {e:?}"))?;
        self.device
            .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
            .map_err(|e| format!("begin cmd: {e:?}"))?;

        if !self.osd_ready {
            self.upload_osd(cmd)?;
        } else if self.osd_staged {
            self.upload_osd_update(cmd);
            self.osd_staged = false;
        }

        if upload {
            self.blit_nv12(cmd);
        } else {
            self.prepare_dmabuf(cmd)?;
        }

        let clear = [vk::ClearValue {
            color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] },
        }];
        let rp_begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(self.framebuffers[idx as usize])
            .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: self.extent })
            .clear_values(&clear);
        self.device.cmd_begin_render_pass(
            cmd,
            &rp_begin,
            vk::SubpassContents::INLINE,
        );
        let rect = fit_rect(self.frame_size.0, self.frame_size.1, sw, sh);
        let viewport = [vk::Viewport {
            x: rect.x as f32,
            y: rect.y as f32,
            width: rect.w as f32,
            height: rect.h as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissor = [vk::Rect2D {
            offset: vk::Offset2D { x: rect.x, y: rect.y },
            extent: vk::Extent2D { width: rect.w, height: rect.h },
        }];
        self.device.cmd_set_viewport(cmd, 0, &viewport);
        self.device.cmd_set_scissor(cmd, 0, &scissor);
        self.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
        self.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::GRAPHICS,
            self.pipeline_layout,
            0,
            &[self.desc_set],
            &[],
        );
        self.device.cmd_draw(cmd, 3, 1, 0, 0);
        if let Some((card, alpha)) = self.osd_draw {
            self.draw_osd(cmd, card, alpha);
        }
        self.device.cmd_end_render_pass(cmd);
        self.device
            .end_command_buffer(cmd)
            .map_err(|e| format!("end cmd: {e:?}"))?;

        let wait = [self.image_available];
        let signal = [self.render_done];
        let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let cmds = [cmd];
        let submit = [vk::SubmitInfo::default()
            .wait_semaphores(&wait)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&cmds)
            .signal_semaphores(&signal)];
        self.device
            .queue_submit(self.queue, &submit, self.in_flight)
            .map_err(|e| format!("queue submit: {e:?}"))?;
        let swapchains = [self.swapchain];
        let indices = [idx];
        let present = vk::PresentInfoKHR::default()
            .wait_semaphores(&signal)
            .swapchains(&swapchains)
            .image_indices(&indices);
        match self.swapchain_loader.queue_present(self.queue, &present) {
            Ok(_) => {}
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.recreate_swapchain(win)?;
            }
            Err(e) => return Err(format!("queue present: {e:?}")),
        }
        Ok(())
    }

    unsafe fn render_black(&mut self) {
        let win = self.extent;
        if win.width == 0 || win.height == 0 {
            return;
        }
        let (idx, _) = match self.swapchain_loader.acquire_next_image(
            self.swapchain,
            u64::MAX,
            self.image_available,
            vk::Fence::null(),
        ) {
            Ok(v) => v,
            Err(_) => return,
        };
        let cmd = self.cmd;
        let _ = self.device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty());
        let _ = self.device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default());
        let clear = [vk::ClearValue {
            color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] },
        }];
        let rp = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(self.framebuffers[idx as usize])
            .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: self.extent })
            .clear_values(&clear);
        self.device.cmd_begin_render_pass(cmd, &rp, vk::SubpassContents::INLINE);
        self.device.cmd_end_render_pass(cmd);
        let _ = self.device.end_command_buffer(cmd);
        let wait = [self.image_available];
        let signal = [self.render_done];
        let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let cmds = [cmd];
        let submit = [vk::SubmitInfo::default()
            .wait_semaphores(&wait)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&cmds)
            .signal_semaphores(&signal)];
        let _ = self.device.queue_submit(self.queue, &submit, vk::Fence::null());
        let _ = self.device.queue_wait_idle(self.queue);
        let swapchains = [self.swapchain];
        let indices = [idx];
        let present = vk::PresentInfoKHR::default()
            .wait_semaphores(&signal)
            .swapchains(&swapchains)
            .image_indices(&indices);
        let _ = self.swapchain_loader.queue_present(self.queue, &present);
        let _ = self.device.device_wait_idle();
    }

    unsafe fn recreate_swapchain(&mut self, win: (u32, u32)) -> Result<(), String> {
        if win.0 == 0 || win.1 == 0 {
            return Ok(());
        }
        self.device.device_wait_idle().map_err(|e| format!("wait idle: {e:?}"))?;
        let _ = self.device.queue_wait_idle(self.queue);
        // Recreate the frame semaphores too: an out-of-date present can leave
        // them in an undefined state and poison the next frame.
        self.device.destroy_semaphore(self.image_available, None);
        self.device.destroy_semaphore(self.render_done, None);
        self.image_available = self
            .device
            .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            .map_err(|e| format!("semaphore: {e:?}"))?;
        self.render_done = self
            .device
            .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            .map_err(|e| format!("semaphore: {e:?}"))?;
        let caps = self
            .surface_loader
            .get_physical_device_surface_capabilities(self.physical, self.surface)
            .map_err(|e| format!("caps: {e:?}"))?;
        let extent = pick_extent(&caps, win);
        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(self.surface)
            .min_image_count((caps.min_image_count + 1).max(3))
            .image_format(self.format)
            .image_color_space(vk::ColorSpaceKHR::SRGB_NONLINEAR)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(if self.vsync {
                vk::PresentModeKHR::FIFO
            } else {
                vk::PresentModeKHR::IMMEDIATE
            })
            .clipped(true)
            .old_swapchain(self.swapchain);
        let new_swapchain = self
            .swapchain_loader
            .create_swapchain(&info, None)
            .map_err(|e| format!("recreate swapchain: {e:?}"))?;
        for fb in self.framebuffers.drain(..) {
            self.device.destroy_framebuffer(fb, None);
        }
        for view in self.views.drain(..) {
            self.device.destroy_image_view(view, None);
        }
        self.swapchain_loader.destroy_swapchain(self.swapchain, None);
        self.swapchain = new_swapchain;
        self.images = self
            .swapchain_loader
            .get_swapchain_images(self.swapchain)
            .map_err(|e| format!("images: {e:?}"))?;
        self.extent = extent;
        self.views = create_views(&self.device, &self.images, self.format)?;
        self.framebuffers = create_framebuffers(&self.device, self.render_pass, &self.views, extent)?;
        Ok(())
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            if let Some(imported) = self.dmabuf.take() {
                destroy_imported(&self.device, imported);
            }
            for (_, imported) in self.dmabuf_cache.drain() {
                destroy_imported(&self.device, imported);
            }
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_sampler(self.osd_sampler, None);
            self.device.destroy_descriptor_pool(self.desc_pool, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.pipeline_layout, None);
            self.device.destroy_render_pass(self.render_pass, None);
            for fb in self.framebuffers.drain(..) {
                self.device.destroy_framebuffer(fb, None);
            }
            for view in self.views.drain(..) {
                self.device.destroy_image_view(view, None);
            }
            destroy_texture(&self.device, &mut self.tex_y);
            destroy_texture(&self.device, &mut self.tex_uv);
            destroy_texture(&self.device, &mut self.osd_tex);
            destroy_staging(&self.device, &mut self.stage_y);
            destroy_staging(&self.device, &mut self.stage_uv);
            destroy_staging(&self.device, &mut self.osd_stage);
            self.device.destroy_pipeline(self.osd_pipeline, None);
            self.device.destroy_pipeline_layout(self.osd_pipeline_layout, None);
            self.device.destroy_descriptor_pool(self.osd_desc_pool, None);
            self.device.destroy_descriptor_set_layout(self.osd_desc_layout, None);
            self.swapchain_loader.destroy_swapchain(self.swapchain, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_semaphore(self.image_available, None);
            self.device.destroy_semaphore(self.render_done, None);
            self.device.destroy_fence(self.in_flight, None);
            self.device.destroy_descriptor_set_layout(self.desc_layout, None);
            self.device.destroy_device(None);
            self.surface_loader.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
            let _ = &self.entry;
        }
    }
}

fn pick_extent(caps: &vk::SurfaceCapabilitiesKHR, win: (u32, u32)) -> vk::Extent2D {
    if caps.current_extent.width != u32::MAX {
        caps.current_extent
    } else {
        vk::Extent2D {
            width: win.0.clamp(caps.min_image_extent.width, caps.max_image_extent.width).max(1),
            height: win.1.clamp(caps.min_image_extent.height, caps.max_image_extent.height).max(1),
        }
    }
}

fn create_render_pass(device: &ash::Device, format: vk::Format) -> Result<vk::RenderPass, String> {
    unsafe {
        let attachment = vk::AttachmentDescription::default()
            .format(format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::PRESENT_SRC_KHR);
        let color_ref = vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        let color_refs = [color_ref];
        let subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_refs);
        let dep = vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
        let attachments = [attachment];
        let subpasses = [subpass];
        let deps = [dep];
        device
            .create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attachments)
                    .subpasses(&subpasses)
                    .dependencies(&deps),
                None,
            )
            .map_err(|e| format!("render pass: {e:?}"))
    }
}

fn create_desc_layout(device: &ash::Device) -> Result<vk::DescriptorSetLayout, String> {
    unsafe {
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        device
            .create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
            .map_err(|e| format!("descriptor layout: {e:?}"))
    }
}

fn create_sampler(device: &ash::Device) -> Result<vk::Sampler, String> {
    unsafe {
        device
            .create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )
            .map_err(|e| format!("sampler: {e:?}"))
    }
}

fn create_osd_desc_layout(device: &ash::Device) -> Result<vk::DescriptorSetLayout, String> {
    unsafe {
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        device
            .create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
            .map_err(|e| format!("OSD descriptor layout: {e:?}"))
    }
}

fn write_osd_descriptor(device: &ash::Device, set: vk::DescriptorSet, tex: &Texture, sampler: vk::Sampler) {
    unsafe {
        let info = [vk::DescriptorImageInfo::default()
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image_view(tex.view)
            .sampler(sampler)];
        device.update_descriptor_sets(
            &[vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&info)],
            &[],
        );
    }
}

fn create_osd_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    layout: vk::PipelineLayout,
) -> Result<vk::Pipeline, String> {
    unsafe {
        let vert = shader_module(device, OSD_VERT_SPV)?;
        let frag = shader_module(device, OSD_FRAG_SPV)?;
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vert)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(frag)
                .name(c"main"),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA);
        let blend_attachments = [blend_attachment];
        let blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(&blend_attachments);
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic_state)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0);
        let pipelines = device
            .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
            .map_err(|(_, e)| format!("OSD pipeline: {e:?}"))?;
        device.destroy_shader_module(vert, None);
        device.destroy_shader_module(frag, None);
        Ok(pipelines[0])
    }
}

fn create_rgba_texture(
    device: &ash::Device,
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    w: u32,
    h: u32,
    rgba: &[u8],
) -> Result<(Texture, Staging), String> {
    unsafe {
        if rgba.len() < (w as usize) * (h as usize) * 4 {
            return Err("OSD atlas is smaller than its extent".into());
        }
        let format = vk::Format::R8G8B8A8_UNORM;
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D { width: w.max(1), height: h.max(1), depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = device.create_image(&image_info, None).map_err(|e| format!("OSD image: {e:?}"))?;
        let req = device.get_image_memory_requirements(image);
        let mem_type = find_memory(instance, pd, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        let memory = device
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(mem_type),
                None,
            )
            .map_err(|e| format!("OSD image memory: {e:?}"))?;
        device.bind_image_memory(image, memory, 0).map_err(|e| format!("bind OSD image: {e:?}"))?;
        let view = device
            .create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .base_mip_level(0)
                            .level_count(1)
                            .base_array_layer(0)
                            .layer_count(1),
                    ),
                None,
            )
            .map_err(|e| format!("OSD image view: {e:?}"))?;
        let size = (w as u64) * (h as u64) * 4;
        let buffer = device
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
            .map_err(|e| format!("OSD staging buffer: {e:?}"))?;
        let breq = device.get_buffer_memory_requirements(buffer);
        let btype = find_memory(
            instance,
            pd,
            breq.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let bmem = device
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(breq.size)
                    .memory_type_index(btype),
                None,
            )
            .map_err(|e| format!("OSD staging memory: {e:?}"))?;
        device.bind_buffer_memory(buffer, bmem, 0).map_err(|e| format!("bind OSD staging: {e:?}"))?;
        let ptr = device
            .map_memory(bmem, 0, breq.size, vk::MemoryMapFlags::empty())
            .map_err(|e| format!("map OSD staging: {e:?}"))? as *mut u8;
        std::ptr::copy_nonoverlapping(rgba.as_ptr(), ptr, (w as usize) * (h as usize) * 4);
        Ok((
            Texture { image, view, memory, width: w.max(1), height: h.max(1) },
            Staging { buffer, memory: bmem, ptr, pitch: 0 },
        ))
    }
}

fn create_osd_sampler(device: &ash::Device) -> Result<vk::Sampler, String> {
    unsafe {
        device
            .create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::NEAREST)
                    .min_filter(vk::Filter::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )
            .map_err(|e| format!("OSD sampler: {e:?}"))
    }
}

fn create_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    layout: vk::PipelineLayout,
) -> Result<vk::Pipeline, String> {
    unsafe {
        let vert = shader_module(device, VERT_SPV)?;
        let frag = shader_module(device, FRAG_SPV)?;
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vert)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(frag)
                .name(c"main"),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(false)
            .color_write_mask(vk::ColorComponentFlags::RGBA);
        let blend_attachments = [blend_attachment];
        let blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(&blend_attachments);
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic_state)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0);
        let pipelines = device
            .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
            .map_err(|(_, e)| format!("graphics pipeline: {e:?}"))?;
        device.destroy_shader_module(vert, None);
        device.destroy_shader_module(frag, None);
        Ok(pipelines[0])
    }
}

fn shader_module(device: &ash::Device, bytes: &[u8]) -> Result<vk::ShaderModule, String> {
    unsafe {
        let mut cursor = std::io::Cursor::new(bytes);
        let code = ash::util::read_spv(&mut cursor).map_err(|e| format!("SPIR-V: {e}"))?;
        device
            .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
            .map_err(|e| format!("shader module: {e:?}"))
    }
}

fn write_descriptors(
    device: &ash::Device,
    set: vk::DescriptorSet,
    y: &Texture,
    uv: &Texture,
    sampler: vk::Sampler,
) {
    unsafe {
        let y_info = [vk::DescriptorImageInfo::default()
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image_view(y.view)
            .sampler(sampler)];
        let uv_info = [vk::DescriptorImageInfo::default()
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image_view(uv.view)
            .sampler(sampler)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&y_info),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&uv_info),
        ];
        device.update_descriptor_sets(&writes, &[]);
    }
}

fn write_view_descriptors(
    device: &ash::Device,
    set: vk::DescriptorSet,
    y: vk::ImageView,
    uv: vk::ImageView,
    sampler: vk::Sampler,
) {
    unsafe {
        let y_info = [vk::DescriptorImageInfo::default()
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image_view(y)
            .sampler(sampler)];
        let uv_info = [vk::DescriptorImageInfo::default()
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image_view(uv)
            .sampler(sampler)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&y_info),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&uv_info),
        ];
        device.update_descriptor_sets(&writes, &[]);
    }
}

/// Translates the decoder's dma-buf description into the Vulkan images that
/// have to be created for it. Fails for anything the renderer cannot sample.
fn dmabuf_layers(frame: &crate::dec::Frame, d: &crate::dec::Dmabuf) -> Result<Vec<Layer>, String> {
    const DRM_FORMAT_NV12: u32 = 0x3231_564e;
    if d.fourcc != DRM_FORMAT_NV12 {
        return Err(format!(
            "decoder exported DRM fourcc {:#x}, only NV12 is supported",
            d.fourcc
        ));
    }
    if frame.width < 2 || frame.height < 2 || !frame.width.is_multiple_of(2) || !frame.height.is_multiple_of(2) {
        return Err(format!(
            "{}x{} is not a valid 4:2:0 frame size",
            frame.width, frame.height
        ));
    }
    let plane = |i: usize| {
        vk::SubresourceLayout::default()
            .offset(d.offsets[i] as u64)
            .row_pitch(d.pitches[i] as u64)
    };
    let full = vk::Extent2D {
        width: frame.width,
        height: frame.height,
    };
    match d.layers {
        // One NV12 layer: both planes live in one dma-buf object at different
        // offsets, which becomes a single two-plane VkImage.
        1 if d.fds[0] == d.fds[1] => Ok(vec![Layer {
            name: "NV12",
            format: vk::Format::G8_B8R8_2PLANE_420_UNORM,
            extent: full,
            layouts: vec![plane(0), plane(1)],
            fd: d.fds[0],
        }]),
        1 => Err("a single NV12 layer must export both planes from one dma-buf object".into()),
        // Two layers: separate R8 and R8G8 images.
        2 => Ok(vec![
            Layer {
                name: "R8",
                format: vk::Format::R8_UNORM,
                extent: full,
                layouts: vec![plane(0)],
                fd: d.fds[0],
            },
            Layer {
                name: "R8G8",
                format: vk::Format::R8G8_UNORM,
                extent: vk::Extent2D {
                    width: frame.width / 2,
                    height: frame.height / 2,
                },
                layouts: vec![plane(1)],
                fd: d.fds[1],
            },
        ]),
        n => Err(format!("decoder exported {n} dma-buf layers")),
    }
}

fn create_texture(
    device: &ash::Device,
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    w: u32,
    h: u32,
    luma: bool,
) -> Result<(Texture, Staging), String> {
    unsafe {
        let format = if luma { vk::Format::R8_UNORM } else { vk::Format::R8G8_UNORM };
        let (tw, th) = if luma { (w, h) } else { (w / 2, h / 2) };
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D { width: tw.max(1), height: th.max(1), depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = device.create_image(&image_info, None).map_err(|e| format!("image: {e:?}"))?;
        let req = device.get_image_memory_requirements(image);
        let mem_type = find_memory(instance, pd, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        let memory = device
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(mem_type),
                None,
            )
            .map_err(|e| format!("image memory: {e:?}"))?;
        device.bind_image_memory(image, memory, 0).map_err(|e| format!("bind image: {e:?}"))?;
        let view = device
            .create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .base_mip_level(0)
                            .level_count(1)
                            .base_array_layer(0)
                            .layer_count(1),
                    ),
                None,
            )
            .map_err(|e| format!("image view: {e:?}"))?;
        let text = Texture { image, view, memory, width: tw.max(1), height: th.max(1) };
        // Start with tightly packed rows; a padded decoder stride resizes the
        // staging buffer on the first frame.
        let texel_bytes = if luma { 1 } else { 2 };
        let staging = create_staging(device, instance, pd, tw, th, texel_bytes)?;
        Ok((text, staging))
    }
}

/// Creates a host-visible staging buffer for `rows` rows of `pitch` texels
/// (`texel_bytes` bytes each) and maps it.
unsafe fn create_staging(
    device: &ash::Device,
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    pitch: u32,
    rows: u32,
    texel_bytes: u32,
) -> Result<Staging, String> {
    let size = (pitch as u64) * (rows as u64) * (texel_bytes as u64);
    let buffer = device
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE),
            None,
        )
        .map_err(|e| format!("staging buffer: {e:?}"))?;
    let breq = device.get_buffer_memory_requirements(buffer);
    let btype = find_memory(
        instance,
        pd,
        breq.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )?;
    let bmem = device
        .allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(breq.size)
                .memory_type_index(btype),
            None,
        )
        .map_err(|e| format!("staging memory: {e:?}"))?;
    device.bind_buffer_memory(buffer, bmem, 0).map_err(|e| format!("bind staging: {e:?}"))?;
    let ptr = device
        .map_memory(bmem, 0, breq.size, vk::MemoryMapFlags::empty())
        .map_err(|e| format!("map staging: {e:?}"))? as *mut u8;
    Ok(Staging { buffer, memory: bmem, ptr, pitch })
}

fn find_memory(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    bits: u32,
    flags: vk::MemoryPropertyFlags,
) -> Result<u32, String> {
    unsafe {
        let props = instance.get_physical_device_memory_properties(pd);
        for i in 0..props.memory_type_count {
            let ok = bits & (1 << i) != 0
                && props.memory_types[i as usize].property_flags.contains(flags);
            if ok {
                return Ok(i);
            }
        }
        Err("no memory type for the requested flags".into())
    }
}

fn create_views(
    device: &ash::Device,
    images: &[vk::Image],
    format: vk::Format,
) -> Result<Vec<vk::ImageView>, String> {
    unsafe {
        let mut out = Vec::with_capacity(images.len());
        for &image in images {
            out.push(
                device
                    .create_image_view(
                        &vk::ImageViewCreateInfo::default()
                            .image(image)
                            .view_type(vk::ImageViewType::TYPE_2D)
                            .format(format)
                            .subresource_range(
                                vk::ImageSubresourceRange::default()
                                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                                    .base_mip_level(0)
                                    .level_count(1)
                                    .base_array_layer(0)
                                    .layer_count(1),
                            ),
                        None,
                    )
                    .map_err(|e| format!("swapchain view: {e:?}"))?,
            );
        }
        Ok(out)
    }
}

fn create_framebuffers(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    views: &[vk::ImageView],
    extent: vk::Extent2D,
) -> Result<Vec<vk::Framebuffer>, String> {
    unsafe {
        let mut out = Vec::with_capacity(views.len());
        for &view in views {
            out.push(
                device
                    .create_framebuffer(
                        &vk::FramebufferCreateInfo::default()
                            .render_pass(render_pass)
                            .attachments(&[view])
                            .width(extent.width)
                            .height(extent.height)
                            .layers(1),
                        None,
                    )
                    .map_err(|e| format!("framebuffer: {e:?}"))?,
            );
        }
        Ok(out)
    }
}

unsafe fn destroy_texture(device: &ash::Device, t: &mut Texture) {
    device.destroy_image_view(t.view, None);
    device.destroy_image(t.image, None);
    device.free_memory(t.memory, None);
    *t = Texture { image: vk::Image::null(), view: vk::ImageView::null(), memory: vk::DeviceMemory::null(), width: 0, height: 0 };
}

/// Releases an imported dma-buf frame: views first, then the images, then
/// the imported memory (which drops Vulkan's reference to the dma-buf).
unsafe fn destroy_imported(device: &ash::Device, f: ImportedFrame) {
    for view in f.views {
        if view != vk::ImageView::null() {
            device.destroy_image_view(view, None);
        }
    }
    for image in f.images {
        device.destroy_image(image, None);
    }
    for memory in f.memories {
        device.free_memory(memory, None);
    }
}

unsafe fn destroy_staging(device: &ash::Device, s: &mut Staging) {
    if !s.ptr.is_null() {
        device.unmap_memory(s.memory);
    }
    device.destroy_buffer(s.buffer, None);
    device.free_memory(s.memory, None);
    *s = Staging {
        buffer: vk::Buffer::null(),
        memory: vk::DeviceMemory::null(),
        ptr: std::ptr::null_mut(),
        pitch: 0,
    };
}
