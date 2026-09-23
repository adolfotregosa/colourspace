//! Startup window: ColourSpace IP, an "Enable HDR" checkbox and dropdown menus for the
//! HDR options (signal, primaries and the static metadata values).
//!
//! `tinyfiledialogs` can only show a single text field, so this is a small self-contained
//! SDL window drawn with an embedded 8x8 bitmap font (no SDL_ttf needed).
//!
//! The window owns its *own* short-lived SDL context. It is dropped before `main` starts
//! SDL again, which lets `main` pick the video driver (Wayland is required for HDR) based
//! on what was chosen here.

use font8x8::{BASIC_FONTS, UnicodeFonts};
use sdl2::event::Event;
use sdl2::keyboard::{Keycode, Mod};
use sdl2::mouse::MouseButton;
use sdl2::pixels::Color;
use sdl2::rect::{Point, Rect};
use sdl2::render::{Canvas, RenderTarget};

use crate::hdr::{HdrMode, Primaries};

/// Everything the startup window collects (and is pre-filled from the command line).
#[derive(Debug, Clone)]
pub struct Settings {
    pub remote: String,
    pub hdr: HdrMode,
    pub primaries: Primaries,
    pub max_luminance: f32,
    pub min_luminance: f32,
    pub max_cll: f32,
    pub max_fall: f32,
}

// -------------------------------------------------------------------------------------
// Layout
// -------------------------------------------------------------------------------------

const W: i32 = 580;
const H: i32 = 520;
const SCALE: i32 = 2; // 8x8 glyphs drawn at 16x16
const GLYPH: i32 = 8 * SCALE;
const LABEL_X: i32 = 24;
const CTRL_X: i32 = 270;
const CTRL_W: i32 = 286;
const ROW_H: i32 = 32;

const IP_Y: i32 = 64;
const CHECK_Y: i32 = 112;
const DD_Y0: i32 = 160;
const DD_STEP: i32 = 44;
const BTN_Y: i32 = 440;
const BTN_W: i32 = 136;
const BTN_H: i32 = 38;

// Focus order (Tab cycles through these).
const F_IP: usize = 0;
const F_CHECK: usize = 1;
const F_DD0: usize = 2; // dropdowns are F_DD0 + 0..6
const DD_COUNT: usize = 6;
const F_CONNECT: usize = F_DD0 + DD_COUNT;
const F_CANCEL: usize = F_CONNECT + 1;
const F_COUNT: usize = F_CANCEL + 1;

// Dropdown indices
const DD_SIGNAL: usize = 0;
const DD_PRIMARIES: usize = 1;
const DD_MAX_LUM: usize = 2;
const DD_MIN_LUM: usize = 3;
const DD_MAX_CLL: usize = 4;
const DD_MAX_FALL: usize = 5;

fn ip_rect() -> Rect {
    Rect::new(CTRL_X, IP_Y, CTRL_W as u32, ROW_H as u32)
}
fn check_rect() -> Rect {
    Rect::new(CTRL_X, CHECK_Y + 4, 24, 24)
}
/// Clickable area for the checkbox: the box plus its text.
fn check_hit_rect() -> Rect {
    Rect::new(CTRL_X, CHECK_Y, CTRL_W as u32, ROW_H as u32)
}
fn dd_rect(i: usize) -> Rect {
    Rect::new(CTRL_X, DD_Y0 + i as i32 * DD_STEP, CTRL_W as u32, ROW_H as u32)
}
fn connect_rect() -> Rect {
    Rect::new(CTRL_X + BTN_W + 14, BTN_Y, BTN_W as u32, BTN_H as u32)
}
fn cancel_rect() -> Rect {
    Rect::new(CTRL_X, BTN_Y, BTN_W as u32, BTN_H as u32)
}

// -------------------------------------------------------------------------------------
// Colours
// -------------------------------------------------------------------------------------

