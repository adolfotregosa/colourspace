//! Our own colour description for the HDR window, sent straight to the compositor through the
//! Wayland colour-management protocol (wp_color_management_v1).
//!
//! Why: when Mesa describes a Vulkan HDR window for us it always declares reference white as
//! 203 cd/m² and the "perceptual" rendering intent. KWin then scales every PQ value by
//! (its reference white) / 203 — its reference white being the SDR brightness setting — and may
//! tone-map. So PQ only arrived unchanged with SDR brightness at exactly 203, which makes the
//! desktop very bright.
//!
//! Instead the window is created as "pass-through" (Mesa then leaves its colour description
//! alone) and this module declares it: PQ or HLG, BT.2020, the reference white the compositor
//! itself uses for the screen (so the scale factor is exactly 1), the "relative colorimetric"
//! intent (KWin never tone-maps that), and the screen's own peak as the mastering peak (the HDR
//! values from the settings are not used: see `plan`). It keeps
//! listening, so a change of SDR brightness or of the brightness slider mid-session is followed.
//!
//! Everything is checked before anything is set on the window: if the compositor lacks something
//! this needs, `OwnDescription::new` returns the reason and nothing has been changed, so the
//! caller can use the previous way (Mesa's description) instead.

use std::ffi::c_void;

use wayland_client::backend::{Backend, ObjectId};
use wayland_client::protocol::{wl_registry, wl_surface::WlSurface};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_surface_feedback_v1::{self, WpColorManagementSurfaceFeedbackV1},
    wp_color_management_surface_v1::WpColorManagementSurfaceV1,
    wp_color_manager_v1::{self, Feature, Primaries as WlPrimaries, RenderIntent, TransferFunction, WpColorManagerV1},
    wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
    wp_image_description_info_v1::{self, WpImageDescriptionInfoV1},
    wp_image_description_v1::{self, WpImageDescriptionV1},
};

use crate::hdr::HdrMode;

// -------------------------------------------------------------------------------------
// What to declare (pure, unit-tested)
// -------------------------------------------------------------------------------------

/// The image description we create, in protocol units.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub transfer: TransferFunction,
    /// set_luminances: (min × 10000, max, reference white) in cd/m².
    pub luminances: (u32, u32, u32),
    /// set_mastering_display_primaries: r, g, b, white as x/y × 1 000 000.
    pub mastering_primaries: Option<[i32; 8]>,
    /// set_mastering_luminance: (min × 10000, max) in cd/m².
    pub mastering_luminance: Option<(u32, u32)>,
}

/// What the compositor said it can do (from wp_color_manager_v1's events).
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub intents: Vec<RenderIntent>,
    pub features: Vec<Feature>,
    pub transfers: Vec<TransferFunction>,
    pub primaries: Vec<WlPrimaries>,
}

impl Capabilities {
    fn has_feature(&self, f: Feature) -> bool {
        self.features.contains(&f)
    }
}

/// Decide what to declare for `mode`, given the compositor's reference white (cd/m², rounded as
/// the protocol carries it), the screen's peak if reported, and capabilities. `Err` says what is
/// missing; nothing returned here can be a protocol error (every rule of the protocol is checked).
///
/// The HDR values from the settings (Primaries, luminances, MaxCLL, MaxFALL) are not used: with
/// the relative intent KWin does not tone-map, and the only thing it takes from the metadata is
/// where to clip (MaxCLL when set, else the mastering max), so a value below the screen's peak
/// could only lower the light output, and one above it changes nothing. So the declaration is the
/// screen's own: BT.2020, black 0, the screen's peak, and no MaxCLL / MaxFALL.
pub fn plan(mode: HdrMode, reference: u32, screen_peak: Option<u32>, caps: &Capabilities) -> Result<Plan, String> {
    let transfer = match mode {
        HdrMode::Hdr10 => TransferFunction::St2084Pq,
        HdrMode::Hlg => TransferFunction::Hlg,
        HdrMode::Sdr => return Err("SDR does not use its own description".into()),
    };
    if !caps.has_feature(Feature::Parametric) {
        return Err("the compositor does not accept parametric image descriptions".into());
    }
    if !caps.has_feature(Feature::SetLuminances) {
        return Err("the compositor does not accept a reference white (set_luminances)".into());
    }
    if !caps.intents.contains(&RenderIntent::Relative) {
        return Err("the compositor does not offer the relative colorimetric intent".into());
    }
    if !caps.transfers.contains(&transfer) {
        return Err(format!("the compositor does not offer the {:?} transfer function", transfer));
    }
    if !caps.primaries.contains(&WlPrimaries::Bt2020) {
        return Err("the compositor does not offer BT.2020 primaries".into());
    }
    if reference == 0 {
        return Err("the compositor reported no reference white".into());
    }

    // Primary colour volume: the signal's own range. Black at 0 like the standards, so code 0 is
    // 0 cd/m². For PQ the protocol ignores 'max' (always min + 10000); HLG's nominal peak is
    // 1000 cd/m². The reference white is the compositor's own, so KWin's scale factor
    // (its reference / ours) is exactly 1.
    let max = match mode {
        HdrMode::Hlg => 1000,
        _ => 10_000,
    };
    let luminances = (0, max, reference);

    // Mastering display: BT.2020 (the signal's own primaries) from black 0 up to the screen's
    // peak, inside the primary volume. Without a reported peak nothing is sent, and KWin clips
    // at the screen anyway.
    let mastering_ok = caps.has_feature(Feature::SetMasteringDisplayPrimaries);
    let mastering_primaries = mastering_ok.then_some([708_000, 292_000, 170_000, 797_000, 131_000, 46_000, 312_700, 329_000]);
    let peak = screen_peak.unwrap_or(0).min(max);
    let mastering_luminance = (mastering_ok && peak > 0).then_some((0, peak));

    Ok(Plan { transfer, luminances, mastering_primaries, mastering_luminance })
}

