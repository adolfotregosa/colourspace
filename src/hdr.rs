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
use std::time::{Duration, Instant};

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
    /// The word used on the command line and in the saved settings.
    pub fn key(self) -> &'static str {
        match self {
            HdrMode::Sdr => "sdr",
            HdrMode::Hdr10 => "hdr10",
            HdrMode::Hlg => "hlg",
        }
    }

    pub fn from_key(text: &str) -> Option<Self> {
        <Self as argh::FromArgValue>::from_arg_value(text).ok()
    }

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

impl Primaries {
    /// The word used on the command line and in the saved settings.
    pub fn key(self) -> &'static str {
        match self {
            Primaries::Bt2020 => "bt2020",
            Primaries::P3D65 => "p3d65",
        }
    }

    pub fn from_key(text: &str) -> Option<Self> {
        <Self as argh::FromArgValue>::from_arg_value(text).ok()
    }
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

/// When must the swapchain be (re)built?
///
/// * the window's drawable size changed since it was created, or
/// * there is no swapchain (window was minimised) - retried at most every 100 ms, because a
///   minimise/restore does not always change the reported size.
///
/// Comparing against the size the swapchain was *requested* at (not `current_extent`) matters:
/// on some platforms the surface reports a fixed extent that differs from SDL's drawable size,
/// and comparing against that would rebuild the swapchain on every single frame.
fn rebuild_needed(
    drawable: (u32, u32),
    requested: (u32, u32),
    have_swapchain: bool,
    since_last_rebuild: Duration,
) -> bool {
    drawable != requested || (!have_swapchain && since_last_rebuild >= Duration::from_millis(100))
}

// -------------------------------------------------------------------------------------
// Availability check (used by the startup window)
// -------------------------------------------------------------------------------------

/// Which HDR outputs this system can really provide, as found by `probe`.
#[derive(Debug, Clone, Default)]
pub struct HdrSupport {
    pub hdr10: bool,
    pub hlg: bool,
    /// A 10-bit surface in the normal SDR (sRGB) colour space is offered: SDR patches can be
    /// shown with their full 10-bit values instead of being rounded to 8 bits.
    pub sdr10: bool,
    /// SDL video driver the check ran on ("wayland", "x11", ...).
    pub driver: String,
    /// Set when the check itself could not be completed (no Vulkan, no window, ...).
    pub problem: Option<String>,
    /// KDE reports that HDR is switched off in the display settings. Vulkan cannot tell:
    /// KWin still offers HDR10 surfaces on such a display (it tone-maps them to SDR), so
    /// a measurement would silently be taken from a converted SDR signal.
    pub kde_hdr_off: bool,
}

impl HdrSupport {
    /// Can an HDR window be used (and trusted) here?
    pub fn any(&self) -> bool {
        (self.hdr10 || self.hlg) && !self.kde_hdr_off
    }
}

/// Remove ANSI colour sequences (kscreen-doctor colours its output).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Read the HDR state out of `kscreen-doctor -o` (text, "HDR: enabled") or `-j` (JSON,
/// `"hdr": true`) output. `Some(true)` if any display has HDR on, `Some(false)` if displays
/// report it and all are off, `None` if the output says nothing usable (fail-safe: unknown).
fn parse_kde_hdr_state(text: &str) -> Option<bool> {
    let clean = strip_ansi(text).to_lowercase();
    let (mut on, mut off) = (false, false);
    for key in ["hdr:", "\"hdr\":"] {
        let mut rest = clean.as_str();
        while let Some(pos) = rest.find(key) {
            rest = &rest[pos + key.len()..];
            let word: String = rest.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
            match word.as_str() {
                "enabled" | "true" => on = true,
                "disabled" | "false" => off = true,
                _ => {} // e.g. "incapable"
            }
        }
    }
    if on {
        Some(true)
    } else if off {
        Some(false)
    } else {
        None
    }
}

/// Ask KDE whether HDR is enabled. `run` executes `kscreen-doctor` with the given arguments
/// and returns everything it printed. Only KDE sessions are asked.
fn kde_hdr_state_with(desktop: &str, run: impl Fn(&[&str]) -> Option<String>) -> Option<bool> {
    if !desktop.to_lowercase().contains("kde") {
        return None;
    }
    for args in [&["-o"][..], &["-j"][..]] {
        if let Some(state) = run(args).and_then(|text| parse_kde_hdr_state(&text)) {
            return Some(state);
        }
    }
    None
}

