use argh::FromArgs;
use tinyfiledialogs as tfd;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use std::thread::{sleep, spawn};
use std::error::Error;

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
    #[argh(option, default = "HdrMode::Sdr")]
    hdr: HdrMode,

    /// mastering display primaries in the HDR metadata: bt2020 (default) or p3d65
    #[argh(option, default = "Primaries::Bt2020")]
    primaries: Primaries,

    /// mastering display peak luminance in cd/m2 for the HDR metadata (default 1000)
    #[argh(option, default = "1000.0")]
    max_luminance: f32,

    /// mastering display black level in cd/m2 for the HDR metadata (default 0.0001)
    #[argh(option, default = "0.0001")]
    min_luminance: f32,

    /// maximum content light level (MaxCLL) in cd/m2 for the HDR metadata (default 0 = unspecified)
    #[argh(option, default = "0.0")]
    max_cll: f32,

    /// maximum frame average light level (MaxFALL) in cd/m2 for the HDR metadata (default 0 = unspecified)
    #[argh(option, default = "0.0")]
    max_fall: f32,
}

/// Where patches end up: the original 8-bit SDL renderer, or the Vulkan HDR presenter.
enum Output {
    Sdr(Canvas<Window>),
    Hdr(HdrPresenter),
}

impl Output {
    fn window_mut(&mut self) -> &mut Window {
        match self {
            Output::Sdr(canvas) => canvas.window_mut(),
            Output::Hdr(hdr) => hdr.window_mut(),
        }
    }