// -------------------------------------------------------------------------------------
// Protocol state
// -------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Pending,
    Ready,
    Failed(String),
}

/// Which image description an event is for.
#[derive(Debug, Clone, Copy)]
enum Desc {
    Preferred,
    Ours,
}

#[derive(Debug, Default)]
struct State {
    manager_global: Option<(u32, u32)>,
    caps: Capabilities,
    preferred: Option<Status>,
    ours: Option<Status>,
    /// From the preferred description's information: luminances(min, max, reference) and
    /// target_luminance(min, max).
    luminances: Option<(u32, u32, u32)>,
    target_luminance: Option<(u32, u32)>,
    info_done: bool,
    preferred_changed: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(state: &mut Self, _: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            if interface == WpColorManagerV1::interface().name {
                state.manager_global = Some((name, version));
            }
        }
    }
}

impl Dispatch<WpColorManagerV1, ()> for State {
    fn event(state: &mut Self, _: &WpColorManagerV1, event: wp_color_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use wp_color_manager_v1::Event as E;
        match event {
            E::SupportedIntent { render_intent: WEnum::Value(v) } => state.caps.intents.push(v),
            E::SupportedFeature { feature: WEnum::Value(v) } => state.caps.features.push(v),
            E::SupportedTfNamed { tf: WEnum::Value(v) } => state.caps.transfers.push(v),
            E::SupportedPrimariesNamed { primaries: WEnum::Value(v) } => state.caps.primaries.push(v),
            _ => {}
        }
    }
}

impl Dispatch<WpColorManagementSurfaceFeedbackV1, ()> for State {
    fn event(state: &mut Self, _: &WpColorManagementSurfaceFeedbackV1, event: wp_color_management_surface_feedback_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use wp_color_management_surface_feedback_v1::Event as E;
        if matches!(event, E::PreferredChanged { .. } | E::PreferredChanged2 { .. }) {
            state.preferred_changed = true;
        }
    }
}

impl Dispatch<WpImageDescriptionV1, Desc> for State {
    fn event(state: &mut Self, _: &WpImageDescriptionV1, event: wp_image_description_v1::Event, which: &Desc, _: &Connection, _: &QueueHandle<Self>) {
        use wp_image_description_v1::Event as E;
        let status = match event {
            E::Ready { .. } | E::Ready2 { .. } => Status::Ready,
            E::Failed { cause, msg } => Status::Failed(format!("{msg} ({cause:?})")),
            _ => return,
        };
        match which {
            Desc::Preferred => state.preferred = Some(status),
            Desc::Ours => state.ours = Some(status),
        }
    }
}

impl Dispatch<WpImageDescriptionInfoV1, ()> for State {
    fn event(state: &mut Self, _: &WpImageDescriptionInfoV1, event: wp_image_description_info_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use wp_image_description_info_v1::Event as E;
        match event {
            E::Luminances { min_lum, max_lum, reference_lum } => state.luminances = Some((min_lum, max_lum, reference_lum)),
            E::TargetLuminance { min_lum, max_lum } => state.target_luminance = Some((min_lum, max_lum)),
            E::Done => state.info_done = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WpColorManagementSurfaceV1);
delegate_noop!(State: ignore WpImageDescriptionCreatorParamsV1);
delegate_noop!(State: ignore WlSurface);

// -------------------------------------------------------------------------------------
// The live description
// -------------------------------------------------------------------------------------

/// What the compositor uses for the screen, from its preferred description.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Screen {
    pub reference: u32,
    /// target_luminance: black (cd/m²) and peak (cd/m²), if reported.
    pub black: Option<f32>,
    pub peak: Option<u32>,
}

/// Our description, set on the HDR window. Dropping it removes it again (before the window goes).
pub struct OwnDescription {
    _conn: Connection,
    queue: EventQueue<State>,
    state: State,
    manager: WpColorManagerV1,
    feedback: WpColorManagementSurfaceFeedbackV1,
    surface: Option<WpColorManagementSurfaceV1>,
    mode: HdrMode,
    screen: Screen,
    plan: Plan,
}

fn wayland_handles(window: &sdl2::video::Window) -> Result<(*mut c_void, *mut c_void), String> {
    use sdl2::sys::{SDL_GetVersion, SDL_GetWindowWMInfo, SDL_SYSWM_TYPE, SDL_SysWMinfo, SDL_bool};
    let mut info: SDL_SysWMinfo = unsafe { std::mem::zeroed() };
    unsafe {
        SDL_GetVersion(&mut info.version);
        if SDL_GetWindowWMInfo(window.raw(), &mut info) == SDL_bool::SDL_FALSE {
            return Err(format!("SDL has no window information: {}", sdl2::get_error()));
        }
    }
    if info.subsystem != SDL_SYSWM_TYPE::SDL_SYSWM_WAYLAND {
        return Err("the window is not a native Wayland window".into());
    }
    let wl = unsafe { info.info.wl };
    if wl.display.is_null() || wl.surface.is_null() {
        return Err("SDL did not provide the Wayland display and surface".into());
    }
    Ok((wl.display as *mut c_void, wl.surface as *mut c_void))
}

fn dispatch_error(what: &str, e: impl std::fmt::Display) -> String {
    format!("Wayland error while {what}: {e}")
}

impl OwnDescription {
    /// Check that the compositor can take our description, read its reference white, create the
    /// description and set it on `window`'s surface (it takes effect with the next frame).
    ///
    /// Must be called after the Vulkan swapchain for the window was created as pass-through
    /// (Mesa only attaches its own colour object to the surface for other colour spaces) and
    /// before the first frame is presented. On `Err` nothing was set on the window.
    pub fn new(window: &sdl2::video::Window, mode: HdrMode) -> Result<Self, String> {
        let (display, surface_ptr) = wayland_handles(window)?;
        unsafe { Self::from_raw(display, surface_ptr, mode) }
    }

