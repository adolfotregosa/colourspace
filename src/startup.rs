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

use crate::config;
use crate::hdr::{HdrMode, HdrSupport, KdeLuminance, Primaries};

/// Everything the startup window collects (and is pre-filled from the command line and from
/// what was remembered).
#[derive(Debug, Clone)]
pub struct Settings {
    pub remote: String,
    /// What will be used: `Sdr` when the HDR checkbox is off, otherwise the chosen signal.
    pub hdr: HdrMode,
    /// The Signal menu's choice (HDR10 or HLG), kept even while the checkbox is off.
    pub signal: HdrMode,
    pub primaries: Primaries,
    pub max_luminance: f32,
    pub min_luminance: f32,
    pub max_cll: f32,
    pub max_fall: f32,
    /// "Bypass KDE scaling": HDR on KDE declares its own colour description, so KWin passes PQ
    /// and HLG unscaled whatever the SDR brightness is. Has no effect on other desktops.
    pub bypass_kde_scaling: bool,
}

impl Settings {
    /// The values used when nothing else says otherwise (same as the command-line defaults).
    pub fn built_in() -> Self {
        Settings {
            remote: String::new(),
            hdr: HdrMode::Sdr,
            signal: HdrMode::Hdr10,
            primaries: Primaries::Bt2020,
            max_luminance: 2000.0,
            min_luminance: 0.0001,
            max_cll: 0.0,
            max_fall: 0.0,
            bypass_kde_scaling: true,
        }
    }

    /// The defaults for this system: the built-in values, with Max and Min luminance taken from
    /// what KDE uses for the HDR display (its peak and minimum) where it reports them. Metadata
    /// that matches what KWin works with keeps KWin from tone-mapping the patches.
    pub fn defaults_for(kde: Option<&KdeLuminance>) -> Self {
        let mut defaults = Self::built_in();
        defaults.apply_kde(kde);
        defaults
    }

    /// Take Max and Min luminance from KDE's values for the HDR display, where it has them
    /// (limited to what the fields accept).
    pub fn apply_kde(&mut self, kde: Option<&KdeLuminance>) {
        let Some(kde) = kde else { return };
        if let Some(peak) = kde.peak {
            self.max_luminance = peak.clamp(1.0, 10_000.0);
        }
        if let Some(min) = kde.min {
            self.min_luminance = min.clamp(0.0, 100.0);
        }
    }

    /// Put remembered values on top (only those that were saved and are valid).
    pub fn apply_saved(&mut self, saved: &config::Saved) {
        if let Some(ip) = &saved.ip {
            self.remote = ip.clone();
        }
        if let Some(signal) = saved.signal.as_deref().and_then(HdrMode::from_key).filter(|m| *m != HdrMode::Sdr) {
            self.signal = signal;
        }
        if let Some(on) = saved.hdr_enabled {
            self.hdr = if on { self.signal } else { HdrMode::Sdr };
        }
        if let Some(p) = saved.primaries.as_deref().and_then(Primaries::from_key) {
            self.primaries = p;
        }
        if let Some(v) = saved.max_luminance {
            self.max_luminance = v;
        }
        if let Some(v) = saved.min_luminance {
            self.min_luminance = v;
        }
        if let Some(v) = saved.max_cll {
            self.max_cll = v;
        }
        if let Some(v) = saved.max_fall {
            self.max_fall = v;
        }
        if let Some(v) = saved.bypass_kde_scaling {
            self.bypass_kde_scaling = v;
        }
    }

    /// What to remember after a successful connection, starting from what was remembered
    /// before. The address is always updated. The HDR choices are only updated when HDR could
    /// actually be chosen (`hdr_usable`): while HDR is unavailable the checkbox is forced off,
    /// and that must not wipe the preference.
    pub fn to_saved(&self, hdr_usable: bool, previous: &config::Saved) -> config::Saved {
        let mut saved = previous.clone();
        if config::is_valid_address(self.remote.trim()) {
            saved.ip = Some(self.remote.trim().to_string());
        }
        if hdr_usable {
            saved.hdr_enabled = Some(self.hdr != HdrMode::Sdr);
            saved.signal = Some(self.signal.key().to_string());
            saved.primaries = Some(self.primaries.key().to_string());
            saved.max_luminance = Some(self.max_luminance);
            saved.min_luminance = Some(self.min_luminance);
            saved.max_cll = Some(self.max_cll);
            saved.max_fall = Some(self.max_fall);
            saved.bypass_kde_scaling = Some(self.bypass_kde_scaling);
        }
        saved
    }
}

// -------------------------------------------------------------------------------------
// Layout
// -------------------------------------------------------------------------------------

/// Wide enough for the whole window title: KDE centres the title between its left and right
/// button groups, so a narrow window elides it ("Calibratio...ient Linux").
const W: i32 = 680;
const SCALE: i32 = 2; // 8x8 glyphs drawn at 16x16
const GLYPH: i32 = 8 * SCALE;
const LABEL_X: i32 = 24;
const CTRL_X: i32 = 270;
const CTRL_W: i32 = W - CTRL_X - 24; // the controls stretch to a 24px right margin
const ROW_H: i32 = 32;

const IP_Y: i32 = 64;
const CHECK_Y: i32 = 118;
const NOTE_Y: i32 = 154; // status line(s) under the HDR checkbox
/// Size of the status lines' font: the 8x8 font drawn at 1.5x (the labels are 2x).
const NOTE_PX: i32 = 12;
const NOTE_LINE_H: i32 = 15;
/// Room is kept for this many status lines (longer messages are wrapped, then cut short).
const NOTE_MAX_LINES: usize = 3;
const DD_Y0: i32 = NOTE_Y + NOTE_MAX_LINES as i32 * NOTE_LINE_H + 7; // the Signal menu
const DD_STEP: i32 = 44;
const BYPASS_Y: i32 = DD_Y0 + DD_STEP; // the "Bypass KDE scaling" checkbox, under Signal
const BTN_W: i32 = 136;
const BTN_H: i32 = 38;

// Dropdown indices, top to bottom.
const DD_COUNT: usize = 6;
const DD_SIGNAL: usize = 0;
const DD_PRIMARIES: usize = 1;
const DD_MAX_LUM: usize = 2;
const DD_MIN_LUM: usize = 3;
const DD_MAX_CLL: usize = 4;
const DD_MAX_FALL: usize = 5;

// Focus order (Tab cycles through these, top to bottom and left to right as they appear).
const F_IP: usize = 0;
const F_CHECK: usize = 1;
const F_SIGNAL: usize = 2;
const F_BYPASS: usize = 3; // "Bypass KDE scaling"
const F_DD1: usize = 4; // Primaries .. MaxFALL are F_DD1 + 0..5
const F_DEFAULTS: usize = F_DD1 + (DD_COUNT - 1);
const F_CANCEL: usize = F_DEFAULTS + 1;
const F_CONNECT: usize = F_CANCEL + 1;
const F_COUNT: usize = F_CONNECT + 1;

const BYPASS_LABEL: &str = "Bypass KDE scaling";

/// The focus stop of dropdown `i`.
fn dd_focus_id(i: usize) -> usize {
    if i == DD_SIGNAL {
        F_SIGNAL
    } else {
        F_DD1 + i - 1
    }
}