const BG: Color = Color::RGB(30, 34, 39);
const FIELD: Color = Color::RGB(45, 50, 56);
const BORDER: Color = Color::RGB(90, 98, 108);
const FOCUS: Color = Color::RGB(80, 160, 255);
const ACCENT: Color = Color::RGB(46, 125, 225);
const TEXT: Color = Color::RGB(230, 232, 235);
const MUTED: Color = Color::RGB(110, 116, 124);
const ERROR: Color = Color::RGB(224, 80, 80);
const HOVER: Color = Color::RGB(66, 74, 84);

// -------------------------------------------------------------------------------------
// Form state
// -------------------------------------------------------------------------------------

struct Dropdown {
    label: &'static str,
    items: Vec<String>,
    /// Numeric value behind each item (unused for the signal / primaries menus).
    values: Vec<f32>,
    selected: usize,
    open: bool,
}

impl Dropdown {
    fn enumerated(label: &'static str, items: &[&str], selected: usize) -> Self {
        Self {
            label,
            items: items.iter().map(|s| s.to_string()).collect(),
            values: Vec::new(),
            selected,
            open: false,
        }
    }

    /// A menu of preset numbers. If `current` (e.g. from the command line) is not one of the
    /// presets it is added, so the menu always shows what will actually be used.
    fn numeric(label: &'static str, presets: &[f32], current: f32, zero_label: Option<&str>) -> Self {
        let current = if current.is_finite() && current >= 0.0 { current } else { presets[0] };
        let mut values = presets.to_vec();
        if !values.iter().any(|v| (v - current).abs() < 1e-9) {
            values.push(current);
            values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        }
        let selected = values.iter().position(|v| (v - current).abs() < 1e-9).unwrap_or(0);
        let items = values
            .iter()
            .map(|v| match zero_label {
                Some(z) if *v == 0.0 => z.to_string(),
                _ => format!("{} cd/m2", v),
            })
            .collect();
        Self { label, items, values, selected, open: false }
    }

    fn value(&self) -> f32 {
        self.values.get(self.selected).copied().unwrap_or(0.0)
    }

    fn step(&mut self, delta: i32) {
        let n = self.items.len() as i32;
        self.selected = (self.selected as i32 + delta).clamp(0, n - 1) as usize;
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Submit,
    Cancel,
    Paste,
}

struct Form {
    ip: String,
    hdr_on: bool,
    dd: Vec<Dropdown>,
    focus: usize,
    ip_error: bool,
    mouse: Point,
}

impl Form {
    fn new(defaults: &Settings) -> Self {
        let dd = vec![
            Dropdown::enumerated(
                "Signal",
                &["HDR10 (PQ)", "HLG"],
                if defaults.hdr == HdrMode::Hlg { 1 } else { 0 },
            ),
            Dropdown::enumerated(
                "Primaries",
                &["BT.2020", "P3-D65"],
                if defaults.primaries == Primaries::P3D65 { 1 } else { 0 },
            ),
            Dropdown::numeric(
                "Max luminance",
                &[100.0, 400.0, 600.0, 1000.0, 2000.0, 4000.0, 10000.0],
                defaults.max_luminance,
                None,
            ),
            Dropdown::numeric(
                "Min luminance",
                &[0.0, 0.0001, 0.001, 0.005, 0.01, 0.05],
                defaults.min_luminance,
                None,
            ),
            Dropdown::numeric(
                "MaxCLL",
                &[0.0, 400.0, 600.0, 1000.0, 2000.0, 4000.0],
                defaults.max_cll,
                Some("Unspecified"),
            ),
            Dropdown::numeric(
                "MaxFALL",
                &[0.0, 100.0, 400.0, 600.0, 1000.0],
                defaults.max_fall,
                Some("Unspecified"),
            ),
        ];
        Self {
            ip: defaults.remote.clone(),
            hdr_on: defaults.hdr != HdrMode::Sdr,
            dd,
            focus: F_IP,
            ip_error: false,
            mouse: Point::new(-1, -1),
        }
    }