/// Run `kscreen-doctor`, giving up after a few seconds. Its text output goes through Qt's
/// logging, i.e. stderr, so both streams are returned.
fn run_kscreen_doctor(args: &[&str]) -> Option<String> {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new("kscreen-doctor")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push('\n');
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            Some(text)
        }
        _ => None,
    }
}

/// 10-bit formats the presenter can use.
const HDR_FORMATS: [vk::Format; 2] = [
    vk::Format::A2B10G10R10_UNORM_PACK32, // preferred: NVIDIA can scan this out directly
    vk::Format::A2R10G10B10_UNORM_PACK32,
];

/// (HDR10 PQ offered, HLG offered, 10-bit SDR offered) among a surface's formats.
fn classify_formats(formats: &[vk::SurfaceFormatKHR]) -> (bool, bool, bool) {
    let offers = |space: vk::ColorSpaceKHR| {
        formats.iter().any(|f| HDR_FORMATS.contains(&f.format) && f.color_space == space)
    };
    (
        offers(vk::ColorSpaceKHR::HDR10_ST2084_EXT),
        offers(vk::ColorSpaceKHR::HDR10_HLG_EXT),
        offers(vk::ColorSpaceKHR::SRGB_NONLINEAR),
    )
}

/// HDR needs the native Wayland SDL driver (XWayland has no HDR). Ask for it unless the user
/// chose a driver explicitly. Must run before SDL is initialised.
pub fn prefer_wayland_driver() {
    if std::env::var_os("SDL_VIDEODRIVER").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        sdl2::hint::set("SDL_VIDEODRIVER", "wayland");
    }
}

/// Find out whether an HDR10 / HLG window can be created here, without showing anything:
/// open a small hidden Vulkan window on the driver HDR would use and ask which surface
/// formats it is offered. Uses (and fully releases) its own SDL context.
pub fn probe() -> HdrSupport {
    probe_with(true)
}

/// Same, but without asking the desktop whether HDR is switched on. Enough for deciding
/// whether SDR patches can use the 10-bit path (`sdr10`), which is all a plain SDR run needs.
pub fn probe_quick() -> HdrSupport {
    probe_with(false)
}

fn probe_with(ask_desktop: bool) -> HdrSupport {
    prefer_wayland_driver();
    let mut support = match probe_inner() {
        Ok(support) => support,
        Err(problem) => HdrSupport { problem: Some(problem), ..Default::default() },
    };
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let kde_state = if ask_desktop && (support.hdr10 || support.hlg) {
        kde_hdr_state_with(&desktop, run_kscreen_doctor)
    } else {
        None
    };
    support.kde_hdr_off = kde_state == Some(false);
    eprintln!(
        "HDR check: HDR10={} HLG={} SDR-10bit={} (SDL driver: {}){}{}",
        support.hdr10,
        support.hlg,
        support.sdr10,
        if support.driver.is_empty() { "?" } else { &support.driver },
        match kde_state {
            Some(true) => ", KDE display HDR: on",
            Some(false) => ", KDE display HDR: OFF",
            None if desktop.to_lowercase().contains("kde") => ", KDE display HDR: unknown",
            None => "",
        },
        support.problem.as_ref().map(|p| format!(", problem: {p}")).unwrap_or_default()
    );
    support
}

