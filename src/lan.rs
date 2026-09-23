use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
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
    /// Bit depth announced by a `<bits>`/`<depth>` element in a `<result>`, if any.
    pub depth_bits: Option<u8>,
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
    let mut res = MeasurementResult { red: r, green: g, blue: b, x: None, y: None, y_lum: None, depth_bits: None, shapes: Vec::new() };
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
                        return Err(format!("received command '{}' more than once in one message", command));
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
                            else { return Err("received rectangle command missing required attributes".to_string()); }
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
                    "bits" | "depth" | "bitDepth" => { if let Ok(v) = txt_trimmed.parse::<u8>() { if (1..=16).contains(&v) { res.depth_bits = Some(v); } } }
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
        else { return Err("received rectangle command missing required attributes".to_string()); }
    }

    res.shapes = parsed_shapes;

    // Debug output for received command: prefer the first parsed shape's color if available
    let (bit_depth, r_val, g_val, b_val) = if let Some(shape) = res.shapes.get(0) {
        match shape { ShapeInstruction::Rectangle(rsh) => ( rsh.color.depth_bits, rsh.color.red, rsh.color.green, rsh.color.blue ) }
    } else { (8u8, res.red, res.green, res.blue) };

    println!("Bit depth = {} , R = {} , G = {} , B = {}", bit_depth, r_val, g_val, b_val);

    Ok(res)
}

/// Largest message we accept. Real ColourSpace messages are a few hundred bytes; the cap only
/// stops a corrupt or hostile length header from making us allocate up to 2 GiB.
const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

/// Read a length-prefixed message.
/// Header is a 4-byte big-endian signed i32. Negative means disconnect.
/// Returns Ok(Some(string)) for a payload, Ok(None) for negative header, Err on io.
fn read_message_from_stream(stream: &mut impl Read) -> std::io::Result<Option<String>> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    let signed_len = i32::from_be_bytes(header);
    if signed_len < 0 { return Ok(None); }
    let len = signed_len as usize;
    if len == 0 { return Ok(Some(String::new())); }
    if len > MAX_MESSAGE_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("message length {} exceeds the {} byte limit", len, MAX_MESSAGE_LEN),
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    String::from_utf8(payload).map(Some).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid utf8 payload"))
}

/// Connect to an address string like "192.168.168.11:20002" with a timeout.
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

/// Turn on TCP keepalive so a ColourSpace PC that vanishes from the network (power loss,
/// cable pulled) is noticed within about half a minute instead of never: ColourSpace only
/// talks when it wants a patch, so silence on the socket is normal and cannot be used.
fn configure_socket(stream: &TcpStream) {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(10))
        .with_interval(Duration::from_secs(5));
    #[cfg(target_os = "linux")]
    let keepalive = keepalive.with_retries(3);
    if let Err(e) = socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive) {
        eprintln!("Could not enable TCP keepalive: {}", e);
    }
}

/// Shared state between drawing and network threads.
pub struct SharedState {
    pub connected: bool,
    pub shapes: Vec<ShapeInstruction>,
    pub current_measure_colour: ColorRGB,
    pub request_colour: ColorRGB,
    /// Bit depth (per channel) of the most recent patch; `None` until the first one arrives.
    pub patch_bits: Option<u8>,
}

impl Default for SharedState {
    fn default() -> Self { Self { connected: false, shapes: Vec::new(), current_measure_colour: ColorRGB::default(), request_colour: ColorRGB::default(), patch_bits: None } }
}

/// Lock helpers that survive a poisoned lock: the state is plain data, so the last written
/// values are still usable, and one panicking thread must not take the drawing loop down.
pub fn read_state(state: &RwLock<SharedState>) -> RwLockReadGuard<'_, SharedState> {
    state.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_state(state: &RwLock<SharedState>) -> RwLockWriteGuard<'_, SharedState> {
    state.write().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn set_connected(state: &RwLock<SharedState>, connected: bool) {
    write_state(state).connected = connected;
}

/// Marks the link as down when the worker thread ends for any reason (including a panic),
/// so the display never keeps claiming to be connected to a dead worker.
struct DisconnectOnExit(Arc<RwLock<SharedState>>);

impl Drop for DisconnectOnExit {
    fn drop(&mut self) { set_connected(&self.0, false); }
}

/// Guess the bit depth of plain `<red>/<green>/<blue>` values that arrive without any depth
/// information: anything above 255 cannot be 8-bit.
fn infer_depth_bits(r: u16, g: u16, b: u16) -> u8 {
    match r.max(g).max(b) {
        0..=255 => 8,
        256..=1023 => 10,
        1024..=4095 => 12,
        _ => 16,
    }
}

