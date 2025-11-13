use std::env;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

mod lan;
use lan::{MeasureRequest, spawn_worker};

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
    let mut frames = 0u32;

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

    let mut worker: Option<(
        std::sync::mpsc::Sender<MeasureRequest>,
        std::sync::mpsc::Receiver<Result<lan::MeasurementResult, String>>,
    )> = None;
    if let Some(addr) = remote_addr.clone() {
        match spawn_worker(&addr) {
            Ok((tx, rx)) => {
                worker = Some((tx, rx));
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

        let elapsed = t0.elapsed().as_secs_f32();
        let r = ((elapsed * 0.3).sin() * 0.5 + 0.5) * 255.0;
        let g = ((elapsed * 0.5).sin() * 0.5 + 0.5) * 255.0;
        let b = ((elapsed * 0.7).sin() * 0.5 + 0.5) * 255.0;

        canvas.set_draw_color(sdl2::pixels::Color::RGB(r as u8, g as u8, b as u8));
        canvas.clear();
        canvas.present();

        // If worker exists, send a measurement request once per second and poll for responses.
        if let Some((tx, rx)) = worker.as_ref() {
            // send measurement at approx 1 Hz using elapsed seconds
            if elapsed.fract() < 0.02 {
                // best-effort non-blocking send: ignore if receiver is gone
                let _ = tx.send(MeasureRequest {
                    red: r as u8,
                    green: g as u8,
                    blue: b as u8,
                });
            }

            // non-blocking receive of any available responses
            loop {
                match rx.try_recv() {
                    Ok(Ok(res)) => println!(
                        "Measured result: x={:?} y={:?} y_lum={:?}",
                        res.x, res.y, res.y_lum
                    ),
                    Ok(Err(e)) => println!("Measurement error: {}", e),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        println!("ColourSpace worker disconnected");
                        break 'running;
                    }
                }
            }
        }

        frames += 1;
        if last_fps.elapsed() >= Duration::from_secs(1) {
            //println!("FPS: {}", frames);
            frames = 0;
            last_fps = Instant::now();
        }

        // Cap to ~60Hz
        std::thread::sleep(Duration::from_millis(16));
    }

    Ok(())
}
