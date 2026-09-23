//! HDR output path (HDR10 / PQ and HLG).
//!
//! SDL2's 2D renderer is 8-bit SDR only, so for HDR we drive a Vulkan swapchain
//! directly on the SDL window:
//!
//! * The swapchain uses a 10-bit format (`A2B10G10R10` preferred) together with the
//!   `HDR10_ST2084` (PQ) or `HDR10_HLG` colour space, so the compositor/driver treats
//!   the image contents as BT.2020 PQ/HLG encoded.
//! * The values coming from ColourSpace are already encoded code values, so they are
//!   written to the image **unchanged** (only normalised to 0.0..=1.0). No transfer
//!   function, matrix or tone mapping is applied by this program.
//! * Patches are drawn with `vkCmdClearAttachments`, so no shaders/pipelines are needed
//!   and every patch is a flat, exact colour.
//! * `VK_EXT_hdr_metadata` is used to pass the mastering display / MaxCLL / MaxFALL
//!   information to the compositor (and from there to the display's HDR infoframe).

use ash::vk::Handle;
use ash::{ext, khr, vk, Entry, StaticFn};
use sdl2::video::Window;
use std::ffi::{c_char, CStr, CString};

use crate::lan::ColorRGB;

#[derive(Debug, thiserror::Error)]
pub enum HdrError {
    #[error("Vulkan error: {0}")]
    Vk(#[from] vk::Result),
    #[error("{0}")]
    Other(String),
}

fn other<T>(msg: impl Into<String>) -> Result<T, HdrError> {
    Err(HdrError::Other(msg.into()))
}

// -------------------------------------------------------------------------------------
// Public configuration types
// -------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HdrMode {
    /// Normal 8-bit SDR output through the SDL renderer (original behaviour).
    Sdr,
    /// HDR10: BT.2020 primaries, SMPTE ST 2084 (PQ) transfer function.
    Hdr10,
    /// HLG: BT.2020 primaries, ARIB STD-B67 transfer function.
    Hlg,
}

impl HdrMode {
    pub fn label(self) -> &'static str {
        match self {
            HdrMode::Sdr => "SDR",
            HdrMode::Hdr10 => "HDR10",
            HdrMode::Hlg => "HLG",
        }
    }
}

impl argh::FromArgValue for HdrMode {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "sdr" | "off" => Ok(HdrMode::Sdr),
            "hdr10" | "pq" | "st2084" => Ok(HdrMode::Hdr10),
            "hlg" => Ok(HdrMode::Hlg),
            other => Err(format!("unknown mode '{other}', expected sdr, hdr10 or hlg")),
        }
    }
}

/// Mastering display primaries reported in the HDR metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primaries {
    Bt2020,
    P3D65,
}

impl argh::FromArgValue for Primaries {
    fn from_arg_value(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "bt2020" | "rec2020" | "2020" => Ok(Primaries::Bt2020),
            "p3d65" | "p3" | "dci-p3" => Ok(Primaries::P3D65),
            other => Err(format!("unknown primaries '{other}', expected bt2020 or p3d65")),
        }
    }
}

/// Static HDR metadata (CTA-861.3 / SMPTE ST 2086) sent alongside the signal.
#[derive(Debug, Clone, Copy)]
pub struct HdrMetadata {
    pub primaries: Primaries,
    /// Mastering display peak luminance in cd/m².
    pub max_luminance: f32,
    /// Mastering display black level in cd/m².
    pub min_luminance: f32,
    /// MaxCLL in cd/m² (0 = unspecified).
    pub max_cll: f32,
    /// MaxFALL in cd/m² (0 = unspecified).
    pub max_fall: f32,
}

impl Default for HdrMetadata {
    fn default() -> Self {
        Self {
            primaries: Primaries::Bt2020,
            max_luminance: 1000.0,
            min_luminance: 0.0001,
            max_cll: 0.0,
            max_fall: 0.0,
        }
    }
}