    fn to_settings(&self) -> Settings {
        Settings {
            remote: self.ip.trim().to_string(),
            hdr: if !self.hdr_on {
                HdrMode::Sdr
            } else if self.dd[DD_SIGNAL].selected == 1 {
                HdrMode::Hlg
            } else {
                HdrMode::Hdr10
            },
            primaries: if self.dd[DD_PRIMARIES].selected == 1 { Primaries::P3D65 } else { Primaries::Bt2020 },
            max_luminance: self.dd[DD_MAX_LUM].value(),
            min_luminance: self.dd[DD_MIN_LUM].value(),
            max_cll: self.dd[DD_MAX_CLL].value(),
            max_fall: self.dd[DD_MAX_FALL].value(),
        }
    }

    fn dd_enabled(&self) -> bool {
        self.hdr_on
    }

    fn close_all(&mut self) {
        for d in &mut self.dd {
            d.open = false;
        }
    }

    fn open_dropdown(&self) -> Option<usize> {
        self.dd.iter().position(|d| d.open)
    }

    /// Rectangle of the open list; opens upwards when there is no room below.
    fn list_rect(&self, i: usize) -> Rect {
        let f = dd_rect(i);
        let h = self.dd[i].items.len() as i32 * ROW_H;
        if f.bottom() + h <= H {
            Rect::new(f.x(), f.bottom(), CTRL_W as u32, h as u32)
        } else {
            Rect::new(f.x(), f.y() - h, CTRL_W as u32, h as u32)
        }
    }

    fn item_rect(&self, i: usize, k: usize) -> Rect {
        let l = self.list_rect(i);
        Rect::new(l.x(), l.y() + k as i32 * ROW_H, CTRL_W as u32, ROW_H as u32)
    }

    fn focusable(&self, f: usize) -> bool {
        !(F_DD0..F_DD0 + DD_COUNT).contains(&f) || self.dd_enabled()
    }

    fn move_focus(&mut self, dir: i32) {
        self.close_all();
        for _ in 0..F_COUNT {
            self.focus = ((self.focus as i32 + dir).rem_euclid(F_COUNT as i32)) as usize;
            if self.focusable(self.focus) {
                break;
            }
        }
    }

    pub fn insert_text(&mut self, text: &str) {
        for ch in text.chars() {
            let ok = ch.is_ascii_alphanumeric() || matches!(ch, '.' | ':' | '-' | '_' | '[' | ']');
            if ok && self.ip.len() < 64 {
                self.ip.push(ch);
                self.ip_error = false;
            }
        }
    }

    fn submit(&mut self) -> Action {
        if self.ip.trim().is_empty() {
            self.ip_error = true;
            self.focus = F_IP;
            self.close_all();
            Action::None
        } else {
            Action::Submit
        }
    }

    fn activate(&mut self, f: usize) -> Action {
        match f {
            F_CONNECT => self.submit(),
            F_CANCEL => Action::Cancel,
            _ => Action::None,
        }
    }

    fn handle_event(&mut self, event: &Event) -> Action {
        match event {
            Event::Quit { .. } => Action::Cancel,
            Event::TextInput { text, .. } => {
                if self.focus == F_IP {
                    self.insert_text(text);
                }
                Action::None
            }
            Event::MouseMotion { x, y, .. } => {
                self.mouse = Point::new(*x, *y);
                Action::None
            }
            Event::MouseButtonDown { mouse_btn: MouseButton::Left, x, y, .. } => {
                self.mouse = Point::new(*x, *y);
                self.handle_click(self.mouse)
            }
            Event::KeyDown { keycode: Some(key), keymod, .. } => self.handle_key(*key, *keymod),
            _ => Action::None,
        }
    }

