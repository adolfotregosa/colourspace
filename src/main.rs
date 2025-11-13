use std::cmp::Ordering;
use std::env;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

mod lan;
use lan::{ColorRGB, MeasureRequest, ShapeInstruction, spawn_worker};
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

    // Force desktop fullscreen
    window.set_fullscreen(sdl2::video::FullscreenType::Desktop)?;

    // Use SDL2's software renderer to fill the screen with a solid color (simple prototype)
    let mut canvas = window.into_canvas().build()?;

    let mut event_pump = sdl_context.event_pump()?;
    let mut last_fps = Instant::now();
    let mut _frames = 0u32;

    // Color animate over time for visible effect
    let t0 = Instant::now();

    // Check for remote argument: support both `--remote host[:port]` and `--remote=host[:port]`.
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
        let background = Color::RGB(0, 0, 0);
        canvas.set_draw_color(background);
        canvas.clear();

        for shape in shapes {
            match shape {
                ShapeInstruction::Rectangle(rect) => {
                    let geom = rect.geometry;
                    let width_ratio = geom.width.clamp(0.0, 1.0);
                    let height_ratio = geom.height.clamp(0.0, 1.0);
                    let center_x_ratio = geom.center_x.clamp(0.0, 1.0);
                    let center_y_ratio = geom.center_y.clamp(0.0, 1.0);

                    let width_px = (width_ratio * width as f32).round().max(1.0) as u32;
                    let height_px = (height_ratio * height as f32).round().max(1.0) as u32;
                    let cx_px = (center_x_ratio * width as f32).round() as i32;
                    let cy_px = (center_y_ratio * height as f32).round() as i32;
                    let left = cx_px - (width_px as i32 / 2);
                    let top = cy_px - (height_px as i32 / 2);

                    let draw_color = Color::RGB(rect.color.red, rect.color.green, rect.color.blue);
                    canvas.set_draw_color(draw_color);
                    let rect = Rect::new(left, top, width_px.max(1), height_px.max(1));
                    let _ = canvas.fill_rect(rect);
                }
            }
        }
    }

    let mut remote_addr: Option<String> = None;
    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--remote" {
            if i + 1 < args.len() {
                remote_addr = Some(add_default_port(&args[i + 1]));
                i += 1;
            }
        } else if arg.starts_with("--remote=") {
            let v = &arg[9..];
            remote_addr = Some(add_default_port(v));
        }
        i += 1;
    }

    let measurement_interval = Duration::from_secs(1);
    let mut need_initial_measure = false;
    let mut last_measure_sent = Instant::now();
    let mut current_measure_colour = ColorRGB::default();
    let mut shapes: Vec<ShapeInstruction> = Vec::new();

    let mut worker: Option<(
        std::sync::mpsc::Sender<MeasureRequest>,
        std::sync::mpsc::Receiver<Result<lan::MeasurementResult, String>>,
    )> = None;
    if let Some(addr) = remote_addr.clone() {
        match spawn_worker(&addr) {
            Ok((tx, rx)) => {
                worker = Some((tx, rx));
                need_initial_measure = true;
            }
            Err(e) => {
                eprintln!("Failed to spawn ColourSpace worker for {}: {}", addr, e);
            }
        }
    }

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

        let mut worker_disconnected = false;
        if let Some((tx, rx)) = worker.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(Ok(res)) => {
                        let lan::MeasurementResult {
                            red,
                            green,
                            blue,
                            x,
                            y,
                            y_lum,
                            shapes: new_shapes,
                        } = res;
                        let measurement_colour = ColorRGB::from_components(red, green, blue);
                        if !new_shapes.is_empty() {
                            let shape_colour =
                                select_measure_colour(&new_shapes).unwrap_or(measurement_colour);
                            current_measure_colour = shape_colour;
                            shapes = new_shapes;
                        } else {
                            current_measure_colour = measurement_colour;
                            shapes.clear();
                        }
                        println!("Measured result: x={:?} y={:?} y_lum={:?}", x, y, y_lum);
                    }
                    Ok(Err(e)) => {
                        println!("Measurement error: {}", e);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        println!("ColourSpace worker disconnected");
                        worker_disconnected = true;
                        break;
                    }
                }
            }

            if !worker_disconnected
                && (need_initial_measure || last_measure_sent.elapsed() >= measurement_interval)
            {
                let colour = current_measure_colour;
                match tx.send(MeasureRequest {
                    red: colour.red,
                    green: colour.green,
                    blue: colour.blue,
                }) {
                    Ok(_) => {
                        need_initial_measure = false;
                        last_measure_sent = Instant::now();
                    }
                    Err(_) => {
                        worker_disconnected = true;
                    }
                }
            }
        }

        if worker_disconnected {
            worker = None;
            need_initial_measure = false;
            shapes.clear();
        }

        if worker.is_some() {
            if shapes.is_empty() {
                let colour = current_measure_colour;
                canvas.set_draw_color(Color::RGB(colour.red, colour.green, colour.blue));
                canvas.clear();
            } else {
                draw_shapes(&mut canvas, &shapes, width, height);
            }
        } else {
            let elapsed = t0.elapsed().as_secs_f32();
            let r = ((elapsed * 0.3).sin() * 0.5 + 0.5) * 255.0;
            let g = ((elapsed * 0.5).sin() * 0.5 + 0.5) * 255.0;
            let b = ((elapsed * 0.7).sin() * 0.5 + 0.5) * 255.0;
            current_measure_colour = ColorRGB::from_components(r as u8, g as u8, b as u8);
            canvas.set_draw_color(Color::RGB(
                current_measure_colour.red,
                current_measure_colour.green,
                current_measure_colour.blue,
            ));
            canvas.clear();
        }

        canvas.present();

        _frames += 1;
        if last_fps.elapsed() >= Duration::from_secs(1) {
            //println!("FPS: {}", _frames);
            _frames = 0;
            last_fps = Instant::now();
        }

        std::thread::sleep(Duration::from_millis(16));
    }

    Ok(())
}