impl HdrMetadata {
    fn to_vk(self) -> vk::HdrMetadataEXT<'static> {
        let xy = |x, y| vk::XYColorEXT { x, y };
        let (r, g, b) = match self.primaries {
            Primaries::Bt2020 => (xy(0.708, 0.292), xy(0.170, 0.797), xy(0.131, 0.046)),
            Primaries::P3D65 => (xy(0.680, 0.320), xy(0.265, 0.690), xy(0.150, 0.060)),
        };
        vk::HdrMetadataEXT::default()
            .display_primary_red(r)
            .display_primary_green(g)
            .display_primary_blue(b)
            .white_point(xy(0.3127, 0.3290))
            .max_luminance(self.max_luminance)
            .min_luminance(self.min_luminance)
            .max_content_light_level(self.max_cll)
            .max_frame_average_light_level(self.max_fall)
    }
}

/// A flat-coloured rectangle in output pixels. `rgb` is the encoded signal, 0.0..=1.0.
#[derive(Debug, Clone, Copy)]
pub struct FillRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub rgb: [f32; 3],
}

/// Normalise a `ColorRGB` (u16 code values + bit depth) to 0.0..=1.0 signal levels.
/// This is a pure code-value scale: no gamma, matrix or range conversion happens here.
pub fn color_to_unit_rgb(color: ColorRGB) -> [f32; 3] {
    let bits = if color.depth_bits == 0 { 8 } else { color.depth_bits.min(16) };
    let max_in = ((1u32 << bits as u32) - 1) as f32;
    let n = |v: u16| (v as f32 / max_in).clamp(0.0, 1.0);
    [n(color.red), n(color.green), n(color.blue)]
}

// -------------------------------------------------------------------------------------
// Vulkan presenter
// -------------------------------------------------------------------------------------

pub struct HdrPresenter {
    // NOTE: `window` must outlive every Vulkan object created from it; `Drop` below
    // destroys them all before the fields (including `window`) are dropped.
    window: Window,
    _entry: Entry,
    instance: ash::Instance,
    surface_loader: khr::surface::Instance,
    surface: vk::SurfaceKHR,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    swapchain_loader: khr::swapchain::Device,
    hdr_loader: Option<ext::hdr_metadata::Device>,
    surface_format: vk::SurfaceFormatKHR,
    metadata: HdrMetadata,
    render_pass: vk::RenderPass,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    image_available: vk::Semaphore,
    in_flight: vk::Fence,
    // swapchain-dependent state
    swapchain: vk::SwapchainKHR,
    extent: vk::Extent2D,
    image_views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    render_finished: Vec<vk::Semaphore>,
}