    /// The same, for a raw `wl_display` and `wl_surface` (SDL's, or a test's).
    ///
    /// # Safety
    /// Both pointers must be live libwayland objects of the same connection, and must outlive
    /// the returned value.
    unsafe fn from_raw(display: *mut c_void, surface_ptr: *mut c_void, mode: HdrMode) -> Result<Self, String> {
        // SDL's own Wayland connection, wrapped (not taken over): our objects live on our own
        // event queue, and SDL keeps reading the socket as before.
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let conn = Connection::from_backend(backend);
        let mut queue = conn.new_event_queue::<State>();
        let qh = queue.handle();
        let mut state = State::default();

        let _registry = conn.display().get_registry(&qh, ());
        queue.roundtrip(&mut state).map_err(|e| dispatch_error("listing globals", e))?;
        let (name, version) = state.manager_global.ok_or("the compositor has no colour-management protocol")?;
        let registry = _registry;
        let manager: WpColorManagerV1 = registry.bind(name, version.min(3), &qh, ());
        queue.roundtrip(&mut state).map_err(|e| dispatch_error("reading capabilities", e))?;

        // SDL's wl_surface, as a proxy on this connection (not owned: SDL destroys it)
        let surface_id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface_ptr.cast()) }
            .map_err(|_| "the window's Wayland surface is not usable".to_string())?;
        let wl_surface = WlSurface::from_id(&conn, surface_id).map_err(|_| "the window's Wayland surface is not usable".to_string())?;

