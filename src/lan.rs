use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use quick_xml::Reader;
use quick_xml::events::Event;
use quick_xml::events::BytesStart;

#[derive(Debug, Clone)]
pub struct MeasurementResult {
    // store as u16 so we can carry 10/12/16-bit values
    pub red: u16,
    pub green: u16,
    pub blue: u16,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub y_lum: Option<f64>,
    pub shapes: Vec<ShapeInstruction>,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ColorRGB {
    // allow storage up to 16-bit per channel
    pub red: u16,
    pub green: u16,
    pub blue: u16,
    // how many bits per channel the source values represent (8,10,12,16...)
    pub depth_bits: u8,
}

impl Default for ColorRGB {
    fn default() -> Self {
        Self {
            red: 0,
            green: 0,
            blue: 0,
            depth_bits: 8,
        }
    }
}

impl ColorRGB {
    pub fn from_components_u16(red: u16, green: u16, blue: u16, bits: u8) -> Self {
        let bits = if bits == 0 { 8 } else { bits };
        Self { red, green, blue, depth_bits: bits }
    }
    // to_u8_tuple intentionally removed — consumer should perform downscale.
}

#[derive(Debug, Clone, Copy)]
pub struct RectangleGeometry { pub width: f32, pub height: f32 }

#[derive(Debug, Clone)]
pub struct RectangleShape { pub color: ColorRGB, pub geometry: RectangleGeometry }

#[derive(Debug, Clone)]
pub enum ShapeInstruction { Rectangle(RectangleShape) }

/// Parse XML string into a MeasurementResult. The `r,g,b` parameters are the
/// requested components that will be used as fallback initial values in the
/// result (keeps previous behavior). These are now u16 to allow >8-bit defaults.
fn parse_measurement_from_xml(xml: &str, r: u16, g: u16, b: u16) -> Result<MeasurementResult, String> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);
    let mut buf = Vec::new();
    let mut in_result = false;
    let mut cur_elem = String::new();
    let mut res = MeasurementResult { red: r, green: g, blue: b, x: None, y: None, y_lum: None, shapes: Vec::new() };
    let mut element_stack: Vec<String> = Vec::new();
    let mut reported_commands: HashSet<String> = HashSet::new();
    let mut parsed_shapes: Vec<ShapeInstruction> = Vec::new();

    #[derive(Default)]
    struct RectangleBuilder { color: Option<ColorRGB>, width: Option<f32>, height: Option<f32> }
    impl RectangleBuilder {
        fn build(self) -> Option<RectangleShape> {
            let color = self.color?;
            let width = self.width.unwrap_or(1.0);
            let height = self.height.unwrap_or(1.0);
            Some(RectangleShape { color, geometry: RectangleGeometry { width, height } })
        }
    }
    let mut rect_builder: Option<RectangleBuilder> = None;

    // apply_color now understands "bits" attribute and larger numeric values
    let apply_color = |reader: &Reader<&[u8]>, element: &BytesStart, builder: &mut RectangleBuilder| {
        let mut colour = builder.color.unwrap_or_default();
        let mut updated = false;
        for attr in element.attributes().with_checks(false) {
            if let Ok(attr) = attr {
                if let Ok(value) = attr.decode_and_unescape_value(reader) {
                    match attr.key.as_ref() {
                        b"bits" | b"depth" | b"bitDepth" => { if let Ok(v) = value.parse::<u8>() { colour.depth_bits = v; } }
                        b"red" => { if let Ok(v) = value.parse::<u16>() { colour.red = v; updated = true; } else if let Ok(v8) = value.parse::<u8>() { colour.red = v8 as u16; updated = true; } }
                        b"green" => { if let Ok(v) = value.parse::<u16>() { colour.green = v; updated = true; } else if let Ok(v8) = value.parse::<u8>() { colour.green = v8 as u16; updated = true; } }
                        b"blue" => { if let Ok(v) = value.parse::<u16>() { colour.blue = v; updated = true; } else if let Ok(v8) = value.parse::<u8>() { colour.blue = v8 as u16; updated = true; } }
                        _ => {}
                    }
                }
            }
        }
        if updated { builder.color = Some(colour); }
    };

    let apply_geometry = |reader: &Reader<&[u8]>, element: &BytesStart, builder: &mut RectangleBuilder| {
        for attr in element.attributes().with_checks(false) {
            if let Ok(attr) = attr {
                if let Ok(value) = attr.decode_and_unescape_value(reader) {
                    match attr.key.as_ref() {
                        b"cx" => { if let Ok(v) = value.parse::<f32>() { builder.width = Some(v); } }
                        b"cy" => { if let Ok(v) = value.parse::<f32>() { builder.height = Some(v); } }
                        b"x" => { if builder.width.is_none() { if let Ok(v) = value.parse::<f32>() { builder.width = Some(v); } } }
                        b"y" => { if builder.height.is_none() { if let Ok(v) = value.parse::<f32>() { builder.height = Some(v); } } }
                        _ => {}
                    }
                }
            }
        }
    };

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                element_stack.push(name.clone());
                if element_stack.len() == 2 {
                    let command = element_stack[1].clone();
                    if !reported_commands.insert(command.clone()) {
                        panic!("Received command: {} was not satisfied", command);
                    }
                }
                cur_elem = name.clone();
                if name == "result" { in_result = true; }
                if name == "rectangle" { rect_builder = Some(RectangleBuilder::default()); }
                else if name == "color" || name == "colex" { if let Some(builder) = rect_builder.as_mut() { apply_color(&reader, &e, builder); } }
                else if name == "geometry" { if let Some(builder) = rect_builder.as_mut() { apply_geometry(&reader, &e, builder); } }
            }
            Ok(Event::End(e)) => {
                if let Ok(end_name) = std::str::from_utf8(e.name().as_ref()) {
                    if end_name == "result" { in_result = false; }
                    if end_name == "rectangle" {
                        if let Some(builder) = rect_builder.take() {
                            if let Some(rect) = builder.build() { parsed_shapes.push(ShapeInstruction::Rectangle(rect)); }
                            else { panic!("Received rectangle command missing required attributes"); }
                        }
                    }
                }
                element_stack.pop();
            }
            Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "color" || name == "colex" { if let Some(builder) = rect_builder.as_mut() { apply_color(&reader, &e, builder); } }
                else if name == "geometry" { if let Some(builder) = rect_builder.as_mut() { apply_geometry(&reader, &e, builder); } }
            }
            Ok(Event::Text(e)) => {
                let raw_txt = e.unescape().unwrap_or_default().into_owned();
                let txt_trimmed = raw_txt.trim();
                if txt_trimmed.is_empty() { continue; }
                if let Some(command) = element_stack.get(1) {
                    if let Some(param) = element_stack.last() { if command != param { println!("  {} = {}", param, txt_trimmed); } }
                }
                if !in_result { continue; }
                match cur_elem.as_str() {
                    "red" => { if let Ok(v) = txt_trimmed.parse::<u16>() { res.red = v } else if let Ok(v8) = txt_trimmed.parse::<u8>() { res.red = v8 as u16; } }
                    "green" => { if let Ok(v) = txt_trimmed.parse::<u16>() { res.green = v } else if let Ok(v8) = txt_trimmed.parse::<u8>() { res.green = v8 as u16; } }
                    "blue" => { if let Ok(v) = txt_trimmed.parse::<u16>() { res.blue = v } else if let Ok(v8) = txt_trimmed.parse::<u8>() { res.blue = v8 as u16; } }
                    "x" => { if let Ok(v) = txt_trimmed.parse::<f64>() { res.x = Some(v) } }
                    "y" => { if let Ok(v) = txt_trimmed.parse::<f64>() { res.y = Some(v) } }
                    "Y" => { if let Ok(v) = txt_trimmed.parse::<f64>() { res.y_lum = Some(v) } }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => { return Err(format!("xml parse error: {}", e)); }
            _ => {}
        }
        buf.clear();
    }

    if let Some(builder) = rect_builder {
        if let Some(rect) = builder.build() { parsed_shapes.push(ShapeInstruction::Rectangle(rect)); }
        else { panic!("Received rectangle command missing required attributes"); }
    }

    res.shapes = parsed_shapes;

    // Debug output for received command: prefer the first parsed shape's color if available
    let (bit_depth, r_val, g_val, b_val) = if let Some(shape) = res.shapes.get(0) {
        match shape { ShapeInstruction::Rectangle(rsh) => ( rsh.color.depth_bits, rsh.color.red, rsh.color.green, rsh.color.blue ) }
    } else { (8u8, res.red, res.green, res.blue) };

    println!("Bit depth = {} , R = {} , G = {} , B = {}", bit_depth, r_val, g_val, b_val);

    Ok(res)
}