    fn handle_click(&mut self, p: Point) -> Action {
        // An open list gets the click first; any click closes it.
        if let Some(i) = self.open_dropdown() {
            for k in 0..self.dd[i].items.len() {
                if self.item_rect(i, k).contains_point(p) {
                    self.dd[i].selected = k;
                    break;
                }
            }
            self.dd[i].open = false;
            return Action::None;
        }

        if ip_rect().contains_point(p) {
            self.focus = F_IP;
        } else if check_hit_rect().contains_point(p) {
            self.focus = F_CHECK;
            self.hdr_on = !self.hdr_on;
        } else if connect_rect().contains_point(p) {
            self.focus = F_CONNECT;
            return self.submit();
        } else if cancel_rect().contains_point(p) {
            return Action::Cancel;
        } else if self.dd_enabled() {
            for i in 0..DD_COUNT {
                if dd_rect(i).contains_point(p) {
                    self.focus = F_DD0 + i;
                    self.dd[i].open = true;
                    break;
                }
            }
        }
        Action::None
    }

    fn handle_key(&mut self, key: Keycode, keymod: Mod) -> Action {
        let ctrl = keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD);
        let shift = keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
        let dd_focus = (F_DD0..F_DD0 + DD_COUNT).contains(&self.focus).then(|| self.focus - F_DD0);

        match key {
            Keycode::Escape => {
                if self.open_dropdown().is_some() {
                    self.close_all();
                    Action::None
                } else {
                    Action::Cancel
                }
            }
            Keycode::Tab => {
                self.move_focus(if shift { -1 } else { 1 });
                Action::None
            }
            Keycode::Return | Keycode::KpEnter => {
                if self.open_dropdown().is_some() {
                    self.close_all();
                    Action::None
                } else if self.focus == F_CANCEL {
                    Action::Cancel
                } else {
                    self.submit()
                }
            }
            Keycode::Space => {
                match self.focus {
                    F_CHECK => self.hdr_on = !self.hdr_on,
                    F_CONNECT | F_CANCEL => return self.activate(self.focus),
                    _ => {
                        if let Some(i) = dd_focus {
                            let open = self.dd[i].open;
                            self.close_all();
                            self.dd[i].open = !open;
                        }
                    }
                }
                Action::None
            }
            Keycode::Backspace if self.focus == F_IP => {
                self.ip.pop();
                self.ip_error = false;
                Action::None
            }
            Keycode::V if ctrl && self.focus == F_IP => Action::Paste,
            Keycode::Up | Keycode::Down => {
                let delta = if key == Keycode::Up { -1 } else { 1 };
                if let Some(i) = dd_focus {
                    self.dd[i].step(delta);
                } else if self.focus == F_CHECK {
                    self.hdr_on = delta > 0;
                }
                Action::None
            }
            Keycode::Left | Keycode::Right => {
                if self.focus == F_CONNECT || self.focus == F_CANCEL {
                    self.focus = if key == Keycode::Left { F_CANCEL } else { F_CONNECT };
                }
                Action::None
            }
            _ => Action::None,
        }
    }
}

// -------------------------------------------------------------------------------------
// Drawing
// -------------------------------------------------------------------------------------

fn text_width(s: &str) -> i32 {
    s.chars().count() as i32 * GLYPH
}

fn draw_text<T: RenderTarget>(c: &mut Canvas<T>, x: i32, y: i32, s: &str, color: Color) {
    c.set_draw_color(color);
    let mut cx = x;
    for ch in s.chars() {
        let glyph = BASIC_FONTS.get(ch).or_else(|| BASIC_FONTS.get('?')).unwrap_or([0; 8]);
        for (row, bits) in glyph.iter().enumerate() {
            let mut col = 0i32;
            while col < 8 {
                if (bits >> col) & 1 == 1 {
                    // Merge horizontal runs of set pixels into a single rectangle.
                    let start = col;
                    while col < 8 && (bits >> col) & 1 == 1 {
                        col += 1;
                    }
                    let _ = c.fill_rect(Rect::new(
                        cx + start * SCALE,
                        y + row as i32 * SCALE,
                        ((col - start) * SCALE) as u32,
                        SCALE as u32,
                    ));
                } else {
                    col += 1;
                }
            }
        }
        cx += GLYPH;
    }
}

/// Text vertically centred in a row of `ROW_H`.
fn draw_label<T: RenderTarget>(c: &mut Canvas<T>, x: i32, row_y: i32, s: &str, color: Color) {
    draw_text(c, x, row_y + (ROW_H - GLYPH) / 2, s, color);
}