        let feedback = manager.get_surface_feedback(&wl_surface, &qh, ());
        let mut this = Self {
            _conn: conn,
            queue,
            state,
            manager,
            feedback,
            surface: None,
            mode,
            screen: Screen { reference: 0, black: None, peak: None },
            plan: Plan {
                transfer: TransferFunction::St2084Pq,
                luminances: (0, 0, 0),
                mastering_primaries: None,
                mastering_luminance: None,
            },
        };
        let result = (|| {
            this.screen = this.query_screen()?;
            this.plan = plan(mode, this.screen.reference, this.screen.peak, &this.state.caps)?;
            let desc = this.create_description()?;
            // only now, with everything known to work, the window gets our colour object
            let qh = this.queue.handle();
            let surface = this.manager.get_surface(&wl_surface, &qh, ());
            surface.set_image_description(&desc, RenderIntent::Relative);
            desc.destroy();
            this.surface = Some(surface);
            this.queue.flush().map_err(|e| dispatch_error("setting the description", e))?;
            Ok::<(), String>(())
        })();
        result.map(|()| this)
    }

    /// The compositor's preferred description for the window: its reference white and range.
    fn query_screen(&mut self) -> Result<Screen, String> {
        let qh = self.queue.handle();
        self.state.preferred = Some(Status::Pending);
        self.state.luminances = None;
        self.state.target_luminance = None;
        self.state.info_done = false;
        let preferred = self.feedback.get_preferred(&qh, Desc::Preferred);
        self.queue.roundtrip(&mut self.state).map_err(|e| dispatch_error("asking for the screen", e))?;
        if let Some(Status::Failed(why)) = &self.state.preferred {
            preferred.destroy();
            return Err(format!("the compositor did not describe the screen: {why}"));
        }
        let _info = preferred.get_information(&qh, ());
        self.queue.roundtrip(&mut self.state).map_err(|e| dispatch_error("reading the screen", e))?;
        preferred.destroy();
        if !self.state.info_done {
            return Err("the compositor did not finish describing the screen".into());
        }
        let (_, _, reference) = self.state.luminances.ok_or("the compositor reported no reference white")?;
        let (black, peak) = match self.state.target_luminance {
            Some((min, max)) => (Some(min as f32 / 10_000.0), Some(max)),
            None => (None, None),
        };
        Ok(Screen { reference, black, peak })
    }

    /// Create our image description from `self.plan` and wait until the compositor accepts it.
    fn create_description(&mut self) -> Result<WpImageDescriptionV1, String> {
        let qh = self.queue.handle();
        let p = &self.plan;
        let creator = self.manager.create_parametric_creator(&qh, ());
        creator.set_tf_named(p.transfer);
        creator.set_primaries_named(WlPrimaries::Bt2020);
        creator.set_luminances(p.luminances.0, p.luminances.1, p.luminances.2);
        if let Some([rx, ry, gx, gy, bx, by, wx, wy]) = p.mastering_primaries {
            creator.set_mastering_display_primaries(rx, ry, gx, gy, bx, by, wx, wy);
        }
        if let Some((min, max)) = p.mastering_luminance {
            creator.set_mastering_luminance(min, max);
        }
        self.state.ours = Some(Status::Pending);
        let desc = creator.create(&qh, Desc::Ours);
        self.queue.roundtrip(&mut self.state).map_err(|e| dispatch_error("creating the description", e))?;
        match self.state.ours.take() {
            Some(Status::Ready) => Ok(desc),
            Some(Status::Failed(why)) => {
                desc.destroy();
                Err(format!("the compositor refused our description: {why}"))
            }
            _ => {
                desc.destroy();
                Err("the compositor did not answer for our description".into())
            }
        }
    }

    /// Call once per frame: if the compositor's screen description changed (SDR brightness, the
    /// brightness slider, another screen), declare the new reference white and peak. Returns a
    /// message when something changed.
    pub fn update(&mut self) -> Result<Option<String>, String> {
        self.queue.dispatch_pending(&mut self.state).map_err(|e| dispatch_error("reading events", e))?;
        if !std::mem::take(&mut self.state.preferred_changed) {
            return Ok(None);
        }
        let screen = self.query_screen()?;
        if screen.reference == self.screen.reference && screen.peak == self.screen.peak {
            self.screen = screen;
            return Ok(None);
        }
        let old = self.screen;
        self.screen = screen;
        self.plan = plan(self.mode, screen.reference, screen.peak, &self.state.caps)?;
        let desc = self.create_description()?;
        if let Some(surface) = &self.surface {
            surface.set_image_description(&desc, RenderIntent::Relative);
        }
        desc.destroy();
        self.queue.flush().map_err(|e| dispatch_error("setting the description", e))?;
        let peak = |p: Option<u32>| p.map(|v| v.to_string()).unwrap_or_else(|| "?".into());
        Ok(Some(format!(
            "screen changed (reference white {} → {} cd/m², peak {} → {} cd/m²), description updated",
            old.reference,
            screen.reference,
            peak(old.peak),
            peak(screen.peak),
        )))
    }

    /// One line for the terminal: what is declared and what the compositor uses.
    pub fn summary(&self) -> String {
        let p = &self.plan;
        format!(
            "own description: {:?}, BT.2020, reference white {} cd/m² (the compositor's), intent relative; \
             mastering {} (the screen's peak), no MaxCLL/MaxFALL; compositor peak {}, black {}\n  \
             the HDR values from the settings (Primaries, luminances, MaxCLL, MaxFALL) are not used with Bypass KDE scaling",
            p.transfer,
            p.luminances.2,
            p.mastering_luminance
                .map(|(min, max)| format!("{}–{} cd/m²", min as f32 / 10_000.0, max))
                .unwrap_or_else(|| "not sent".into()),
            self.screen.peak.map(|v| format!("{v} cd/m²")).unwrap_or_else(|| "?".into()),
            self.screen.black.map(|v| format!("{v} cd/m²")).unwrap_or_else(|| "?".into()),
        )
    }
}

