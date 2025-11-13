use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use quick_xml::Error as XmlError;
use quick_xml::Reader;
use quick_xml::events::Event;
use quick_xml::events::{BytesStart, BytesText, Event as XmlEvent};

#[derive(Debug, Clone)]
pub struct MeasurementResult {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub y_lum: Option<f64>,
    pub shapes: Vec<ShapeInstruction>,
}

#[derive(PartialEq, Eq, Clone, Copy, Default, Debug)]
pub struct ColorRGB {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}


impl ColorRGB {
    pub fn from_components(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RectangleGeometry {
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone)]
pub struct RectangleShape {
    pub color: ColorRGB,
    pub geometry: RectangleGeometry,
}

#[derive(Debug, Clone)]
pub enum ShapeInstruction {
    Rectangle(RectangleShape),
}

/// Pretty-print XML for optional logging (module-level so threads can call it).
fn pretty_print_xml(xml: &str) -> Result<String, XmlError> {
    fn format_start_tag(reader: &Reader<&[u8]>, element: &BytesStart) -> Result<String, XmlError> {
        let mut line = String::new();
        line.push('<');
        line.push_str(&String::from_utf8_lossy(element.name().as_ref()));
        for attr in element.attributes().with_checks(false) {
            let attr = attr?;
            let key = String::from_utf8_lossy(attr.key.as_ref());
            let value = attr.decode_and_unescape_value(reader)?;
            line.push(' ');
            line.push_str(&key);
            line.push('=');
            line.push('"');
            line.push_str(&value);
            line.push('"');
        }
        line.push('>');
        Ok(line)
    }

    fn indent_line(buf: &mut String, depth: usize, line: &str) {
        for _ in 0..depth {
            buf.push_str("  ");
        }
        buf.push_str(line);
        buf.push('\n');
    }

    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);
    let mut buffer = Vec::new();
    let mut output = String::new();
    let mut depth: usize = 0;

    loop {
        match reader.read_event_into(&mut buffer)? {
            XmlEvent::Start(e) => {
                let line = format_start_tag(&reader, &e)?;
                indent_line(&mut output, depth, &line);
                depth = depth.saturating_add(1);
            }
            XmlEvent::End(e) => {
                depth = depth.saturating_sub(1);
                let line = format!("</{}>", String::from_utf8_lossy(e.name().as_ref()));
                indent_line(&mut output, depth, &line);
            }
            XmlEvent::Empty(e) => {
                let mut line = String::new();
                line.push('<');
                line.push_str(&String::from_utf8_lossy(e.name().as_ref()));
                for attr in e.attributes().with_checks(false) {
                    let attr = attr?;
                    let key = String::from_utf8_lossy(attr.key.as_ref());
                    let value = attr.decode_and_unescape_value(&reader)?;
                    line.push(' ');
                    line.push_str(&key);
                    line.push('=');
                    line.push('"');
                    line.push_str(&value);
                    line.push('"');
                }
                line.push_str(" />");
                indent_line(&mut output, depth, &line);
            }
            XmlEvent::Text(BytesText { .. }) => {
                let text = reader.decoder().decode(buffer.as_ref()).unwrap_or_default();
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    indent_line(&mut output, depth, trimmed);
                }
            }
            XmlEvent::CData(e) => {
                let data = reader.decoder().decode(e.as_ref()).unwrap_or_default();
                let line = format!("<![CDATA[{}]]>", data.trim());
                indent_line(&mut output, depth, &line);
            }
            XmlEvent::Comment(e) => {
                let comment = reader.decoder().decode(e.as_ref()).unwrap_or_default();
                let line = format!("<!--{}-->", comment.trim());
                indent_line(&mut output, depth, &line);
            }
            XmlEvent::Decl(decl) => {
                let mut line = String::from("<?xml");
                if let Ok(version) = decl.version() {
                    if !version.is_empty() {
                        line.push(' ');
                        line.push_str("version=\"");
                        line.push_str(&String::from_utf8_lossy(version.as_ref()));
                        line.push('\"');
                    }
                }
                if let Some(enc_res) = decl.encoding() {
                    let enc = enc_res?;
                    line.push(' ');
                    line.push_str("encoding=\"");
                    line.push_str(&String::from_utf8_lossy(enc.as_ref()));
                    line.push('\"');
                }
                if let Some(st_res) = decl.standalone() {
                    let st = st_res?;
                    line.push(' ');
                    line.push_str("standalone=\"");
                    line.push_str(&String::from_utf8_lossy(st.as_ref()));
                    line.push('\"');
                }
                line.push_str("?>");
                indent_line(&mut output, depth, &line);
            }
            XmlEvent::PI(e) => {
                let data = reader.decoder().decode(e.as_ref()).unwrap_or_default();
                let line = format!("<?{}?>", data.trim());
                indent_line(&mut output, depth, &line);
            }
            XmlEvent::Eof => break,
            _ => {}
        }
        buffer.clear();
    }

    Ok(output)
}