fn probe_inner() -> Result<HdrSupport, String> {
    let vk_err = |e: vk::Result| format!("Vulkan error: {e}");

    let sdl = sdl2::init()?;
    let video = sdl.video()?;
    let mut support = HdrSupport { driver: video.current_video_driver().to_string(), ..Default::default() };
    let window = video
        .window("HDR check", 64, 64)
        .vulkan()
        .hidden()
        .build()
        .map_err(|e| format!("cannot create a Vulkan window: {e}"))?;

    let entry = unsafe {
        let ptr = window
            .subsystem()
            .vulkan_get_proc_address_function()
            .map_err(|e| format!("no Vulkan loader: {e}"))?;
        let get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr = std::mem::transmute(ptr);
        Entry::from_static_fn(StaticFn { get_instance_proc_addr })
    };

    let sdl_exts = window.vulkan_instance_extensions().map_err(|e| format!("SDL Vulkan extensions: {e}"))?;
    let mut ext_names: Vec<CString> = sdl_exts.iter().map(|s| CString::new(*s).unwrap()).collect();
    let available = unsafe { entry.enumerate_instance_extension_properties(None) }.map_err(vk_err)?;
    let has_colourspace_ext = available.iter().any(|e| {
        e.extension_name_as_c_str().map(|n| n == ext::swapchain_colorspace::NAME).unwrap_or(false)
    });
    if has_colourspace_ext {
        ext_names.push(ext::swapchain_colorspace::NAME.to_owned());
    } else {
        // HDR is impossible without it, but plain SDR surfaces (and their 10-bit variants) do
        // not need it, so the rest of the check still runs.
        support.problem = Some("the Vulkan driver lacks VK_EXT_swapchain_colorspace".to_string());
    }
    let ext_ptrs: Vec<*const c_char> = ext_names.iter().map(|s| s.as_ptr()).collect();

    let app_info = vk::ApplicationInfo::default()
        .application_name(c"calibrationclient")
        .api_version(vk::API_VERSION_1_1);
    let instance = unsafe {
        entry
            .create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app_info).enabled_extension_names(&ext_ptrs),
                None,
            )
            .map_err(vk_err)?
    };

    let surface_loader = khr::surface::Instance::new(&entry, &instance);
    let surface = match window.vulkan_create_surface(instance.handle().as_raw() as usize) {
        Ok(raw) => vk::SurfaceKHR::from_raw(raw),
        Err(e) => {
            unsafe { instance.destroy_instance(None) };
            return Err(format!("cannot create a Vulkan surface: {e}"));
        }
    };

    let queried = (|| -> Result<(bool, bool, bool), String> {
        let (mut hdr10, mut hlg, mut sdr10) = (false, false, false);
        for pd in unsafe { instance.enumerate_physical_devices() }.map_err(vk_err)? {
            let dev_exts = unsafe { instance.enumerate_device_extension_properties(pd) }.map_err(vk_err)?;
            let has_swapchain = dev_exts
                .iter()
                .any(|e| e.extension_name_as_c_str().map(|n| n == khr::swapchain::NAME).unwrap_or(false));
            let can_present = unsafe { instance.get_physical_device_queue_family_properties(pd) }
                .iter()
                .enumerate()
                .any(|(i, q)| {
                    q.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                        && unsafe {
                            surface_loader.get_physical_device_surface_support(pd, i as u32, surface).unwrap_or(false)
                        }
                });
            if !has_swapchain || !can_present {
                continue;
            }
            let formats =
                unsafe { surface_loader.get_physical_device_surface_formats(pd, surface) }.map_err(vk_err)?;
            let (a, b, c) = classify_formats(&formats);
            hdr10 |= a;
            hlg |= b;
            sdr10 |= c;
        }
        Ok((hdr10, hlg, sdr10))
    })();

    // Release everything Vulkan before the window and SDL go away.
    unsafe {
        surface_loader.destroy_surface(surface, None);
        instance.destroy_instance(None);
    }

    let (hdr10, hlg, sdr10) = queried?;
    support.hdr10 = hdr10 && has_colourspace_ext;
    support.hlg = hlg && has_colourspace_ext;
    support.sdr10 = sdr10;
    Ok(support)
}

// -------------------------------------------------------------------------------------
// Vulkan presenter
// -------------------------------------------------------------------------------------

/// Releases the Vulkan objects created so far when `HdrPresenter::new` gives up half-way, so a
/// failed attempt (for example the automatic 10-bit SDR one) leaves nothing behind before the
/// program falls back to the SDL renderer.
struct EarlyCleanup {
    instance: ash::Instance,
    surface_loader: khr::surface::Instance,
    surface: vk::SurfaceKHR,
    device: Option<ash::Device>,
    render_pass: vk::RenderPass,
    command_pool: vk::CommandPool,
    image_available: vk::Semaphore,
    in_flight: vk::Fence,
    armed: bool,
}

impl EarlyCleanup {
    fn new(instance: &ash::Instance, surface_loader: &khr::surface::Instance, surface: vk::SurfaceKHR) -> Self {
        Self {
            instance: instance.clone(),
            surface_loader: surface_loader.clone(),
            surface,
            device: None,
            render_pass: vk::RenderPass::null(),
            command_pool: vk::CommandPool::null(),
            image_available: vk::Semaphore::null(),
            in_flight: vk::Fence::null(),
            armed: true,
        }
    }
}

