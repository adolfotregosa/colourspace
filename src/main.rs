use argh::FromArgs;
use tinyfiledialogs as tfd;
use std::time::{Duration, Instant};
use std::thread::sleep;
use std::error::Error;

mod config;
mod hdr;
mod lan;
mod startup;
use hdr::{color_to_unit_rgb, FillRect, HdrMetadata, HdrMode, HdrPresenter, Primaries};
use lan::{ColorRGB, ShapeInstruction, spawn_worker};
use sdl2::render::Canvas;
use sdl2::video::Window;
use sdl2::pixels::Color;
use sdl2::rect::Rect;

#[derive(FromArgs)]
/// Colourspace viewer
struct Args {
    /// remote server host[:port] (positional). Optional.
    #[argh(positional)]
    remote: Option<String>,

    /// output signal: sdr (default), hdr10 (BT.2020 PQ) or hlg
    #[argh(option)]
    hdr: Option<HdrMode>,

    /// mastering display primaries in the HDR metadata: bt2020 (default) or p3d65
    #[argh(option)]
    primaries: Option<Primaries>,

    /// mastering display peak luminance in cd/m2 for the HDR metadata (default 1000)
    #[argh(option)]
    max_luminance: Option<f32>,

    /// mastering display black level in cd/m2 for the HDR metadata (default 0.0001)
    #[argh(option)]
    min_luminance: Option<f32>,

    /// maximum content light level (MaxCLL) in cd/m2 for the HDR metadata (default 0 = unspecified)
    #[argh(option)]
    max_cll: Option<f32>,

    /// maximum frame average light level (MaxFALL) in cd/m2 for the HDR metadata (default 0 = unspecified)
    #[argh(option)]
    max_fall: Option<f32>,
}

impl Args {
    /// Put the options that were actually given on top of `settings`.
    fn apply_to(&self, settings: &mut startup::Settings) {
        if let Some(remote) = &self.remote {
            settings.remote = remote.clone();
        }
        if let Some(mode) = self.hdr {
            settings.hdr = mode;
            if mode != HdrMode::Sdr {
                settings.signal = mode;
            }
        }
        if let Some(v) = self.primaries {
            settings.primaries = v;
        }
        if let Some(v) = self.max_luminance {
            settings.max_luminance = v;
        }
        if let Some(v) = self.min_luminance {
            settings.min_luminance = v;
        }
        if let Some(v) = self.max_cll {
            settings.max_cll = v;
        }
        if let Some(v) = self.max_fall {
            settings.max_fall = v;
        }
    }
}

/// When HDR is asked for on the command line (so the startup window and its checks are skipped),
/// is it really usable here? `Some(reason)` if not. Vulkan alone is not enough to tell: KDE keeps
/// offering HDR surfaces when HDR is switched off, or when no display can do HDR, and the output
/// would then be a tone-mapped SDR picture while claiming to be HDR.
fn cli_hdr_problem(mode: HdrMode, support: &hdr::HdrSupport) -> Option<String> {
    if mode == HdrMode::Sdr {
        return None;
    }
    if !support.any() {
        return Some(startup::hdr_unavailable_reason(support));
    }
    let offered = match mode {
        HdrMode::Hlg => support.hlg,
        _ => support.hdr10,
    };
    (!offered).then(|| format!("{} is not offered for this display. Try the other HDR signal.", mode.label()))
}

/// Starting values, lowest priority first: the built-in defaults, then what was remembered,
/// then the options given on the command line. Remembered values only apply when the startup
/// window is going to be shown (no address on the command line): a run that names its address
/// does exactly what its command line says, and in particular never turns HDR on by itself.
fn initial_settings(args: &Args, saved: &config::Saved) -> startup::Settings {
    let mut settings = startup::Settings::built_in();
    if args.remote.is_none() {
        settings.apply_saved(saved);
    }
    args.apply_to(&mut settings);
    settings
}

/// Where patches end up: the original 8-bit SDL renderer, or the Vulkan presenter with a 10-bit
/// surface (used for HDR, and for SDR whenever the system offers a 10-bit SDR surface).
enum Output {
    Sdr(Canvas<Window>),
    Vulkan(HdrPresenter),
}

