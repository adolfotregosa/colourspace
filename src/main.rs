use argh::FromArgs;
use tinyfiledialogs as tfd;
use std::time::{Duration, Instant};

mod lan;
use lan::{ColorRGB, ShapeInstruction, spawn_worker};
use sdl2::pixels::Color;
use sdl2::rect::Rect;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sdl_context = sdl2::init()?;
    let video = sdl_context.video()?;

    const DEFAULT_W: u32 = 1280;
    const DEFAULT_H: u32 = 720;

    // Build window (no worker yet) so UI appears quickly
    let window = video
    .window("Calibration Client 2.0", DEFAULT_W, DEFAULT_H)
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

        let title = "Calibration Client 2.0";
        let server = tfd::input_box(
            title,
            &pad("ColourSpace IP:", PAD_WIDTH),
                                    "",
        )?;

        if server.trim().is_empty() { None } else { Some(server) }
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
        shapes.iter().filter_map(|shape| match shape {
            ShapeInstruction::Rectangle(rect) => {
                let area = (rect.geometry.width * rect.geometry.height).max(0.0001);
                Some((area, rect.color))
            }
        })
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
        .map(|(_, color)| color)
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
                    let rw = (geom.width.clamp(0.0, 1.0) * w as f32).round().max(1.0) as u32;
                    let rh = (geom.height.clamp(0.0, 1.0) * h as f32).round().max(1.0) as u32;

                    let left = ((w as f32 - rw as f32) / 2.0).round() as i32;
                    let top  = ((h as f32 - rh as f32) / 2.0).round() as i32;

                    let color = rect.color;
                    canvas.set_draw_color(Color::RGB(color.red, color.green, color.blue));
                    let _ = canvas.fill_rect(Rect::new(left, top, rw, rh));
                }
            }
        }
    }

    // ---------------------------------------------------------------------
    // ARG PARSING (ONLY POSITIONAL IP)
    // ---------------------------------------------------------------------
    let args: Args = argh::from_env();

    let remote = match args.remote {
        Some(r) => r,
        None => match show_startup_ui() {
            Some(ip) => ip,
            None => return Ok(()),
        },
    };

    let remote_addr = add_default_port(&remote);

    // Build canvas WITH vsync so present() blocks to refresh
    let mut canvas = window.into_canvas().present_vsync().build()?;
    // Show an initial clear immediately so the window appears fast
    canvas.set_draw_color(Color::RGB(0, 0, 0));
    canvas.clear();
    canvas.present();

    // Now spawn the worker (may do network operations); UI is already visible
    let mut current_measure_colour = ColorRGB::default();
    let worker = match spawn_worker(&remote_addr, false) {
        Ok(state) => {
            state.write().unwrap().request_colour = current_measure_colour;
            Some(state)
        }
        Err(e) => {
            eprintln!("Failed to spawn worker: {}", e);
            None
        }
    };

    let mut event_pump = sdl_context.event_pump()?;

    // double-click detection
    let mut last_click_time = None::<Instant>;
    let mut is_fullscreen = false;
    let dc_threshold = Duration::from_millis(400);

    // bookkeeping for dirty-check
    let mut last_shapes_hash: u64 = 0;
    let mut last_shapes_len: usize = 0;
    let mut last_measure_colour = ColorRGB::default();
    let mut last_rendered_fullscreen = is_fullscreen;

    // event wait timeout (u32)
    const EVENT_WAIT_MS: u32 = 8;

    'running: loop {
        // Wait for a single event or timeout; then drain any extra queued events
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
                                canvas.window_mut().set_fullscreen(sdl2::video::FullscreenType::Off).ok();
                                is_fullscreen = false;
                            } else {
                                canvas.window_mut().set_fullscreen(sdl2::video::FullscreenType::Desktop).ok();
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

            // Drain remaining events to avoid reprocessing next frame
            for evt in event_pump.poll_iter() {
                match evt {
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
                                    canvas.window_mut().set_fullscreen(sdl2::video::FullscreenType::Off).ok();
                                    is_fullscreen = false;
                                } else {
                                    canvas.window_mut().set_fullscreen(sdl2::video::FullscreenType::Desktop).ok();
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

        // Read worker state once per frame
        let (disconnected, shapes_snapshot_len, computed_shapes_hash, worker_current_colour) =
        if let Some(state) = worker.as_ref() {
            let r = state.read().unwrap();

            // If there are no shapes, skip hashing loop entirely (fast path).
            if r.shapes.is_empty() {
                ( !r.connected, 0usize, 0u64, r.current_measure_colour )
            } else {
                // Compute a cheap hash from shapes *without cloning* the Vec
                let mut h: u64 = r.shapes.len() as u64;
                for shape in r.shapes.iter() {
                    match shape {
                        ShapeInstruction::Rectangle(rect) => {
                            let w_bits = rect.geometry.width.to_bits() as u64;
                            let h_bits = rect.geometry.height.to_bits() as u64;
                            let color_word = ((rect.color.red as u64) << 16)
                            | ((rect.color.green as u64) << 8)
                            | (rect.color.blue as u64);
                            h = h.wrapping_mul(31).wrapping_add(w_bits);
                            h = h.wrapping_mul(31).wrapping_add(h_bits);
                            h = h.wrapping_mul(31).wrapping_add(color_word);
                        }
                    }
                }
                ( !r.connected, r.shapes.len(), h, r.current_measure_colour )
            }
        } else {
            ( true, 0usize, 0u64, ColorRGB::default() )
        };

        // Decide whether to repaint:
        let measure_changed = worker_current_colour != last_measure_colour;
        let fullscreen_changed = last_rendered_fullscreen != is_fullscreen;
        let shapes_changed = computed_shapes_hash != last_shapes_hash || shapes_snapshot_len != last_shapes_len;

        // If shapes changed and we need the full shapes for drawing, clone them now.
        // Otherwise avoid cloning entirely.
        let mut shapes_clone: Vec<ShapeInstruction> = Vec::new();
        if shapes_changed {
            if let Some(state) = worker.as_ref() {
                let r = state.read().unwrap();
                shapes_clone = r.shapes.clone(); // only clone when checksum changed
            }
        }

        // Update current_measure_colour similar to previous behaviour:
        if disconnected {
            if worker.is_none() {
                // keep existing current_measure_colour
            } else {
                current_measure_colour = worker_current_colour;
            }
        } else {
            if shapes_clone.is_empty() && shapes_snapshot_len == 0 {
                current_measure_colour = worker_current_colour;
            } else if !shapes_clone.is_empty() {
                current_measure_colour = select_measure_colour(&shapes_clone).unwrap_or(current_measure_colour);
            }
        }

        // Decide whether we actually need to render/present:
        let need_present = shapes_changed || measure_changed || fullscreen_changed;

        if need_present {
            let (cw, ch) = canvas.output_size()?;

            if !disconnected && !shapes_clone.is_empty() {
                draw_shapes(&mut canvas, &shapes_clone, cw, ch);
            } else {
                let c = current_measure_colour;
                canvas.set_draw_color(Color::RGB(c.red, c.green, c.blue));
                canvas.clear();
            }

            // Present once per frame (vsync will throttle)
            canvas.present();

            // update last seen values
            last_shapes_hash = computed_shapes_hash;
            last_shapes_len = shapes_snapshot_len;
            last_measure_colour = worker_current_colour;
            last_rendered_fullscreen = is_fullscreen;
        }

        // No extra sleep needed: wait_event_timeout handles idle waiting and present_vsync throttles drawing.
    }

    Ok(())
}