impl Drop for EarlyCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        unsafe {
            if let Some(device) = &self.device {
                let _ = device.device_wait_idle();
                if self.in_flight != vk::Fence::null() {
                    device.destroy_fence(self.in_flight, None);
                }
                if self.image_available != vk::Semaphore::null() {
                    device.destroy_semaphore(self.image_available, None);
                }
                if self.command_pool != vk::CommandPool::null() {
                    device.destroy_command_pool(self.command_pool, None);
                }
                if self.render_pass != vk::RenderPass::null() {
                    device.destroy_render_pass(self.render_pass, None);
                }
                device.destroy_device(None);
            }
            self.surface_loader.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

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
    /// Drawable size the current swapchain was requested for (see `rebuild_needed`).
    requested: (u32, u32),
    last_rebuild: Instant,
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
            // Plain SDR: the ordinary sRGB colour space, but with a 10-bit surface so ColourSpace's
            // 10-bit codes are shown as they are instead of being rounded to 8 bits.
            HdrMode::Sdr => vk::ColorSpaceKHR::SRGB_NONLINEAR,
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
        if mode != HdrMode::Sdr {
            if !has_instance_ext(ext::swapchain_colorspace::NAME) {
                return other(
                    "The Vulkan driver does not expose VK_EXT_swapchain_colorspace, \
                     so HDR swapchains are not available.",
                );
            }
            ext_names.push(ext::swapchain_colorspace::NAME.to_owned());
        }
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
        let raw_surface = match window.vulkan_create_surface(instance.handle().as_raw() as usize) {
            Ok(raw) => raw,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return other(format!("SDL could not create a Vulkan surface: {e}"));
            }
        };
        let surface = vk::SurfaceKHR::from_raw(raw_surface);
        let surface_loader = khr::surface::Instance::new(&entry, &instance);
        // From here on, any early return releases what has been created so far.
        let mut cleanup = EarlyCleanup::new(&instance, &surface_loader, surface);

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
            let mut msg = if mode == HdrMode::Sdr {
                String::from("No GPU offers a 10-bit SDR surface for this window.")
            } else {
                format!(
                    "No GPU offers a 10-bit {} surface for this window.\n\n\
                     HDR needs: a compositor with HDR enabled (e.g. KDE Plasma 6 or another \
                     colour-management-v1 compositor), Mesa 25.1+ or a recent NVIDIA driver, and \
                     the SDL Wayland video driver (SDL_VIDEODRIVER=wayland). It does not work \
                     through XWayland.",
                    if mode == HdrMode::Hlg { "HDR10 HLG" } else { "HDR10 PQ" }
                )
            };
            if !seen.is_empty() {
                msg.push_str("\n\nSurface formats offered:\n");
                msg.push_str(&seen.join("\n"));
            }
            return other(msg); // `cleanup` releases the instance and surface
        };
        // Static HDR metadata only makes sense for HDR signals.
        let has_hdr_md = has_hdr_md && mode != HdrMode::Sdr;
        if mode == HdrMode::Sdr {
            eprintln!(
                "SDR: 10-bit output on \"{}\" using {:?} / {:?}",
                device_name, surface_format.format, surface_format.color_space
            );
        } else {
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
        cleanup.device = Some(device.clone());
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

        cleanup.render_pass = render_pass;

        // ---- commands and sync --------------------------------------------------------
        let command_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?
        };
        cleanup.command_pool = command_pool;
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
        cleanup.image_available = image_available;
        let in_flight = unsafe {
            device.create_fence(
                &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                None,
            )?
        };

        cleanup.in_flight = in_flight;

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
            requested: (0, 0),
            last_rebuild: Instant::now(),
            image_views: Vec::new(),
            framebuffers: Vec::new(),
            render_finished: Vec::new(),
        };
        // From here on `presenter` owns everything: if the next step fails, its `Drop` cleans up.
        cleanup.armed = false;
        presenter.recreate_swapchain()?;
        Ok(presenter)
    }

    pub fn window_mut(&mut self) -> &mut Window {
        &mut self.window
    }

    /// Drawable size in physical pixels, as SDL reports it.
    fn drawable_size(&self) -> (u32, u32) {
        self.window.vulkan_drawable_size()
    }

    /// Make sure the swapchain matches the window and return the real framebuffer size.
    /// Call once per frame before laying out patches. (While minimised there is no
    /// framebuffer, and the drawable size is returned instead.)
    pub fn sync_size(&mut self) -> Result<(u32, u32), HdrError> {
        self.rebuild_if_needed()?;
        if self.swapchain == vk::SwapchainKHR::null() {
            Ok(self.drawable_size())
        } else {
            Ok((self.extent.width, self.extent.height))
        }
    }

    fn rebuild_if_needed(&mut self) -> Result<(), HdrError> {
        let needed = rebuild_needed(
            self.drawable_size(),
            self.requested,
            self.swapchain != vk::SwapchainKHR::null(),
            self.last_rebuild.elapsed(),
        );
        if !needed {
            return Ok(());
        }
        match self.recreate_swapchain() {
            Err(HdrError::Vk(vk::Result::ERROR_SURFACE_LOST_KHR)) => self.recreate_surface(),
            other => other,
        }
    }

    /// The compositor can invalidate the whole `VkSurfaceKHR` (not just the swapchain).
    /// Build a new one from the SDL window and carry on, provided it still offers the HDR
    /// format we are running with.
    fn recreate_surface(&mut self) -> Result<(), HdrError> {
        eprintln!("HDR: Vulkan surface lost, recreating it");
        unsafe { self.device.device_wait_idle()? };
        self.destroy_swapchain_resources();
        unsafe {
            if self.swapchain != vk::SwapchainKHR::null() {
                self.swapchain_loader.destroy_swapchain(self.swapchain, None);
                self.swapchain = vk::SwapchainKHR::null();
            }
            self.surface_loader.destroy_surface(self.surface, None);
        }
        // Null in the meantime so `Drop` never destroys the old handle a second time.
        self.surface = vk::SurfaceKHR::null();

        let raw_surface = self
            .window
            .vulkan_create_surface(self.instance.handle().as_raw() as usize)
            .map_err(|e| HdrError::Other(format!("SDL could not recreate the Vulkan surface: {e}")))?;
        self.surface = vk::SurfaceKHR::from_raw(raw_surface);

        let formats = unsafe {
            self.surface_loader
                .get_physical_device_surface_formats(self.physical_device, self.surface)?
        };
        let still_offered = formats.iter().any(|f| {
            f.format == self.surface_format.format && f.color_space == self.surface_format.color_space
        });
        if !still_offered {
            return other(
                "The new window surface no longer offers the HDR format. \
                 Was HDR switched off in the compositor? Restart the client.",
            );
        }
        self.recreate_swapchain()
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
        let (dw, dh) = self.drawable_size();
        self.requested = (dw, dh);
        self.last_rebuild = Instant::now();
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
        match self.draw_frame(background, rects) {
            // A lost surface is recoverable; everything else (device lost, out of memory...)
            // is reported to the caller.
            Err(HdrError::Vk(vk::Result::ERROR_SURFACE_LOST_KHR)) => self.recreate_surface(),
            other => other,
        }
    }

    fn draw_frame(&mut self, background: [f32; 3], rects: &[FillRect]) -> Result<(), HdrError> {
        // Resize / recreate if the window size changed or we have no swapchain yet.
        self.rebuild_if_needed()?;
        if self.swapchain == vk::SwapchainKHR::null() {
            return Ok(()); // minimised
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
                // "Suboptimal" can persist for as long as the window exists on some
                // compositors; rebuilding on every frame would be far worse than a slightly
                // suboptimal swapchain, so it is rebuilt at most once per second.
                Ok(true) => {
                    if self.last_rebuild.elapsed() >= Duration::from_secs(1) {
                        self.recreate_swapchain()?;
                    }
                }
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.recreate_swapchain()?,
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
    fn hdr_formats_are_recognised_per_colour_space() {
        let f = |format, color_space| vk::SurfaceFormatKHR { format, color_space };
        let sdr = f(vk::Format::B8G8R8A8_UNORM, vk::ColorSpaceKHR::SRGB_NONLINEAR);
        let pq = f(vk::Format::A2B10G10R10_UNORM_PACK32, vk::ColorSpaceKHR::HDR10_ST2084_EXT);
        let hlg = f(vk::Format::A2R10G10B10_UNORM_PACK32, vk::ColorSpaceKHR::HDR10_HLG_EXT);
        let wrong_depth = f(vk::Format::B8G8R8A8_UNORM, vk::ColorSpaceKHR::HDR10_ST2084_EXT);

        let sdr_10bit = f(vk::Format::A2R10G10B10_UNORM_PACK32, vk::ColorSpaceKHR::SRGB_NONLINEAR);
        let sdr_10bit_abgr = f(vk::Format::A2B10G10R10_UNORM_PACK32, vk::ColorSpaceKHR::SRGB_NONLINEAR);

        assert_eq!(classify_formats(&[]), (false, false, false));
        assert_eq!(classify_formats(&[sdr, wrong_depth]), (false, false, false), "8-bit SDR is not 10-bit SDR");
        assert_eq!(classify_formats(&[sdr, pq]), (true, false, false));
        assert_eq!(classify_formats(&[hlg]), (false, true, false));
        assert_eq!(classify_formats(&[pq, hlg]), (true, true, false));
        assert_eq!(classify_formats(&[sdr, sdr_10bit]), (false, false, true));
        assert_eq!(classify_formats(&[sdr_10bit_abgr]), (false, false, true));
        assert_eq!(classify_formats(&[pq, sdr_10bit]), (true, false, true));
        assert!(HdrSupport { hlg: true, ..Default::default() }.any());
        assert!(!HdrSupport::default().any());
    }

    #[test]
    fn kde_hdr_state_is_read_from_kscreen_doctor_output() {
        let off = "Output: 1 DP-1 enabled connected\n\tGeometry: 0,0 3840x2160\n\tHDR: disabled\n\tWide Color Gamut: disabled\n";
        let on = "Output: 1 DP-1 enabled connected\n\tHDR: enabled\n\tSDR brightness: 300\n";
        assert_eq!(parse_kde_hdr_state(off), Some(false));
        assert_eq!(parse_kde_hdr_state(on), Some(true));
        // one HDR display among several is enough
        assert_eq!(parse_kde_hdr_state(&format!("{off}{on}")), Some(true));
        // coloured output, and everything on one line
        assert_eq!(parse_kde_hdr_state("Output: 1 HDR: \u{1b}[01;31mdisabled\u{1b}[0;0m Vrr: Automatic"), Some(false));
        // JSON form
        assert_eq!(parse_kde_hdr_state("{\"outputs\":[{\"hdr\": false,\"name\":\"DP-1\"}]}"), Some(false));
        assert_eq!(parse_kde_hdr_state("{\"outputs\":[{\"hdr\":true}]}"), Some(true));
        // nothing usable -> unknown, never "off"
        assert_eq!(parse_kde_hdr_state("Output: 1 HDR: incapable"), None);
        assert_eq!(parse_kde_hdr_state("kscreen-doctor: command not found"), None);
        assert_eq!(parse_kde_hdr_state(""), None);
    }

    #[test]
    fn only_kde_sessions_are_asked() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let run = |_: &[&str]| {
            calls.set(calls.get() + 1);
            Some("HDR: disabled".to_string())
        };
        assert_eq!(kde_hdr_state_with("GNOME", &run), None);
        assert_eq!(calls.get(), 0, "other desktops are not queried");
        assert_eq!(kde_hdr_state_with("KDE", &run), Some(false));
        assert_eq!(kde_hdr_state_with("KDE:Plasma", &run), Some(false));

        // -o says nothing usable, -j does
        let json_only = |args: &[&str]| Some(if args == ["-j"] { "\"hdr\": true" } else { "nothing" }.to_string());
        assert_eq!(kde_hdr_state_with("KDE", json_only), Some(true));
        // tool missing
        assert_eq!(kde_hdr_state_with("KDE", |_: &[&str]| None), None);
    }

    /// Needs a stand-in `kscreen-doctor` first on PATH (run with `--ignored`); it must print
    /// its report to *stderr*, like the real tool does through Qt logging.
    #[test]
    #[ignore]
    fn kscreen_doctor_report_is_captured_from_stderr() {
        let text = run_kscreen_doctor(&["-o"]).expect("kscreen-doctor should run");
        assert_eq!(parse_kde_hdr_state(&text), Some(false), "captured: {text:?}");
    }

    #[test]
    fn kde_reporting_hdr_off_disables_hdr_even_if_vulkan_offers_it() {
        let offered = HdrSupport { hdr10: true, hlg: true, ..Default::default() };
        assert!(offered.any());
        assert!(!HdrSupport { kde_hdr_off: true, ..offered }.any());
    }

    #[test]
    fn swapchain_rebuild_decisions() {
        let ms = Duration::from_millis;
        // steady state: nothing to do, whatever the surface's own extent says
        assert!(!rebuild_needed((1280, 720), (1280, 720), true, ms(5000)));
        // resized
        assert!(rebuild_needed((1920, 1080), (1280, 720), true, ms(0)));
        // minimised: retry, but not on every frame
        assert!(!rebuild_needed((1280, 720), (1280, 720), false, ms(10)));
        assert!(rebuild_needed((1280, 720), (1280, 720), false, ms(100)));
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