impl Drop for OwnDescription {
    fn drop(&mut self) {
        if let Some(surface) = self.surface.take() {
            surface.destroy(); // the window goes back to having no description of ours
        }
        self.feedback.destroy();
        self.manager.destroy();
        let _ = self.queue.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kwin() -> Capabilities {
        // what KWin 6.8 advertises (src/wayland/colormanagement_v1.cpp)
        Capabilities {
            intents: vec![RenderIntent::Perceptual, RenderIntent::Relative, RenderIntent::Absolute, RenderIntent::RelativeBpc],
            features: vec![
                Feature::Parametric,
                Feature::SetPrimaries,
                Feature::SetTfPower,
                Feature::SetLuminances,
                Feature::SetMasteringDisplayPrimaries,
                Feature::ExtendedTargetVolume,
            ],
            transfers: vec![TransferFunction::Srgb, TransferFunction::St2084Pq, TransferFunction::Hlg, TransferFunction::Gamma22],
            primaries: vec![WlPrimaries::Srgb, WlPrimaries::Bt2020],
        }
    }

    const BT2020: [i32; 8] = [708_000, 292_000, 170_000, 797_000, 131_000, 46_000, 312_700, 329_000];

    #[test]
    fn the_screens_own_reference_white_and_peak_are_declared() {
        let p = plan(HdrMode::Hdr10, 94, Some(1740), &kwin()).unwrap();
        assert_eq!(p.transfer, TransferFunction::St2084Pq);
        assert_eq!(p.luminances, (0, 10_000, 94), "black 0, PQ range, KWin's 94 cd/m² as reference");
        assert_eq!(p.mastering_luminance, Some((0, 1740)), "KWin clips at the screen's peak, never lower");
        assert_eq!(p.mastering_primaries, Some(BT2020));

        let hlg = plan(HdrMode::Hlg, 203, Some(800), &kwin()).unwrap();
        assert_eq!((hlg.transfer, hlg.luminances, hlg.mastering_luminance), (TransferFunction::Hlg, (0, 1000, 203), Some((0, 800))));
    }

    #[test]
    fn nothing_that_would_be_a_protocol_error_is_ever_sent() {
        // a peak above the signal's volume is limited to it
        assert_eq!(plan(HdrMode::Hdr10, 94, Some(20_000), &kwin()).unwrap().mastering_luminance, Some((0, 10_000)));
        assert_eq!(plan(HdrMode::Hlg, 94, Some(1740), &kwin()).unwrap().mastering_luminance, Some((0, 1000)));
        // no (or a zero) peak: no mastering luminance (max must lie above min)
        assert_eq!(plan(HdrMode::Hdr10, 94, None, &kwin()).unwrap().mastering_luminance, None);
        assert_eq!(plan(HdrMode::Hdr10, 94, Some(0), &kwin()).unwrap().mastering_luminance, None);
    }

    #[test]
    fn without_mastering_support_no_mastering_values_are_sent() {
        let mut caps = kwin();
        caps.features.retain(|f| *f != Feature::SetMasteringDisplayPrimaries);
        let p = plan(HdrMode::Hdr10, 94, Some(1740), &caps).unwrap();
        assert_eq!((p.mastering_primaries, p.mastering_luminance), (None, None));
    }

    #[test]
    fn anything_missing_means_the_previous_way_is_used() {
        let without = |f: &dyn Fn(&mut Capabilities)| {
            let mut caps = kwin();
            f(&mut caps);
            plan(HdrMode::Hdr10, 94, Some(1740), &caps)
        };
        assert!(without(&|c| c.features.retain(|f| *f != Feature::SetLuminances)).unwrap_err().contains("reference white"));
        assert!(without(&|c| c.features.retain(|f| *f != Feature::Parametric)).is_err());
        assert!(without(&|c| c.intents.retain(|i| *i != RenderIntent::Relative)).unwrap_err().contains("relative"));
        assert!(without(&|c| c.transfers.retain(|t| *t != TransferFunction::St2084Pq)).is_err());
        assert!(without(&|c| c.primaries.retain(|p| *p != WlPrimaries::Bt2020)).is_err());
        assert!(plan(HdrMode::Hdr10, 0, Some(1740), &kwin()).is_err(), "no reference white reported");
        assert!(plan(HdrMode::Sdr, 94, Some(1740), &kwin()).is_err());
        // HLG needs HLG
        let mut caps = kwin();
        caps.transfers.retain(|t| *t != TransferFunction::Hlg);
        assert!(plan(HdrMode::Hlg, 94, Some(1740), &caps).is_err());
        assert!(plan(HdrMode::Hdr10, 94, Some(1740), &caps).is_ok());
    }
}

/// Talks to a stand-in compositor that implements the colour-management protocol the way KWin
/// 6.8 does (what it advertises, the answers it sends, and the protocol errors it raises), over a
/// real Wayland connection in libwayland: everything our client sends and does with the answers.
#[cfg(test)]
mod protocol_tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::wl_compositor::WlCompositor as CCompositor;
    use wayland_protocols::wp::color_management::v1::server as cm;
    use wayland_server::protocol::{wl_compositor, wl_surface};
    use wayland_server::{backend::ClientData, Client, DataInit, Display, DisplayHandle, GlobalDispatch, New, Resource};

    // ---- the stand-in compositor -------------------------------------------------------

    #[derive(Clone, Default)]
    struct Options {
        no_manager: bool,
        no_relative: bool,
        refuse_descriptions: bool,
    }

    struct Srv {
        opts: Options,
        log: Arc<Mutex<Vec<String>>>,
        reference: Arc<AtomicU32>,
        feedbacks: Vec<cm::wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1>,
        /// Information objects to answer right after the request that created them (this server
        /// library cannot destroy an object inside the handler that creates it; libwayland can).
        pending_info: Vec<cm::wp_image_description_info_v1::WpImageDescriptionInfoV1>,
    }

    impl Srv {
        fn log(&self, line: String) {
            self.log.lock().unwrap().push(line);
        }
    }

    #[derive(Debug, Default)]
    struct Params {
        tf: Option<String>,
        primaries: Option<String>,
        luminances: Option<(u32, u32, u32)>,
        mastering_primaries: bool,
        mastering_luminance: Option<(u32, u32)>,
        max_cll: Option<u32>,
        max_fall: Option<u32>,
    }

    enum Kind {
        Preferred,
        Created(String),
    }

    struct NoData;
    impl ClientData for NoData {}