/// Publish a parsed message to the drawing thread.
fn apply_measurement(state: &RwLock<SharedState>, meas: MeasurementResult) {
    let first_colour = match meas.shapes.first() {
        Some(ShapeInstruction::Rectangle(rect)) => Some(rect.color),
        None => None,
    };
    // The depth the patches are in (a colour without a depth counts as 8-bit, as everywhere else).
    let shapes_bits = meas
        .shapes
        .iter()
        .map(|ShapeInstruction::Rectangle(rect)| if rect.color.depth_bits == 0 { 8 } else { rect.color.depth_bits })
        .max();
    let mut w = write_state(state);
    w.connected = true;
    match first_colour {
        Some(colour) => {
            w.current_measure_colour = colour;
            w.shapes = meas.shapes;
            w.patch_bits = shapes_bits;
        }
        None => {
            let bits = meas.depth_bits.unwrap_or_else(|| infer_depth_bits(meas.red, meas.green, meas.blue));
            w.current_measure_colour = ColorRGB::from_components_u16(meas.red, meas.green, meas.blue, bits);
            w.shapes.clear();
            w.patch_bits = Some(bits);
        }
    }
}

const INIT_PROFILE: &[u8] = b"<?xml version=\"1.0\" encoding=\"UTF-8\" ?><CS_RMC version=1><command>init profile</command></CS_RMC>";

/// Run one connection until it fails. Returns a human-readable reason.
fn run_session(mut stream: TcpStream, state: &RwLock<SharedState>) -> String {
    // One-off mandatory handshake, repeated on every (re)connection.
    if let Err(e) = stream.write_all(INIT_PROFILE) {
        return format!("handshake failed: {}", e);
    }
    if let Err(e) = stream.flush() {
        return format!("handshake failed: {}", e);
    }

    let mut first_message = true;
    loop {
        let msg = match read_message_from_stream(&mut stream) {
            Ok(Some(msg)) => msg,
            Ok(None) => return "ColourSpace ended the session".to_string(),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return "ColourSpace closed the connection".to_string(),
            Err(e) => return format!("read error: {}", e),
        };
        if first_message {
            eprintln!("First message received from ColourSpace");
            first_message = false;
        }

        let (r, g, b) = {
            let rguard = read_state(state);
            (rguard.request_colour.red, rguard.request_colour.green, rguard.request_colour.blue)
        };

        // A bad message is reported and skipped; it must never stop the receiver.
        match parse_measurement_from_xml(&msg, r, g, b) {
            Ok(meas) => apply_measurement(state, meas),
            Err(e) => eprintln!("Ignoring unusable message from ColourSpace: {}", e),
        }
    }
}

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(5);
const RECONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

