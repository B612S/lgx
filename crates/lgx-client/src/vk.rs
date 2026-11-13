//! Vulkan presentation of decoded NV12 frames.
//!
//! The decoder produces CPU NV12 (hardware decode + transfer), which is
//! uploaded into two R8/R8G8 images and converted to RGB in a fragment
//! shader with the same full-range BT.709 matrix as the rest of lgx. The
//! swapchain is created from SDL's Vulkan surface; presentation is immediate
//! unless vsync is requested.

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
}

pub struct Renderer {
    entry: ash::Entry,
    instance: ash::Instance,
    surface_loader: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    device: ash::Device,
    physical: vk::PhysicalDevice,
    queue: vk::Queue,
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
    tex_y: Texture,
    tex_uv: Texture,
    stage_y: Staging,
    stage_uv: Staging,
    osd_tex: Texture,
    osd_stage: Staging,
    osd_ready: bool,
    osd_draw: Option<(bool, f32)>,
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
            .application_version(vk::make_api_version(0, 0, 2, 0))
            .engine_name(app_name.as_c_str())
            .engine_version(vk::make_api_version(0, 0, 2, 0))
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
        let mut chosen = None;
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
            let discrete = props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU;
            let score = if discrete { 2 } else { 1 };
            if chosen.as_ref().map_or(true, |(_, s, _)| score > *s) {
                chosen = Some((pd, score, family));
            }
        }
        let (pd, _, family) = chosen.ok_or("no Vulkan device with a present queue")?;
        let props = instance.get_physical_device_properties(pd);
        let device_name = CStr::from_ptr(props.device_name.as_ptr()).to_string_lossy().into_owned();

        let priority = [1.0f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&priority)];
        let device_exts = [ash::khr::swapchain::NAME.as_ptr()];
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_info)
            .enabled_extension_names(&device_exts);
        let device = instance
            .create_device(pd, &device_info, None)
            .map_err(|e| format!("vkCreateDevice: {e:?}"))?;
        let queue = device.get_device_queue(family, 0);
        let swapchain_loader = ash::khr::swapchain::Device::new(&instance, &device);

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

        // OSD atlas: generated once from system fonts, uploaded before the
        // first frame. A missing font still leaves a blank card rather than
        // breaking video.
        let (osd_w, osd_h, osd_rgba) = match crate::osd::atlas() {
            Some(atlas) => (atlas.width, atlas.height, atlas.rgba),
            None => {
                eprintln!("lgx-client: OSD font not found; OSD overlay disabled");
                (2, 1, vec![0u8; 8])
            }
        };
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
        write_osd_descriptor(&device, osd_desc_set, &osd_tex, sampler);
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
            tex_y,
            tex_uv,
            stage_y,
            stage_uv,
            osd_tex,
            osd_stage,
            osd_ready: false,
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
        })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn can_import_dmabuf(&self) -> bool {
        false
    }

    pub fn size(&self) -> (u32, u32) {
        (self.extent.width, self.extent.height)
    }

    pub fn set_osd(&mut self, osd: Option<(bool, f32)>) {
        self.osd_draw = osd;
    }

    /// Clears the window once so it never shows undefined memory.
    pub fn clear(&mut self, win: (u32, u32)) {
        let _ = win;
        unsafe {
            self.render_black();
        }
    }

    pub fn draw(&mut self, frame: &crate::dec::Frame, win: (u32, u32)) -> bool {
        let crate::dec::Pixels::Nv12(nv) = &frame.pixels else {
            return false;
        };
        unsafe {
            if frame.width != self.tex_y.width || frame.height != self.tex_y.height {
                if let Err(e) = self.resize_textures(frame.width, frame.height) {
                    eprintln!("lgx-client: Vulkan textures: {e}");
                    return false;
                }
            }
            self.upload(frame.width, frame.height, nv.y, nv.y_stride, nv.uv, nv.uv_stride);
            match self.present(win) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("lgx-client: Vulkan present: {e}");
                    false
                }
            }
        }
    }

    unsafe fn upload(
        &mut self,
        w: u32,
        h: u32,
        y: *const u8,
        y_stride: u32,
        uv: *const u8,
        uv_stride: u32,
    ) {
        for row in 0..h as usize {
            std::ptr::copy_nonoverlapping(
                y.add(row * y_stride as usize),
                self.stage_y.ptr.add(row * w as usize),
                w as usize,
            );
        }
        for row in 0..(h / 2) as usize {
            std::ptr::copy_nonoverlapping(
                uv.add(row * uv_stride as usize),
                self.stage_uv.ptr.add(row * w as usize),
                w as usize,
            );
        }
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
        Ok(())
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

    unsafe fn draw_osd(&self, cmd: vk::CommandBuffer, enabled: bool, alpha: f32) {
        if alpha <= 0.0 {
            return;
        }
        let sw = self.extent.width as f32;
        let sh = self.extent.height as f32;
        let scale = (sh / 1080.0).clamp(0.8, 1.75);
        let card_w = 360.0 * scale;
        let card_h = 84.0 * scale;
        let x0 = (sw - card_w) / 2.0;
        let y1 = sh - 64.0 * scale;
        let y0 = y1 - card_h;
        let nx0 = 2.0 * x0 / sw - 1.0;
        let nx1 = 2.0 * (x0 + card_w) / sw - 1.0;
        let ny0 = 2.0 * y0 / sh - 1.0;
        let ny1 = 2.0 * y1 / sh - 1.0;
        let uv = if enabled {
            [0.0, 0.0, 0.5, 1.0]
        } else {
            [0.5, 0.0, 1.0, 1.0]
        };
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

    unsafe fn present(&mut self, win: (u32, u32)) -> Result<(), String> {
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
        }

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
            .buffer_row_length(0)
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
        let rect = fit_rect(self.tex_y.width, self.tex_y.height, sw, sh);
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
        if let Some((enabled, alpha)) = self.osd_draw {
            self.draw_osd(cmd, enabled, alpha);
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
            self.device.destroy_sampler(self.sampler, None);
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
            Staging { buffer, memory: bmem, ptr },
        ))
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
        let stage_size = if luma {
            (w as u64) * (h as u64)
        } else {
            (w as u64) * (h as u64) / 2
        };
        let buffer = device
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(stage_size)
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
        Ok((
            Texture { image, view, memory, width: tw.max(1), height: th.max(1) },
            Staging { buffer, memory: bmem, ptr },
        ))
    }
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

unsafe fn destroy_staging(device: &ash::Device, s: &mut Staging) {
    if !s.ptr.is_null() {
        device.unmap_memory(s.memory);
    }
    device.destroy_buffer(s.buffer, None);
    device.free_memory(s.memory, None);
    *s = Staging { buffer: vk::Buffer::null(), memory: vk::DeviceMemory::null(), ptr: std::ptr::null_mut() };
}