/// Log received XML to `COLOURSPACE_XML_LOG` (or `colourspace_commands.log`).
fn log_received_xml(xml: &str) -> std::io::Result<()> {
    let pretty = match pretty_print_xml(xml) {
        Ok(pretty) => pretty,
        Err(err) => {
            eprintln!("Failed to pretty print XML: {}", err);
            xml.to_string()
        }
    };

    let log_path = std::env::var("COLOURSPACE_XML_LOG")
        .unwrap_or_else(|_| "colourspace_commands.log".to_string());
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    writeln!(file, "----- Received at {:.3} -----", timestamp)?;
    file.write_all(pretty.as_bytes())?;
    if !pretty.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    writeln!(file, "----- End -----")?;
    Ok(())
}

/// Shared state between drawing and network threads. The drawing (main) thread
/// and the receiving thread must share the same status via an RwLock as
/// requested. The sending thread will read the `request_color` periodically
/// and write it to the TCP stream.
pub struct SharedState {
    pub connected: bool,
    pub shapes: Vec<ShapeInstruction>,
    pub current_measure_colour: ColorRGB,
    pub request_colour: ColorRGB,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            connected: false,
            shapes: Vec::new(),
            current_measure_colour: ColorRGB::default(),
            request_colour: ColorRGB::default(),
        }
    }
}