/// Read a length-prefixed message from the blocking TCP stream.
/// Header is a 4-byte big-endian signed i32. Negative means disconnect.
/// Returns Ok(Some(string)) for a payload, Ok(None) for negative header, Err on io.
fn read_message_from_stream(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    let signed_len = i32::from_be_bytes(header);
    if signed_len < 0 { return Ok(None); }
    let len = signed_len as usize;
    if len == 0 { return Ok(Some(String::new())); }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    String::from_utf8(payload).map(Some).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid utf8 payload"))
}

/// Connect to an address string like "192.168.168.11:20002" with a short timeout.
/// Tries all resolved socket addrs and returns the first successful TcpStream.
fn connect_with_timeout(addr_str: &str, timeout: Duration) -> std::io::Result<TcpStream> {
    let addrs = addr_str.to_socket_addrs()?;
    let mut last_err: Option<std::io::Error> = None;

    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => { return Ok(stream); }
            Err(e) => { last_err = Some(e); }
        }
    }

    Err(last_err.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no socket addresses found")))
}

/// Shared state between drawing and network threads.
pub struct SharedState { pub connected: bool, pub shapes: Vec<ShapeInstruction>, pub current_measure_colour: ColorRGB, pub request_colour: ColorRGB }

impl Default for SharedState {
    fn default() -> Self { Self { connected: false, shapes: Vec::new(), current_measure_colour: ColorRGB::default(), request_colour: ColorRGB::default() } }
}

