use argh::FromArgs;
use std::cmp::Ordering;
use std::time::{Duration, Instant};

mod lan;
use lan::{ColorRGB, ShapeInstruction, SharedState, spawn_worker};
use sdl2::pixels::Color;
use sdl2::rect::Rect;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sdl_context = sdl2::init()?;
    let video = sdl_context.video()?;

    // Create a window sized to the desktop resolution and force fullscreen
    let display_index = 0;
    let dm = video.desktop_display_mode(display_index)?;
    let width = dm.w as u32;
    let height = dm.h as u32;

    let mut window = video
        .window("Colourspace", width, height)
        .position_centered()
        .vulkan() // allow Vulkan if needed later, but we use SDL2 renderer for prototype
        .build()?;

    // Window mode and canvas will be configured after CLI parsing so we can
    // honor `--windowed`, `--width`, and `--height` flags.

    // Do NOT animate colors locally. The program must only display colours
    // provided by the server. Keep an initial colour, but never modify it
    // locally (no screensaver/rainbow behaviour).

    // If no port is provided, use default 20002.
    fn add_default_port(s: &str) -> String {
        // If there's a trailing :port that parses as u16, keep it. Otherwise append default.
        if let Some(pos) = s.rfind(':') {
            if let Ok(_) = s[pos + 1..].parse::<u16>() {
                return s.to_owned();
            }
        }
        format!("{}:20002", s)
    }

    #[derive(FromArgs)]
    /// Colourspace viewer
    struct Args {
        /// remote server host[:port]
        #[argh(option)]
        remote: Option<String>,

        /// pretty-print and save received XML messages
        #[argh(switch, long = "pretty-print")]
        pretty_print: bool,
        /// run windowed instead of fullscreen
        #[argh(switch)]
        windowed: bool,
        /// window width when running windowed
        #[argh(option)]
        width: Option<u32>,
        /// window height when running windowed
        #[argh(option)]
        height: Option<u32>,
    }

    fn select_measure_colour(shapes: &[ShapeInstruction]) -> Option<ColorRGB> {
        shapes
            .iter()
            .filter_map(|shape| match shape {
                ShapeInstruction::Rectangle(rect) => {
                    let area = rect.area();
                    let sanitized = if area.is_finite() && area > 0.0 {
                        area
                    } else {
                        f32::MAX
                    };
                    Some((sanitized, rect.color))
                }
            })
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal))
            .map(|(_, color)| color)
    }

    fn draw_shapes(
        canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
        shapes: &[ShapeInstruction],
        width: u32,
        height: u32,
    ) {
        canvas.set_draw_color(Color::RGB(0, 0, 0));
        canvas.clear();

        for shape in shapes {
            match shape {
                ShapeInstruction::Rectangle(rect) => {
                    let geom = rect.geometry;
                    let width_ratio = geom.width.clamp(0.0, 1.0);
                    let height_ratio = geom.height.clamp(0.0, 1.0);

                    let width_px_f32 = (width_ratio * width as f32).clamp(1.0, width as f32);
                    let height_px_f32 = (height_ratio * height as f32).clamp(1.0, height as f32);
                    let width_px = width_px_f32.round() as u32;
                    let height_px = height_px_f32.round() as u32;
                    let left = ((width as f32 - width_px_f32) / 2.0).round() as i32;
                    let top = ((height as f32 - height_px_f32) / 2.0).round() as i32;

                    let draw_color = Color::RGB(rect.color.red, rect.color.green, rect.color.blue);
                    canvas.set_draw_color(draw_color);
                    let rect = Rect::new(left, top, width_px.max(1), height_px.max(1));
                    let _ = canvas.fill_rect(rect);
                }
            }
        }
    }

    let args: Args = argh::from_env();
    let remote_addr: Option<String> = args.remote.map(|s| add_default_port(&s));

    let mut current_measure_colour = ColorRGB::default();

    let mut worker: Option<std::sync::Arc<std::sync::RwLock<SharedState>>> = None;
    if let Some(addr) = remote_addr.clone() {
        match spawn_worker(&addr, args.pretty_print) {
            Ok(state) => {
                // store the state and write the initial request colour so the
                // sending thread sends an initial measurement immediately.
                state.write().unwrap().request_colour = current_measure_colour;
                worker = Some(state);
            }
            Err(e) => {
                eprintln!("Failed to spawn ColourSpace worker for {}: {}", addr, e);
            }
        }
    }

    // Apply windowed/size flags from CLI and then create the canvas.
    if args.windowed {
        let w = args.width.unwrap_or(800);
        let h = args.height.unwrap_or(600);
        let _ = window.set_size(w, h);
    } else {
        let _ = window.set_fullscreen(sdl2::video::FullscreenType::Desktop);
    }

    // Use SDL2's software renderer to fill the screen with a solid color (simple prototype)
    let mut canvas = window.into_canvas().build()?;

    let mut event_pump = sdl_context.event_pump()?;
    let mut last_fps = Instant::now();
    let mut _frames = 0u32;

    'running: loop {
        for event in event_pump.poll_iter() {
            match event {
                sdl2::event::Event::Quit { .. }
                | sdl2::event::Event::KeyDown {
                    keycode: Some(sdl2::keyboard::Keycode::Escape),
                    ..
                } => break 'running,
                _ => {}
            }
        }

        if let Some(state) = worker.as_ref() {
            // Do NOT update `request_colour` each frame from the UI. The
            // request colour is set once at startup and must not be changed
            // by local animation or drawing logic.

            // Read the latest shared state each frame (no dirty flag). Use a read lock.
            let (worker_disconnected, shapes) = {
                let r = state.read().unwrap();
                (!r.connected, r.shapes.clone())
            };

            if worker_disconnected {
                // keep the worker handle (threads still own clones). Just reset the visual state until connection recovers.
                current_measure_colour = ColorRGB::default();
            }

            if !worker_disconnected {
                if shapes.is_empty() {
                    // use the measured colour from the shared state
                    let r = state.read().unwrap();
                    current_measure_colour = r.current_measure_colour;
                    let colour = current_measure_colour;
                    canvas.set_draw_color(Color::RGB(colour.red, colour.green, colour.blue));
                    canvas.clear();
                } else {
                    // choose measurement colour from shapes deterministically
                    current_measure_colour =
                        select_measure_colour(&shapes).unwrap_or(current_measure_colour);
                    draw_shapes(&mut canvas, &shapes, width, height);
                }
                canvas.present();

                _frames += 1;
                if last_fps.elapsed() >= Duration::from_secs(1) {
                    _frames = 0;
                    last_fps = Instant::now();
                }

                continue;
            }
        }

        // No local colour updates — draw the most recent colour available in
        // `current_measure_colour` (which is kept in sync with the shared
        // state above). If there is no worker, this remains the initial
        // colour and will not change.
        canvas.set_draw_color(Color::RGB(
            current_measure_colour.red,
            current_measure_colour.green,
            current_measure_colour.blue,
        ));
        canvas.clear();
        canvas.present();

        _frames += 1;
        if last_fps.elapsed() >= Duration::from_secs(1) {
            _frames = 0;
            last_fps = Instant::now();
        }

        std::thread::sleep(Duration::from_millis(8));
    }

    Ok(())
}