fn send_xml_on_stream(stream: &mut TcpStream, xml: &str) -> std::io::Result<()> {
    let count = xml.as_bytes().len();
    if count > i32::MAX as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "payload too large for 4-byte header",
        ));
    }
    let header = (count as i32).to_be_bytes();
    stream.write_all(&header)?;
    stream.write_all(xml.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn read_message_from_stream(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    let signed_len = i32::from_be_bytes(header);
    if signed_len < 0 {
        return Ok(None);
    }
    let len = signed_len as usize;
    if len == 0 {
        return Ok(Some(String::new()));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    String::from_utf8(payload)
        .map(Some)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid utf8 payload"))
}

/// Parse XML string into a MeasurementResult. The `r,g,b` parameters are the
/// requested components that will be used as fallback initial values in the
/// result (keeps previous behavior).
fn parse_measurement_from_xml(xml: &str, r: u8, g: u8, b: u8) -> Result<MeasurementResult, String> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);
    let mut buf = Vec::new();
    let mut in_result = false;
    let mut cur_elem = String::new();
    let mut res = MeasurementResult {
        red: r,
        green: g,
        blue: b,
        x: None,
        y: None,
        y_lum: None,
        shapes: Vec::new(),
    };
    let mut element_stack: Vec<String> = Vec::new();
    let mut reported_commands: HashSet<String> = HashSet::new();
    let mut parsed_shapes: Vec<ShapeInstruction> = Vec::new();
    #[derive(Default)]
    struct RectangleBuilder {
        color: Option<ColorRGB>,
        width: Option<f32>,
        height: Option<f32>,
    }
    impl RectangleBuilder {
        fn build(self) -> Option<RectangleShape> {
            let color = self.color?;
            let width = self.width.unwrap_or(1.0);
            let height = self.height.unwrap_or(1.0);
            Some(RectangleShape {
                color,
                geometry: RectangleGeometry { width, height },
            })
        }
    }
    let mut rect_builder: Option<RectangleBuilder> = None;
    let apply_color =
        |reader: &Reader<&[u8]>, element: &BytesStart, builder: &mut RectangleBuilder| {
            let mut colour = builder.color.unwrap_or_default();
            let mut updated = false;
            for attr in element.attributes().with_checks(false) {
                if let Ok(attr) = attr {
                    if let Ok(value) = attr.decode_and_unescape_value(reader) {
                        match attr.key.as_ref() {
                            b"red" => {
                                if let Ok(v) = value.parse::<u8>() {
                                    colour.red = v;
                                    updated = true;
                                }
                            }
                            b"green" => {
                                if let Ok(v) = value.parse::<u8>() {
                                    colour.green = v;
                                    updated = true;
                                }
                            }
                            b"blue" => {
                                if let Ok(v) = value.parse::<u8>() {
                                    colour.blue = v;
                                    updated = true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            if updated {
                builder.color = Some(colour);
            }
        };
    let apply_geometry =
        |reader: &Reader<&[u8]>, element: &BytesStart, builder: &mut RectangleBuilder| {
            for attr in element.attributes().with_checks(false) {
                if let Ok(attr) = attr {
                    if let Ok(value) = attr.decode_and_unescape_value(reader) {
                        match attr.key.as_ref() {
                            b"cx" => {
                                if let Ok(v) = value.parse::<f32>() {
                                    builder.width = Some(v);
                                }
                            }
                            b"cy" => {
                                if let Ok(v) = value.parse::<f32>() {
                                    builder.height = Some(v);
                                }
                            }
                            b"x" => {
                                if builder.width.is_none() {
                                    if let Ok(v) = value.parse::<f32>() {
                                        builder.width = Some(v);
                                    }
                                }
                            }
                            b"y" => {
                                if builder.height.is_none() {
                                    if let Ok(v) = value.parse::<f32>() {
                                        builder.height = Some(v);
                                    }
                                }
                            }
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
                if name == "result" {
                    in_result = true;
                }
                if name == "rectangle" {
                    rect_builder = Some(RectangleBuilder::default());
                } else if name == "color" {
                    if let Some(builder) = rect_builder.as_mut() {
                        apply_color(&reader, &e, builder);
                    }
                } else if name == "geometry" {
                    if let Some(builder) = rect_builder.as_mut() {
                        apply_geometry(&reader, &e, builder);
                    }
                }
            }
            Ok(Event::End(e)) => {
                if let Ok(end_name) = std::str::from_utf8(e.name().as_ref()) {
                    if end_name == "result" {
                        in_result = false;
                    }
                    if end_name == "rectangle" {
                        if let Some(builder) = rect_builder.take() {
                            if let Some(rect) = builder.build() {
                                parsed_shapes.push(ShapeInstruction::Rectangle(rect));
                            } else {
                                panic!("Received rectangle command missing required attributes");
                            }
                        }
                    }
                }
                element_stack.pop();
            }
            Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "color" {
                    if let Some(builder) = rect_builder.as_mut() {
                        apply_color(&reader, &e, builder);
                    }
                } else if name == "geometry" {
                    if let Some(builder) = rect_builder.as_mut() {
                        apply_geometry(&reader, &e, builder);
                    }
                }
            }
            Ok(Event::Text(e)) => {
                let raw_txt = e.unescape().unwrap_or_default().into_owned();
                let txt_trimmed = raw_txt.trim();
                if txt_trimmed.is_empty() {
                    continue;
                }
                if let Some(command) = element_stack.get(1) {
                    if let Some(param) = element_stack.last() {
                        if command != param {
                            println!("  {} = {}", param, txt_trimmed);
                        }
                    }
                }
                if !in_result {
                    continue;
                }
                match cur_elem.as_str() {
                    "red" => {
                        if let Ok(v) = txt_trimmed.parse::<u8>() {
                            res.red = v
                        }
                    }
                    "green" => {
                        if let Ok(v) = txt_trimmed.parse::<u8>() {
                            res.green = v
                        }
                    }
                    "blue" => {
                        if let Ok(v) = txt_trimmed.parse::<u8>() {
                            res.blue = v
                        }
                    }
                    "x" => {
                        if let Ok(v) = txt_trimmed.parse::<f64>() {
                            res.x = Some(v)
                        }
                    }
                    "y" => {
                        if let Ok(v) = txt_trimmed.parse::<f64>() {
                            res.y = Some(v)
                        }
                    }
                    "Y" => {
                        if let Ok(v) = txt_trimmed.parse::<f64>() {
                            res.y_lum = Some(v)
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(format!("xml parse error: {}", e));
            }
            _ => {}
        }
        buf.clear();
    }
    if let Some(builder) = rect_builder {
        if let Some(rect) = builder.build() {
            parsed_shapes.push(ShapeInstruction::Rectangle(rect));
        } else {
            panic!("Received rectangle command missing required attributes");
        }
    }
    res.shapes = parsed_shapes;
    Ok(res)
}

/// Spawn a background worker thread that keeps a connection and performs measurements.
/// Returns (request_sender, response_receiver).
/// Spawn two background threads: a receiving thread that blocks on
/// `read_exact` (uses `read_message_from_stream`) and updates the shared state
/// under an `RwLock`, and a sending thread that periodically reads the
/// requested colour from the shared state and writes measurement requests to
/// the stream. Returns an `Arc<RwLock<SharedState>>` that the caller (drawing
/// thread) can use to read the current shapes and measured colour.
pub fn spawn_worker(addr: &str, pretty_print: bool) -> std::io::Result<Arc<RwLock<SharedState>>> {
    let addr = addr.to_owned();
    // Try to connect immediately; if connection fails, still return state but
    // mark it as disconnected.
    let stream_res = TcpStream::connect(&addr);
    let stream = match stream_res {
        Ok(s) => {
            // Keep the stream in blocking mode so the receiving thread will
            // block on `read_exact` until data arrives. Timeouts cause
            // spurious `Err` results when the server is idle, which would
            // incorrectly set `connected = false` and make the screen go
            // black. Don't set read/write timeouts here.
            Some(s)
        }
        Err(e) => {
            eprintln!("Failed to connect to {}: {}", addr, e);
            None
        }
    };

    let state = Arc::new(RwLock::new(SharedState::default()));

    if let Some(s) = stream {
        let stream_arc = Arc::new(Mutex::new(s));
        let state_recv = state.clone();
        let stream_recv = stream_arc.clone();
        // Receiving thread: blocks on read_exact as requested
        thread::spawn(move || {
            // initialize profile
            if let Ok(mut guard) = stream_recv.lock() {
                let _ = send_xml_on_stream(
                    &mut *guard,
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<CS_RMC version=1>\n<command>\ninit profile\n</command>\n</CS_RMC>",
                );
            }
            loop {
                let mut guard = match stream_recv.lock() {
                    Ok(g) => g,
                    Err(poison) => poison.into_inner(),
                };
                let msg_opt_res = read_message_from_stream(&mut *guard);
                match msg_opt_res {
                    Ok(opt) => match opt {
                        Some(msg) => {
                            // Pretty-printing/logging if requested by CLI flag
                            if pretty_print {
                                let _ = log_received_xml(&msg);
                            }

                            // parse measurement
                            // we don't know the exact request colour here; use defaults from state
                            let (r, g, b) = {
                                let rguard = state_recv.read().unwrap();
                                (
                                    rguard.request_colour.red,
                                    rguard.request_colour.green,
                                    rguard.request_colour.blue,
                                )
                            };
                            match parse_measurement_from_xml(&msg, r, g, b) {
                                Ok(meas) => {
                                    let mut w = state_recv.write().unwrap();
                                    w.connected = true;
                                    if !meas.shapes.is_empty() {
                                        // choose shape colour logic is done in drawing side; we just store shapes
                                        w.current_measure_colour = meas
                                            .shapes
                                            .get(0)
                                            .map(|s| match s {
                                                ShapeInstruction::Rectangle(r) => r.color,
                                            })
                                            .unwrap_or(ColorRGB::from_components(
                                                meas.red, meas.green, meas.blue,
                                            ));
                                        w.shapes = meas.shapes;
                                    } else {
                                        w.current_measure_colour = ColorRGB::from_components(
                                            meas.red, meas.green, meas.blue,
                                        );
                                        w.shapes.clear();
                                    }
                                }
                                Err(e) => {
                                    panic!("Failed to parse measurement xml: {}", e);
                                }
                            }
                        }
                        None => {
                            // remote signalled end of communication (negative header)
                            let mut w = state_recv.write().unwrap();
                            w.connected = false;
                            // don't break; allow retrying
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    },
                    Err(e) => {
                        eprintln!("Error reading from stream: {}", e);
                        let mut w = state_recv.write().unwrap();
                        w.connected = false;
                        // transient read error: sleep briefly and retry
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        });

        // Sending thread: periodically sends measurement requests based on the
        // `request_colour` in `state`. It runs independently so it won't stall
        // drawing.
        let state_send = state.clone();
        let stream_send = stream_arc.clone();
        thread::spawn(move || {
            let measurement_interval = Duration::from_secs(1);
            loop {
                // read desired request colour
                let (r, g, b, connected) = {
                    let rguard = state_send.read().unwrap();
                    (
                        rguard.request_colour.red,
                        rguard.request_colour.green,
                        rguard.request_colour.blue,
                        rguard.connected,
                    )
                };
                if !connected {
                    // even if not connected, try to send once to establish state
                }
                // No queued commands — only send the current `request_colour`.

                // compose measurement xml
                let xml = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<CS_RMC version=1>\n<measurement>\n<red>{}</red>\n<green>{}</green>\n<blue>{}</blue>\n</measurement>\n</CS_RMC>",
                    r, g, b
                );
                let send_res = {
                    let mut guard = match stream_send.lock() {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    send_xml_on_stream(&mut *guard, &xml)
                };
                if let Err(e) = send_res {
                    eprintln!("Failed to send measurement request: {}", e);
                    let mut w = state_send.write().unwrap();
                    w.connected = false;
                    break;
                } else {
                    let mut w = state_send.write().unwrap();
                    w.connected = true;
                }
                thread::sleep(measurement_interval);
            }
        });
    }

    Ok(state)
}