/// 1px outline built from filled rectangles (renders identically on every SDL backend).
fn outline<T: RenderTarget>(c: &mut Canvas<T>, r: Rect, color: Color) {
    c.set_draw_color(color);
    let (x, y, w, h) = (r.x(), r.y(), r.width(), r.height());
    let _ = c.fill_rect(Rect::new(x, y, w, 1));
    let _ = c.fill_rect(Rect::new(x, y + h as i32 - 1, w, 1));
    let _ = c.fill_rect(Rect::new(x, y, 1, h));
    let _ = c.fill_rect(Rect::new(x + w as i32 - 1, y, 1, h));
}

fn draw_box<T: RenderTarget>(c: &mut Canvas<T>, r: Rect, fill: Color, border: Color) {
    c.set_draw_color(fill);
    let _ = c.fill_rect(r);
    outline(c, r, border);
}

/// Small downward-pointing triangle centred on (cx, cy).
fn draw_arrow<T: RenderTarget>(c: &mut Canvas<T>, cx: i32, cy: i32, color: Color) {
    c.set_draw_color(color);
    for i in 0..6 {
        let half = 6 - i;
        let _ = c.fill_rect(Rect::new(cx - half, cy - 3 + i, (half * 2 + 1) as u32, 1));
    }
}

fn draw_form<T: RenderTarget>(c: &mut Canvas<T>, f: &Form) {
    c.set_draw_color(BG);
    c.clear();

    draw_text(c, LABEL_X, 20, "ColourSpace connection", TEXT);

    // ---- IP -------------------------------------------------------------------------
    draw_label(c, LABEL_X, IP_Y, "ColourSpace IP", TEXT);
    let border = if f.ip_error {
        ERROR
    } else if f.focus == F_IP {
        FOCUS
    } else {
        BORDER
    };
    draw_box(c, ip_rect(), FIELD, border);
    let max_chars = ((CTRL_W - 16) / GLYPH) as usize;
    let shown: String = {
        let chars: Vec<char> = f.ip.chars().collect();
        let keep = max_chars.saturating_sub(1); // leave room for the cursor
        chars[chars.len().saturating_sub(keep)..].iter().collect()
    };
    draw_label(c, CTRL_X + 8, IP_Y, &shown, TEXT);
    if f.focus == F_IP {
        c.set_draw_color(TEXT);
        let cx = CTRL_X + 8 + text_width(&shown);
        let _ = c.fill_rect(Rect::new(cx, IP_Y + 6, 2, (ROW_H - 12) as u32));
    }
    if f.ip_error {
        draw_text(c, CTRL_X, IP_Y + ROW_H + 2, "Enter the ColourSpace IP", ERROR);
    }

    // ---- HDR checkbox ---------------------------------------------------------------
    draw_label(c, LABEL_X, CHECK_Y, "HDR", TEXT);
    let cb = check_rect();
    draw_box(c, cb, FIELD, if f.focus == F_CHECK { FOCUS } else { BORDER });
    if f.hdr_on {
        c.set_draw_color(ACCENT);
        let _ = c.fill_rect(Rect::new(cb.x() + 5, cb.y() + 5, 14, 14));
    }
    draw_label(c, CTRL_X + 36, CHECK_Y, "Enable HDR", TEXT);

    // ---- dropdown fields ------------------------------------------------------------
    let enabled = f.dd_enabled();
    for (i, d) in f.dd.iter().enumerate() {
        let r = dd_rect(i);
        let text_col = if enabled { TEXT } else { MUTED };
        draw_label(c, LABEL_X, r.y(), d.label, text_col);
        let border = if enabled && f.focus == F_DD0 + i { FOCUS } else { BORDER };
        draw_box(c, r, FIELD, border);
        draw_label(c, r.x() + 8, r.y(), &d.items[d.selected], text_col);
        draw_arrow(c, r.right() - 16, r.y() + ROW_H / 2, text_col);
    }

    // ---- buttons --------------------------------------------------------------------
    let cancel = cancel_rect();
    draw_box(c, cancel, FIELD, if f.focus == F_CANCEL { FOCUS } else { BORDER });
    let label = "Cancel";
    draw_text(c, cancel.x() + (BTN_W - text_width(label)) / 2, cancel.y() + (BTN_H - GLYPH) / 2, label, TEXT);

    let connect = connect_rect();
    draw_box(c, connect, ACCENT, if f.focus == F_CONNECT { TEXT } else { ACCENT });
    let label = "Connect";
    draw_text(c, connect.x() + (BTN_W - text_width(label)) / 2, connect.y() + (BTN_H - GLYPH) / 2, label, TEXT);

    // ---- open dropdown list goes on top of everything ---------------------------------
    if let Some(i) = f.open_dropdown() {
        let d = &f.dd[i];
        let l = f.list_rect(i);
        draw_box(c, l, FIELD, FOCUS);
        for k in 0..d.items.len() {
            let r = f.item_rect(i, k);
            if r.contains_point(f.mouse) {
                c.set_draw_color(HOVER);
                let _ = c.fill_rect(Rect::new(r.x() + 1, r.y() + 1, r.width() - 2, r.height() - 2));
            }
            let mark = if k == d.selected { ACCENT } else { TEXT };
            draw_label(c, r.x() + 8, r.y(), &d.items[k], mark);
        }
    }
}