/// The dropdown a focus stop belongs to, if it is one.
fn focus_dropdown(f: usize) -> Option<usize> {
    if f == F_SIGNAL {
        Some(DD_SIGNAL)
    } else if (F_DD1..F_DEFAULTS).contains(&f) {
        Some(f - F_DD1 + 1)
    } else {
        None
    }
}

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
fn bypass_rect() -> Rect {
    Rect::new(CTRL_X, BYPASS_Y + 4, 24, 24)
}
/// Clickable area for "Bypass KDE scaling": the box plus its text.
fn bypass_hit_rect() -> Rect {
    Rect::new(CTRL_X, BYPASS_Y, CTRL_W as u32, ROW_H as u32)
}
/// Signal on the first row, the checkbox on the second, then Primaries .. MaxFALL.
fn dd_rect(i: usize) -> Rect {
    let row = if i == DD_SIGNAL { 0 } else { i as i32 + 1 };
    Rect::new(CTRL_X, DD_Y0 + row * DD_STEP, CTRL_W as u32, ROW_H as u32)
}
/// Width of the arrow (preset list) zone at the right end of a menu.
const ARROW_W: i32 = 36;
/// Numeric fields: the number itself (click to type)...
fn dd_text_zone(i: usize) -> Rect {
    let r = dd_rect(i);
    Rect::new(r.x(), r.y(), (CTRL_W - ARROW_W) as u32, ROW_H as u32)
}
/// ...and the arrow (click for the preset list).
fn dd_arrow_zone(i: usize) -> Rect {
    let r = dd_rect(i);
    Rect::new(r.x() + CTRL_W - ARROW_W, r.y(), ARROW_W as u32, ROW_H as u32)
}
/// Top of the bottom row (Defaults, Cancel, Connect), under the last menu.
fn buttons_y() -> i32 {
    dd_rect(DD_MAX_FALL).bottom() + 28
}
fn window_height() -> i32 {
    buttons_y() + BTN_H + 32
}
/// Defaults, Cancel and Connect are right-aligned with the controls above them.
fn connect_rect() -> Rect {
    Rect::new(CTRL_X + CTRL_W - BTN_W, buttons_y(), BTN_W as u32, BTN_H as u32)
}
fn cancel_rect() -> Rect {
    Rect::new(connect_rect().x() - 14 - BTN_W, buttons_y(), BTN_W as u32, BTN_H as u32)
}
/// Puts the HDR values (Primaries, the four numbers, Bypass) back to their defaults.
fn defaults_rect() -> Rect {
    Rect::new(cancel_rect().x() - 14 - BTN_W, buttons_y(), BTN_W as u32, BTN_H as u32)
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
const SELECTED: Color = Color::RGB(38, 82, 145);
const NOTE_WARN: Color = Color::RGB(226, 168, 70);

// -------------------------------------------------------------------------------------
// HDR availability note
// -------------------------------------------------------------------------------------

/// Message line(s) shown under the HDR checkbox: (colour, lines at 8px per character).
/// Each line must fit the window (checked by a test): at most 66 characters.
/// Why HDR cannot be used here, as one line (the same wording the startup window shows).
pub fn hdr_unavailable_reason(support: &HdrSupport) -> String {
    support_note(support).1.join(" ")
}

fn support_note(support: &HdrSupport) -> (Color, Vec<String>) {
    if (support.hdr10 || support.hlg) && support.kde_no_hdr_display {
        return (
            NOTE_WARN,
            vec![
                "HDR unavailable: KDE reports no HDR-capable display.".to_string(),
                "It needs an HDR monitor on an HDR-capable port and cable.".to_string(),
            ],
        );
    }
    if (support.hdr10 || support.hlg) && support.kde_hdr_off {
        return (
            NOTE_WARN,
            vec![
                "HDR is turned off in the KDE display settings.".to_string(),
                "Turn it on in System Settings > Display & Monitor, then restart.".to_string(),
            ],
        );
    }
    if support.any() {
        let mut modes = Vec::new();
        if support.hdr10 {
            modes.push("HDR10 (PQ)");
        }
        if support.hlg {
            modes.push("HLG");
        }
        return (MUTED, vec![format!("HDR available: {}", modes.join(", "))]);
    }
    if let Some(problem) = &support.problem {
        let mut short: String = problem.chars().take(58).collect();
        if problem.chars().count() > 58 {
            short.push_str("...");
        }
        return (NOTE_WARN, vec!["HDR unavailable: the check failed:".to_string(), short]);
    }
    if support.driver != "wayland" {
        return (
            NOTE_WARN,
            vec![
                "HDR unavailable: it needs a Wayland session.".to_string(),
                format!("This session uses: {}", support.driver.chars().take(40).collect::<String>()),
            ],
        );
    }
    (
        NOTE_WARN,
        vec![
            "HDR unavailable: no HDR10 surface offered for this display.".to_string(),
            "Enable HDR in your desktop's display settings, then restart.".to_string(),
        ],
    )
}

// -------------------------------------------------------------------------------------
// Form state
// -------------------------------------------------------------------------------------

/// The extra state of a menu whose value can also be typed (the numeric HDR metadata fields).
struct Numeric {
    value: f32,
    min: f32,
    max: f32,
    zero_label: Option<&'static str>,
    editing: bool,
    /// What has been typed so far while `editing`. Empty means "keep the current value".
    buffer: String,
}

struct Dropdown {
    label: &'static str,
    items: Vec<String>,
    /// Preset behind each entry of a numeric menu.
    values: Vec<f32>,
    /// Chosen entry (enumerated menus only).
    selected: usize,
    open: bool,
    numeric: Option<Numeric>,
}

/// Longest number that can be typed.
const MAX_TYPED_CHARS: usize = 8;

impl Dropdown {
    fn enumerated(label: &'static str, items: &[&str], selected: usize) -> Self {
        Self {
            label,
            items: items.iter().map(|s| s.to_string()).collect(),
            values: Vec::new(),
            selected,
            open: false,
            numeric: None,
        }
    }

    /// A number that can be typed (limited to `min..=max`) or picked from a list of presets.
    fn numeric(
        label: &'static str,
        presets: &[f32],
        current: f32,
        zero_label: Option<&'static str>,
        min: f32,
        max: f32,
    ) -> Self {
        let current = if current.is_finite() && current >= 0.0 { current } else { presets[0] };
        Self {
            label,
            items: presets.iter().map(|v| Self::format_value(*v, zero_label)).collect(),
            values: presets.to_vec(),
            selected: 0,
            open: false,
            numeric: Some(Numeric {
                value: current.clamp(min, max),
                min,
                max,
                zero_label,
                editing: false,
                buffer: String::new(),
            }),
        }
    }

    fn format_value(v: f32, zero_label: Option<&str>) -> String {
        match zero_label {
            Some(z) if v == 0.0 => z.to_string(),
            _ => format!("{} cd/m2", v),
        }
    }

    fn value(&self) -> f32 {
        self.numeric.as_ref().map(|n| n.value).unwrap_or(0.0)
    }

    fn is_editing(&self) -> bool {
        self.numeric.as_ref().is_some_and(|n| n.editing)
    }

    /// What the field shows when nothing is being typed.
    fn shown(&self) -> String {
        match &self.numeric {
            Some(n) => Self::format_value(n.value, n.zero_label),
            None => self.items[self.selected].clone(),
        }
    }

    /// Which list entry is the current value (highlighted in the open list).
    fn preset_index(&self) -> Option<usize> {
        match &self.numeric {
            Some(n) => self.values.iter().position(|v| (v - n.value).abs() < 1e-9),
            None => Some(self.selected),
        }
    }

    /// Pick entry `k` of the list.
    fn choose(&mut self, k: usize) {
        match &mut self.numeric {
            Some(n) => {
                n.value = self.values[k];
                n.editing = false;
                n.buffer.clear();
            }
            None => self.selected = k,
        }
    }

    /// Set a numeric menu straight to `v` (limited to its range), ending any typing.
    fn set_value(&mut self, v: f32) {
        if let Some(n) = &mut self.numeric {
            n.value = v.clamp(n.min, n.max);
            n.editing = false;
            n.buffer.clear();
        }
    }

    fn start_edit(&mut self) {
        if let Some(n) = &mut self.numeric {
            n.editing = true;
            n.buffer.clear();
        }
    }

    /// Typed characters: digits and a decimal point only.
    fn type_chars(&mut self, text: &str) {
        if let Some(n) = &mut self.numeric {
            n.editing = true;
            for ch in text.chars() {
                if (ch.is_ascii_digit() || ch == '.') && n.buffer.len() < MAX_TYPED_CHARS {
                    n.buffer.push(ch);
                }
            }
        }
    }

    fn backspace(&mut self) {
        if let Some(n) = &mut self.numeric {
            n.buffer.pop();
        }
    }

    /// Accept what was typed (limited to the allowed range). Anything that is not a number,
    /// or nothing at all, leaves the value as it was.
    fn commit_edit(&mut self) {
        if let Some(n) = &mut self.numeric {
            if n.editing {
                if let Ok(v) = n.buffer.parse::<f32>() {
                    if v.is_finite() {
                        n.value = v.clamp(n.min, n.max);
                    }
                }
                n.editing = false;
                n.buffer.clear();
            }
        }
    }

    fn cancel_edit(&mut self) {
        if let Some(n) = &mut self.numeric {
            n.editing = false;
            n.buffer.clear();
        }
    }

    /// Arrow keys: enumerated menus move one entry; numeric ones jump to the next larger /
    /// smaller preset.
    fn step(&mut self, delta: i32) {
        match &mut self.numeric {
            Some(n) => {
                let target = if delta > 0 {
                    self.values.iter().copied().find(|v| *v > n.value + 1e-9)
                } else {
                    self.values.iter().copied().rev().find(|v| *v < n.value - 1e-9)
                };
                if let Some(v) = target {
                    n.value = v;
                }
            }
            None => {
                let n = self.items.len() as i32;
                self.selected = (self.selected as i32 + delta).clamp(0, n - 1) as usize;
            }
        }
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
    /// The address is shown selected (a remembered or previously tried one): the first thing
    /// typed replaces it, Backspace clears it, Enter connects to it as it is.
    ip_selected: bool,
    hdr_on: bool,
    /// "Bypass KDE scaling". Kept as it is on other desktops (shown greyed and unticked there),
    /// so a session elsewhere never changes the remembered choice.
    bypass_on: bool,
    support: HdrSupport,
    /// HDR modes behind the entries of the Signal menu (only the ones this system offers).
    signal_modes: Vec<HdrMode>,
    dd: Vec<Dropdown>,
    focus: usize,
    ip_error: bool,
    mouse: Point,
}

impl Form {
    fn new(defaults: &Settings, support: &HdrSupport) -> Self {
        let mut signal_modes = Vec::new();
        if support.hdr10 {
            signal_modes.push(HdrMode::Hdr10);
        }
        if support.hlg {
            signal_modes.push(HdrMode::Hlg);
        }
        if signal_modes.is_empty() {
            signal_modes.push(HdrMode::Hdr10); // menu is disabled anyway
        }
        let signal_labels: Vec<&str> = signal_modes
            .iter()
            .map(|m| if *m == HdrMode::Hlg { "HLG" } else { "HDR10 (PQ)" })
            .collect();
        let signal_selected = signal_modes.iter().position(|m| *m == defaults.signal).unwrap_or(0);

        let dd = vec![
            Dropdown::enumerated("Signal", &signal_labels, signal_selected),
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
                1.0,
                10000.0,
            ),
            Dropdown::numeric(
                "Min luminance",
                &[0.0, 0.0001, 0.001, 0.005, 0.01, 0.05],
                defaults.min_luminance,
                None,
                0.0,
                100.0,
            ),
            Dropdown::numeric(
                "MaxCLL",
                &[0.0, 400.0, 600.0, 1000.0, 2000.0, 4000.0],
                defaults.max_cll,
                Some("Unspecified"),
                0.0,
                10000.0,
            ),
            Dropdown::numeric(
                "MaxFALL",
                &[0.0, 100.0, 400.0, 600.0, 1000.0],
                defaults.max_fall,
                Some("Unspecified"),
                0.0,
                10000.0,
            ),
        ];
        Self {
            ip: defaults.remote.clone(),
            ip_selected: !defaults.remote.is_empty(),
            hdr_on: support.any() && defaults.hdr != HdrMode::Sdr,
            bypass_on: defaults.bypass_kde_scaling,
            support: support.clone(),
            signal_modes,
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
            } else {
                self.signal_modes[self.dd[DD_SIGNAL].selected]
            },
            signal: self.signal_modes[self.dd[DD_SIGNAL].selected],
            primaries: if self.dd[DD_PRIMARIES].selected == 1 { Primaries::P3D65 } else { Primaries::Bt2020 },
            max_luminance: self.dd[DD_MAX_LUM].value(),
            min_luminance: self.dd[DD_MIN_LUM].value(),
            max_cll: self.dd[DD_MAX_CLL].value(),
            max_fall: self.dd[DD_MAX_FALL].value(),
            bypass_kde_scaling: self.bypass_on,
        }
    }

    fn dd_enabled(&self) -> bool {
        self.hdr_on
    }

    /// "Bypass KDE scaling" can be changed: HDR is ticked and this is KDE.
    fn bypass_enabled(&self) -> bool {
        self.dd_enabled() && self.support.kde_session
    }

    /// Bypass KDE scaling is in effect: the client then declares KDE's own peak and no other
    /// metadata, so Primaries, the luminances, MaxCLL and MaxFALL have no effect.
    fn bypass_active(&self) -> bool {
        self.support.kde_session && self.bypass_on
    }

    /// Menu `i` can be used: HDR is ticked, and (except Signal) Bypass KDE scaling is not in
    /// effect. Greyed-out values are kept and saved as they are.
    fn dd_live(&self, i: usize) -> bool {
        self.dd_enabled() && (i == DD_SIGNAL || !self.bypass_active())
    }

    fn toggle_bypass(&mut self, on: Option<bool>) {
        if self.bypass_enabled() {
            self.bypass_on = on.unwrap_or(!self.bypass_on);
            if self.bypass_active() {
                // the metadata menus just went grey: nothing may stay open or half-typed
                for i in 0..DD_COUNT {
                    if i != DD_SIGNAL {
                        self.dd[i].commit_edit();
                        self.dd[i].open = false;
                    }
                }
            }
        }
    }

    fn toggle_hdr(&mut self, on: Option<bool>) {
        if self.support.any() {
            self.hdr_on = on.unwrap_or(!self.hdr_on);
            if !self.hdr_on {
                self.commit_edits();
                self.close_all();
            }
        }
    }

    fn close_all(&mut self) {
        for d in &mut self.dd {
            d.open = false;
        }
    }

    /// The numeric field currently being typed into, if any.
    fn editing_dropdown(&self) -> Option<usize> {
        self.dd.iter().position(|d| d.is_editing())
    }

    fn commit_edits(&mut self) {
        for d in &mut self.dd {
            d.commit_edit();
        }
    }

    /// The focused menu, if it is a numeric one that is currently usable.
    fn numeric_focus(&self) -> Option<usize> {
        let i = focus_dropdown(self.focus)?;
        (self.dd_live(i) && self.dd[i].numeric.is_some()).then_some(i)
    }

    /// Height of the window.
    fn height(&self) -> i32 {
        window_height()
    }

    fn connect_rect(&self) -> Rect {
        connect_rect()
    }

    fn cancel_rect(&self) -> Rect {
        cancel_rect()
    }

    /// Put the HDR values back to their defaults: Primaries, the four numbers (Max and Min
    /// luminance from KDE's values for the HDR display when it reports them) and Bypass KDE
    /// scaling (on). The HDR checkbox and the Signal menu are left alone.
    fn reset_hdr_values(&mut self) {
        let d = Settings::defaults_for(self.support.kde_luminance.as_ref());
        self.close_all();
        for dd in &mut self.dd {
            dd.cancel_edit();
        }
        self.dd[DD_PRIMARIES].selected = if d.primaries == Primaries::P3D65 { 1 } else { 0 };
        self.dd[DD_MAX_LUM].set_value(d.max_luminance);
        self.dd[DD_MIN_LUM].set_value(d.min_luminance);
        self.dd[DD_MAX_CLL].set_value(d.max_cll);
        self.dd[DD_MAX_FALL].set_value(d.max_fall);
        self.bypass_on = d.bypass_kde_scaling;
    }

    fn open_dropdown(&self) -> Option<usize> {
        self.dd.iter().position(|d| d.open)
    }

    /// Rectangle of the open list; opens upwards when there is no room below.
    fn list_rect(&self, i: usize) -> Rect {
        let f = dd_rect(i);
        let h = self.dd[i].items.len() as i32 * ROW_H;
        if f.bottom() + h <= self.height() {
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
        match f {
            F_CHECK => self.support.any(),
            F_BYPASS => self.bypass_enabled(),
            F_DEFAULTS => self.dd_enabled(),
            _ => match focus_dropdown(f) {
                Some(i) => self.dd_live(i),
                None => true,
            },
        }
    }

    fn move_focus(&mut self, dir: i32) {
        self.ip_selected = false;
        self.commit_edits();
        self.close_all();
        for _ in 0..F_COUNT {
            self.focus = ((self.focus as i32 + dir).rem_euclid(F_COUNT as i32)) as usize;
            if self.focusable(self.focus) {
                break;
            }
        }
    }

    /// Text typed (or pasted) into the address field: replaces a selected address.
    fn type_into_ip(&mut self, text: &str) {
        if text.chars().any(config::is_address_char) {
            if self.ip_selected {
                self.ip.clear();
            }
            self.ip_selected = false;
        }
        self.insert_text(text);
    }

    pub fn insert_text(&mut self, text: &str) {
        for ch in text.chars() {
            let ok = config::is_address_char(ch);
            if ok && self.ip.len() < config::MAX_ADDRESS_LEN {
                self.ip.push(ch);
                self.ip_error = false;
            }
        }
    }

    fn submit(&mut self) -> Action {
        self.commit_edits(); // a number still being typed counts
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
            F_DEFAULTS => {
                if self.dd_enabled() {
                    self.reset_hdr_values();
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_event(&mut self, event: &Event) -> Action {
        match event {
            Event::Quit { .. } => Action::Cancel,
            Event::TextInput { text, .. } => {
                if self.focus == F_IP {
                    self.type_into_ip(text);
                } else if let Some(i) = self.numeric_focus() {
                    // typing on a focused number field starts editing it
                    self.close_all();
                    self.dd[i].type_chars(text);
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
        // A number being typed: a click inside it keeps editing, a click anywhere else accepts it.
        if let Some(e) = self.editing_dropdown() {
            if self.dd_live(e) && dd_text_zone(e).contains_point(p) {
                return Action::None;
            }
            self.dd[e].commit_edit();
        }

        // An open list gets the click first; any click closes it.
        if let Some(i) = self.open_dropdown() {
            for k in 0..self.dd[i].items.len() {
                if self.item_rect(i, k).contains_point(p) {
                    self.dd[i].choose(k);
                    break;
                }
            }
            self.dd[i].open = false;
            return Action::None;
        }

        if ip_rect().contains_point(p) {
            self.focus = F_IP;
            self.ip_selected = false; // clicking puts the cursor in the text instead
        } else if check_hit_rect().contains_point(p) {
            if self.support.any() {
                self.focus = F_CHECK;
                self.toggle_hdr(None);
            }
        } else if self.connect_rect().contains_point(p) {
            self.focus = F_CONNECT;
            return self.submit();
        } else if self.cancel_rect().contains_point(p) {
            return Action::Cancel;
        } else if defaults_rect().contains_point(p) {
            if self.dd_enabled() {
                self.focus = F_DEFAULTS;
                self.reset_hdr_values();
            }
        } else if bypass_hit_rect().contains_point(p) {
            if self.bypass_enabled() {
                self.focus = F_BYPASS;
                self.toggle_bypass(None);
            }
        } else {
            for i in 0..DD_COUNT {
                if self.dd_live(i) && dd_rect(i).contains_point(p) {
                    self.focus = dd_focus_id(i);
                    if self.dd[i].numeric.is_some() && dd_text_zone(i).contains_point(p) {
                        self.dd[i].start_edit(); // the number: type a value
                    } else {
                        self.dd[i].open = true; // the arrow (or an enumerated menu): the list
                    }
                    break;
                }
            }
        }
        Action::None
    }

    fn handle_key(&mut self, key: Keycode, keymod: Mod) -> Action {
        let ctrl = keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD);
        let shift = keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
        let dd_focus = focus_dropdown(self.focus);

        // While a number is being typed, these keys belong to the number.
        if let Some(i) = self.editing_dropdown() {
            match key {
                Keycode::Return | Keycode::KpEnter => {
                    self.dd[i].commit_edit();
                    return Action::None;
                }
                Keycode::Escape => {
                    self.dd[i].cancel_edit();
                    return Action::None;
                }
                Keycode::Backspace => {
                    self.dd[i].backspace();
                    return Action::None;
                }
                Keycode::Space => return Action::None,
                // moving on accepts what was typed, then the key does its usual job
                Keycode::Tab | Keycode::Up | Keycode::Down => self.dd[i].commit_edit(),
                _ => {}
            }
        }

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
                } else if matches!(self.focus, F_CANCEL | F_DEFAULTS) {
                    // these are buttons: Enter presses the focused one (Enter anywhere else connects)
                    self.activate(self.focus)
                } else {
                    self.submit()
                }
            }
            Keycode::Space => {
                match self.focus {
                    F_CHECK => self.toggle_hdr(None),
                    F_BYPASS => self.toggle_bypass(None),
                    F_CONNECT | F_CANCEL | F_DEFAULTS => return self.activate(self.focus),
                    _ => {
                        if let Some(i) = dd_focus.filter(|&i| self.dd_live(i)) {
                            let open = self.dd[i].open;
                            self.close_all();
                            self.dd[i].open = !open;
                        }
                    }
                }
                Action::None
            }
            Keycode::Backspace if self.focus == F_IP => {
                if self.ip_selected {
                    self.ip.clear(); // the whole selected address goes at once
                    self.ip_selected = false;
                } else {
                    self.ip.pop();
                }
                self.ip_error = false;
                Action::None
            }
            Keycode::V if ctrl && self.focus == F_IP => Action::Paste,
            Keycode::Up | Keycode::Down => {
                let delta = if key == Keycode::Up { -1 } else { 1 };
                if let Some(i) = dd_focus.filter(|&i| self.dd_live(i)) {
                    self.dd[i].step(delta);
                } else if self.focus == F_CHECK {
                    self.toggle_hdr(Some(delta > 0));
                } else if self.focus == F_BYPASS {
                    self.toggle_bypass(Some(delta > 0));
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
    draw_text_scaled(c, x, y, s, color, SCALE);
}

/// Same, with each font pixel drawn as `scale` x `scale` pixels (1 = small print).
fn draw_text_scaled<T: RenderTarget>(c: &mut Canvas<T>, x: i32, y: i32, s: &str, color: Color, scale: i32) {
    draw_text_px(c, x, y, s, color, 8 * scale);
}

/// Text with each 8x8 glyph drawn `px` pixels wide and high. Sizes that are not a multiple of 8
/// (12 = 1.5x) spread the font pixels as evenly as whole screen pixels allow.
fn draw_text_px<T: RenderTarget>(c: &mut Canvas<T>, x: i32, y: i32, s: &str, color: Color, px: i32) {
    c.set_draw_color(color);
    let edge = |i: i32| i * px / 8; // screen offset of font pixel i's edge
    let mut cx = x;
    for ch in s.chars() {
        let glyph = BASIC_FONTS.get(ch).or_else(|| BASIC_FONTS.get('?')).unwrap_or([0; 8]);
        for (row, bits) in glyph.iter().enumerate() {
            let row = row as i32;
            let mut col = 0i32;
            while col < 8 {
                if (bits >> col) & 1 == 1 {
                    // Merge horizontal runs of set pixels into a single rectangle.
                    let start = col;
                    while col < 8 && (bits >> col) & 1 == 1 {
                        col += 1;
                    }
                    let _ = c.fill_rect(Rect::new(
                        cx + edge(start),
                        y + edge(row),
                        (edge(col) - edge(start)) as u32,
                        (edge(row + 1) - edge(row)) as u32,
                    ));
                } else {
                    col += 1;
                }
            }
        }
        cx += px;
    }
}

/// Break `text` into lines of at most `max` characters, at spaces where possible.
fn wrap(text: &str, max: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        while word.chars().count() > max {
            // a word longer than a whole line: break it
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            let head: String = word.chars().take(max).collect();
            word = word.chars().skip(max).collect();
            lines.push(head);
        }
        if line.is_empty() {
            line = word;
        } else if line.chars().count() + 1 + word.chars().count() <= max {
            line.push(' ');
            line.push_str(&word);
        } else {
            lines.push(std::mem::replace(&mut line, word));
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The status lines under the HDR checkbox, wrapped to the window: what HDR can do here, then
/// (on KDE) what KDE uses for the HDR display. At most `NOTE_MAX_LINES`; a longer text is cut
/// short with "...".
fn note_lines(support: &HdrSupport) -> Vec<(Color, String)> {
    let max = ((W - 2 * LABEL_X) / NOTE_PX) as usize;
    let (colour, messages) = support_note(support);
    let mut lines: Vec<(Color, String)> = Vec::new();
    // each message keeps its own line(s); a two-line message is one sentence pair, so wrap each
    for message in &messages {
        lines.extend(wrap(message, max).into_iter().map(|l| (colour, l)));
    }
    if let Some(kde) = kde_info_line(support) {
        lines.extend(wrap(&kde, max).into_iter().map(|l| (MUTED, l)));
    }
    if lines.len() > NOTE_MAX_LINES {
        lines.truncate(NOTE_MAX_LINES);
        let last = &mut lines[NOTE_MAX_LINES - 1].1;
        let keep = max.saturating_sub(3).min(last.chars().count());
        *last = last.chars().take(keep).collect::<String>() + "...";
    }
    lines
}

/// Shown at the bottom left of the window: "v" plus the major.minor of the package version, so
/// changing `version` in Cargo.toml (0.1.x -> 0.2.0) is all it takes to update it.
const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION_MAJOR"), ".", env!("CARGO_PKG_VERSION_MINOR"));

/// Read-only line under "HDR available": what KDE uses for the HDR display (HDR on KDE declares
/// its reference white to match the SDR brightness shown here).
fn kde_info_line(support: &HdrSupport) -> Option<String> {
    let k = support.kde_luminance.as_ref()?;
    let v = |v: Option<f32>| v.map(|v| v.to_string()).unwrap_or_else(|| "?".into());
    Some(format!("KDE: SDR brightness {}, peak {}, black {} nits", v(k.sdr_brightness), v(k.peak), v(k.min)))
}

const DEFAULTS_LABEL: &str = "Defaults";

/// Shown in red under the IP field when Connect is pressed with it empty.
const IP_REQUIRED: &str = "IP address needed";

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
    if f.ip_selected && f.focus == F_IP && !shown.is_empty() {
        c.set_draw_color(SELECTED);
        let _ = c.fill_rect(Rect::new(CTRL_X + 6, IP_Y + 5, (text_width(&shown) + 4) as u32, (ROW_H - 10) as u32));
    }
    draw_label(c, CTRL_X + 8, IP_Y, &shown, TEXT);
    if f.focus == F_IP && !f.ip_selected {
        c.set_draw_color(TEXT);
        let cx = CTRL_X + 8 + text_width(&shown);
        let _ = c.fill_rect(Rect::new(cx, IP_Y + 6, 2, (ROW_H - 12) as u32));
    }
    if f.ip_error {
        draw_text(c, CTRL_X, IP_Y + ROW_H + 2, IP_REQUIRED, ERROR);
    }

    // ---- HDR checkbox ---------------------------------------------------------------
    let hdr_ok = f.support.any();
    let hdr_text = if hdr_ok { TEXT } else { MUTED };
    draw_label(c, LABEL_X, CHECK_Y, "HDR", hdr_text);
    let cb = check_rect();
    draw_box(c, cb, FIELD, if !hdr_ok { MUTED } else if f.focus == F_CHECK { FOCUS } else { BORDER });
    if f.hdr_on {
        c.set_draw_color(ACCENT);
        let _ = c.fill_rect(Rect::new(cb.x() + 5, cb.y() + 5, 14, 14));
    }
    draw_label(c, CTRL_X + 36, CHECK_Y, "Enable HDR", hdr_text);

    for (i, (colour, line)) in note_lines(&f.support).iter().enumerate() {
        draw_text_px(c, LABEL_X, NOTE_Y + 1 + i as i32 * NOTE_LINE_H, line, *colour, NOTE_PX);
    }

    // ---- Bypass KDE scaling (under Signal) --------------------------------------------
    // Greyed out unless HDR is ticked and this is KDE; shown unticked on other desktops, where
    // it has no effect.
    let bypass_live = f.bypass_enabled();
    let bypass_text = if bypass_live { TEXT } else { MUTED };
    let bb = bypass_rect();
    draw_box(c, bb, FIELD, if !bypass_live { MUTED } else if f.focus == F_BYPASS { FOCUS } else { BORDER });
    if f.bypass_on && f.support.kde_session {
        c.set_draw_color(if bypass_live { ACCENT } else { MUTED });
        let _ = c.fill_rect(Rect::new(bb.x() + 5, bb.y() + 5, 14, 14));
    }
    draw_label(c, CTRL_X + 36, BYPASS_Y, BYPASS_LABEL, bypass_text);

    // ---- dropdown fields ------------------------------------------------------------
    for i in 0..DD_COUNT {
        let enabled = f.dd_live(i);
        let d = &f.dd[i];
        let r = dd_rect(i);
        let text_col = if enabled { TEXT } else { MUTED };
        draw_label(c, LABEL_X, r.y(), d.label, text_col);
        let border = if enabled && f.focus == dd_focus_id(i) { FOCUS } else { BORDER };
        draw_box(c, r, FIELD, border);
        match &d.numeric {
            None => {
                draw_label(c, r.x() + 8, r.y(), &d.shown(), text_col);
                draw_arrow(c, r.right() - 16, r.y() + ROW_H / 2, text_col);
            }
            Some(n) => {
                let zone = dd_text_zone(i);
                let arrow = dd_arrow_zone(i);
                if n.editing && !n.buffer.is_empty() {
                    // what is being typed, a cursor, and the unit
                    draw_label(c, zone.x() + 8, r.y(), &n.buffer, text_col);
                    let cx = zone.x() + 8 + text_width(&n.buffer);
                    c.set_draw_color(text_col);
                    let _ = c.fill_rect(Rect::new(cx, r.y() + 6, 2, (ROW_H - 12) as u32));
                    draw_label(c, cx + 8, r.y(), "cd/m2", MUTED);
                } else if n.editing {
                    // nothing typed yet: the current value stays visible, greyed, under the cursor
                    draw_label(c, zone.x() + 8, r.y(), &d.shown(), MUTED);
                    c.set_draw_color(text_col);
                    let _ = c.fill_rect(Rect::new(zone.x() + 6, r.y() + 6, 2, (ROW_H - 12) as u32));
                } else {
                    draw_label(c, zone.x() + 8, r.y(), &d.shown(), text_col);
                }
                // thin divider: number on the left, preset list on the right
                c.set_draw_color(BORDER);
                let _ = c.fill_rect(Rect::new(arrow.x(), r.y() + 4, 1, (ROW_H - 8) as u32));
                draw_arrow(c, arrow.center().x(), r.y() + ROW_H / 2, text_col);
            }
        }
    }

    // ---- version, bottom left, level with the buttons ---------------------------------------
    draw_text(c, LABEL_X, f.cancel_rect().y() + (BTN_H - GLYPH) / 2, VERSION, MUTED);

    // ---- buttons --------------------------------------------------------------------
    let defaults = defaults_rect();
    let defaults_live = f.dd_enabled();
    draw_box(c, defaults, FIELD, if defaults_live && f.focus == F_DEFAULTS { FOCUS } else { BORDER });
    draw_text(
        c,
        defaults.x() + (BTN_W - text_width(DEFAULTS_LABEL)) / 2,
        defaults.y() + (BTN_H - GLYPH) / 2,
        DEFAULTS_LABEL,
        if defaults_live { TEXT } else { MUTED },
    );

    let cancel = f.cancel_rect();
    draw_box(c, cancel, FIELD, if f.focus == F_CANCEL { FOCUS } else { BORDER });
    let label = "Cancel";
    draw_text(c, cancel.x() + (BTN_W - text_width(label)) / 2, cancel.y() + (BTN_H - GLYPH) / 2, label, TEXT);

    let connect = f.connect_rect();
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
            let mark = if d.preset_index() == Some(k) { ACCENT } else { TEXT };
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
pub fn show(defaults: Settings, support: &HdrSupport) -> Result<Option<Settings>, String> {
    let sdl = sdl2::init()?;
    let video = sdl.video()?;
    let form = Form::new(&defaults, support);
    let mut applied_height = form.height();
    let window = video
        .window("Calibration Client Linux", W as u32, applied_height as u32)
        .position_centered()
        // Resizable so the window can follow the "Advanced" section on every backend (a window
        // with a fixed size may refuse to change it); the minimum size below stops it ever being
        // made too small for what is on screen.
        .resizable()
        .build()
        .map_err(|e| e.to_string())?;
    // The software renderer is plenty for a static form and avoids any GPU/driver setup.
    let mut canvas = window.into_canvas().software().build().map_err(|e| e.to_string())?;
    let _ = canvas.window_mut().set_minimum_size(W as u32, applied_height as u32);
    let mut pump = sdl.event_pump()?;
    let text_input = video.text_input();
    text_input.start();

    let mut form = form;
    loop {
        // "Advanced" opening or closing changes how much window is needed.
        if form.height() != applied_height {
            applied_height = form.height();
            let _ = canvas.window_mut().set_minimum_size(W as u32, applied_height as u32);
            let _ = canvas.window_mut().set_size(W as u32, applied_height as u32);
        }
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
                        form.type_into_ip(text.trim());
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
        Settings::built_in()
    }

    fn full() -> HdrSupport {
        HdrSupport { hdr10: true, hlg: true, sdr10: true, driver: "wayland".into(), problem: None, kde_hdr_off: false, kde_no_hdr_display: false, kde_luminance: None, kde_session: true }
    }

    fn none() -> HdrSupport {
        HdrSupport { hdr10: false, hlg: false, sdr10: false, driver: "wayland".into(), problem: None, kde_hdr_off: false, kde_no_hdr_display: false, kde_luminance: None, kde_session: true }
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

    fn typed(form: &mut Form, text: &str) {
        form.handle_event(&Event::TextInput { timestamp: 0, window_id: 0, text: text.to_string() });
    }

    /// A form with HDR ticked, an IP entered and Bypass KDE scaling off, so every menu can be
    /// used (most tests here work on the menus).
    fn hdr_form() -> Form {
        let mut d = defaults();
        d.remote = "10.0.0.2".into();
        d.hdr = HdrMode::Hdr10;
        d.bypass_kde_scaling = false;
        Form::new(&d, &full())
    }

    /// The buttons move with the "Advanced" section, so ask the form where they are.
    fn click_connect(form: &mut Form) -> Action {
        let r = form.connect_rect();
        click(form, r)
    }

    fn click_cancel(form: &mut Form) -> Action {
        let r = form.cancel_rect();
        click(form, r)
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
        let mut form = Form::new(&defaults(), &full());
        form.insert_text("192.168.1.5");
        let s = form.to_settings();
        assert_eq!(s.remote, "192.168.1.5");
        assert_eq!(s.hdr, HdrMode::Sdr);
        assert_eq!(s.max_luminance, 2000.0);
        assert_eq!(s.min_luminance, 0.0001);
        assert_eq!(s.max_cll, 0.0);
    }

    #[test]
    fn cli_values_outside_the_presets_are_kept() {
        let mut d = defaults();
        d.hdr = HdrMode::Hlg;
        d.signal = HdrMode::Hlg;
        d.primaries = Primaries::P3D65;
        d.max_luminance = 1500.0;
        let s = Form::new(&d, &full()).to_settings();
        assert_eq!(s.hdr, HdrMode::Hlg);
        assert_eq!(s.primaries, Primaries::P3D65);
        assert_eq!(s.max_luminance, 1500.0);
    }

    #[test]
    fn checkbox_and_dropdown_clicks() {
        let mut form = Form::new(&defaults(), &full());
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
        let mut form = Form::new(&defaults(), &full());
        assert_eq!(click_connect(&mut form), Action::None);
        assert!(form.ip_error);
        form.insert_text("10.0.0.2");
        assert_eq!(click_connect(&mut form), Action::Submit);
        assert_eq!(click_cancel(&mut form), Action::Cancel);
    }

    #[test]
    fn keyboard_navigation() {
        let mut form = Form::new(&defaults(), &full());
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
        let form = Form::new(&defaults(), &full());
        for i in 0..DD_COUNT {
            let l = form.list_rect(i);
            assert!(l.y() >= 0 && l.bottom() <= form.height(), "dropdown {i} list at {l:?}");
        }
    }

    fn remembered() -> config::Saved {
        config::Saved {
            ip: Some("192.168.1.5".into()),
            hdr_enabled: Some(true),
            signal: Some("hlg".into()),
            primaries: Some("p3d65".into()),
            max_luminance: Some(1200.0),
            min_luminance: Some(0.0005),
            max_cll: Some(1000.0),
            max_fall: Some(400.0),
            bypass_kde_scaling: None,
        }
    }

    #[test]
    fn remembered_choices_fill_the_form_when_hdr_is_available() {
        let mut d = defaults();
        d.apply_saved(&remembered());
        let form = Form::new(&d, &full());
        assert_eq!(form.ip, "192.168.1.5");
        assert!(form.hdr_on, "the checkbox comes back ticked");
        let s = form.to_settings();
        assert_eq!((s.hdr, s.signal, s.primaries), (HdrMode::Hlg, HdrMode::Hlg, Primaries::P3D65));
        assert_eq!((s.max_luminance, s.min_luminance, s.max_cll, s.max_fall), (1200.0, 0.0005, 1000.0, 400.0));
    }

    #[test]
    fn remembered_hdr_is_still_disabled_when_hdr_is_no_longer_available() {
        let mut d = defaults();
        d.apply_saved(&remembered());
        for support in [
            none(),
            HdrSupport { driver: "x11".into(), ..none() },
            HdrSupport { kde_hdr_off: true, ..full() },
            HdrSupport { kde_no_hdr_display: true, ..full() },
        ] {
            let mut form = Form::new(&d, &support);
            assert!(!form.hdr_on, "unticked: {support:?}");
            click(&mut form, check_hit_rect());
            assert!(!form.hdr_on, "and it cannot be ticked");
            assert_eq!(form.to_settings().hdr, HdrMode::Sdr);
            assert_eq!(form.ip, "192.168.1.5", "the address is still offered");
        }
    }

    #[test]
    fn a_remembered_signal_that_is_gone_falls_back_to_one_that_exists() {
        let mut d = defaults();
        d.apply_saved(&remembered()); // HLG
        let pq_only = HdrSupport { hdr10: true, ..none() };
        let form = Form::new(&d, &pq_only);
        assert!(form.hdr_on);
        assert_eq!(form.to_settings().hdr, HdrMode::Hdr10);
    }

    #[test]
    fn what_is_saved_follows_the_form_when_hdr_can_be_chosen() {
        let mut d = defaults();
        d.apply_saved(&remembered());
        d.bypass_kde_scaling = false; // the luminance menus are only usable without it
        let mut form = Form::new(&d, &full());
        click(&mut form, check_hit_rect()); // untick
        click(&mut form, dd_text_zone(DD_MAX_LUM)); // type a number (unlocked again by re-ticking)
        click(&mut form, check_hit_rect()); // tick again
        click(&mut form, dd_text_zone(DD_MAX_LUM));
        typed(&mut form, "900");
        key(&mut form, Keycode::Return);
        let saved = form.to_settings().to_saved(true, &config::Saved::default());
        assert_eq!(saved.hdr_enabled, Some(true));
        assert_eq!(saved.signal.as_deref(), Some("hlg"));
        assert_eq!(saved.primaries.as_deref(), Some("p3d65"));
        assert_eq!(saved.max_luminance, Some(900.0));
        assert_eq!(saved.ip.as_deref(), Some("192.168.1.5"));

        // an unticked box is remembered too, together with the menu choices
        let mut form = Form::new(&d, &full());
        click(&mut form, check_hit_rect());
        let saved = form.to_settings().to_saved(true, &config::Saved::default());
        assert_eq!(saved.hdr_enabled, Some(false));
        assert_eq!(saved.signal.as_deref(), Some("hlg"), "the signal survives an unticked box");
    }

    #[test]
    fn unusable_hdr_never_overwrites_the_remembered_choices() {
        let before = remembered();
        let mut d = defaults();
        d.apply_saved(&before);
        d.remote = "10.9.9.9".into(); // a different address this time
        let settings = Form::new(&d, &none()).to_settings(); // HDR unavailable: forced off
        assert_eq!(settings.hdr, HdrMode::Sdr);

        let after = settings.to_saved(false, &before);
        assert_eq!(after.ip.as_deref(), Some("10.9.9.9"), "the address is updated");
        assert_eq!(
            config::Saved { ip: before.ip.clone(), ..after.clone() },
            before,
            "everything else stays exactly as it was"
        );

        // and when HDR is usable again, the forced-off value would have been the bug
        assert_ne!(settings.to_saved(true, &before).hdr_enabled, before.hdr_enabled);
    }

    /// The focus stops reached by pressing Tab repeatedly from the address field.
    fn tab_order(form: &mut Form, presses: usize) -> Vec<usize> {
        form.focus = F_IP;
        (0..presses)
            .map(|_| {
                key(form, Keycode::Tab);
                form.focus
            })
            .collect()
    }

#[test]
    fn tab_follows_the_order_on_screen() {
        let mut d = defaults();
        d.hdr = HdrMode::Hdr10;
        d.bypass_kde_scaling = false;
        let mut form = Form::new(&d, &full());
        let mut expected = vec![F_CHECK, F_SIGNAL, F_BYPASS];
        expected.extend((1..DD_COUNT).map(dd_focus_id));
        expected.extend([F_DEFAULTS, F_CANCEL, F_CONNECT, F_IP]);
        assert_eq!(tab_order(&mut form, expected.len()), expected);

        // Bypass KDE scaling on: the greyed-out metadata menus are skipped
        d.bypass_kde_scaling = true;
        let mut form = Form::new(&d, &full());
        let expected = vec![F_CHECK, F_SIGNAL, F_BYPASS, F_DEFAULTS, F_CANCEL, F_CONNECT, F_IP];
        assert_eq!(tab_order(&mut form, expected.len()), expected);

        // HDR unticked: everything HDR is skipped
        let mut form = Form::new(&defaults(), &full());
        assert_eq!(tab_order(&mut form, 4), vec![F_CHECK, F_CANCEL, F_CONNECT, F_IP]);
    }

#[test]
    fn the_defaults_button_resets_the_hdr_values() {
        let mut d = defaults();
        d.apply_saved(&remembered()); // HLG, P3-D65, 1200 / 0.0005 / 1000 / 400, HDR ticked
        d.bypass_kde_scaling = false;
        let mut form = Form::new(&d, &full());

        click(&mut form, dd_text_zone(DD_MIN_LUM));
        typed(&mut form, "0.05"); // half-typed when the button is pressed
        click(&mut form, defaults_rect());

        let s = form.to_settings();
        assert_eq!(s.primaries, Primaries::Bt2020);
        assert_eq!((s.max_luminance, s.min_luminance, s.max_cll, s.max_fall), (2000.0, 0.0001, 0.0, 0.0));
        assert!(s.bypass_kde_scaling, "Bypass KDE scaling goes back on");
        assert!(!form.dd[DD_MIN_LUM].is_editing(), "typing in progress is dropped");
        // everything else is untouched
        assert_eq!((s.hdr, s.signal), (HdrMode::Hlg, HdrMode::Hlg));
        assert_eq!(form.ip, "192.168.1.5");

        // it also works from the keyboard, without connecting
        form.dd[DD_MAX_LUM].set_value(500.0);
        form.focus = F_DEFAULTS;
        assert_eq!(key(&mut form, Keycode::Return), Action::None);
        assert_eq!(form.to_settings().max_luminance, 2000.0);
        form.dd[DD_MAX_LUM].set_value(500.0);
        assert_eq!(key(&mut form, Keycode::Space), Action::None);
        assert_eq!(form.to_settings().max_luminance, 2000.0);
    }

    fn kde(peak: Option<f32>, min: Option<f32>) -> HdrSupport {
        HdrSupport { kde_luminance: Some(KdeLuminance { peak, min, sdr_brightness: Some(94.0) }), ..full() }
    }

#[test]
    fn the_defaults_button_uses_kdes_peak_and_minimum() {
        let mut d = defaults();
        d.apply_saved(&remembered()); // 1200 / 0.0005 / 1000 / 400, P3
        let mut form = Form::new(&d, &kde(Some(1740.0), Some(0.0)));
        click(&mut form, defaults_rect());
        let s = form.to_settings();
        assert_eq!((s.max_luminance, s.min_luminance), (1740.0, 0.0), "from KDE");
        assert_eq!((s.primaries, s.max_cll, s.max_fall), (Primaries::Bt2020, 0.0, 0.0), "the rest as before");

        // KDE reports only one of the two: the other is the built-in default
        let mut form = Form::new(&d, &kde(None, Some(0.005)));
        click(&mut form, defaults_rect());
        assert_eq!((form.to_settings().max_luminance, form.to_settings().min_luminance), (2000.0, 0.005));

        // no KDE values (other desktop, or kscreen-doctor missing): the built-in defaults
        let mut form = Form::new(&d, &full());
        click(&mut form, defaults_rect());
        assert_eq!((form.to_settings().max_luminance, form.to_settings().min_luminance), (2000.0, 0.0001));

        // values outside what the fields accept are limited to them
        let mut form = Form::new(&d, &kde(Some(50_000.0), Some(500.0)));
        click(&mut form, defaults_rect());
        assert_eq!((form.to_settings().max_luminance, form.to_settings().min_luminance), (10_000.0, 100.0));
    }

#[test]
    fn the_defaults_button_does_nothing_while_hdr_is_off() {
        let mut d = defaults();
        d.apply_saved(&remembered());
        d.hdr = HdrMode::Sdr; // HDR unticked (available, just off)
        let mut form = Form::new(&d, &full());
        assert!(!form.hdr_on);
        click(&mut form, defaults_rect());
        assert_eq!(form.to_settings().max_luminance, 1200.0, "the HDR values are locked until HDR is ticked");
        assert!(!form.focusable(F_DEFAULTS));
    }

#[test]
    fn bypass_kde_scaling_is_on_by_default_and_can_be_switched_on_kde() {
        assert!(Settings::built_in().bypass_kde_scaling, "on by default");
        let mut form = form_with_bypass(true); // KDE, HDR ticked, as by default
        assert!(form.bypass_enabled());
        click(&mut form, bypass_hit_rect());
        assert!(!form.to_settings().bypass_kde_scaling);
        assert_eq!(form.focus, F_BYPASS);
        key(&mut form, Keycode::Space);
        assert!(form.to_settings().bypass_kde_scaling);
        key(&mut form, Keycode::Up);
        assert!(!form.to_settings().bypass_kde_scaling);
        key(&mut form, Keycode::Down);
        assert!(form.to_settings().bypass_kde_scaling);
        // Enter on it connects, as on the HDR checkbox
        assert_eq!(key(&mut form, Keycode::Return), Action::Submit);

        // remembered, both ways
        let off = form_with_bypass(false).to_settings().to_saved(true, &config::Saved::default());
        assert_eq!(off.bypass_kde_scaling, Some(false));
        let mut d = defaults();
        d.apply_saved(&off);
        assert!(!d.bypass_kde_scaling);
    }

    #[test]
    fn bypass_kde_scaling_greys_out_the_hdr_values_but_keeps_them() {
        let mut form = form_with_bypass(true);
        assert!(form.dd_live(DD_SIGNAL), "the signal still matters");
        for i in [DD_PRIMARIES, DD_MAX_LUM, DD_MIN_LUM, DD_MAX_CLL, DD_MAX_FALL] {
            assert!(!form.dd_live(i), "{i}");
        }
        let before = form.to_settings();
        // clicks, typing and arrows do nothing on the greyed-out values
        click(&mut form, dd_text_zone(DD_MAX_LUM));
        typed(&mut form, "500");
        key(&mut form, Keycode::Return);
        click(&mut form, dd_rect(DD_PRIMARIES));
        assert!(form.open_dropdown().is_none());
        assert_eq!(form.to_settings().max_luminance, before.max_luminance);
        assert_eq!(form.to_settings().primaries, before.primaries);

        // switching Bypass off makes them usable again, with their values kept
        click(&mut form, bypass_hit_rect());
        assert!(form.dd_live(DD_MAX_LUM));
        click(&mut form, dd_text_zone(DD_MAX_LUM));
        typed(&mut form, "500");
        // switching it on accepts what was being typed and greys the menus again
        click(&mut form, bypass_hit_rect());
        assert!(!form.dd_live(DD_MAX_LUM) && form.editing_dropdown().is_none());
        assert_eq!(form.to_settings().max_luminance, 500.0);
    }

    #[test]
    fn the_hdr_values_stay_usable_where_bypass_has_no_effect() {
        let mut d = defaults();
        d.hdr = HdrMode::Hdr10;
        d.bypass_kde_scaling = true;
        let mut support = full();
        support.kde_session = false; // GNOME: Mesa sends the metadata on
        let form = Form::new(&d, &support);
        for i in 0..DD_COUNT {
            assert!(form.dd_live(i), "{i}");
        }
    }

    fn form_with_bypass(on: bool) -> Form {
        let mut form = hdr_form();
        form.bypass_on = on;
        form
    }

    #[test]
    fn bypass_kde_scaling_is_greyed_out_elsewhere_and_keeps_its_value() {
        // another desktop: greyed out, cannot be changed, and the remembered value is kept
        let mut d = defaults();
        d.remote = "10.0.0.2".into();
        d.hdr = HdrMode::Hdr10;
        d.bypass_kde_scaling = false;
        let gnome = HdrSupport { kde_session: false, ..full() };
        let mut form = Form::new(&d, &gnome);
        assert!(!form.bypass_enabled());
        assert!(!form.focusable(F_BYPASS));
        click(&mut form, bypass_hit_rect());
        assert!(!form.to_settings().bypass_kde_scaling, "unchanged");
        let mut d_on = d.clone();
        d_on.bypass_kde_scaling = true;
        let form = Form::new(&d_on, &gnome);
        assert!(form.to_settings().bypass_kde_scaling, "a GNOME session never changes the KDE choice");

        // KDE but HDR unticked, or HDR unavailable: greyed out too
        let mut form = Form::new(&defaults(), &full());
        assert!(!form.hdr_on && !form.bypass_enabled());
        click(&mut form, bypass_hit_rect());
        assert!(form.to_settings().bypass_kde_scaling);
        for support in [none(), HdrSupport { kde_no_hdr_display: true, ..full() }] {
            assert!(!Form::new(&d_on, &support).bypass_enabled(), "{support:?}");
        }
    }

    #[test]
    fn the_version_is_shown_and_fits_beside_the_buttons() {
        // "v" + major.minor of the package version
        assert!(VERSION.starts_with('v'));
        assert!(env!("CARGO_PKG_VERSION").starts_with(&VERSION[1..]), "{VERSION} vs {}", env!("CARGO_PKG_VERSION"));
        for open in [false, true] {
            assert!(
                LABEL_X + text_width(VERSION) < defaults_rect().x(),
                "the version must not run into the Defaults button ({open})"
            );
        }
    }

    #[test]
    fn the_window_is_wide_enough_for_a_full_title_bar() {
        // a 24-character title at a typical 8-9px per character, centred between two button
        // groups of up to ~180px each (KDE with a few extra buttons); a 614px window was seen to
        // show it whole and a 582px one to elide it
        assert!(W >= 660, "window is {W}px wide");
        // the controls use the width, keeping the same right margin
        assert_eq!(CTRL_X + CTRL_W, W - 24);
        assert_eq!(connect_rect().right(), CTRL_X + CTRL_W);
        assert!(cancel_rect().right() < connect_rect().x());
        assert!(defaults_rect().x() > LABEL_X);
    }

    #[test]
    fn long_text_is_wrapped_at_spaces() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("short", 20), ["short"]);
        assert_eq!(wrap("abcdefghij kl", 4), ["abcd", "efgh", "ij", "kl"], "a too-long word is broken");
        assert_eq!(wrap("  spaced   out  ", 20), ["spaced out"]);
        assert!(wrap("", 10).is_empty());
        // cut short with "..." when there is no room left
        let lines = note_lines(&HdrSupport { problem: Some("word ".repeat(60)), ..none() });
        assert_eq!(lines.len(), NOTE_MAX_LINES);
        assert!(lines.last().unwrap().1.ends_with("..."), "{lines:?}");
    }

#[test]
    fn the_window_fits_what_is_showing() {
        let h = window_height();
        for r in [connect_rect(), cancel_rect(), defaults_rect()] {
            assert!(r.bottom() < h && r.y() > 0, "buttons inside a {h}px window");
        }
        assert!(dd_rect(DD_MAX_FALL).bottom() < connect_rect().y(), "menus stay above the buttons");
        // the bottom row, left to right: Defaults, Cancel, Connect, all clear of each other
        assert!(defaults_rect().right() < cancel_rect().x());
        assert!(cancel_rect().right() < connect_rect().x());
        assert_eq!(connect_rect().right(), CTRL_X + CTRL_W);
        // the checkbox row sits between Signal and Primaries
        assert!(dd_rect(DD_SIGNAL).bottom() <= bypass_hit_rect().y());
        assert!(bypass_hit_rect().bottom() <= dd_rect(DD_PRIMARIES).y());
        // every menu row is distinct
        for i in 1..DD_COUNT {
            assert!(dd_rect(i - 1).bottom() <= dd_rect(i).y() || i == DD_PRIMARIES);
        }
    }

    #[test]
    fn a_remembered_address_is_selected_so_typing_replaces_it() {
        let mut d = defaults();
        d.remote = "192.168.1.5".into();
        let mut form = Form::new(&d, &full());
        assert!(form.ip_selected);
        assert_eq!(form.to_settings().remote, "192.168.1.5");

        typed(&mut form, "10.0.0.");
        assert_eq!(form.ip, "10.0.0.", "the first typing replaced the old address");
        assert!(!form.ip_selected);
        typed(&mut form, "9");
        assert_eq!(form.ip, "10.0.0.9", "later typing appends");
    }

    #[test]
    fn selected_address_can_be_cleared_kept_or_connected_as_is() {
        let mut d = defaults();
        d.remote = "192.168.1.5".into();

        // Enter connects straight away to the remembered address
        let mut form = Form::new(&d, &full());
        assert_eq!(key(&mut form, Keycode::Return), Action::Submit);
        assert_eq!(form.to_settings().remote, "192.168.1.5");

        // Backspace clears all of it at once, then edits normally
        let mut form = Form::new(&d, &full());
        key(&mut form, Keycode::Backspace);
        assert_eq!(form.ip, "");
        typed(&mut form, "abc");
        key(&mut form, Keycode::Backspace);
        assert_eq!(form.ip, "ab");

        // clicking in the field keeps the text and puts the cursor in it
        let mut form = Form::new(&d, &full());
        click(&mut form, ip_rect());
        typed(&mut form, "9");
        assert_eq!(form.ip, "192.168.1.59");

        // moving away and back keeps the text too
        let mut form = Form::new(&d, &full());
        key(&mut form, Keycode::Tab);
        assert!(!form.ip_selected);
        assert_eq!(form.ip, "192.168.1.5");

        // characters that cannot be part of an address change nothing
        let mut form = Form::new(&d, &full());
        typed(&mut form, " /;");
        assert_eq!(form.ip, "192.168.1.5");
        assert!(form.ip_selected);

        // pasting replaces it as well
        let mut form = Form::new(&d, &full());
        form.type_into_ip("10.1.1.1");
        assert_eq!(form.ip, "10.1.1.1");

        // nothing remembered: nothing is selected
        assert!(!Form::new(&defaults(), &full()).ip_selected);
    }

    #[test]
    fn a_custom_value_can_be_typed_into_a_number_field() {
        let mut form = hdr_form();
        click(&mut form, dd_text_zone(DD_MAX_LUM));
        assert!(form.dd[DD_MAX_LUM].is_editing());
        assert!(form.open_dropdown().is_none(), "clicking the number does not open the list");

        typed(&mut form, "1200");
        assert_eq!(form.to_settings().max_luminance, 2000.0, "nothing changes until it is accepted");
        assert_eq!(key(&mut form, Keycode::Return), Action::None, "Enter accepts the number, it does not connect");
        assert_eq!(form.to_settings().max_luminance, 1200.0);
        assert!(!form.dd[DD_MAX_LUM].is_editing());
        assert_eq!(form.dd[DD_MAX_LUM].shown(), "1200 cd/m2");

        assert_eq!(key(&mut form, Keycode::Return), Action::Submit, "a second Enter connects");
    }

    #[test]
    fn typed_values_are_limited_and_bad_input_is_ignored() {
        let mut form = hdr_form();

        // too big -> the largest allowed
        click(&mut form, dd_text_zone(DD_MAX_LUM));
        typed(&mut form, "99999");
        key(&mut form, Keycode::Return);
        assert_eq!(form.to_settings().max_luminance, 10000.0);

        // too small for a peak -> the smallest allowed
        click(&mut form, dd_text_zone(DD_MAX_LUM));
        typed(&mut form, "0");
        key(&mut form, Keycode::Return);
        assert_eq!(form.to_settings().max_luminance, 1.0);

        // decimals, and letters / symbols are dropped
        click(&mut form, dd_text_zone(DD_MIN_LUM));
        typed(&mut form, "0a.0-0 05");
        key(&mut form, Keycode::Return);
        assert_eq!(form.to_settings().min_luminance, 0.0005);

        // backspace edits what was typed
        click(&mut form, dd_text_zone(DD_MAX_CLL));
        typed(&mut form, "1050");
        key(&mut form, Keycode::Backspace);
        key(&mut form, Keycode::Return);
        assert_eq!(form.to_settings().max_cll, 105.0);

        // just a dot, or nothing at all, keeps the old value
        click(&mut form, dd_text_zone(DD_MAX_FALL));
        typed(&mut form, "400");
        key(&mut form, Keycode::Return);
        click(&mut form, dd_text_zone(DD_MAX_FALL));
        typed(&mut form, ".");
        key(&mut form, Keycode::Return);
        assert_eq!(form.to_settings().max_fall, 400.0);
        click(&mut form, dd_text_zone(DD_MAX_FALL));
        key(&mut form, Keycode::Return);
        assert_eq!(form.to_settings().max_fall, 400.0);

        // MaxCLL / MaxFALL: 0 means "unspecified"
        click(&mut form, dd_text_zone(DD_MAX_FALL));
        typed(&mut form, "0");
        key(&mut form, Keycode::Return);
        assert_eq!(form.dd[DD_MAX_FALL].shown(), "Unspecified");
    }

    #[test]
    fn escape_cancels_typing_and_clicking_elsewhere_accepts_it() {
        let mut form = hdr_form();
        click(&mut form, dd_text_zone(DD_MAX_LUM));
        typed(&mut form, "500");
        assert_eq!(key(&mut form, Keycode::Escape), Action::None, "Esc stops typing, it does not close the window");
        assert_eq!(form.to_settings().max_luminance, 2000.0);

        click(&mut form, dd_text_zone(DD_MAX_LUM));
        typed(&mut form, "500");
        click(&mut form, ip_rect()); // click away
        assert_eq!(form.to_settings().max_luminance, 500.0);
        assert!(!form.dd[DD_MAX_LUM].is_editing());
    }

    #[test]
    fn the_arrow_still_opens_the_preset_list() {
        let mut form = hdr_form();
        click(&mut form, dd_arrow_zone(DD_MAX_CLL));
        assert_eq!(form.open_dropdown(), Some(DD_MAX_CLL));
        assert!(!form.dd[DD_MAX_CLL].is_editing());
        let item = form.item_rect(DD_MAX_CLL, 3); // 1000
        click(&mut form, item);
        assert_eq!(form.to_settings().max_cll, 1000.0);

        // the list highlights the preset that equals the value, and none for a typed one
        assert_eq!(form.dd[DD_MAX_CLL].preset_index(), Some(3));
        click(&mut form, dd_text_zone(DD_MAX_CLL));
        typed(&mut form, "750");
        key(&mut form, Keycode::Return);
        assert_eq!(form.dd[DD_MAX_CLL].preset_index(), None);
    }

    #[test]
    fn numbers_can_be_typed_from_the_keyboard_and_arrows_step_presets() {
        let mut form = hdr_form();
        form.focus = dd_focus_id(DD_MAX_FALL);
        typed(&mut form, "300"); // no click needed once the field has focus
        assert!(form.dd[DD_MAX_FALL].is_editing());
        key(&mut form, Keycode::Tab); // accepts it and moves on (to the Defaults button)
        assert_eq!(form.to_settings().max_fall, 300.0);
        assert_eq!(form.focus, F_DEFAULTS);

        form.focus = dd_focus_id(DD_MAX_LUM); // 2000
        key(&mut form, Keycode::Down);
        assert_eq!(form.to_settings().max_luminance, 4000.0);
        key(&mut form, Keycode::Up);
        key(&mut form, Keycode::Up);
        assert_eq!(form.to_settings().max_luminance, 1000.0);
        for _ in 0..10 {
            key(&mut form, Keycode::Down);
        }
        assert_eq!(form.to_settings().max_luminance, 10000.0, "stops at the last preset");
    }

    #[test]
    fn connect_uses_a_number_that_is_still_being_typed() {
        let mut form = hdr_form();
        click(&mut form, dd_text_zone(DD_MAX_LUM));
        typed(&mut form, "1500");
        assert_eq!(click_connect(&mut form), Action::Submit);
        assert_eq!(form.to_settings().max_luminance, 1500.0);
    }

    #[test]
    fn number_fields_do_nothing_while_hdr_is_off() {
        let mut d = defaults();
        d.remote = "10.0.0.2".into();
        let mut form = Form::new(&d, &full()); // HDR unticked
        click(&mut form, dd_text_zone(DD_MAX_LUM));
        form.focus = dd_focus_id(DD_MAX_LUM);
        typed(&mut form, "1200");
        assert!(!form.dd[DD_MAX_LUM].is_editing());
        assert_eq!(form.to_settings().max_luminance, 2000.0);
    }

    #[test]
    fn hdr_cannot_be_enabled_when_unsupported() {
        let mut d = defaults();
        d.hdr = HdrMode::Hdr10; // e.g. --hdr hdr10 given on the command line
        d.remote = "10.0.0.2".into();
        let mut form = Form::new(&d, &none());
        assert!(!form.hdr_on, "starts unticked");

        click(&mut form, check_hit_rect());
        assert!(!form.hdr_on, "mouse cannot tick it");
        form.focus = F_CHECK;
        key(&mut form, Keycode::Space);
        key(&mut form, Keycode::Down);
        assert!(!form.hdr_on, "keyboard cannot tick it");
        click(&mut form, dd_rect(DD_SIGNAL));
        assert!(form.open_dropdown().is_none(), "menus stay shut");

        // Tab from the IP field goes straight to the buttons (Defaults is locked without HDR)
        form.focus = F_IP;
        key(&mut form, Keycode::Tab);
        assert_eq!(form.focus, F_CANCEL);
        key(&mut form, Keycode::Tab);
        assert_eq!(form.focus, F_CONNECT);

        assert_eq!(form.to_settings().hdr, HdrMode::Sdr);
        assert_eq!(key(&mut form, Keycode::Return), Action::Submit);
    }

    #[test]
    fn hdr_switched_off_in_kde_greys_out_the_checkbox() {
        let kde_off = HdrSupport { kde_hdr_off: true, ..full() };
        let mut d = defaults();
        d.hdr = HdrMode::Hdr10;
        let mut form = Form::new(&d, &kde_off);
        assert!(!form.hdr_on);
        click(&mut form, check_hit_rect());
        assert!(!form.hdr_on);
        assert_eq!(form.to_settings().hdr, HdrMode::Sdr);
        let note = support_note(&kde_off).1.join(" ");
        assert!(note.contains("KDE"), "{note}");
    }

    #[test]
    fn a_machine_without_an_hdr_display_cannot_tick_hdr() {
        // the reported bug: Vulkan offers HDR surfaces, but KDE knows there is no HDR display
        let no_display = HdrSupport { kde_no_hdr_display: true, ..full() };
        let mut d = defaults();
        d.hdr = HdrMode::Hdr10;
        let mut form = Form::new(&d, &no_display);
        assert!(!form.hdr_on);
        click(&mut form, check_hit_rect());
        assert!(!form.hdr_on);
        assert_eq!(form.to_settings().hdr, HdrMode::Sdr);
        let note = support_note(&no_display).1.join(" ");
        assert!(note.contains("no HDR-capable display"), "{note}");
    }

    #[test]
    fn signal_menu_only_offers_available_modes() {
        let hlg_only = HdrSupport { hlg: true, ..none() };
        let mut form = Form::new(&defaults(), &hlg_only);
        assert_eq!(form.dd[DD_SIGNAL].items, vec!["HLG".to_string()]);
        click(&mut form, check_hit_rect());
        assert_eq!(form.to_settings().hdr, HdrMode::Hlg);

        // HLG asked for on the command line but only HDR10 exists: shows what will really be used
        let pq_only = HdrSupport { hdr10: true, ..none() };
        let mut d = defaults();
        d.hdr = HdrMode::Hlg;
        d.signal = HdrMode::Hlg;
        let form = Form::new(&d, &pq_only);
        assert_eq!(form.to_settings().hdr, HdrMode::Hdr10);
        assert_eq!(form.dd[DD_SIGNAL].items, vec!["HDR10 (PQ)".to_string()]);
    }

    #[test]
    fn all_text_fits_in_the_window() {
        // the red message under the IP field (drawn at CTRL_X, 24px right margin)
        assert!(CTRL_X + text_width(IP_REQUIRED) <= W - LABEL_X, "IP message is {}px wide", text_width(IP_REQUIRED));
        assert!(CTRL_X + 36 + text_width("Enable HDR") <= W - LABEL_X);

        // every status note, in every situation, drawn at 8px per character
        let situations = [
            full(),
            HdrSupport { hdr10: true, ..none() },
            HdrSupport { hlg: true, ..none() },
            HdrSupport { kde_hdr_off: true, ..full() },
            HdrSupport { kde_no_hdr_display: true, ..full() },
            none(),
            HdrSupport { driver: "x11".into(), ..none() },
            HdrSupport { driver: "some-very-long-video-driver-name-that-goes-on-and-on".into(), ..none() },
            HdrSupport { problem: Some("x".repeat(300)), ..none() },
        ];
        let with_kde = |s: HdrSupport| HdrSupport {
            kde_luminance: Some(KdeLuminance { peak: Some(10000.0), min: Some(0.0005), sdr_brightness: Some(10000.0) }),
            ..s
        };
        let mut all = situations.to_vec();
        all.extend(situations.iter().cloned().map(with_kde));
        for support in &all {
            let lines = note_lines(support);
            assert!(lines.len() <= NOTE_MAX_LINES, "{} lines: {lines:?}", lines.len());
            for (_, line) in &lines {
                let width = line.chars().count() as i32 * NOTE_PX;
                assert!(width <= W - 2 * LABEL_X, "note too wide ({width}px): {line}");
            }
        }
        // the status block stays clear of the HDR checkbox above and the Signal menu below
        assert!(NOTE_Y >= check_hit_rect().bottom());
        assert!(NOTE_Y + 1 + NOTE_MAX_LINES as i32 * NOTE_LINE_H <= dd_rect(DD_SIGNAL).y() - 4);

        // a number being typed: digits, cursor and the unit must fit in front of the divider
        let typing = 8 + text_width(&"8".repeat(MAX_TYPED_CHARS)) + 8 + text_width("cd/m2");
        assert!(typing <= CTRL_W - ARROW_W - 4, "typing area needs {typing}px");

        // the KDE line comes right after "HDR available", on one line for typical values
        let typical = HdrSupport {
            kde_luminance: Some(KdeLuminance { peak: Some(1740.0), min: Some(0.0), sdr_brightness: Some(94.0) }),
            ..full()
        };
        let lines = note_lines(&typical);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].1.starts_with("HDR available"));
        assert_eq!(lines[1].1, "KDE: SDR brightness 94, peak 1740, black 0 nits");
        assert_eq!(kde_info_line(&full()), None, "no KDE values, no line");
        assert_eq!(note_lines(&full()).len(), 1);

        // the checkbox text and the Defaults button label
        assert!(CTRL_X + 36 + text_width(BYPASS_LABEL) <= W - LABEL_X, "Bypass KDE scaling label");
        assert!(text_width(DEFAULTS_LABEL) + 8 <= BTN_W, "Defaults label fits its button");

        // labels fit their column and menu entries fit the menu (leaving room for the arrow)
        let form = Form::new(&defaults(), &full());
        for d in &form.dd {
            assert!(LABEL_X + text_width(d.label) < CTRL_X - 8, "label '{}' is too wide", d.label);
            for item in &d.items {
                assert!(text_width(item) + 16 + 24 <= CTRL_W, "menu entry '{item}' is too wide");
            }
        }
    }

    /// Renders the form to /tmp so it can be looked at (run with `--ignored`).
    #[test]
    #[ignore]
    fn render_preview() {
        let render = |name: &str, form: &Form| {
            let surface = Surface::new(W as u32, form.height() as u32, PixelFormatEnum::RGB24).unwrap();
            let mut canvas = surface.into_canvas().unwrap();
            draw_form(&mut canvas, form);
            canvas.into_surface().save_bmp(format!("/tmp/startup_{name}.bmp")).unwrap();
        };

        let mut d = defaults();
        d.remote = "192.168.168.207".into();
        d.hdr = HdrMode::Hdr10;
        let mut form = Form::new(&d, &full());
        render("adv_closed", &form);

        // closed, with custom values inside: the reminder
        let mut custom = d.clone();
        custom.apply_saved(&remembered());
        let mut form_custom = Form::new(&custom, &full());
        form_custom.ip_selected = false;
        render("custom", &form_custom);

        render("all_visible", &form);
        form.dd[DD_MAX_LUM].open = true;
        form.mouse = form.item_rect(DD_MAX_LUM, 1).center();
        render("list_open", &form);
        form.close_all();
        form.focus = dd_focus_id(DD_MAX_LUM);
        form.dd[DD_MAX_LUM].type_chars("1200");
        form.dd[DD_MIN_LUM].start_edit(); // empty edit: shows the current value greyed
        render("typing", &form);

        // HDR unavailable / off: the HDR options are greyed out
        let mut empty = defaults();
        empty.hdr = HdrMode::Hdr10;
        let mut form = Form::new(&empty, &none());
        form.ip_error = true;
        render("unsupported_wayland", &form);
        render("unsupported_x11", &Form::new(&d, &HdrSupport { driver: "x11".into(), ..none() }));
        render("kde_off", &Form::new(&d, &HdrSupport { kde_hdr_off: true, ..full() }));
        render("kde_info", &Form::new(&d, &kde(Some(1740.0), Some(0.0))));
        render("gnome", &Form::new(&d, &HdrSupport { kde_session: false, ..full() }));
        render("kde_no_display", &Form::new(&d, &HdrSupport { kde_no_hdr_display: true, ..full() }));
    }
}