impl Output {
    fn window_mut(&mut self) -> &mut Window {
        match self {
            Output::Sdr(canvas) => canvas.window_mut(),
            Output::Vulkan(hdr) => hdr.window_mut(),
        }
    }

    fn is_vulkan(&self) -> bool {
        matches!(self, Output::Vulkan(_))
    }

    /// Bits per channel this output really delivers: 8 for SDL, 8 or 10 for the Vulkan surface.
    fn output_bits(&self) -> u8 {
        match self {
            Output::Sdr(_) => 8,
            Output::Vulkan(hdr) => hdr.surface_bits(),
        }
    }

    /// ColourSpace is sending patches of `bits` bits: let SDR switch to the surface format that
    /// matches, and describe what is now in use (for the terminal).
    fn apply_patch_bits(&mut self, bits: u8) -> Result<String, Box<dyn Error>> {
        Ok(match self {
            Output::Sdr(_) if bits > 8 => format!(
                "patch bit depth {bits}: SDL renderer, 8-bit output (10-bit is not available here, \
                 the values are rounded to 8 bits)"
            ),
            Output::Sdr(_) => format!("patch bit depth {bits}: SDL renderer, 8-bit output"),
            Output::Vulkan(hdr) => {
                hdr.set_patch_bits(bits)?;
                let out_bits = hdr.surface_bits();
                let mut msg =
                    format!("patch bit depth {bits}: {out_bits}-bit surface, format {}", hdr.format_description());
                if bits > 8 && out_bits < 10 {
                    msg.push_str(" (10-bit is not available here, the values are rounded to 8 bits)");
                } else if bits > out_bits {
                    msg.push_str(&format!(" (the values are reduced to {out_bits} bits)"));
                }
                msg
            }
        })
    }

    /// Current drawable size in pixels. For HDR this also rebuilds the swapchain after a
    /// resize, so the returned size is always the real framebuffer size.
    fn size(&mut self) -> Result<(u32, u32), Box<dyn Error>> {
        match self {
            Output::Sdr(canvas) => Ok(canvas.output_size()?),
            Output::Vulkan(hdr) => Ok(hdr.sync_size()?),
        }
    }
}

/// Append the default ColourSpace port (20002) when the address has none.
/// Handles host names, IPv4, bracketed IPv6 (`[::1]`, `[::1]:20002`) and bare IPv6 (`::1`).
fn add_default_port(s: &str) -> String {
    const DEFAULT_PORT: u16 = 20002;
    let s = s.trim();

    if let Some(rest) = s.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((_, "")) => format!("{s}:{DEFAULT_PORT}"),
            // "[addr]:port" (or anything else malformed) is passed on; the resolver reports it.
            _ => s.to_string(),
        };
    }

    match s.matches(':').count() {
        0 => format!("{s}:{DEFAULT_PORT}"),
        1 => {
            let (host, port) = s.split_once(':').unwrap();
            if port.parse::<u16>().is_ok() {
                s.to_string()
            } else if port.is_empty() {
                format!("{host}:{DEFAULT_PORT}") // "host:" with the port left off
            } else {
                format!("{s}:{DEFAULT_PORT}")
            }
        }
        _ => format!("[{s}]:{DEFAULT_PORT}"), // bare IPv6 address
    }
}

/// What the title bar says about the SDR output in use:
///
/// * `SDR 8-bit` / `SDR 10-bit`: the surface matching the patches (Vulkan path)
/// * `... SDL`: only the SDL renderer is available (always 8-bit)
/// * `SDR 10-bit not available, 8-bit`: ColourSpace sends 10-bit patches but the output can only
///   show 8 bits, so they are rounded
/// * `SDR 10-bit, input 12-bit`: patches deeper than 10 bits are reduced to 10
fn sdr_tag(vulkan: bool, output_bits: u8, patch_bits: Option<u8>) -> String {
    let mut tag = match patch_bits {
        Some(bits) if bits > 8 && output_bits < 10 => format!("SDR 10-bit not available, {output_bits}-bit"),
        Some(bits) if bits > output_bits => format!("SDR {output_bits}-bit, input {bits}-bit"),
        _ => format!("SDR {output_bits}-bit"),
    };
    if !vulkan {
        tag.push_str(" SDL");
    }
    tag
}