// -------------------------------------------------------------------------------------
// Public entry point
// -------------------------------------------------------------------------------------

/// Show the startup window. Returns `None` if the user cancelled / closed it.
///
/// Creates and drops its own SDL context, so the caller can (re)initialise SDL afterwards,
/// for example with a different video driver.
pub fn show(defaults: Settings) -> Result<Option<Settings>, String> {
    let sdl = sdl2::init()?;
    let video = sdl.video()?;
    let window = video
        .window("Calibration Client Linux", W as u32, H as u32)
        .position_centered()
        .build()
        .map_err(|e| e.to_string())?;
    // The software renderer is plenty for a static form and avoids any GPU/driver setup.
    let mut canvas = window.into_canvas().software().build().map_err(|e| e.to_string())?;
    let mut pump = sdl.event_pump()?;
    let text_input = video.text_input();
    text_input.start();

    let mut form = Form::new(&defaults);
    loop {
        draw_form(&mut canvas, &form);
        canvas.present();

        // Block (with a timeout so exposure/redraws still happen) instead of spinning.
        let first = pump.wait_event_timeout(250);
        for event in first.into_iter().chain(pump.poll_iter().collect::<Vec<_>>()) {
            match form.handle_event(&event) {
                Action::None => {}
                Action::Cancel => return Ok(None),
                Action::Submit => return Ok(Some(form.to_settings())),
                Action::Paste => {
                    if let Ok(text) = video.clipboard().clipboard_text() {
                        form.insert_text(text.trim());
                    }
                }
            }
        }
    }
}

