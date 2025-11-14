use argh::FromArgs;
use tinyfiledialogs as tfd;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use std::thread::{sleep, spawn};
use std::error::Error;

mod lan;
use lan::{ColorRGB, ShapeInstruction, spawn_worker};
use sdl2::pixels::Color;
use sdl2::rect::Rect;

fn main() -> Result<(), Box<dyn Error>> {
    let sdl_context = sdl2::init()?;
    let video = sdl_context.video()?;

    const DEFAULT_W: u32 = 1280;
    const DEFAULT_H: u32 = 720;

    // Always start windowed; fullscreen only via double-click
    let window = video
    .window("Calibration Client Linux", DEFAULT_W, DEFAULT_H)
    .position_centered()
    .vulkan()
    .resizable()
    .allow_highdpi()
    .build()?;

    #[derive(FromArgs)]
    /// Colourspace viewer
    struct Args {
        /// remote server host[:port] (positional). Optional.
        #[argh(positional)]
        remote: Option<String>,
    }

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
                    let geom = rect.geometry;
                    // clamp widths/heights and ensure at least 1 pixel
                    let rw = (geom.width.clamp(0.0, 1.0) * w as f32).round().max(1.0) as u32;
                    let rh = (geom.height.clamp(0.0, 1.0) * h as f32).round().max(1.0) as u32;

                    let left = ((w as f32 - rw as f32) / 2.0).round() as i32;
                    let top = ((h as f32 - rh as f32) / 2.0).round() as i32;

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
    // ARG PARSING (ONLY POSITIONAL IP)
    // ---------------------------------------------------------------------
    let args: Args = argh::from_env();

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

    // Build the canvas once we have a worker (or the user cancelled earlier).
    let mut canvas = window.into_canvas().build()?;
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
                                canvas
                                .window_mut()
                                .set_fullscreen(sdl2::video::FullscreenType::Off)
                                .ok();
                                is_fullscreen = false;
                            } else {
                                canvas
                                .window_mut()
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
                                    canvas
                                    .window_mut()
                                    .set_fullscreen(sdl2::video::FullscreenType::Off)
                                    .ok();
                                    is_fullscreen = false;
                                } else {
                                    canvas
                                    .window_mut()
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
        let (cw, ch) = canvas.output_size()?;
        if !disconnected && !shapes.is_empty() {
            draw_shapes(&mut canvas, &shapes, cw, ch);
        } else {
            let c = current_measure_colour;
            // downscale before giving to SDL using the helper
            let (r8, g8, b8) = color_to_u8_tuple(c);
            canvas.set_draw_color(Color::RGB(r8, g8, b8));
            canvas.clear();
        }

        // Present once per frame (consistent timing fixes the double-click quirk)
        canvas.present();

        // small sleep to avoid burning CPU in pathological cases
        sleep(Duration::from_millis(1));
    }

    Ok(())
}