    impl GlobalDispatch<wl_compositor::WlCompositor, ()> for Srv {
        fn bind(_: &mut Self, _: &DisplayHandle, _: &Client, r: New<wl_compositor::WlCompositor>, _: &(), init: &mut DataInit<'_, Self>) {
            init.init(r, ());
        }
    }
    impl wayland_server::Dispatch<wl_compositor::WlCompositor, ()> for Srv {
        fn request(_: &mut Self, _: &Client, _: &wl_compositor::WlCompositor, req: wl_compositor::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
            if let wl_compositor::Request::CreateSurface { id } = req {
                init.init(id, ());
            }
        }
    }
    impl wayland_server::Dispatch<wl_surface::WlSurface, ()> for Srv {
        fn request(_: &mut Self, _: &Client, _: &wl_surface::WlSurface, _: wl_surface::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
    }

    impl GlobalDispatch<cm::wp_color_manager_v1::WpColorManagerV1, ()> for Srv {
        fn bind(state: &mut Self, _: &DisplayHandle, _: &Client, r: New<cm::wp_color_manager_v1::WpColorManagerV1>, _: &(), init: &mut DataInit<'_, Self>) {
            use cm::wp_color_manager_v1::{Feature as F, Primaries as P, RenderIntent as I, TransferFunction as T};
            let m = init.init(r, ());
            // as KWin 6.8 (src/wayland/colormanagement_v1.cpp)
            m.supported_intent(I::Perceptual);
            if !state.opts.no_relative {
                m.supported_intent(I::Relative);
            }
            m.supported_intent(I::Absolute);
            m.supported_intent(I::RelativeBpc);
            for f in [F::Parametric, F::SetPrimaries, F::SetTfPower, F::SetLuminances, F::SetMasteringDisplayPrimaries, F::ExtendedTargetVolume] {
                m.supported_feature(f);
            }
            for t in [T::Srgb, T::St2084Pq, T::Hlg, T::Gamma22] {
                m.supported_tf_named(t);
            }
            for p in [P::Srgb, P::Bt2020] {
                m.supported_primaries_named(p);
            }
            m.done();
        }
    }
    impl wayland_server::Dispatch<cm::wp_color_manager_v1::WpColorManagerV1, ()> for Srv {
        fn request(state: &mut Self, _: &Client, _: &cm::wp_color_manager_v1::WpColorManagerV1, req: cm::wp_color_manager_v1::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
            use cm::wp_color_manager_v1::Request as R;
            match req {
                R::GetSurface { id, .. } => {
                    init.init(id, ());
                    state.log("get_surface".into());
                }
                R::GetSurfaceFeedback { id, .. } => {
                    let fb = init.init(id, ());
                    state.feedbacks.push(fb);
                }
                R::CreateParametricCreator { obj } => {
                    init.init(obj, Mutex::new(Params::default()));
                }
                R::Destroy => state.log("manager destroyed".into()),
                other => panic!("unexpected request {other:?}"),
            }
        }
    }

    impl wayland_server::Dispatch<cm::wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1, Mutex<Params>> for Srv {
        fn request(
            state: &mut Self,
            _: &Client,
            res: &cm::wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
            req: cm::wp_image_description_creator_params_v1::Request,
            params: &Mutex<Params>,
            _: &DisplayHandle,
            init: &mut DataInit<'_, Self>,
        ) {
            use cm::wp_image_description_creator_params_v1::{Error as E, Request as R};
            let mut p = params.lock().unwrap();
            match req {
                R::SetTfNamed { tf } => p.tf = Some(format!("{tf:?}")),
                R::SetPrimariesNamed { primaries } => p.primaries = Some(format!("{primaries:?}")),
                R::SetLuminances { min_lum, max_lum, reference_lum } => {
                    // the protocol's (and KWin's) rules
                    if (max_lum as u64) * 10_000 <= min_lum as u64 || (reference_lum as u64) * 10_000 <= min_lum as u64 {
                        res.post_error(E::InvalidLuminance, "luminances");
                        return;
                    }
                    p.luminances = Some((min_lum, max_lum, reference_lum));
                }
                R::SetMasteringDisplayPrimaries { .. } => p.mastering_primaries = true,
                R::SetMasteringLuminance { min_lum, max_lum } => {
                    if (max_lum as u64) * 10_000 <= min_lum as u64 {
                        res.post_error(E::InvalidLuminance, "mastering luminance");
                        return;
                    }
                    p.mastering_luminance = Some((min_lum, max_lum));
                }
                R::SetMaxCll { max_cll } => p.max_cll = Some(max_cll),
                R::SetMaxFall { max_fall } => p.max_fall = Some(max_fall),
                R::Create { image_description } => {
                    if p.tf.is_none() || p.primaries.is_none() {
                        res.post_error(E::IncompleteSet, "tf or primaries missing");
                        return;
                    }
                    let summary = format!("{:?}", *p);
                    let d = init.init(image_description, Kind::Created(summary.clone()));
                    if state.opts.refuse_descriptions {
                        d.failed(cm::wp_image_description_v1::Cause::Unsupported, "refused by the test".into());
                    } else {
                        d.ready2(0, 7);
                    }
                    state.log(format!("created {summary}"));
                }
                other => panic!("unexpected request {other:?}"),
            }
        }
    }