/// Spawn a background worker thread that keeps a connection and performs measurements.
/// Returns an Arc<RwLock<SharedState>> that the caller (drawing thread) can use to read
/// the current shapes and measured colour.
pub fn spawn_worker(addr: &str, _pretty_print: bool) -> std::io::Result<Arc<RwLock<SharedState>>> {
    let addr = addr.to_owned();

    const CONNECT_TIMEOUT_MS: u64 = 500;
    let stream_res = connect_with_timeout(&addr, Duration::from_millis(CONNECT_TIMEOUT_MS));
    let stream = match stream_res { Ok(s) => Some(s), Err(e) => { eprintln!("Failed to connect to {}: {}", addr, e); None } };

    let state = Arc::new(RwLock::new(SharedState::default()));

    // If connection succeeded, spawn ONLY the receiving thread.
    if let Some(s) = stream {
        let stream_arc = Arc::new(Mutex::new(s));
        let state_recv = state.clone();
        let stream_recv = stream_arc.clone();

        thread::spawn(move || {
            // Send init profile (one-off mandatory handshake) without helper function.
            if let Ok(mut guard) = stream_recv.lock() {
                let _ = guard.write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\" ?><CS_RMC version=1><command>init profile</command></CS_RMC>");
                let _ = guard.flush();
            }

            loop {
                let mut guard = match stream_recv.lock() { Ok(g) => g, Err(poison) => poison.into_inner() };

                let msg_opt_res = read_message_from_stream(&mut *guard);

                match msg_opt_res {
                    Ok(Some(msg)) => {
                        let (r, g, b) = { let rguard = state_recv.read().unwrap(); ( rguard.request_colour.red, rguard.request_colour.green, rguard.request_colour.blue ) };

                        match parse_measurement_from_xml(&msg, r, g, b) {
                            Ok(meas) => {
                                let mut w = state_recv.write().unwrap();
                                w.connected = true;

                                if !meas.shapes.is_empty() {
                                    w.current_measure_colour = meas.shapes.get(0).map(|s| match s { ShapeInstruction::Rectangle(r) => r.color }).unwrap_or(ColorRGB::from_components_u16(meas.red, meas.green, meas.blue, 8));
                                    w.shapes = meas.shapes;
                                } else {
                                    w.current_measure_colour = ColorRGB::from_components_u16(meas.red, meas.green, meas.blue, 8);
                                    w.shapes.clear();
                                }
                            }
                            Err(e) => panic!("Failed to parse measurement xml: {}", e),
                        }
                    }

                    Ok(None) => { let mut w = state_recv.write().unwrap(); w.connected = false; thread::sleep(Duration::from_millis(50)); }

                    Err(e) => { eprintln!("Error reading from stream: {}", e); let mut w = state_recv.write().unwrap(); w.connected = false; thread::sleep(Duration::from_millis(50)); }
                }
            }
        }); // end thread::spawn
    } // end if let Some(s)

    Ok(state)
}