/// Owns the connection for the lifetime of the program: runs sessions and, when one ends,
/// keeps trying to reconnect (with a growing pause) instead of spinning on a dead socket.
fn worker_loop(addr: String, first: TcpStream, state: Arc<RwLock<SharedState>>) {
    let _mark_down_on_exit = DisconnectOnExit(state.clone());
    let mut next = Some(first);
    let mut delay = RECONNECT_MIN;

    loop {
        let stream = match next.take() {
            Some(stream) => stream,
            None => {
                thread::sleep(delay);
                match connect_with_timeout(&addr, RECONNECT_ATTEMPT_TIMEOUT) {
                    Ok(stream) => {
                        configure_socket(&stream);
                        eprintln!("Reconnected to {}", addr);
                        stream
                    }
                    Err(_) => {
                        delay = (delay * 2).min(RECONNECT_MAX);
                        continue;
                    }
                }
            }
        };
        delay = RECONNECT_MIN;

        set_connected(&state, true);
        let reason = run_session(stream, &state);
        set_connected(&state, false);
        eprintln!("Connection to {} lost ({}); trying to reconnect...", addr, reason);
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Connect to ColourSpace and spawn a background worker thread that keeps the connection
/// (reconnecting if it drops) and receives patch instructions.
///
/// Fails immediately if the first TCP connection cannot be made, so the caller can tell the
/// user at once. `connected` in the returned state is true from the moment the TCP link is up;
/// ColourSpace only sends its first patch when it is ready, which can take several seconds.
pub fn spawn_worker(addr: &str, _pretty_print: bool) -> std::io::Result<Arc<RwLock<SharedState>>> {
    let addr = addr.to_owned();
    let stream = connect_with_timeout(&addr, CONNECT_TIMEOUT)?;
    configure_socket(&stream);

    let state = Arc::new(RwLock::new(SharedState::default()));
    set_connected(&state, true);

    let worker_state = state.clone();
    thread::spawn(move || worker_loop(addr, stream, worker_state));

    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;
    use std::time::Instant;

    const RECT: &str = r#"<CS_RMC version="1"><rectangle><color red="512" green="256" blue="1023" bits="10"/><geometry cx="0.5" cy="0.25"/></rectangle></CS_RMC>"#;

    fn frame(xml: &str) -> Vec<u8> {
        let mut v = (xml.len() as i32).to_be_bytes().to_vec();
        v.extend_from_slice(xml.as_bytes());
        v
    }

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if cond() { return; }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for: {what}");
    }

    fn accept_with_timeout(listener: &TcpListener) -> TcpStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match listener.accept() {
                Ok((s, _)) => { s.set_nonblocking(false).unwrap(); return s; }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Err(e) => panic!("accept failed: {e}"),
            }
        }
    }

    // ---- parser -----------------------------------------------------------------

    #[test]
    fn rectangle_message_keeps_bit_depth_and_geometry() {
        let m = parse_measurement_from_xml(RECT, 0, 0, 0).unwrap();
        let ShapeInstruction::Rectangle(r) = &m.shapes[0];
        assert_eq!((r.color.red, r.color.green, r.color.blue, r.color.depth_bits), (512, 256, 1023, 10));
        assert_eq!((r.geometry.width, r.geometry.height), (0.5, 0.25));
    }

    #[test]
    fn duplicate_command_is_an_error_not_a_panic() {
        let xml = r#"<CS_RMC><rectangle><color red="1" green="1" blue="1"/></rectangle><rectangle><color red="2" green="2" blue="2"/></rectangle></CS_RMC>"#;
        let err = parse_measurement_from_xml(xml, 0, 0, 0).unwrap_err();
        assert!(err.contains("rectangle"), "{err}");
    }

    #[test]
    fn rectangle_without_colour_is_an_error_not_a_panic() {
        let xml = r#"<CS_RMC><rectangle><geometry cx="1" cy="1"/></rectangle></CS_RMC>"#;
        assert!(parse_measurement_from_xml(xml, 0, 0, 0).is_err());
    }

    #[test]
    fn malformed_xml_is_an_error_not_a_panic() {
        assert!(parse_measurement_from_xml("<CS_RMC><a></b></CS_RMC>", 0, 0, 0).is_err());
    }

    #[test]
    fn plain_result_uses_announced_or_inferred_depth() {
        let xml = r#"<CS_RMC><result><red>1023</red><green>0</green><blue>512</blue><bits>10</bits></result></CS_RMC>"#;
        let m = parse_measurement_from_xml(xml, 0, 0, 0).unwrap();
        assert_eq!((m.red, m.green, m.blue, m.depth_bits), (1023, 0, 512, Some(10)));

        assert_eq!(infer_depth_bits(255, 0, 0), 8);
        assert_eq!(infer_depth_bits(0, 1023, 0), 10);
        assert_eq!(infer_depth_bits(0, 0, 4095), 12);
        assert_eq!(infer_depth_bits(65535, 0, 0), 16);
    }

    // ---- framing ----------------------------------------------------------------

    #[test]
    fn framing_limits_and_disconnect() {
        assert_eq!(read_message_from_stream(&mut Cursor::new(frame("hi"))).unwrap(), Some("hi".to_string()));
        assert_eq!(read_message_from_stream(&mut Cursor::new((-1i32).to_be_bytes())).unwrap(), None);
        let huge = (i32::MAX).to_be_bytes();
        let err = read_message_from_stream(&mut Cursor::new(huge)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let mut truncated = frame("hello");
        truncated.truncate(6);
        assert!(read_message_from_stream(&mut Cursor::new(truncated)).is_err());
    }

    // ---- worker -----------------------------------------------------------------

    #[test]
    fn unreachable_address_fails_immediately() {
        let addr = { let l = TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap() }; // closed again
        let started = Instant::now();
        assert!(spawn_worker(&addr.to_string(), false).is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn worker_handshakes_receives_survives_bad_messages_and_reconnects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let state = spawn_worker(&addr, false).unwrap();
        assert!(read_state(&state).connected, "connected as soon as TCP is up");

        // --- first connection: handshake, a bad message, then a good one ---
        let mut conn = accept_with_timeout(&listener);
        conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 256];
        let n = conn.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("init profile"));

        conn.write_all(&frame("<CS_RMC><a></b></CS_RMC>")).unwrap(); // must not kill the worker
        conn.write_all(&frame(RECT)).unwrap();
        wait_until("first patch", || read_state(&state).shapes.len() == 1);
        assert_eq!(read_state(&state).current_measure_colour.red, 512);
        assert_eq!(read_state(&state).patch_bits, Some(10), "the depth of the latest patch is published");

        // --- ColourSpace drops the connection; the worker must come back by itself ---
        drop(conn);
        let mut conn2 = accept_with_timeout(&listener);
        conn2.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let n = conn2.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("init profile"), "handshake repeated");

        let second = RECT.replace("512", "100").replace("bits=\"10\"", "bits=\"8\"");
        conn2.write_all(&frame(&second)).unwrap();
        wait_until("patch after reconnect", || read_state(&state).current_measure_colour.red == 100);
        assert_eq!(read_state(&state).patch_bits, Some(8), "and it follows a change of depth");
        assert!(read_state(&state).connected);
    }
}