    fn size(&self) -> Result<(u32, u32), String> {
        match self {
            Output::Sdr(canvas) => canvas.output_size(),
            Output::Hdr(hdr) => Ok(hdr.output_size()),
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    // Parse arguments first: HDR needs to pick the SDL video driver before SDL starts.
    let mut args: Args = argh::from_env();

    // No IP on the command line: show the startup window (IP + HDR checkbox and menus),
    // pre-filled from any options that were given. This runs before SDL is started for
    // the main window because the HDR choice decides which video driver SDL must use.
    if args.remote.is_none() {
        let defaults = startup::Settings {
            remote: String::new(),
            hdr: args.hdr,
            primaries: args.primaries,
            max_luminance: args.max_luminance,
            min_luminance: args.min_luminance,
            max_cll: args.max_cll,
            max_fall: args.max_fall,
        };
        match startup::show(defaults)? {
            Some(chosen) => {
                args.remote = Some(chosen.remote);
                args.hdr = chosen.hdr;
                args.primaries = chosen.primaries;
                args.max_luminance = chosen.max_luminance;
                args.min_luminance = chosen.min_luminance;
                args.max_cll = chosen.max_cll;
                args.max_fall = chosen.max_fall;
            }
            None => return Ok(()),
        }
    }

    // HDR only works through the native Wayland driver (XWayland has no HDR).
    if args.hdr != HdrMode::Sdr
        && std::env::var_os("SDL_VIDEODRIVER").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_some()
    {
        sdl2::hint::set("SDL_VIDEODRIVER", "wayland");
    }

    let sdl_context = sdl2::init()?;
    let video = sdl_context.video()?;

    const DEFAULT_W: u32 = 1280;
    const DEFAULT_H: u32 = 720;

    // Always start windowed; fullscreen only via double-click
    let window_title = match args.hdr {
        HdrMode::Sdr => "Calibration Client Linux".to_string(),
        mode => format!("Calibration Client Linux [{}]", mode.label()),
    };
    let window = video
    .window(&window_title, DEFAULT_W, DEFAULT_H)
    .position_centered()
    .vulkan()
    .resizable()
    .allow_highdpi()
    .build()?;

    fn pad(msg: &str, width: usize) -> String {
        let mut s = msg.to_string();
        if s.len() < width {
            s.reserve(width - s.len());
            while s.len() < width {
                s.push(' ');
            }
        }
        s
    }

    fn show_startup_ui() -> Option<String> {
        // Make this large enough to avoid title truncation on your desktop.
        // Try 80..120 if your title is still clipped.
        const PAD_WIDTH: usize = 80;

        let title = "Calibration Client Linux";
        let server = tfd::input_box(title, &pad("ColourSpace IP:", PAD_WIDTH), "")?;

        if server.trim().is_empty() {
            None
        } else {
            Some(server)
        }
    }

    fn add_default_port(s: &str) -> String {
        if let Some(pos) = s.rfind(':') {
            if s[pos + 1..].parse::<u16>().is_ok() {
                return s.to_string();
            }
        }
        format!("{}:20002", s)
    }

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
    /// Note: this is intentionally local to `main.rs` so the `lan` module stays
    /// depth-agnostic. When you add a Vulkan 10-bit pipeline, replace or extend
    /// this helper to return higher-bit buffers or skip the conversion entirely.
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

    // ---------------------------------------------------------------------
    // Create event pump early so we can keep the window responsive during waits
    // ---------------------------------------------------------------------
    let mut event_pump = sdl_context.event_pump()?;

    // ---------------------------------------------------------------------
    // STARTUP UI + NETWORK WORKER SETUP (retry on failure) - with connect timeout
    // and non-freezing dialog handling
    // ---------------------------------------------------------------------
    let mut current_measure_colour = ColorRGB::default();
    let mut maybe_remote = args.remote;

    // Increased timeout to 6000ms to give slower setups time to connect.
    const CONNECT_TIMEOUT_MS: u64 = 6000;
    const CONNECT_POLL_MS: u64 = 50;

    // The loop yields Some(worker_state) when we have a worker that successfully connected.
    // If the user cancels the UI, we exit cleanly.
    let worker = loop {
        // Use CLI-provided address once; otherwise prompt the UI.
        let remote_input = maybe_remote.take().or_else(|| show_startup_ui());

        // If the user cancelled the UI (or provided empty input), exit gracefully.
        let remote = match remote_input {
            Some(r) => r,
            None => return Ok(()),
        };

        let remote_addr = add_default_port(&remote);

        match spawn_worker(&remote_addr, false) {
            Ok(state) => {
                // Tell worker what colour to request initially.
                state.write().unwrap().request_colour = current_measure_colour;

                // Wait a short while for the worker thread to actually establish a connection,
                // but keep the SDL window responsive while we wait.
                let mut elapsed = 0u64;
                let mut connected = {
                    let r = state.read().unwrap();
                    r.connected
                };

                // debug print initial state
                eprintln!("Waiting up to {}ms for ColourSpace to connect (initial connected={})", CONNECT_TIMEOUT_MS, connected);

                while !connected && elapsed < CONNECT_TIMEOUT_MS {
                    // Poll SDL events so the window remains responsive
                    for evt in event_pump.poll_iter() {
                        match evt {
                            sdl2::event::Event::Quit { .. } => return Ok(()),
                            _ => {}
                        }
                    }

                    std::thread::sleep(std::time::Duration::from_millis(CONNECT_POLL_MS));
                    elapsed += CONNECT_POLL_MS;

                    connected = {
                        let r = state.read().unwrap();
                        r.connected
                    };

                    // small debug print every 1s
                    if elapsed % 1000 == 0 {
                        eprintln!("  connect wait: {}ms elapsed, connected={}", elapsed, connected);
                    }
                }

                if connected {
                    // success: worker connected within timeout — keep it.
                    eprintln!("ColourSpace connected after {}ms", elapsed);
                    break Some(state);
                } else {
                    // Timed out: worker never connected. Drop it and show error dialog without freezing the UI.
                    eprintln!(
                        "spawn_worker returned Ok but failed to connect within {}ms (last connected={})",
                              CONNECT_TIMEOUT_MS, connected
                    );

                    // We'll spawn a thread to show the blocking message box, and use an AtomicBool
                    // to detect when the user has dismissed it — while still polling SDL events.
                    let dialog_done = Arc::new(AtomicBool::new(false));
                    let dialog_done_clone = Arc::clone(&dialog_done);

                    // Spawn the dialog on another thread (it will block there until user presses OK).
                    let _dialog_thread = spawn(move || {
                        let _ = tfd::message_box_ok(
                            "Calibration Client Linux",
                            "ColourSpace not reachable, check IP address",
                            tfd::MessageBoxIcon::Error,
                        );
                        dialog_done_clone.store(true, Ordering::SeqCst);
                    });

                    // Wait for the dialog to be dismissed while continuing to poll SDL events.
                    while !dialog_done.load(Ordering::SeqCst) {
                        for evt in event_pump.poll_iter() {
                            match evt {
                                sdl2::event::Event::Quit { .. } => return Ok(()),
                                _ => {}
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }

                    // Loop will continue and re-open show_startup_ui().
                }
            }
            Err(err) => {
                eprintln!("Failed to spawn worker: {}", err);

                // Show the error message non-freezing (same pattern as above)
                let dialog_done = Arc::new(AtomicBool::new(false));
                let dialog_done_clone = Arc::clone(&dialog_done);

                let err_str = format!("ColourSpace not found\n\n{}", err);
                let _dialog_thread = spawn(move || {
                    let _ = tfd::message_box_ok("Calibration Client Linux", &err_str, tfd::MessageBoxIcon::Error);
                    dialog_done_clone.store(true, Ordering::SeqCst);
                });

                while !dialog_done.load(Ordering::SeqCst) {
                    for evt in event_pump.poll_iter() {
                        match evt {
                            sdl2::event::Event::Quit { .. } => return Ok(()),
                            _ => {}
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }

                // loop will continue and re-open show_startup_ui()
            }
        }
    };

    // Build the output once we have a worker (or the user cancelled earlier).
    let mut output = match args.hdr {
        HdrMode::Sdr => Output::Sdr(window.into_canvas().build()?),
        mode => {
            let metadata = HdrMetadata {
                primaries: args.primaries,
                max_luminance: args.max_luminance,
                min_luminance: args.min_luminance,
                max_cll: args.max_cll,
                max_fall: args.max_fall,
            };
            match HdrPresenter::new(window, mode, metadata) {
                Ok(presenter) => Output::Hdr(presenter),
                Err(err) => {
                    // Never fall back to SDR silently: measuring the wrong signal is worse than failing.
                    eprintln!("{} output unavailable: {}", mode.label(), err);
                    let text = format!("{} output is not available\n\n{}", mode.label(), err);
                    let _ = tfd::message_box_ok("Calibration Client Linux", &text, tfd::MessageBoxIcon::Error);
                    return Err(err.into());
                }
            }
        }
    };
    // Note: we already created event_pump earlier; reuse it.

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

        // One read of the worker state per frame (if any)
        let (disconnected, shapes, worker_current_colour) = if let Some(state) = worker.as_ref() {
            let r = state.read().unwrap();
            (!r.connected, r.shapes.clone(), r.current_measure_colour)
        } else {
            (true, Vec::new(), ColorRGB::default())
        };

        // Update current measure colour depending on worker state and shapes
        if disconnected {
            if worker.is_none() {
                // keep whatever current_measure_colour already is
            } else {
                current_measure_colour = worker_current_colour;
            }
        } else {
            if shapes.is_empty() {
                current_measure_colour = worker_current_colour;
            } else {
                current_measure_colour = select_measure_colour(&shapes).unwrap_or(current_measure_colour);
            }
        }

        // Draw
        let (cw, ch) = output.size()?;
        let show_shapes = !disconnected && !shapes.is_empty();
        match &mut output {
            Output::Sdr(canvas) => {
                if show_shapes {
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
            Output::Hdr(hdr) => {
                // Code values go to the 10-bit HDR swapchain untouched (no 8-bit downscale).
                if show_shapes {
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