    impl wayland_server::Dispatch<cm::wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1, ()> for Srv {
        fn request(
            state: &mut Self,
            _: &Client,
            _: &cm::wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1,
            req: cm::wp_color_management_surface_feedback_v1::Request,
            _: &(),
            _: &DisplayHandle,
            init: &mut DataInit<'_, Self>,
        ) {
            use cm::wp_color_management_surface_feedback_v1::Request as R;
            match req {
                R::GetPreferred { image_description } | R::GetPreferredParametric { image_description } => {
                    init.init(image_description, Kind::Preferred).ready2(0, 1);
                }
                R::Destroy => state.log("feedback destroyed".into()),
                other => panic!("unexpected request {other:?}"),
            }
        }
    }

    impl wayland_server::Dispatch<cm::wp_image_description_v1::WpImageDescriptionV1, Kind> for Srv {
        fn request(
            state: &mut Self,
            _: &Client,
            res: &cm::wp_image_description_v1::WpImageDescriptionV1,
            req: cm::wp_image_description_v1::Request,
            kind: &Kind,
            _: &DisplayHandle,
            init: &mut DataInit<'_, Self>,
        ) {
            use cm::wp_image_description_v1::Request as R;
            if let R::GetInformation { information } = req {
                if !matches!(kind, Kind::Preferred) {
                    res.post_error(cm::wp_image_description_v1::Error::NoInformation, "not allowed");
                    return;
                }
                let info = init.init(information, ());
                state.pending_info.push(info);
            }
        }
    }
    impl wayland_server::Dispatch<cm::wp_image_description_info_v1::WpImageDescriptionInfoV1, ()> for Srv {
        fn request(_: &mut Self, _: &Client, _: &cm::wp_image_description_info_v1::WpImageDescriptionInfoV1, _: cm::wp_image_description_info_v1::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
    }

    impl wayland_server::Dispatch<cm::wp_color_management_surface_v1::WpColorManagementSurfaceV1, ()> for Srv {
        fn request(
            state: &mut Self,
            _: &Client,
            _: &cm::wp_color_management_surface_v1::WpColorManagementSurfaceV1,
            req: cm::wp_color_management_surface_v1::Request,
            _: &(),
            _: &DisplayHandle,
            _: &mut DataInit<'_, Self>,
        ) {
            use cm::wp_color_management_surface_v1::Request as R;
            match req {
                R::SetImageDescription { image_description, render_intent } => {
                    let what = match image_description.data::<Kind>() {
                        Some(Kind::Created(s)) => s.clone(),
                        _ => "not one of ours".into(),
                    };
                    state.log(format!("set_image_description {render_intent:?} {what}"));
                }
                R::UnsetImageDescription => state.log("unset_image_description".into()),
                R::Destroy => state.log("surface destroyed".into()),
                other => panic!("unexpected request {other:?}"),
            }
        }
    }