// -------------------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sdl2::pixels::PixelFormatEnum;
    use sdl2::surface::Surface;

    fn defaults() -> Settings {
        Settings {
            remote: String::new(),
            hdr: HdrMode::Sdr,
            primaries: Primaries::Bt2020,
            max_luminance: 1000.0,
            min_luminance: 0.0001,
            max_cll: 0.0,
            max_fall: 0.0,
        }
    }

    fn click(form: &mut Form, r: Rect) -> Action {
        let p = r.center();
        form.handle_event(&Event::MouseButtonDown {
            timestamp: 0,
            window_id: 0,
            which: 0,
            mouse_btn: MouseButton::Left,
            clicks: 1,
            x: p.x(),
            y: p.y(),
        })
    }

    fn key(form: &mut Form, k: Keycode) -> Action {
        form.handle_event(&Event::KeyDown {
            timestamp: 0,
            window_id: 0,
            keycode: Some(k),
            scancode: None,
            keymod: Mod::NOMOD,
            repeat: false,
        })
    }

    #[test]
    fn defaults_round_trip() {
        let mut form = Form::new(&defaults());
        form.insert_text("192.168.1.5");
        let s = form.to_settings();
        assert_eq!(s.remote, "192.168.1.5");
        assert_eq!(s.hdr, HdrMode::Sdr);
        assert_eq!(s.max_luminance, 1000.0);
        assert_eq!(s.min_luminance, 0.0001);
        assert_eq!(s.max_cll, 0.0);
    }

    #[test]
    fn cli_values_outside_the_presets_are_kept() {
        let mut d = defaults();
        d.hdr = HdrMode::Hlg;
        d.primaries = Primaries::P3D65;
        d.max_luminance = 1500.0;
        let s = Form::new(&d).to_settings();
        assert_eq!(s.hdr, HdrMode::Hlg);
        assert_eq!(s.primaries, Primaries::P3D65);
        assert_eq!(s.max_luminance, 1500.0);
    }

    #[test]
    fn checkbox_and_dropdown_clicks() {
        let mut form = Form::new(&defaults());
        // dropdowns are inert while HDR is off
        click(&mut form, dd_rect(DD_SIGNAL));
        assert!(form.open_dropdown().is_none());

        click(&mut form, check_hit_rect());
        assert!(form.hdr_on);

        // open the signal menu and pick HLG (item 1)
        click(&mut form, dd_rect(DD_SIGNAL));
        assert_eq!(form.open_dropdown(), Some(DD_SIGNAL));
        let item = form.item_rect(DD_SIGNAL, 1);
        click(&mut form, item);
        assert!(form.open_dropdown().is_none());
        assert_eq!(form.to_settings().hdr, HdrMode::Hlg);

        // unticking goes back to SDR whatever the menu says
        click(&mut form, check_hit_rect());
        assert_eq!(form.to_settings().hdr, HdrMode::Sdr);
    }

    #[test]
    fn connect_needs_an_ip() {
        let mut form = Form::new(&defaults());
        assert_eq!(click(&mut form, connect_rect()), Action::None);
        assert!(form.ip_error);
        form.insert_text("10.0.0.2");
        assert_eq!(click(&mut form, connect_rect()), Action::Submit);
        assert_eq!(click(&mut form, cancel_rect()), Action::Cancel);
    }

    #[test]
    fn keyboard_navigation() {
        let mut form = Form::new(&defaults());
        form.insert_text("host");
        key(&mut form, Keycode::Tab); // -> checkbox
        key(&mut form, Keycode::Space);
        assert!(form.hdr_on);
        key(&mut form, Keycode::Tab); // -> signal dropdown
        key(&mut form, Keycode::Down);
        assert_eq!(form.to_settings().hdr, HdrMode::Hlg);
        assert_eq!(key(&mut form, Keycode::Return), Action::Submit);
    }

    #[test]
    fn lists_stay_inside_the_window() {
        let form = Form::new(&defaults());
        for i in 0..DD_COUNT {
            let l = form.list_rect(i);
            assert!(l.y() >= 0 && l.bottom() <= H, "dropdown {i} list at {l:?}");
        }
    }

    /// Renders the form to /tmp so it can be looked at (run with `--ignored`).
    #[test]
    #[ignore]
    fn render_preview() {
        let mut d = defaults();
        d.remote = "192.168.168.207".into();
        d.hdr = HdrMode::Hdr10;
        let mut form = Form::new(&d);
        for (name, open) in [("closed", None), ("open", Some(DD_MAX_LUM)), ("open_up", Some(DD_MAX_FALL))] {
            form.close_all();
            if let Some(i) = open {
                form.dd[i].open = true;
                form.mouse = form.item_rect(i, 1).center();
            }
            let surface = Surface::new(W as u32, H as u32, PixelFormatEnum::RGB24).unwrap();
            let mut canvas = surface.into_canvas().unwrap();
            draw_form(&mut canvas, &form);
            canvas.into_surface().save_bmp(format!("/tmp/startup_{name}.bmp")).unwrap();
        }
    }
}