impl HdrPresenter {
    /// Create the presenter on an SDL window that was built with `.vulkan()`.
    ///
    /// Fails (rather than silently falling back to SDR) when the system cannot provide
    /// the requested HDR swapchain; a calibration tool must never measure the wrong signal.
    pub fn new(window: Window, mode: HdrMode, metadata: HdrMetadata) -> Result<Self, HdrError> {
        let colour_space = match mode {
            HdrMode::Hdr10 => vk::ColorSpaceKHR::HDR10_ST2084_EXT,
            HdrMode::Hlg => vk::ColorSpaceKHR::HDR10_HLG_EXT,
            HdrMode::Sdr => return other("HdrPresenter cannot be used for SDR output"),
        };

        // Use SDL's vkGetInstanceProcAddr so ash and SDL share the same Vulkan loader.
        let entry = unsafe {
            let ptr = window
                .subsystem()
                .vulkan_get_proc_address_function()
                .map_err(|e| HdrError::Other(format!("SDL cannot provide Vulkan loader: {e}")))?;
            let get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr = std::mem::transmute(ptr);
            Entry::from_static_fn(StaticFn { get_instance_proc_addr })
        };

        // ---- instance -----------------------------------------------------------------
        let sdl_exts = window
            .vulkan_instance_extensions()
            .map_err(|e| HdrError::Other(format!("SDL Vulkan instance extensions: {e}")))?;
        let mut ext_names: Vec<CString> =
            sdl_exts.iter().map(|s| CString::new(*s).unwrap()).collect();

        let available_instance_exts = unsafe { entry.enumerate_instance_extension_properties(None)? };
        let has_instance_ext = |name: &CStr| {
            available_instance_exts
                .iter()
                .any(|e| e.extension_name_as_c_str().map(|n| n == name).unwrap_or(false))
        };
        if !has_instance_ext(ext::swapchain_colorspace::NAME) {
            return other(
                "The Vulkan driver does not expose VK_EXT_swapchain_colorspace, \
                 so HDR swapchains are not available.",
            );
        }
        ext_names.push(ext::swapchain_colorspace::NAME.to_owned());
        let ext_ptrs: Vec<*const c_char> = ext_names.iter().map(|s| s.as_ptr()).collect();

        let app_info = vk::ApplicationInfo::default()
            .application_name(c"calibrationclient")
            .api_version(vk::API_VERSION_1_1);
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app_info)
                    .enabled_extension_names(&ext_ptrs),
                None,
            )?
        };

        // ---- surface ------------------------------------------------------------------
        let raw_surface = window
            .vulkan_create_surface(instance.handle().as_raw() as usize)
            .map_err(|e| HdrError::Other(format!("SDL could not create a Vulkan surface: {e}")))?;
        let surface = vk::SurfaceKHR::from_raw(raw_surface);
        let surface_loader = khr::surface::Instance::new(&entry, &instance);

        // ---- physical device / queue family / HDR format ------------------------------
        let wanted_formats = [
            vk::Format::A2B10G10R10_UNORM_PACK32, // preferred: NVIDIA can scan this out directly
            vk::Format::A2R10G10B10_UNORM_PACK32,
        ];
        let mut seen: Vec<String> = Vec::new();
        let mut chosen = None;
        for pd in unsafe { instance.enumerate_physical_devices()? } {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            let name = props
                .device_name_as_c_str()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            let dev_exts = unsafe { instance.enumerate_device_extension_properties(pd)? };
            let has_dev_ext = |name: &CStr| {
                dev_exts
                    .iter()
                    .any(|e| e.extension_name_as_c_str().map(|n| n == name).unwrap_or(false))
            };
            if !has_dev_ext(khr::swapchain::NAME) {
                continue;
            }

            let queue_family = unsafe { instance.get_physical_device_queue_family_properties(pd) }
                .iter()
                .enumerate()
                .find(|(i, q)| {
                    q.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                        && unsafe {
                            surface_loader
                                .get_physical_device_surface_support(pd, *i as u32, surface)
                                .unwrap_or(false)
                        }
                })
                .map(|(i, _)| i as u32);
            let Some(queue_family) = queue_family else { continue };

            let formats =
                unsafe { surface_loader.get_physical_device_surface_formats(pd, surface)? };
            for f in &formats {
                seen.push(format!("{name}: {:?} / {:?}", f.format, f.color_space));
            }
            let format = wanted_formats.iter().find_map(|wf| {
                formats
                    .iter()
                    .find(|f| f.format == *wf && f.color_space == colour_space)
                    .copied()
            });
            if let Some(format) = format {
                chosen = Some((pd, queue_family, format, name, has_dev_ext(ext::hdr_metadata::NAME)));
                break;
            }
        }

        let Some((physical_device, queue_family, surface_format, device_name, has_hdr_md)) = chosen
        else {
            let mut msg = format!(
                "No GPU offers a 10-bit {} surface for this window.\n\n\
                 HDR needs: a compositor with HDR enabled (e.g. KDE Plasma 6 or another \
                 colour-management-v1 compositor), Mesa 25.1+ or a recent NVIDIA driver, and \
                 the SDL Wayland video driver (SDL_VIDEODRIVER=wayland). It does not work \
                 through XWayland.",
                match mode {
                    HdrMode::Hlg => "HDR10 HLG",
                    _ => "HDR10 PQ",
                }
            );
            if !seen.is_empty() {
                msg.push_str("\n\nSurface formats offered:\n");
                msg.push_str(&seen.join("\n"));
            }
            unsafe {
                surface_loader.destroy_surface(surface, None);
                instance.destroy_instance(None);
            }
            return other(msg);
        };
        eprintln!(
            "HDR: {} on \"{}\" using {:?} / {:?} (VK_EXT_hdr_metadata: {})",
            mode.label(),
            device_name,
            surface_format.format,
            surface_format.color_space,
            if has_hdr_md { "yes" } else { "no" }
        );
        if !has_hdr_md {
            eprintln!("HDR: warning, VK_EXT_hdr_metadata missing; the display may not receive static metadata");
        }

        // ---- logical device -----------------------------------------------------------
        let mut dev_ext_ptrs: Vec<*const c_char> = vec![khr::swapchain::NAME.as_ptr()];
        if has_hdr_md {
            dev_ext_ptrs.push(ext::hdr_metadata::NAME.as_ptr());
        }
        let priorities = [1.0f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)];
        let device = unsafe {
            instance.create_device(
                physical_device,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_info)
                    .enabled_extension_names(&dev_ext_ptrs),
                None,
            )?
        };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let swapchain_loader = khr::swapchain::Device::new(&instance, &device);
        let hdr_loader = has_hdr_md.then(|| ext::hdr_metadata::Device::new(&instance, &device));

        // ---- render pass (single colour attachment, cleared, presented) ---------------
        let attachment = [vk::AttachmentDescription::default()
            .format(surface_format.format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];
        let colour_ref = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let subpass = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&colour_ref)];
        let dependency = [vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
        let render_pass = unsafe {
            device.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attachment)
                    .subpasses(&subpass)
                    .dependencies(&dependency),
                None,
            )?
        };

        // ---- commands and sync --------------------------------------------------------
        let command_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?
        };
        let command_buffer = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0]
        };
        let image_available =
            unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)? };
        let in_flight = unsafe {
            device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )?
        };

        let mut presenter = Self {
            window,
            _entry: entry,
            instance,
            surface_loader,
            surface,
            physical_device,
            device,
            queue,
            swapchain_loader,
            hdr_loader,
            surface_format,
            metadata,
            render_pass,
            command_pool,
            command_buffer,
            image_available,
            in_flight,
            swapchain: vk::SwapchainKHR::null(),
            extent: vk::Extent2D { width: 0, height: 0 },
            image_views: Vec::new(),
            framebuffers: Vec::new(),
            render_finished: Vec::new(),
        };
        // If this fails, `presenter` is dropped and `Drop` cleans everything up.
        presenter.recreate_swapchain()?;
        Ok(presenter)
    }

    pub fn window_mut(&mut self) -> &mut Window {
        &mut self.window
    }

    /// Drawable size in physical pixels (what the swapchain is sized to).
    pub fn output_size(&self) -> (u32, u32) {
        self.window.vulkan_drawable_size()
    }

    fn destroy_swapchain_resources(&mut self) {
        unsafe {
            for fb in self.framebuffers.drain(..) {
                self.device.destroy_framebuffer(fb, None);
            }
            for iv in self.image_views.drain(..) {
                self.device.destroy_image_view(iv, None);
            }
            for s in self.render_finished.drain(..) {
                self.device.destroy_semaphore(s, None);
            }
        }
    }

    fn recreate_swapchain(&mut self) -> Result<(), HdrError> {
        unsafe { self.device.device_wait_idle()? };
        self.destroy_swapchain_resources();

        let caps = unsafe {
            self.surface_loader
                .get_physical_device_surface_capabilities(self.physical_device, self.surface)?
        };
        let (dw, dh) = self.output_size();
        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: dw.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: dh.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        };

        // Minimised / zero-sized window: nothing to create yet.
        if extent.width == 0 || extent.height == 0 {
            if self.swapchain != vk::SwapchainKHR::null() {
                unsafe { self.swapchain_loader.destroy_swapchain(self.swapchain, None) };
                self.swapchain = vk::SwapchainKHR::null();
            }
            self.extent = extent;
            return Ok(());
        }

        let mut image_count = caps.min_image_count + 1;
        if caps.max_image_count != 0 {
            image_count = image_count.min(caps.max_image_count);
        }
        let composite_alpha = [
            vk::CompositeAlphaFlagsKHR::OPAQUE,
            vk::CompositeAlphaFlagsKHR::INHERIT,
            vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
            vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
        ]
        .into_iter()
        .find(|c| caps.supported_composite_alpha.contains(*c))
        .unwrap_or(vk::CompositeAlphaFlagsKHR::OPAQUE);

        let old = self.swapchain;
        let swapchain = unsafe {
            self.swapchain_loader.create_swapchain(
                &vk::SwapchainCreateInfoKHR::default()
                    .surface(self.surface)
                    .min_image_count(image_count)
                    .image_format(self.surface_format.format)
                    .image_color_space(self.surface_format.color_space)
                    .image_extent(extent)
                    .image_array_layers(1)
                    .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                    .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .pre_transform(caps.current_transform)
                    .composite_alpha(composite_alpha)
                    .present_mode(vk::PresentModeKHR::FIFO)
                    .clipped(true)
                    .old_swapchain(old),
                None,
            )?
        };
        if old != vk::SwapchainKHR::null() {
            unsafe { self.swapchain_loader.destroy_swapchain(old, None) };
        }
        self.swapchain = swapchain;
        self.extent = extent;

        if let Some(hdr) = &self.hdr_loader {
            let md = [self.metadata.to_vk()];
            unsafe { hdr.set_hdr_metadata(&[swapchain], &md) };
        }

        let images = unsafe { self.swapchain_loader.get_swapchain_images(swapchain)? };
        for image in images {
            let view = unsafe {
                self.device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(self.surface_format.format)
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
            self.image_views.push(view);
            let views = [view];
            let fb = unsafe {
                self.device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(self.render_pass)
                        .attachments(&views)
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )?
            };
            self.framebuffers.push(fb);
            let sem = unsafe {
                self.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?
            };
            self.render_finished.push(sem);
        }
        Ok(())
    }

    /// Draw one frame: fill everything with `background`, then each rectangle on top.
    /// All colours are encoded signal levels (0.0..=1.0) and are written as-is.
    pub fn draw(&mut self, background: [f32; 3], rects: &[FillRect]) -> Result<(), HdrError> {
        // Resize / recreate if the window size changed or we have no swapchain yet.
        let (dw, dh) = self.output_size();
        if self.swapchain == vk::SwapchainKHR::null()
            || (dw, dh) != (self.extent.width, self.extent.height)
        {
            self.recreate_swapchain()?;
            if self.swapchain == vk::SwapchainKHR::null() {
                return Ok(()); // minimised
            }
        }

        unsafe { self.device.wait_for_fences(&[self.in_flight], true, u64::MAX)? };

        // Short timeout so a hidden/occluded window can never freeze the event loop.
        let image_index = match unsafe {
            self.swapchain_loader.acquire_next_image(
                self.swapchain,
                100_000_000,
                self.image_available,
                vk::Fence::null(),
            )
        } {
            Ok((idx, _suboptimal)) => idx,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => return self.recreate_swapchain(),
            Err(vk::Result::TIMEOUT) | Err(vk::Result::NOT_READY) => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        unsafe {
            self.device.reset_fences(&[self.in_flight])?;
            let cb = self.command_buffer;
            self.device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
            self.device.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;

            let clear_colour = |rgb: [f32; 3]| vk::ClearValue {
                color: vk::ClearColorValue { float32: [rgb[0], rgb[1], rgb[2], 1.0] },
            };
            let clear_values = [clear_colour(background)];
            self.device.cmd_begin_render_pass(
                cb,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.render_pass)
                    .framebuffer(self.framebuffers[image_index as usize])
                    .render_area(vk::Rect2D { offset: vk::Offset2D { x: 0, y: 0 }, extent: self.extent })
                    .clear_values(&clear_values),
                vk::SubpassContents::INLINE,
            );

            for r in rects {
                // Clip to the framebuffer; clear rects must lie inside the render area.
                let x0 = r.x.clamp(0, self.extent.width as i32);
                let y0 = r.y.clamp(0, self.extent.height as i32);
                let x1 = (r.x as i64 + r.w as i64).clamp(0, self.extent.width as i64) as i32;
                let y1 = (r.y as i64 + r.h as i64).clamp(0, self.extent.height as i64) as i32;
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                let attachment = [vk::ClearAttachment {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    color_attachment: 0,
                    clear_value: clear_colour(r.rgb),
                }];
                let clear_rect = [vk::ClearRect {
                    rect: vk::Rect2D {
                        offset: vk::Offset2D { x: x0, y: y0 },
                        extent: vk::Extent2D { width: (x1 - x0) as u32, height: (y1 - y0) as u32 },
                    },
                    base_array_layer: 0,
                    layer_count: 1,
                }];
                self.device.cmd_clear_attachments(cb, &attachment, &clear_rect);
            }

            self.device.cmd_end_render_pass(cb);
            self.device.end_command_buffer(cb)?;

            let wait = [self.image_available];
            let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            let cbs = [cb];
            let signal = [self.render_finished[image_index as usize]];
            self.device.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default()
                    .wait_semaphores(&wait)
                    .wait_dst_stage_mask(&stages)
                    .command_buffers(&cbs)
                    .signal_semaphores(&signal)],
                self.in_flight,
            )?;

            let swapchains = [self.swapchain];
            let indices = [image_index];
            match self.swapchain_loader.queue_present(
                self.queue,
                &vk::PresentInfoKHR::default()
                    .wait_semaphores(&signal)
                    .swapchains(&swapchains)
                    .image_indices(&indices),
            ) {
                Ok(false) => {}
                Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.recreate_swapchain()?,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

impl Drop for HdrPresenter {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.destroy_swapchain_resources();
            if self.swapchain != vk::SwapchainKHR::null() {
                self.swapchain_loader.destroy_swapchain(self.swapchain, None);
            }
            self.device.destroy_fence(self.in_flight, None);
            self.device.destroy_semaphore(self.image_available, None);
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_device(None);
            self.surface_loader.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_bit_codes_are_exact() {
        let c = ColorRGB::from_components_u16(0, 512, 1023, 10);
        assert_eq!(color_to_unit_rgb(c), [0.0, 512.0 / 1023.0, 1.0]);
    }

    #[test]
    fn eight_bit_full_range_maps_to_one() {
        let c = ColorRGB::from_components_u16(255, 0, 128, 8);
        let [r, g, b] = color_to_unit_rgb(c);
        assert_eq!((r, g), (1.0, 0.0));
        assert!((b - 128.0 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let c = ColorRGB::from_components_u16(2000, 0, 0, 10);
        assert_eq!(color_to_unit_rgb(c)[0], 1.0);
    }

    #[test]
    fn float_to_10bit_round_trips_every_code() {
        // Vulkan converts a float clear value to UNORM with round-to-nearest, so every
        // 10-bit code normalised as code/1023 must land back on the same code.
        for code in 0u32..=1023 {
            let f = code as f32 / 1023.0;
            assert_eq!((f * 1023.0).round() as u32, code);
        }
    }
}