    /// Runs the stand-in compositor on its own thread.
    struct Server {
        log: Arc<Mutex<Vec<String>>>,
        reference: Arc<AtomicU32>,
        changed: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Server {
        fn start(opts: Options, stream: UnixStream) -> Self {
            let log = Arc::new(Mutex::new(Vec::new()));
            let reference = Arc::new(AtomicU32::new(94));
            let changed = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let (l, r, c, s) = (log.clone(), reference.clone(), changed.clone(), stop.clone());
            let thread = std::thread::spawn(move || {
                let mut display: Display<Srv> = Display::new().unwrap();
                let mut dh = display.handle();
                dh.create_global::<Srv, wl_compositor::WlCompositor, ()>(6, ());
                if !opts.no_manager {
                    dh.create_global::<Srv, cm::wp_color_manager_v1::WpColorManagerV1, ()>(3, ());
                }
                dh.insert_client(stream, Arc::new(NoData)).unwrap();
                let mut srv = Srv { opts, log: l, reference: r, feedbacks: Vec::new(), pending_info: Vec::new() };
                while !s.load(Ordering::SeqCst) {
                    if c.swap(false, Ordering::SeqCst) {
                        for fb in &srv.feedbacks {
                            fb.preferred_changed2(0, 2);
                        }
                    }
                    let _ = display.dispatch_clients(&mut srv);
                    for info in srv.pending_info.drain(..) {
                        // what KWin sends for an HDR screen: PQ range, the reference white, black/peak
                        info.luminances(0, 10_000, srv.reference.load(Ordering::SeqCst));
                        info.target_luminance(0, 1740);
                        info.done();
                    }
                    let _ = display.flush_clients();
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            Server { log, reference, changed, stop, thread: Some(thread) }
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        /// The log once it has at least `n` lines (requests sent without a reply arrive a little
        /// later), or as it is after 3 seconds.
        fn log_with(&self, n: usize) -> Vec<String> {
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.log().len() < n && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.log()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    // ---- the client side: plays SDL (owns the connection and the surface) ----------------

    struct Sdl;
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Sdl {
        fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }
    delegate_noop!(Sdl: ignore CCompositor);
    delegate_noop!(Sdl: ignore WlSurface);

    struct Setup {
        server: Server,
        conn: Connection,
        queue: EventQueue<Sdl>,
        surface: WlSurface,
    }

    fn setup(opts: Options) -> Setup {
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let server = Server::start(opts, server_end);
        let conn = Connection::from_backend(Backend::connect(client_end).expect("libwayland-client"));
        let (globals, mut queue) = registry_queue_init::<Sdl>(&conn).unwrap();
        let compositor: CCompositor = globals.bind(&queue.handle(), 1..=6, ()).unwrap();
        let surface = compositor.create_surface(&queue.handle(), ());
        queue.roundtrip(&mut Sdl).unwrap();
        Setup { server, conn, queue, surface }
    }

    fn own(s: &Setup, mode: HdrMode) -> Result<OwnDescription, String> {
        let display = s.conn.backend().display_ptr() as *mut c_void;
        let surface = s.surface.id().as_ptr() as *mut c_void;
        unsafe { OwnDescription::from_raw(display, surface, mode) }
    }

    #[test]
    fn our_description_is_declared_with_the_compositors_reference_white() {
        let s = setup(Options::default());
        let d = own(&s, HdrMode::Hdr10).expect("accepted");
        let log = s.server.log_with(3);
        assert_eq!(log.len(), 3, "{log:#?}");
        assert!(log[0].starts_with("created"), "description created first: {log:#?}");
        assert!(log[0].contains("tf: Some(\"Value(St2084Pq)\")") && log[0].contains("primaries: Some(\"Value(Bt2020)\")"), "{}", log[0]);
        assert!(log[0].contains("luminances: Some((0, 10000, 94))"), "KWin's reference white 94: {}", log[0]);
        assert!(log[0].contains("mastering_luminance: Some((0, 1740))") && log[0].contains("mastering_primaries: true"), "{}", log[0]);
        assert!(log[0].contains("max_cll: None") && log[0].contains("max_fall: None"), "{}", log[0]);
        assert_eq!(log[1], "get_surface", "the window gets our colour object only after the description was accepted");
        assert!(log[2].starts_with("set_image_description Value(Relative) "), "{}", log[2]);
        assert!(d.summary().contains("reference white 94"));
        assert!(d.summary().contains("peak 1740"));
        assert!(d.summary().contains("not used with Bypass KDE scaling"));

        // going away removes everything again
        drop(d);
        assert_eq!(&s.server.log_with(6)[3..], ["surface destroyed", "feedback destroyed", "manager destroyed"]);
    }

    #[test]
    fn a_new_reference_white_is_followed() {
        let mut s = setup(Options::default());
        let mut d = own(&s, HdrMode::Hdr10).unwrap();
        assert_eq!(d.update().unwrap(), None, "nothing changed yet");

        s.server.reference.store(150, Ordering::SeqCst); // e.g. SDR brightness moved
        s.server.changed.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(3);
        let message = loop {
            // in the program, SDL's event pump reads the socket; here the "SDL" queue does
            let _ = s.queue.roundtrip(&mut Sdl);
            if let Some(m) = d.update().unwrap() {
                break m;
            }
            assert!(Instant::now() < deadline, "the change was never seen");
        };
        assert!(message.contains("94 → 150") && message.contains("peak 1740 → 1740"), "{message}");
        let log = s.server.log_with(5);
        assert!(log[3].starts_with("created") && log[3].contains("luminances: Some((0, 10000, 150))"), "{log:#?}");
        assert!(log[4].starts_with("set_image_description Value(Relative) ") && log[4].contains("150"), "{log:#?}");
        assert!(d.summary().contains("reference white 150"));
    }

    #[test]
    fn hlg_is_declared_with_the_screens_peak_inside_its_range() {
        // the stand-in screen's peak is 1740 cd/m²; HLG's volume ends at 1000
        let s = setup(Options::default());
        own(&s, HdrMode::Hlg).unwrap();
        let created = &s.server.log_with(1)[0];
        assert!(created.contains("Value(Hlg)") && created.contains("luminances: Some((0, 1000, 94))"), "{created}");
        assert!(created.contains("mastering_luminance: Some((0, 1000))"), "{created}");
        assert!(created.contains("max_cll: None") && created.contains("max_fall: None"), "{created}");
    }

    #[test]
    fn nothing_is_set_on_the_window_when_the_compositor_lacks_something() {
        // no colour-management protocol at all (e.g. an older compositor)
        let s = setup(Options { no_manager: true, ..Default::default() });
        let err = own(&s, HdrMode::Hdr10).err().unwrap();
        assert!(err.contains("no colour-management protocol"), "{err}");

        // no relative intent
        let s = setup(Options { no_relative: true, ..Default::default() });
        let err = own(&s, HdrMode::Hdr10).err().unwrap();
        assert!(err.contains("relative"), "{err}");
        assert!(!s.server.log().iter().any(|l| l == "get_surface"), "{:#?}", s.server.log());

        // our description refused
        let s = setup(Options { refuse_descriptions: true, ..Default::default() });
        let err = own(&s, HdrMode::Hdr10).err().unwrap();
        assert!(err.contains("refused"), "{err}");
        assert!(!s.server.log().iter().any(|l| l == "get_surface"), "the window was left alone: {:#?}", s.server.log());

        // and in every case the connection is still healthy for its owner (SDL)
        let mut s = setup(Options { refuse_descriptions: true, ..Default::default() });
        let _ = own(&s, HdrMode::Hdr10);
        s.queue.roundtrip(&mut Sdl).expect("SDL's connection still works");
    }
}
