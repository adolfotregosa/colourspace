use argh::FromArgs;
use tinyfiledialogs as tfd;
use std::time::{Duration, Instant};
use std::thread::sleep;

mod lan;
use lan::{ColorRGB, ShapeInstruction, spawn_worker};
use sdl2::pixels::Color;
use sdl2::rect::Rect;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sdl_context = sdl2::init()?;
    let video = sdl_context.video()?;

    const DEFAULT_W: u32 = 1280;
    const DEFAULT_H: u32 = 720;

    // Always start windowed; fullscreen only via double-click
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
                    // clamp widths/heights and ensure at least 1 pixel
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

    // ---------------------------------------------------------------------
    // NETWORK WORKER SETUP
    // ---------------------------------------------------------------------
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

    let mut canvas = window.into_canvas().build()?;
    let mut event_pump = sdl_context.event_pump()?;

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
            canvas.set_draw_color(Color::RGB(c.red, c.green, c.blue));
            canvas.clear();
        }

        // Present once per frame (consistent timing fixes the double-click quirk)
        canvas.present();

        // small sleep to avoid burning CPU in pathological cases
        sleep(Duration::from_millis(1));
    }

    Ok(())
}