/// Window title: shows the output in use (`tag`: "HDR10", "HLG", "SDR 10-bit"; none for the
/// plain SDL renderer) and whether the link to ColourSpace is up.
fn window_title(tag: Option<&str>, disconnected: bool) -> String {
    let mut title = String::from("Calibration Client Linux");
    if let Some(tag) = tag {
        title.push_str(&format!(" [{tag}]"));
    }
    if disconnected {
        title.push_str(" - connection lost, reconnecting...");
    }
    title
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Args = argh::from_env();

    // Everything the startup window edits: pre-filled with what was used last time, unless
    // the command line says otherwise.
    let saved = config::load();
    let mut settings = initial_settings(&args, &saved);

    // Connect to ColourSpace *before* SDL and the main window exist. That way a failed
    // connection can simply bring the startup window back (with the same values) and the HDR
    // choice can still decide the SDL video driver, which must be set before SDL starts.
    let mut use_cli_address = args.remote.is_some();
    // What HDR can do here. Checked every time the startup window opens (HDR may have been
    // switched on or off in the desktop in the meantime), and for HDR asked for on the command line.
    let mut hdr_support: Option<hdr::HdrSupport> = None;
    // Whether the startup window was used: only then are the HDR choices worth remembering.
    let mut window_shown = false;

    // HDR on the command line skips the startup window, but not its checks.
    if use_cli_address && settings.hdr != HdrMode::Sdr {
        let support = hdr::probe();
        if let Some(reason) = cli_hdr_problem(settings.hdr, &support) {
            let text = format!("{} output is not available\n\n{}", settings.hdr.label(), reason);
            eprintln!("{}", text.replace("\n\n", ": "));
            let _ = tfd::message_box_ok("Calibration Client Linux", &text, tfd::MessageBoxIcon::Error);
            return Err(text.into());
        }
        hdr_support = Some(support);
    }

    let worker = loop {
        if !use_cli_address {
            let support = hdr::probe();
            window_shown = true;
            let chosen = startup::show(settings.clone(), &support)?;
            hdr_support = Some(support);
            match chosen {
                Some(chosen) => settings = chosen,
                None => return Ok(()),
            }
        }
        use_cli_address = false;

        let remote_addr = add_default_port(&settings.remote);
        match spawn_worker(&remote_addr, false) {
            Ok(state) => {
                eprintln!("Connected to {}; waiting for ColourSpace to send patches", remote_addr);
                // Only what worked is remembered. The HDR choices are only updated when the
                // startup window was shown and HDR could really be chosen there; a temporary
                // "HDR unavailable" must not wipe the preference.
                let hdr_usable = window_shown && hdr_support.as_ref().is_some_and(|s| s.any());
                config::save(&settings.to_saved(hdr_usable, &saved));
                break state;
            }
            Err(err) => {
                eprintln!("Failed to connect to {}: {}", remote_addr, err);
                let text = format!(
                    "ColourSpace not reachable at {}\n\n{}\n\nCheck the IP address and that ColourSpace is running.",
                    remote_addr, err
                );
                let _ = tfd::message_box_ok("Calibration Client Linux", &text, tfd::MessageBoxIcon::Error);
                // loop round: the startup window comes back so nothing has to be retyped
            }
        }
    };
    let hdr_mode = settings.hdr;

    // SDR is normally drawn by SDL's 8-bit renderer, which rounds ColourSpace's 10-bit codes to
    // 8 bits. When the system offers a 10-bit SDR surface, the Vulkan path is used instead so the
    // codes reach the compositor as they are. Automatic: if it is not offered (or Vulkan is not
    // available at all) nothing changes and the SDL renderer is used exactly as before.
    // (The startup window already checked this if it was shown.)
    let use_sdr10 = hdr_mode == HdrMode::Sdr
        && hdr_support.as_ref().map(|s| s.sdr10).unwrap_or_else(|| hdr::probe_quick().sdr10);

    // HDR, and 10-bit SDR, work through the native Wayland driver (XWayland offers neither).
    if hdr_mode != HdrMode::Sdr || use_sdr10 {
        hdr::prefer_wayland_driver();
    }

    let sdl_context = sdl2::init()?;
    let video = sdl_context.video()?;

    const DEFAULT_W: u32 = 1280;
    const DEFAULT_H: u32 = 720;

    // What the title bar says about the output ("HDR10", "HLG", "SDR 10-bit"); none for plain SDL.
    let mut title_tag: Option<String> = match hdr_mode {
        HdrMode::Hdr10 => Some("HDR10".to_string()),
        HdrMode::Hlg => Some("HLG".to_string()),
        // the Vulkan path starts on its 10-bit surface until the first patch says otherwise
        HdrMode::Sdr => Some(sdr_tag(use_sdr10, if use_sdr10 { 10 } else { 8 }, None)),
    };

    // Always start windowed; fullscreen only via double-click.
    // `vulkan`: the window can be used for a Vulkan swapchain (needed for HDR and 10-bit SDR).
    let make_window = |tag: Option<&str>, vulkan: bool| {
        let mut builder = video.window(&window_title(tag, false), DEFAULT_W, DEFAULT_H);
        builder.position_centered().resizable().allow_highdpi();
        if vulkan {
            builder.vulkan();
        }
        builder.build()
    };
    // The window for the plain SDL renderer. It is created the way it always was (with the
    // Vulkan flag), and only if that is refused, because this system has no usable Vulkan at
    // all (no loader or no driver), without it. Without this the program would not start on such
    // a system even though the SDL renderer needs no Vulkan.
    let make_sdl_window = |tag: Option<&str>| match make_window(tag, true) {
        Ok(window) => Ok(window),
        Err(err) => {
            eprintln!("Vulkan is not available here ({err}); using the SDL renderer without it");
            make_window(tag, false)
        }
    };

    fn select_measure_colour(shapes: &[ShapeInstruction]) -> Option<ColorRGB> {
        shapes
        .iter()
        .filter_map(|shape| match shape {
            ShapeInstruction::Rectangle(rect) => {
                let area = (rect.geometry.width * rect.geometry.height).max(0.0001);
                Some((area, rect.color))
            }
        })
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
        .map(|(_, color)| color)
    }

    /// Helper: convert a `ColorRGB` (u16 + depth_bits) into an 8-bit RGB tuple.
    ///
    /// Only used by the SDL renderer, which is 8-bit. When the system offers a 10-bit surface
    /// (HDR always, SDR automatically) the Vulkan path is used instead and the codes are never
    /// rounded to 8 bits. This helper is intentionally local to `main.rs` so the `lan` module
    /// stays depth-agnostic.
    fn color_to_u8_tuple(color: ColorRGB) -> (u8, u8, u8) {
        let bits = if color.depth_bits == 0 { 8 } else { color.depth_bits };
        let max_in: u32 = if bits >= 16 {
            0xFFFF
        } else {
            (1u32 << bits as u32) - 1
        };

        // avoid division by zero (defensive)
        let max_in = if max_in == 0 { 255 } else { max_in };

        let r = ((color.red as u32 * 255 + max_in / 2) / max_in) as u8;
        let g = ((color.green as u32 * 255 + max_in / 2) / max_in) as u8;
        let b = ((color.blue as u32 * 255 + max_in / 2) / max_in) as u8;
        (r, g, b)
    }

    /// Centre a rectangle (width/height as 0..1 fractions of the output) on the output.
    /// Returns (left, top, width, height) in pixels; always at least 1x1.
    fn centered_rect(geom: lan::RectangleGeometry, w: u32, h: u32) -> (i32, i32, u32, u32) {
        let rw = (geom.width.clamp(0.0, 1.0) * w as f32).round().max(1.0) as u32;
        let rh = (geom.height.clamp(0.0, 1.0) * h as f32).round().max(1.0) as u32;
        let left = ((w as f32 - rw as f32) / 2.0).round() as i32;
        let top = ((h as f32 - rh as f32) / 2.0).round() as i32;
        (left, top, rw, rh)
    }

    /// HDR path: turn shape instructions into flat rectangles with unmodified signal levels.
    fn shapes_to_fill_rects(shapes: &[ShapeInstruction], w: u32, h: u32) -> Vec<FillRect> {
        shapes
            .iter()
            .map(|shape| match shape {
                ShapeInstruction::Rectangle(rect) => {
                    let (x, y, rw, rh) = centered_rect(rect.geometry, w, h);
                    FillRect { x, y, w: rw, h: rh, rgb: color_to_unit_rgb(rect.color) }
                }
            })
            .collect()
    }

    fn draw_shapes(
        canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
        shapes: &[ShapeInstruction],
        w: u32,
        h: u32,
    ) {
        canvas.set_draw_color(Color::RGB(0, 0, 0));
        canvas.clear();

        for shape in shapes {
            match shape {
                ShapeInstruction::Rectangle(rect) => {
                    let (left, top, rw, rh) = centered_rect(rect.geometry, w, h);

                    let color = rect.color;
                    // downscale from u16/depth to u8 here using local helper
                    let (r8, g8, b8) = color_to_u8_tuple(color);
                    canvas.set_draw_color(Color::RGB(r8, g8, b8));
                    let _ = canvas.fill_rect(Rect::new(left, top, rw, rh));
                }
            }
        }
    }

    let mut event_pump = sdl_context.event_pump()?;
    let mut current_measure_colour = ColorRGB::default();

    // Build the output once we have a worker (or the user cancelled earlier).
    let metadata = HdrMetadata {
        primaries: settings.primaries,
        max_luminance: settings.max_luminance,
        min_luminance: settings.min_luminance,
        max_cll: settings.max_cll,
        max_fall: settings.max_fall,
    };
    let mut output = match hdr_mode {
        HdrMode::Sdr => {
            let mut presenter = None;
            if use_sdr10 {
                match HdrPresenter::new(make_window(title_tag.as_deref(), true)?, HdrMode::Sdr, metadata) {
                    Ok(p) => presenter = Some(p),
                    Err(err) => {
                        // Unlike HDR, falling back is exactly right here: the SDL renderer is the
                        // proven path, and SDR is what was asked for either way.
                        eprintln!("10-bit SDR output could not be started ({err}); using the standard 8-bit renderer");
                        title_tag = Some(sdr_tag(false, 8, None));
                    }
                }
            }
            match presenter {
                Some(p) => Output::Vulkan(p),
                None => Output::Sdr(make_sdl_window(title_tag.as_deref())?.into_canvas().build()?),
            }
        }
        mode => match HdrPresenter::new(make_window(title_tag.as_deref(), true)?, mode, metadata) {
            Ok(presenter) => Output::Vulkan(presenter),
            Err(err) => {
                // Never fall back to SDR silently: measuring the wrong signal is worse than failing.
                eprintln!("{} output unavailable: {}", mode.label(), err);
                let text = format!("{} output is not available\n\n{}", mode.label(), err);
                let _ = tfd::message_box_ok("Calibration Client Linux", &text, tfd::MessageBoxIcon::Error);
                return Err(err.into());
            }
        },
    };
    // What the title bar currently shows, and the bit depth of the patches the output was last
    // adapted to.
    let mut shown_title = window_title(title_tag.as_deref(), false);
    let mut applied_bits: Option<u8> = None;

    // The mouse pointer is hidden while the window is fullscreen (it would sit on top of the
    // patch and light it) and comes back in windowed mode.
    let mouse = sdl_context.mouse();
    let mut pointer_hidden = false;
    let mut seen_fullscreen = false;

    // double-click detection
    let mut last_click_time = None::<Instant>;
    let mut is_fullscreen = false;
    let dc_threshold = Duration::from_millis(400);

    // FPS bookkeeping (unused but left intentionally)
    let _last_fps = Instant::now();
    let mut _frames = 0u32;

    // Use u32 here because wait_event_timeout expects u32
    const EVENT_WAIT_MS: u32 = 8;

    'running: loop {
        // wait_event_timeout takes a u32; it returns None on timeout
        // handle the first event (if any) and then drain remaining queued events via poll_iter()
        if let Some(event) = event_pump.wait_event_timeout(EVENT_WAIT_MS) {
            match event {
                sdl2::event::Event::Quit { .. }
                | sdl2::event::Event::KeyDown {
                    keycode: Some(sdl2::keyboard::Keycode::Escape),
                    ..
                } => break 'running,

                sdl2::event::Event::MouseButtonDown {
                    mouse_btn: sdl2::mouse::MouseButton::Left,
                    ..
                } => {
                    let now = Instant::now();
                    if let Some(prev) = last_click_time {
                        if now.duration_since(prev) <= dc_threshold {
                            // Toggle fullscreen
                            if is_fullscreen {
                                output.window_mut()
                                .set_fullscreen(sdl2::video::FullscreenType::Off)
                                .ok();
                                is_fullscreen = false;
                            } else {
                                output.window_mut()
                                .set_fullscreen(sdl2::video::FullscreenType::Desktop)
                                .ok();
                                is_fullscreen = true;
                            }
                            last_click_time = None;
                        } else {
                            last_click_time = Some(now);
                        }
                    } else {
                        last_click_time = Some(now);
                    }
                }

                _ => {}
            }

            // Drain any other queued events so we don't process them next frame
            for event in event_pump.poll_iter() {
                match event {
                    sdl2::event::Event::Quit { .. }
                    | sdl2::event::Event::KeyDown {
                        keycode: Some(sdl2::keyboard::Keycode::Escape),
                        ..
                    } => break 'running,

                    sdl2::event::Event::MouseButtonDown {
                        mouse_btn: sdl2::mouse::MouseButton::Left,
                        ..
                    } => {
                        let now = Instant::now();
                        if let Some(prev) = last_click_time {
                            if now.duration_since(prev) <= dc_threshold {
                                if is_fullscreen {
                                    output.window_mut()
                                    .set_fullscreen(sdl2::video::FullscreenType::Off)
                                    .ok();
                                    is_fullscreen = false;
                                } else {
                                    output.window_mut()
                                    .set_fullscreen(sdl2::video::FullscreenType::Desktop)
                                    .ok();
                                    is_fullscreen = true;
                                }
                                last_click_time = None;
                            } else {
                                last_click_time = Some(now);
                            }
                        } else {
                            last_click_time = Some(now);
                        }
                    }

                    _ => {}
                }
            }
        }

        // Follow the window's *actual* fullscreen state (it can also be changed by the desktop,
        // not just by our double-click): hide the pointer in fullscreen, show it otherwise.
        let fullscreen_now = output.window_mut().fullscreen_state() != sdl2::video::FullscreenType::Off;
        if fullscreen_now != pointer_hidden {
            mouse.show_cursor(!fullscreen_now);
            pointer_hidden = fullscreen_now;
        }
        if !fullscreen_now && seen_fullscreen {
            is_fullscreen = false; // the desktop took us out of fullscreen; keep the toggle in step
        }
        seen_fullscreen = fullscreen_now;

        // One read of the worker state per frame
        let (disconnected, shapes, worker_current_colour, patch_bits) = {
            let r = lan::read_state(&worker);
            (!r.connected, r.shapes.clone(), r.current_measure_colour, r.patch_bits)
        };

        // ColourSpace changed the bit depth of its patches: SDR switches to the surface format
        // that matches (8-bit patches -> 8-bit surface, 10-bit -> 10-bit) so nothing downstream
        // has to round. The terminal says which format is now in use.
        if let Some(bits) = patch_bits {
            if applied_bits != Some(bits) {
                applied_bits = Some(bits);
                eprintln!("Output: {}", output.apply_patch_bits(bits)?);
            }
        }

        // The title shows the output in use (SDR: 8/10-bit, SDL or not) and a lost connection
        // (the worker reconnects itself).
        let tag = if hdr_mode == HdrMode::Sdr {
            Some(sdr_tag(output.is_vulkan(), output.output_bits(), applied_bits))
        } else {
            title_tag.clone()
        };
        let desired_title = window_title(tag.as_deref(), disconnected);
        if desired_title != shown_title {
            output.window_mut().set_title(&desired_title).ok();
            shown_title = desired_title;
        }

        // Update current measure colour depending on worker state and shapes
        if disconnected || shapes.is_empty() {
            current_measure_colour = worker_current_colour;
        } else {
            current_measure_colour = select_measure_colour(&shapes).unwrap_or(current_measure_colour);
        }

        // Draw. While the connection is down the screen is black: a full field of the last patch
        // colour could sit on screen for a long time (hard on OLEDs, especially bright HDR patches).
        let (cw, ch) = output.size()?;
        let show_shapes = !disconnected && !shapes.is_empty();
        match &mut output {
            Output::Sdr(canvas) => {
                if disconnected {
                    canvas.set_draw_color(Color::RGB(0, 0, 0));
                    canvas.clear();
                } else if show_shapes {
                    draw_shapes(canvas, &shapes, cw, ch);
                } else {
                    let c = current_measure_colour;
                    // downscale before giving to SDL using the helper
                    let (r8, g8, b8) = color_to_u8_tuple(c);
                    canvas.set_draw_color(Color::RGB(r8, g8, b8));
                    canvas.clear();
                }

                // Present once per frame (consistent timing fixes the double-click quirk)
                canvas.present();
            }
            Output::Vulkan(hdr) => {
                // Code values go to the swapchain untouched (no 8-bit downscale).
                if disconnected {
                    hdr.draw([0.0, 0.0, 0.0], &[])?;
                } else if show_shapes {
                    let rects = shapes_to_fill_rects(&shapes, cw, ch);
                    hdr.draw([0.0, 0.0, 0.0], &rects)?;
                } else {
                    hdr.draw(color_to_unit_rgb(current_measure_colour), &[])?;
                }
            }
        }

        // small sleep to avoid burning CPU in pathological cases
        sleep(Duration::from_millis(1));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_args() -> Args {
        Args {
            remote: None,
            hdr: None,
            primaries: None,
            max_luminance: None,
            min_luminance: None,
            max_cll: None,
            max_fall: None,
        }
    }

    fn remembered() -> config::Saved {
        config::Saved {
            ip: Some("192.168.1.5".into()),
            hdr_enabled: Some(true),
            signal: Some("hlg".into()),
            primaries: Some("p3d65".into()),
            max_luminance: Some(1200.0),
            min_luminance: Some(0.0005),
            max_cll: Some(1000.0),
            max_fall: Some(400.0),
        }
    }

    #[test]
    fn the_startup_window_starts_from_what_was_remembered() {
        let s = initial_settings(&no_args(), &remembered());
        assert_eq!(s.remote, "192.168.1.5");
        assert_eq!((s.hdr, s.signal, s.primaries), (HdrMode::Hlg, HdrMode::Hlg, Primaries::P3D65));
        assert_eq!((s.max_luminance, s.min_luminance, s.max_cll, s.max_fall), (1200.0, 0.0005, 1000.0, 400.0));

        // nothing remembered: the built-in defaults
        let s = initial_settings(&no_args(), &config::Saved::default());
        assert_eq!((s.remote.as_str(), s.hdr, s.signal, s.max_luminance), ("", HdrMode::Sdr, HdrMode::Hdr10, 1000.0));
    }

    #[test]
    fn explicit_options_beat_remembered_values() {
        let args = Args { hdr: Some(HdrMode::Hdr10), max_luminance: Some(500.0), ..no_args() };
        let s = initial_settings(&args, &remembered());
        assert_eq!((s.hdr, s.signal), (HdrMode::Hdr10, HdrMode::Hdr10));
        assert_eq!(s.max_luminance, 500.0);
        assert_eq!(s.primaries, Primaries::P3D65, "options that were not given still come from memory");

        // --hdr sdr unticks the box but keeps the remembered signal for the menu
        let args = Args { hdr: Some(HdrMode::Sdr), ..no_args() };
        let s = initial_settings(&args, &remembered());
        assert_eq!((s.hdr, s.signal), (HdrMode::Sdr, HdrMode::Hlg));
    }

    #[test]
    fn a_run_with_an_address_ignores_remembered_hdr_choices() {
        let args = Args { remote: Some("10.0.0.7".into()), ..no_args() };
        let s = initial_settings(&args, &remembered());
        assert_eq!(s.remote, "10.0.0.7");
        assert_eq!(s.hdr, HdrMode::Sdr, "HDR is never switched on behind the command line's back");
        assert_eq!((s.max_luminance, s.primaries), (1000.0, Primaries::Bt2020));

        let args = Args { remote: Some("10.0.0.7".into()), hdr: Some(HdrMode::Hlg), ..no_args() };
        assert_eq!(initial_settings(&args, &remembered()).hdr, HdrMode::Hlg);
    }

    fn support(hdr10: bool, hlg: bool) -> hdr::HdrSupport {
        hdr::HdrSupport { hdr10, hlg, driver: "wayland".into(), ..Default::default() }
    }

    #[test]
    fn hdr_from_the_command_line_is_refused_where_the_window_would_refuse_it() {
        assert_eq!(cli_hdr_problem(HdrMode::Sdr, &support(false, false)), None, "SDR needs nothing");
        assert_eq!(cli_hdr_problem(HdrMode::Hdr10, &support(true, true)), None);
        assert_eq!(cli_hdr_problem(HdrMode::Hlg, &support(true, true)), None);

        // KDE says HDR is off, or there is no HDR display, although Vulkan offers the surfaces
        let kde_off = hdr::HdrSupport { kde_hdr_off: true, ..support(true, true) };
        let reason = cli_hdr_problem(HdrMode::Hdr10, &kde_off).expect("refused");
        assert!(reason.contains("turned off"), "{reason}");
        let no_display = hdr::HdrSupport { kde_no_hdr_display: true, ..support(true, true) };
        assert!(cli_hdr_problem(HdrMode::Hdr10, &no_display).unwrap().contains("no HDR-capable display"));

        // nothing offered at all, and X11
        assert!(cli_hdr_problem(HdrMode::Hdr10, &support(false, false)).is_some());
        let x11 = hdr::HdrSupport { driver: "x11".into(), ..support(false, false) };
        assert!(cli_hdr_problem(HdrMode::Hdr10, &x11).unwrap().contains("Wayland"));

        // the asked-for signal must be the one on offer
        assert!(cli_hdr_problem(HdrMode::Hlg, &support(true, false)).unwrap().contains("HLG"));
        assert!(cli_hdr_problem(HdrMode::Hdr10, &support(false, true)).unwrap().contains("HDR10"));
    }

    #[test]
    fn default_port_is_added_only_when_missing() {
        assert_eq!(add_default_port("192.168.1.5"), "192.168.1.5:20002");
        assert_eq!(add_default_port("192.168.1.5:1234"), "192.168.1.5:1234");
        assert_eq!(add_default_port("colourspace-pc"), "colourspace-pc:20002");
        assert_eq!(add_default_port("colourspace-pc:1234"), "colourspace-pc:1234");
        assert_eq!(add_default_port("host:"), "host:20002");
        assert_eq!(add_default_port("  10.0.0.2  "), "10.0.0.2:20002");
    }

    #[test]
    fn ipv6_addresses_are_not_mistaken_for_host_port() {
        assert_eq!(add_default_port("::1"), "[::1]:20002");
        assert_eq!(add_default_port("fe80::1"), "[fe80::1]:20002");
        assert_eq!(add_default_port("[::1]"), "[::1]:20002");
        assert_eq!(add_default_port("[::1]:5000"), "[::1]:5000");
    }

    #[test]
    fn the_sdr_tag_says_what_is_really_in_use() {
        // Vulkan path: the surface follows the patches
        assert_eq!(sdr_tag(true, 8, Some(8)), "SDR 8-bit");
        assert_eq!(sdr_tag(true, 10, Some(10)), "SDR 10-bit");
        assert_eq!(sdr_tag(true, 10, None), "SDR 10-bit", "before the first patch");
        assert_eq!(sdr_tag(true, 10, Some(12)), "SDR 10-bit, input 12-bit");
        // only SDL (8-bit): 8-bit patches are fine, 10-bit ones cannot be shown as such
        assert_eq!(sdr_tag(false, 8, None), "SDR 8-bit SDL");
        assert_eq!(sdr_tag(false, 8, Some(8)), "SDR 8-bit SDL");
        assert_eq!(sdr_tag(false, 8, Some(10)), "SDR 10-bit not available, 8-bit SDL");
        assert_eq!(sdr_tag(false, 8, Some(12)), "SDR 10-bit not available, 8-bit SDL");
        // a Vulkan surface that has no 10-bit format
        assert_eq!(sdr_tag(true, 8, Some(10)), "SDR 10-bit not available, 8-bit");
    }

    #[test]
    fn title_reflects_mode_and_connection() {
        assert_eq!(window_title(None, false), "Calibration Client Linux");
        assert_eq!(window_title(Some("HDR10"), false), "Calibration Client Linux [HDR10]");
        assert_eq!(window_title(Some("SDR 10-bit"), false), "Calibration Client Linux [SDR 10-bit]");
        assert!(window_title(Some("HLG"), true).contains("reconnecting"));
    }
}
