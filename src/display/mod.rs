//! The A380X's cockpit screens: FlyByWire's instruments draw a command
//! stream per screen (docs/display-stream.md) and this module shows them on
//! X-Plane cockpit devices (docs/screens.md).
//!
//! A new stream is read and tessellated once, when it arrives, on the
//! thread the instruments run on ([`Tess`]), and the mesh handed to the draw
//! side under a brief lock. X-Plane calls each device's draw callback every
//! frame, which uploads the vertices when the stream is new and replays the
//! cached batches into the device's texture, dimmed to its knobs. Mouse clicks,
//! drags and the wheel over a device come back as screen events for the
//! scripts, in the screen's CSS pixels.

pub mod image;
pub mod path;
pub mod popout;
pub mod plan;
pub mod screens;
pub mod stream;
pub mod tessellate;
mod view_scale;
pub mod text;
mod gl;
#[cfg(test)]
mod soft;
#[cfg(test)]
mod tests;
mod xphfbw;

/// The display units' ambient-light sensor: automatic brightness on top of
/// each screen's knob. GENERIC curve (not from FlyByWire or Airbus figures):
/// a screen runs at [`NIGHT_LEVEL`] of its knob setting in darkness and at
/// the full knob setting in daylight, rising with the square root of the
/// daylight level (a photocell reads dim light as relatively bright). The
/// level lags with [`TIME_CONSTANT_S`] and moves in 1/256 steps, so a settled
/// sensor stops asking for redraws.
///
/// Daylight is X-Plane's `sim/graphics/misc/outside_light_level` (0..1);
/// without it, the sun's elevation over civil twilight (-6 to +6 degrees)
/// dimmed by up to [`CLOUD_DIM`] under the densest cloud layer.
pub(crate) mod ambient {
    pub const NIGHT_LEVEL: f64 = 0.6;
    pub const TIME_CONSTANT_S: f64 = 4.;
    pub const CLOUD_DIM: f64 = 0.4;

    pub fn daylight_from_sun(sun_pitch_deg: f64, cloud_coverage: f64) -> f64 {
        let sun = ((sun_pitch_deg + 6.) / 12.).clamp(0., 1.);
        sun * (1. - CLOUD_DIM * cloud_coverage.clamp(0., 1.))
    }

    /// The settled brightness factor for a daylight level.
    pub fn target(daylight: f64) -> f64 {
        NIGHT_LEVEL + (1. - NIGHT_LEVEL) * daylight.clamp(0., 1.).sqrt()
    }

    fn step(v: f64) -> f32 {
        ((v * 256.).round() / 256.) as f32
    }

    #[derive(Default)]
    pub struct Sensor {
        level: Option<f64>,
    }

    impl Sensor {
        /// The factor the knob brightness is multiplied by, after `dt_s`
        /// seconds at `daylight`. Full brightness until daylight is known.
        pub fn update(&mut self, daylight: Option<f64>, dt_s: f64) -> f32 {
            let Some(daylight) = daylight.filter(|d| d.is_finite()) else { return self.level.map_or(1., step) };
            let target = target(daylight);
            let level = match self.level {
                None => target,
                Some(l) => l + (target - l) * (1. - (-dt_s.max(0.) / TIME_CONSTANT_S).exp()),
            };
            self.level = Some(level);
            step(level)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn night_dims_to_the_floor_and_day_is_the_full_knob() {
            assert_eq!(target(0.), NIGHT_LEVEL);
            assert_eq!(target(1.), 1.);
            assert!(target(0.25) > 0.75 && target(0.25) < 0.85);
            assert_eq!(target(7.), 1.);
        }

        #[test]
        fn the_sun_fallback_ramps_over_civil_twilight_and_clouds_dim_it() {
            assert_eq!(daylight_from_sun(-10., 0.), 0.);
            assert_eq!(daylight_from_sun(0., 0.), 0.5);
            assert_eq!(daylight_from_sun(30., 0.), 1.);
            assert!((daylight_from_sun(30., 1.) - (1. - CLOUD_DIM)).abs() < 1e-12);
        }

        #[test]
        fn the_sensor_starts_settled_then_lags() {
            let mut s = Sensor::default();
            assert_eq!(s.update(None, 0.1), 1.);
            assert_eq!(s.update(Some(1.), 0.1), 1.);
            let after_one = s.update(Some(0.), 1.);
            assert!(after_one < 1. && after_one > 0.85, "{after_one}");
            let mut level = after_one;
            for _ in 0..600 {
                level = s.update(Some(0.), 0.05);
            }
            assert!((level - NIGHT_LEVEL as f32).abs() < 1. / 256.);
            assert_eq!(s.update(None, 1.), level, "an unreadable sensor holds its level");
        }
    }
}

use std::collections::{HashSet, VecDeque};
use std::ffi::{c_char, c_int, c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::cds_reconf::Du;
use crate::xp::{self, AvionicsId, Xplm};
use crate::xphfbw_bridge::{Input, InputKind, ScreenBlock, ScreenHeader, Session, MAX_DIRTY_RECTS};
use crate::xphfbw_bridge_views::{ViewDef, EFB_SCREEN};
use screens::{ScreenDef, SCREENS};
use stream::Op;
use tessellate::{Images, Mesh, Resources, Tessellator};
use text::{Atlas, Fonts};

#[cfg(feature = "native-displays")]
pub(crate) use xphfbw::plan_upload;

/// The instruments' root under the aircraft folder, laid out as the MSFS
/// package's `html_ui`: image URLs (`/Images/fbw-a380x/...`) resolve from
/// it, and the fonts are in [`FONTS_DIR`] under it.
pub const HTML_UI_DIR: &str = "html_ui";
pub const FONTS_DIR: &str = "Fonts/fbw-a380x";

/// A mouse event for the scripts' `__screenEvent`.
#[derive(Clone, Debug, PartialEq)]
pub struct ScreenEvent {
    pub screen: &'static str,
    pub kind: &'static str,
    pub x: f64,
    pub y: f64,
    pub button: i32,
    pub delta: f64,
}

/// What the last tessellation of a screen cost and made.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub tessellate_ms: f64,
    pub vertices: usize,
    pub batches: usize,
    pub draw_calls: usize,
}

/// A screen rides through a power interruption this short (RTCA DO-160 section 16, category Z).
const POWER_RIDE_THROUGH: std::time::Duration = std::time::Duration::from_millis(200);

struct Screen {
    def: &'static ScreenDef,
    /// Counts meshes made, so the GPU knows when its copy is old.
    stream: u64,
    /// Device pixels per CSS pixel the device is drawn at.
    scale: (f64, f64),
    mesh: Mesh,
    steps: Vec<plan::Step>,
    gpu: gl::ScreenGpu,
    stats: Stats,
    /// Each dimming region's brightness, 0..1.
    brightness: Vec<f32>,
    /// When each dimming region last had power, for the DO-160 200 ms interruption ride-through.
    powered_at: Vec<Option<std::time::Instant>>,
    handle: AvionicsId,
    pressed: bool,
    hover: Option<(c_int, c_int)>,
    pointer: Option<(c_int, c_int)>,
    hover_moved: Option<std::time::Instant>,
    redraw_key: u64,
    redrawn_at: Option<std::time::Instant>,
    window: popout::Watch,
    window_viewport_logged: bool,
    toggled_at: Option<std::time::Instant>,
    right_press: Option<RightPress>,
}

impl Screen {
    fn track_pointer(&mut self, x: c_int, y: c_int) {
        if self.pointer != Some((x, y)) {
            self.pointer = Some((x, y));
            self.hover_moved = Some(std::time::Instant::now());
        }
    }
}

/// One screen's link to XPHFBW: the `ScreenBlock` this module creates for it
/// (docs/briefs/xphfbw-js-bridge.md, "Screens"), its GPU texture, and rule
/// 6's upload bookkeeping. `None` when this screen's `ScreenBlock` could not
/// be made (index out of [`xphfbw_bridge::MAX_VIEWS`]-sized limits does not
/// apply here, but the shared-memory object itself could fail to open).
struct BridgeScreen {
    block: ScreenBlock,
    gpu: gl::BridgeGpu,
    /// The last header `frame` this screen was fully uploaded through.
    last_uploaded: u64,
    /// The ND's terrain or weather radar picture drawn under Chromium's ND
    /// page, and the layer generation last uploaded (0: none yet).
    underlay: gl::BridgeGpu,
    underlay_generation: u64,
    /// When this screen's texture was last brought up to date.
    last_upload_at: Option<std::time::Instant>,
    /// How many uploads this screen has had, for `log_screen_stats`.
    uploads: u64,
    upload_bytes: u64,
    reported: (u64, u64),
}

/// Screen pixels copied into X-Plane's textures in one X-Plane frame, at
/// most: each copy stalls X-Plane's GPU, and 15 screens refreshed in full
/// every frame (58 MB) took X-Plane down to 2-3 fps. Screens take turns.
/// On an approach every display moves at once (tapes, a short-range ND,
/// terrain), which reached 25 MB a frame under the old 80 MB ceiling.
const PACE_BUDGET: std::time::Duration = std::time::Duration::from_millis(6);
const UPLOAD_BUDGET_BYTES: usize = 16 << 20;

const UNIT_SCREENS: [(Du, &str); 6] = [
    (Du::CaptPfd, "SCREEN_DU_PFDL"),
    (Du::CaptNd, "SCREEN_DU_NDL"),
    (Du::FoPfd, "SCREEN_DU_PFDR"),
    (Du::FoNd, "SCREEN_DU_NDR"),
    (Du::Ewd, "SCREEN_DU_EWD"),
    (Du::Sd, "SCREEN_DU_SD"),
];
const MFD_SCREEN: &str = "SCREEN_DU_MFD";
const MFD_HALVES: [(Du, f32); 2] = [(Du::CaptMfd, 0.), (Du::FoMfd, 878.)];
const MFD_HALF_WIDTH: f32 = 768.;
const MFD_HEIGHT: f32 = 1024.;
/// `FBW_SCREENS=off` draws no cockpit display at all: no upload, no quad,
/// no underlay. Purely a measuring tool -- run once with it and once
/// without, and the difference in frame time is what the twenty screens
/// actually cost, which no timer inside the plugin can tell you. The
/// browser views keep running and the systems keep working; only the
/// drawing stops, so the aircraft is still flyable with blank displays.
fn screens_disabled() -> bool {
    use std::sync::OnceLock;
    static OFF: OnceLock<bool> = OnceLock::new();
    *OFF.get_or_init(|| {
        let off = std::env::var("FBW_SCREENS").is_ok_and(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "off" || v == "0" || v == "false" || v == "no"
        });
        if off {
            crate::log("screens: FBW_SCREENS=off, drawing no cockpit displays (measuring what they cost)");
        }
        off
    })
}

/// The shortest gap between two uploads of one Chromium screen: about the
/// instruments' own 30 Hz. Native screens upload every sim frame. Only a screen whose contents actually changed uploads at all
/// (rule 6's dirty rectangles), so a static screen still uploads nothing.
const UPLOAD_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(28);
/// A screen that has waited this long is refreshed even over the budget, so
/// a big one (the FCU's 2560x1280) never starves.
const UPLOAD_MAX_WAIT: std::time::Duration = std::time::Duration::from_millis(150);

/// XPHFBW's session for this plugin run (agent C/A hands this in once the
/// plugin's `Session` opens; see [`set_bridge`]).
struct Bridge {
    session: &'static Session,
    /// Indexed like [`SCREENS`] / [`Displays::screens`]; also the `screen`
    /// index in the `Input` records this module pushes ([`Session::input`]
    /// order matches `ScreenBlock` order, both `SCREENS` order).
    screens: Vec<Option<BridgeScreen>>,
    /// Screen index (`SCREENS` order) -> the `ViewDef::index` whose
    /// `Loaded` report governs it (`SlotHeader::view_loaded`, S10), or
    /// `None` for a screen with no view of its own (the EFB --
    /// `bridge_active`'s doc comment). Computed once, from panel.cfg's own
    /// view list, when the session attaches ([`set_bridge`]): the mapping
    /// cannot change without a new session (a new panel.cfg parse), so
    /// there is no need to recompute it every frame the way the per-screen
    /// loaded state itself does.
    view_for_screen: Vec<Option<u32>>,
}

/// What X-Plane's main thread draws from: the meshes tessellation made, a
/// copy of the glyph atlas and the images they use, and the devices. The
/// instruments' thread holds it only briefly, to hand over a new mesh.
pub struct Displays {
    gpu_reported_at: Option<std::time::Instant>,
    view_scale: view_scale::ViewScale,
    xplm: Option<&'static Xplm>,
    pub(crate) api: Option<xp::AvionicsApi>,
    pub(crate) screens: Vec<Screen>,
    /// The atlas and images as the meshes need them; no fonts.
    res: Resources,
    hud_targets: std::collections::HashMap<usize, gl::HudTarget>,
    renderer: Option<Result<gl::Renderer, String>>,
    fbo: Option<xp::DataRef>,
    /// `sim/graphics/view/viewport` (int[4], read-only): the current OpenGL
    /// viewport, read the same way `fbo` reads `current_gl_fbo` instead of
    /// `glGetIntegerv(GL_VIEWPORT)` (developer.x-plane.com, "Plugin Guidance
    /// for OpenGL Drawing": "Do not use glGet to retrieve OpenGL state").
    /// `Renderer::viewport` (the old glGet read) is kept only as the
    /// fallback for an X-Plane build that does not publish this dataref.
    viewport: Option<xp::DataRef>,
    events: VecDeque<ScreenEvent>,
    /// XPHFBW's screens, once attached; `None` keeps every screen on the
    /// QuickJS drawing path. Once attached, each screen switches over on
    /// its own (rule 7, per screen: `bridge_active`'s doc comment) rather
    /// than waiting for every screened view to have loaded.
    bridge: Option<Bridge>,
    /// What is left of this X-Plane frame's [`UPLOAD_BUDGET_BYTES`].
    upload_budget: usize,
    pace_budget: std::time::Duration,
    efis_refs: Option<[Option<xp::DataRef>; 4]>,
    /// The ambient-light sensor ([`ambient`]), its datarefs (outside light
    /// level, sun pitch, cloud coverage) and when it last read.
    ambient: ambient::Sensor,
    ambient_refs: Option<[Option<xp::DataRef>; 3]>,
    ambient_at: Option<Instant>,
    /// `log_screen_stats`: when it last printed, and each screen's
    /// (published frame, upload count) at that moment.
    stats_at: Option<std::time::Instant>,
    stats_baseline: [(u64, u64); SCREENS.len()],
    /// Screens the native displays draw (`FBW_NATIVE_SCREENS`); `None` when
    /// none are picked, which leaves every screen on the paths above.
    #[cfg(feature = "native-displays")]
    native: Option<NativeRoute>,
}

/// One screen fed by the native render thread: its texture and what has
/// been uploaded into it.
#[cfg(feature = "native-displays")]
struct NativeScreen {
    slot: usize,
    gpu: gl::BridgeGpu,
    size: (u32, u32),
    uploaded: u64,
    last_upload_at: Option<std::time::Instant>,
    uploads: u64,
    upload_bytes: u64,
    /// `uploads`, `upload_bytes` and the render thread's frame count at the
    /// last report.
    reported: (u64, u64, u64),
}

#[cfg(feature = "native-displays")]
struct NativeRoute {
    source: crate::native_displays::screen::Native,
    /// The screens drawn in their own screen callbacks (`screen::inline_mode`).
    inline: Option<crate::native_displays::screen::Inline>,
    /// Indexed like [`SCREENS`].
    screens: Vec<Option<NativeScreen>>,
    reported_at: std::time::Instant,
    /// The `SlotHeader::native_paint_off` mask last published.
    paint_off: u64,
    /// A tiny black texture: what a native screen shows before its first
    /// native frame.
    black: gl::BridgeGpu,
    /// Sim frames since the last `native_displays::cost` report, and when that was.
    cost_frames: u64,
    cost_at: std::time::Instant,
}

#[cfg(feature = "native-displays")]
const BLACK_SIDE: u32 = 4;

/// A native screen showing another screen's picture draws black rather than
/// that screen's Chromium view.
#[cfg(feature = "native-displays")]
fn must_draw_black(dest_native: bool, src_native: bool) -> bool {
    dest_native && !src_native
}

#[cfg(feature = "native-displays")]
const NATIVE_REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// The screens whose Chromium view can stop painting: picked native and with
/// a native frame already uploaded. The Chromium picture is never shown on a
/// native screen either way (`draw_native_source`).
#[cfg(feature = "native-displays")]
fn native_paint_off(selection: &crate::native_displays::screen::Selection, landed: impl Fn(&str) -> bool) -> u64 {
    crate::xphfbw_bridge_views::paint_off_mask(selection.targets.iter().map(|t| t.screen).filter(|s| landed(s)))
}

#[cfg(feature = "native-displays")]
fn native_route() -> Option<NativeRoute> {
    use crate::native_displays::screen;
    let selection = screen::selection();
    if !selection.unknown.is_empty() {
        log(&format!("{}: unknown names {:?} ignored", screen::ENV, selection.unknown));
    }
    if screen::inline_mode() {
        let mut route = native_route_for(screen::Native::idle(selection)?);
        route.inline = screen::Inline::new(selection, Box::new(crate::native_displays::inputs::live_feed()));
        log(&format!("native displays: {} screens from {}", selection.targets.len(), screen::ENV));
        return Some(route);
    }
    if !selection.is_empty() && std::env::var("FBW_NATIVE_SHARE").as_deref() != Ok("0") {
        match crate::native_displays::gpu_share(screen::render_threads()) {
            Ok(made) => log(&format!("native displays: {made} GL contexts share textures with X-Plane")),
            Err(e) => log(&format!("native displays: no shared GL contexts, pixels read back: {e}")),
        }
    }
    let route = native_route_for(screen::Native::start_with(selection, (0..crate::native_displays::screen::render_threads()).map(|_| Box::new(crate::native_displays::inputs::live_feed()) as Box<dyn screen::Feed>).collect())?);
    log(&format!("native displays: {} screens from {}", selection.targets.len(), screen::ENV));
    Some(route)
}

#[cfg(feature = "native-displays")]
fn native_route_for(source: crate::native_displays::screen::Native) -> NativeRoute {
    let screens = SCREENS.iter().map(|def| source.slot_of(def.id).map(|slot| NativeScreen { slot, gpu: gl::BridgeGpu::default(), size: (0, 0), uploaded: 0, last_upload_at: None, uploads: 0, upload_bytes: 0, reported: (0, 0, 0) })).collect();
    let now = std::time::Instant::now();
    NativeRoute { source, inline: None, screens, reported_at: now, paint_off: 0, black: gl::BridgeGpu::default(), cost_frames: 0, cost_at: now }
}

// X-Plane's handles are only touched on its main thread.
unsafe impl Send for Displays {}

/// Events kept for the scripts at most; older ones are dropped.
const EVENT_LIMIT: usize = 256;

static DISPLAYS: Mutex<Option<Displays>> = Mutex::new(None);

/// Tessellation: the fonts, the glyph atlas it fills and the images, with
/// each screen's last stream. It runs where the streams arrive (the
/// instruments' thread), so X-Plane's frame never waits on it.
static TESSELLATION: Mutex<Option<Tess>> = Mutex::new(None);

/// Device scales the draw callbacks found changed, for tessellation to make
/// those screens' meshes again.
static RESCALED: Mutex<Vec<(usize, (f64, f64))>> = Mutex::new(Vec::new());

fn log(message: &str) {
    crate::log(&format!("display: {message}"));
}

/// A new mesh for a screen.
struct Made {
    index: usize,
    mesh: Mesh,
    steps: Vec<plan::Step>,
    stats: Stats,
}

/// Changes to the glyph atlas since the draw side's copy.
enum AtlasUpdate {
    Whole { size: u32, generation: u64, pixels: Vec<u8> },
    Rows { first: u32, pixels: Vec<u8> },
}

struct TessScreen {
    ops: Vec<Op>,
    raw: Vec<f64>,
    strings: Vec<String>,
    scale: (f64, f64),
    have: bool,
}

/// Tessellation's state (see [`TESSELLATION`]).
pub struct Tess {
    tessellator: Tessellator,
    res: Resources,
    screens: Vec<TessScreen>,
    /// The atlas generation and size the draw side has, and how many images.
    sent_atlas: (u64, u32),
    sent_pictures: usize,
    logged: HashSet<String>,
    /// Per screen, the largest stream FBW_DUMP_STREAMS has written out.
    dumped: std::collections::HashMap<String, usize>,
}

impl Tess {
    pub fn new(html_ui: PathBuf) -> Self {
        let fonts_dir = html_ui.join(FONTS_DIR);
        let (fonts, problems) = Fonts::load(&fonts_dir);
        for p in problems {
            log(&p);
        }
        if fonts.is_empty() {
            log(&format!("no fonts in {}; text will not be drawn", fonts_dir.display()));
        }
        Self {
            tessellator: Tessellator::default(),
            res: Resources { fonts, atlas: Atlas::default(), images: Images::new(html_ui) },
            screens: SCREENS
                .iter()
                .map(|_| TessScreen { ops: Vec::new(), raw: Vec::new(), strings: Vec::new(), scale: (1., 1.), have: false })
                .collect(),
            sent_atlas: (0, 0),
            sent_pictures: 0,
            logged: HashSet::new(),
            dumped: std::collections::HashMap::new(),
        }
    }

    fn log_once(&mut self, message: String) {
        if self.logged.len() < 1000 && self.logged.insert(message.clone()) {
            log(&message);
        }
    }

    /// Write one screen's stream out, when `FBW_DUMP_STREAMS` names a folder.
    ///
    /// The screens draw correctly offline. Every op sequence captured from
    /// the instruments renders its geometry in the right place, under
    /// rotation and anisotropic scale alike, and the tessellator, the clip
    /// stack, the affine algebra and the glyph placement all check out
    /// against them. Yet in the cockpit the same screens show their text and
    /// none of their shapes, with the text sheared.
    ///
    /// Two things that cannot be tested offline remain: the live OpenGL path
    /// in `gl.rs`, and whether the stream the instruments really emit is the
    /// same kind of stream as the captures. This settles the second. Replay
    /// what comes out of here through the tessellator and a software
    /// rasteriser: if it draws wrong, the fault is upstream in whatever
    /// builds the stream; if it draws right, only `gl.rs` is left.
    ///
    /// The largest stream each screen has sent, not the first.
    ///
    /// The first was the obvious choice and the wrong one: a screen's
    /// opening stream is `SAVE / TRANSFORM / RESTORE` and nothing else --
    /// eight numbers, the frame before it has anything to draw. Keeping the
    /// largest converges on a representative frame within a second or two
    /// of the instruments running, and costs one comparison per submit.
    fn dump_stream(&mut self, screen: &str, ops: &[f64], strings: &[String]) {
        let Some(dir) = std::env::var_os("FBW_DUMP_STREAMS") else { return };
        let biggest = self.dumped.entry(screen.to_owned()).or_insert(0);
        if ops.len() <= *biggest {
            return;
        }
        *biggest = ops.len();
        let path = Path::new(&dir).join(format!("{screen}.json"));
        let body = serde_json::json!({ "screen": screen, "ops": ops, "strings": strings });
        match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, body.to_string())) {
            Ok(()) => println!("FBW A380 systems: display: wrote {} ops for {screen} to {}", ops.len(), path.display()),
            Err(e) => println!("FBW A380 systems: display: could not write {}: {e}", path.display()),
        }
    }

    /// A new stream for a screen, read and tessellated; nothing for a stream
    /// identical to the last.
    fn submit(&mut self, screen: &str, ops: &[f64], strings: Vec<String>) -> Result<Vec<Made>, String> {
        let index = screens::find(screen).ok_or_else(|| format!("there is no screen {screen}"))?;
        self.dump_stream(screen, ops, &strings);
        let s = &mut self.screens[index];
        if s.have && s.raw == ops && s.strings == strings {
            return Ok(Vec::new());
        }
        let parsed = stream::parse(ops, strings.len()).map_err(|e| format!("{screen}: {e}"))?;
        s.ops = parsed;
        s.raw.clear();
        s.raw.extend_from_slice(ops);
        s.strings = strings;
        s.have = true;
        Ok(self.make(index))
    }

    /// Tessellate a screen again at a new device scale.
    fn rescale(&mut self, index: usize, scale: (f64, f64)) -> Vec<Made> {
        let s = &mut self.screens[index];
        if s.scale == scale {
            return Vec::new();
        }
        s.scale = scale;
        if s.have {
            self.make(index)
        } else {
            Vec::new()
        }
    }

    /// The screen's mesh, and every other screen's too should the atlas
    /// have started again (their glyph coordinates are then stale).
    fn make(&mut self, index: usize) -> Vec<Made> {
        let generation = self.res.atlas.generation;
        let mut out: Vec<Made> = self.tessellate(index).into_iter().collect();
        if self.res.atlas.generation != generation {
            let others: Vec<usize> = (0..self.screens.len()).filter(|&i| i != index && self.screens[i].have).collect();
            for i in others {
                out.extend(self.tessellate(i));
            }
        }
        out
    }

    fn tessellate(&mut self, index: usize) -> Option<Made> {
        let started = Instant::now();
        let def = &SCREENS[index];
        let s = &self.screens[index];
        let size = ((def.width as f64 * s.scale.0).round() as u32, (def.height as f64 * s.scale.1).round() as u32);
        let mut mesh = None;
        // The atlas may fill and start again (a new generation) part way;
        // then everything is tessellated against the new one.
        for _ in 0..4 {
            if let Ok(m) = self.tessellator.run(&s.ops, &s.strings, def.id, size, s.scale, &mut self.res) {
                mesh = Some(m);
                break;
            }
        }
        let Some(mut mesh) = mesh else {
            self.log_once(format!("{}: the glyph atlas cannot hold this screen's text", def.id));
            return None;
        };
        let (sx, sy) = s.scale;
        let regions: Vec<[f32; 4]> = def
            .dimming
            .iter()
            .map(|d| {
                let [x, y, w, h] = d.region.map(|v| v as f64);
                [x * sx, y * sy, (x + w) * sx, (y + h) * sy].map(|v| v.round() as f32)
            })
            .collect();
        tessellate::add_dimming(&mut mesh, &regions);
        let steps = plan::plan(&mesh);
        let stats = Stats {
            tessellate_ms: started.elapsed().as_secs_f64() * 1000.,
            vertices: mesh.vertices.len(),
            batches: mesh.batches.len(),
            draw_calls: plan::draw_calls(&steps),
        };
        for p in std::mem::take(&mut mesh.problems) {
            self.log_once(format!("{}: {p}", def.id));
        }
        for family in std::mem::take(&mut self.res.fonts.missing) {
            self.log_once(format!("no font for the family {family}"));
        }
        Some(Made { index, mesh, steps, stats })
    }

    /// What the draw side's atlas copy lacks.
    fn atlas_update(&mut self) -> Option<AtlasUpdate> {
        let atlas = &mut self.res.atlas;
        if self.sent_atlas != (atlas.generation, atlas.size) {
            self.sent_atlas = (atlas.generation, atlas.size);
            atlas.dirty = None;
            return Some(AtlasUpdate::Whole { size: atlas.size, generation: atlas.generation, pixels: atlas.pixels.clone() });
        }
        let (first, last) = atlas.dirty.take()?;
        let row = atlas.size as usize;
        Some(AtlasUpdate::Rows { first, pixels: atlas.pixels[first as usize * row..last as usize * row].to_vec() })
    }

    pub fn measure_text(&mut self, screen: Option<&str>, family: &str, size: f64, text: &str) -> f64 {
        match self.res.fonts.select(screen.unwrap_or(""), family, 400, false) {
            Some(face) if size.is_finite() && size > 0. => self.res.fonts.layout(face, size, text).width,
            _ => 0.,
        }
    }

    pub fn font_metrics(&mut self, screen: Option<&str>, family: &str, size: f64) -> (f64, f64) {
        match self.res.fonts.select(screen.unwrap_or(""), family, 400, false) {
            Some(face) if size.is_finite() && size > 0. => self.res.fonts.metrics(face, size),
            _ => (0., 0.),
        }
    }
}

impl Displays {
    pub fn new(xplm: Option<&'static Xplm>, html_ui: PathBuf) -> Self {
        let screens = SCREENS
            .iter()
            .map(|def| Screen {
                def,
                stream: 0,
                scale: (1., 1.),
                mesh: Mesh { width: def.width, height: def.height, clips: vec![Default::default()], ..Default::default() },
                steps: Vec::new(),
                gpu: gl::ScreenGpu::default(),
                stats: Stats::default(),
                brightness: vec![0.; def.dimming.len()],
                powered_at: vec![None; def.dimming.len()],
                handle: std::ptr::null_mut(),
                pressed: false,
                hover: None,
                pointer: None,
                hover_moved: None,
                redraw_key: 0,
                redrawn_at: None,
                window: popout::Watch::default(),
                window_viewport_logged: false,
                toggled_at: None,
                right_press: None,
            })
            .collect();
        Self {
            xplm,
            api: xplm.and_then(|x| x.avionics()),
            screens,
            res: Resources { fonts: Fonts::default(), atlas: Atlas::default(), images: Images::new(html_ui) },
            hud_targets: std::collections::HashMap::new(),
            renderer: None,
            gpu_reported_at: None,
            view_scale: view_scale::ViewScale::new(xplm),
            fbo: xplm.and_then(|x| x.find("sim/graphics/view/current_gl_fbo")),
            viewport: xplm.and_then(|x| x.find("sim/graphics/view/viewport")),
            events: VecDeque::new(),
            bridge: None,
            upload_budget: UPLOAD_BUDGET_BYTES,
            pace_budget: PACE_BUDGET,
            efis_refs: None,
            ambient: ambient::Sensor::default(),
            ambient_refs: None,
            ambient_at: None,
            stats_at: None,
            stats_baseline: [(0, 0); SCREENS.len()],
            #[cfg(feature = "native-displays")]
            native: None,
        }
    }

    /// Take tessellation's new meshes, atlas changes and images.
    fn install(&mut self, made: Vec<Made>, atlas: Option<AtlasUpdate>, pictures: Vec<Arc<image::Picture>>) {
        match atlas {
            Some(AtlasUpdate::Whole { size, generation, pixels }) => {
                let a = &mut self.res.atlas;
                a.size = size;
                a.generation = generation;
                a.pixels = pixels;
                a.dirty = Some((0, size));
            }
            Some(AtlasUpdate::Rows { first, pixels }) => {
                let a = &mut self.res.atlas;
                let row = a.size as usize;
                let start = first as usize * row;
                if row > 0 && start + pixels.len() <= a.pixels.len() {
                    a.pixels[start..start + pixels.len()].copy_from_slice(&pixels);
                    let last = first + (pixels.len() / row) as u32;
                    a.dirty = Some(match a.dirty {
                        Some((f, l)) => (f.min(first), l.max(last)),
                        None => (first, last),
                    });
                }
            }
            None => {}
        }
        self.res.images.pictures.extend(pictures);
        for m in made {
            let s = &mut self.screens[m.index];
            s.mesh = m.mesh;
            s.steps = m.steps;
            s.stats = m.stats;
            s.stream += 1;
            if s.stream == 1 {
                let st = s.stats;
                log(&format!(
                    "{}: first stream, {} vertices, {} batches, {} draw calls, {:.2} ms",
                    s.def.id, st.vertices, st.batches, st.draw_calls, st.tessellate_ms
                ));
            }
        }
    }

    /// Draw a screen, from its device's X-Plane draw callback.
    fn draw_screen_timed(&mut self, index: usize) {
        if let Some(Ok(r)) = self.renderer.as_mut() {
            r.gpu_begin(index);
        }
        self.draw_screen(index);
        if let Some(Ok(r)) = self.renderer.as_mut() {
            r.gpu_end(index);
            let now = std::time::Instant::now();
            if self.gpu_reported_at.is_none_or(|t| now - t >= std::time::Duration::from_secs(10)) {
                let window = self.gpu_reported_at.map_or(10.0, |t| (now - t).as_secs_f64());
                self.gpu_reported_at = Some(now);
                let rows = r.gpu_report();
                if !rows.is_empty() {
                    let total: f64 = rows.iter().map(|(_, ms, n)| ms * *n as f64).sum::<f64>() / window;
                    let parts: Vec<String> = rows.iter().map(|(i, ms, n)| format!("{} {:.2} ms x{}", SCREENS[*i].id, ms, n)).collect();
                    log(&format!("gpu (screen callbacks): {total:.1} ms per second | {}", parts.join(", ")));
                }
            }
        }
    }

    fn draw_screen(&mut self, index: usize) {
        if screens_disabled() {
            return;
        }
        let (Some(xplm), Some(api)) = (self.xplm, self.api) else { return };
        // Nothing to draw for when another aircraft is loaded.
        let handle = self.screens[index].handle;
        if handle.is_null() || unsafe { (api.is_bound)(handle) } == 0 {
            return;
        }
        if self.renderer.is_none() {
            let r = gl::Renderer::new(api);
            match &r {
                Ok(r) => log(&format!(
                    "OpenGL ready, {}",
                    if r.anti_aliased() { format!("{}x multisampling", gl::SAMPLES) } else { "without multisampling".into() }
                )),
                Err(e) => log(&format!("cannot draw: {e}")),
            }
            self.renderer = Some(r);
        }
        let Some(Ok(renderer)) = self.renderer.as_mut() else { return };
        renderer.prefilter_lod = self.view_scale.lod(self.screens[index].def.id);
        let framebuffer = self.fbo.map_or(0, |d| xplm.get_i(d)) as u32;
        // `glGetIntegerv(GL_VIEWPORT)` (the old `renderer.viewport()` path)
        // is exactly what developer.x-plane.com's "Plugin Guidance for
        // OpenGL Drawing" says not to call ("stall for the driver"); the
        // same page names `sim/graphics/view/viewport` (int[4], read-only)
        // as the dataref to read it from instead -- the same pattern `fbo`
        // above already uses for `current_gl_fbo`. Falls back to the old
        // glGet read for an X-Plane build too old to publish the dataref,
        // or one answering with anything but the 4 ints a viewport is.
        let from_dataref = self.viewport.and_then(|d| {
            let mut v = [0; 4];
            (xplm.get_vi(d, &mut v) == 4).then_some(v)
        });
        let windowed = popout::is_shown(self, index) || popout::is_out(self, index);
        let Some(Ok(renderer)) = self.renderer.as_mut() else { return };
        let viewport = if windowed {
            let gl_viewport = renderer.viewport();
            if !self.screens[index].window_viewport_logged {
                self.screens[index].window_viewport_logged = true;
                log(&format!("display: {} in a window, GL viewport {gl_viewport:?}, dataref {from_dataref:?}", self.screens[index].def.id));
            }
            gl_viewport
        } else {
            resolve_viewport(from_dataref, || renderer.viewport())
        };
        let target = gl::Target { framebuffer, viewport };
        let [_, _, vw, vh] = target.viewport;
        let def = self.screens[index].def;
        // X-Plane draws devices at the size they were made with; a mesh is
        // made again should the viewport ever differ.
        let scale = (vw as f64 / def.width as f64, vh as f64 / def.height as f64);
        if self.screens[index].scale != scale && scale.0 > 0. && scale.1 > 0. {
            // Tessellation makes the mesh again at this scale. Only needed
            // for the QuickJS path (below); harmless to keep current while
            // the bridge is drawing, since a screen it takes back over must
            // start from an up to date scale.
            self.screens[index].scale = scale;
            if let Ok(mut r) = RESCALED.lock() {
                r.push((index, scale));
            }
        }
        if !self.draw_routed(index, target) {
            let Some(Ok(renderer)) = self.renderer.as_mut() else { return };
            let s = &mut self.screens[index];
            let region = [0, 0, s.mesh.width as i32, s.mesh.height as i32];
            let natives: Vec<_> =
                s.mesh.natives.iter().map(|id| crate::mapdata::plugin::native_image(id)).collect();
            renderer.draw(&mut s.gpu, s.stream, &s.mesh, &s.steps, region, target, &s.brightness, &natives, &mut self.res);
        }
        if windowed {
            if let Some(Ok(renderer)) = self.renderer.as_mut() {
                renderer.make_opaque(target);
            }
        }
        self.grab_device(index, target);
    }

    fn draw_hud(&mut self, frame: &HudFrame) {
        let report = crate::hud::diag_due(5);
        let why = |r: &str| {
            if report {
                crate::log(&format!("hud draw {}: {r}", frame.screen));
            }
        };
        if screens_disabled() {
            return why("the vector screens are disabled");
        }
        let Some(xplm) = self.xplm else { return why("no XPLM") };
        let Some(index) = screens::find(frame.screen) else { return why("no such vector screen") };
        if self.screens[index].stream == 0 {
            if !self.bridge_active(index) {
                return why("neither a vector page nor a loaded Chromium view");
            }
            let Some((w, h)) = self.refresh_bridge(index, &[]) else { return why("the Chromium view has no screen block") };
            let texture = self.bridge.as_ref().and_then(|b| b.screens.get(index)?.as_ref()).map_or(0, |bs| bs.gpu.texture());
            if texture == 0 {
                return why("the Chromium view has not painted yet");
            }
            let Some(Ok(renderer)) = self.renderer.as_mut() else { return why("no renderer") };
            if report {
                crate::log(&format!("hud draw {}: Chromium view {w}x{h}, texture {texture}", frame.screen));
            }
            let triangles: Vec<[f32; 5]> = frame.triangles.iter().map(|t| [t[0], t[1], t[2], 1. - t[3], t[4]]).collect();
            renderer.draw_hud(texture, (w, h), &triangles, frame.color, frame.glow, frame.viewport);
            return;
        }
        let framebuffer = self.fbo.map_or(0, |d| xplm.get_i(d)) as u32;
        let viewport = frame.viewport;
        let Some(Ok(renderer)) = self.renderer.as_mut() else { return why("no renderer") };
        let s = &mut self.screens[index];
        let (w, h) = (s.def.width, s.def.height);
        if report {
            crate::log(&format!("hud draw {}: screen {index} {w}x{h}, stream {:?}, {} mesh steps", frame.screen, s.stream, s.steps.len()));
        }
        let natives: Vec<_> = s.mesh.natives.iter().map(|id| crate::mapdata::plugin::native_image(id)).collect();
        let target = self.hud_targets.entry(index).or_default();
        let Some(texture) = renderer.draw_offscreen(target, &mut s.gpu, s.stream, &s.mesh, &s.steps, w, h, framebuffer, viewport, &natives, &mut self.res) else {
            return why("the offscreen page draw returned nothing");
        };
        renderer.draw_hud(texture, (w, h), &frame.triangles, frame.color, frame.glow, viewport);
    }

    fn grab_device(&mut self, index: usize, target: gl::Target) {
        let def = self.screens[index].def;
        let dir = {
            let Ok(mut grabs) = DEVICE_GRABS.lock() else { return };
            let Some((dir, done)) = grabs.as_mut() else { return };
            if !done.insert(def.id) {
                return;
            }
            dir.clone()
        };
        let Some(Ok(renderer)) = self.renderer.as_mut() else { return };
        let (dw, dh) = crate::xphfbw_bridge_views::device_size(def.id, def.width, def.height);
        match renderer.read_target(target) {
            Ok((w, h, mut rgba)) => {
                log(&format!("display: {} device viewport {w}x{h} at ({}, {}), made {dw}x{dh}", def.id, target.viewport[0], target.viewport[1]));
                rgba.chunks_exact_mut(4).for_each(|p| p[3] = 255);
                let path = dir.join(format!("DEVICE_{}_{w}x{h}.png", def.id));
                std::thread::Builder::new()
                    .name("fbw-device-grab".into())
                    .spawn(move || {
                        let _ = std::fs::create_dir_all(&dir);
                        crate::diag::write_png_rgba(&path, w, h, &rgba);
                    })
                    .ok();
            }
            Err(e) => log(&format!("display: {} device readback failed: {e}", def.id)),
        }
    }

    /// The bridge path for one screen's draw callback (rules 6 and 7):
    /// upload the rectangles [`xphfbw::plan_upload`] plans into this
    /// screen's texture and draw it, its dimming regions applied on top
    /// just as the QuickJS path dims its mesh. `true` if this screen was
    /// drawn this way, so the caller must not also run the QuickJS path;
    /// `false` (no session attached, this screen's own `bridge_active` is
    /// still false, or this screen has no `ScreenBlock`) leaves it to the
    /// caller's QuickJS path.
    fn draw_bridge_screen(&mut self, index: usize, target: gl::Target) -> bool {
        let def = self.screens[index].def;
        let dimming: Vec<[f32; 4]> = def
            .dimming
            .iter()
            .map(|d| {
                let [x, y, w, h] = d.region;
                [x as f32, y as f32, (x + w) as f32, (y + h) as f32]
            })
            .collect();
        let brightness = self.unit_brightness(index);
        self.draw_bridge_source(index, None, target, brightness, dimming)
    }

    fn unit_brightness(&self, index: usize) -> Vec<f32> {
        let mut brightness = self.screens[index].brightness.clone();
        if screen_breaker_open(self.screens[index].def.id) {
            brightness.iter_mut().for_each(|b| *b = 0.);
        }
        brightness
    }

    fn picture(format: Du) -> Option<(usize, Option<[f32; 4]>)> {
        let half = |x0: f32| Some((screens::find(MFD_SCREEN)?, Some([x0, 0., x0 + MFD_HALF_WIDTH, MFD_HEIGHT])));
        match format {
            Du::CaptMfd => half(MFD_HALVES[0].1),
            Du::FoMfd => half(MFD_HALVES[1].1),
            f => Some((screens::find(UNIT_SCREENS.iter().find(|(d, _)| *d == f)?.1)?, None)),
        }
    }

    fn draw_routed(&mut self, index: usize, target: gl::Target) -> bool {
        let id = self.screens[index].def.id;
        let route = crate::cds_reconf::route();
        if id == MFD_SCREEN {
            if !self.draw_bridge_screen(index, target) {
                return false;
            }
            let [vx, vy, vw, vh] = target.viewport;
            let scale = vw as f32 / self.screens[index].def.width as f32;
            let brightness = self.unit_brightness(index);
            for (half, (du, x0)) in MFD_HALVES.iter().enumerate() {
                let Some(shown) = route[du.id()].filter(|f| f != du) else { continue };
                let Some((src, crop)) = Self::picture(shown) else { continue };
                let sub = gl::Target { framebuffer: target.framebuffer, viewport: [vx + (x0 * scale).round() as c_int, vy, (MFD_HALF_WIDTH * scale).round() as c_int, vh] };
                let region = crop.unwrap_or([0., 0., SCREENS[src].width as f32, SCREENS[src].height as f32]);
                self.draw_routed_source(index, src, crop, sub, vec![brightness.get(half).copied().unwrap_or(0.)], vec![region]);
            }
            return true;
        }
        let Some(&(du, _)) = UNIT_SCREENS.iter().find(|(_, s)| *s == id) else { return self.draw_bridge_screen(index, target) };
        match route[du.id()].filter(|f| *f != du).and_then(Self::picture) {
            Some((src, crop)) => {
                let region = crop.unwrap_or([0., 0., SCREENS[src].width as f32, SCREENS[src].height as f32]);
                let brightness = vec![self.unit_brightness(index).first().copied().unwrap_or(0.)];
                self.draw_routed_source(index, src, crop, target, brightness, vec![region]) || self.draw_bridge_screen(index, target)
            }
            None => self.draw_bridge_screen(index, target),
        }
    }

    /// Brings a bridge screen's texture up to date with XPHFBW's latest
    /// paint (shared GPU copy, or dirty rectangles within the frame's upload
    /// budget) and returns its size in pixels.
    fn refresh_bridge(&mut self, index: usize, brightness: &[f32]) -> Option<(u32, u32)> {
        let def = self.screens[index].def;
        let bridge = self.bridge.as_mut()?;
        let Some(bs) = bridge.screens.get_mut(index)?.as_mut() else { return None };
        let header = bs.block.header();
        let width = header.width.load(Ordering::Relaxed);
        let height = header.height.load(Ordering::Relaxed);
        let now = std::time::Instant::now();
        let waited = bs.last_upload_at.map_or(UPLOAD_MAX_WAIT, |t| now - t);
        // A screen with no dimming regions (SCREEN_ISIS_1, Clock, RTPI, BAT,
        // SCREEN_EFB) dims in its own drawing rather than through this
        // mechanism (`ScreenDef::dimming`'s doc comment) and is never
        // considered dark here: `brightness.iter().all(..)` is vacuously
        // true on an empty slice, which would otherwise skip every upload
        // after the first (the "already dark, no point re-uploading" branch
        // below) and freeze such a screen on its first frame forever.
        let dark = !brightness.is_empty() && brightness.iter().all(|b| *b <= 0.);
        let due = header.frame.load(Ordering::Relaxed) != bs.last_uploaded
            && waited >= UPLOAD_MIN_INTERVAL
            && !(dark && bs.last_upload_at.is_some());
        let gpu_generation = header.gpu_generation.load(Ordering::Acquire);
        if gpu_generation != 0 && header.gpu_refused.load(Ordering::Relaxed) == 0 {
            let frame = header.frame.load(Ordering::Acquire);
            if frame != bs.last_uploaded {
                if let Some(Ok(renderer)) = self.renderer.as_mut() {
                    match renderer.bridge_gpu_copy(&mut bs.gpu, &bs.block.gpu_texture_name(gpu_generation), gpu_generation, width, height) {
                        Ok(true) => {
                            bs.last_uploaded = frame;
                            bs.last_upload_at = Some(now);
                            bs.uploads += 1;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            log(&format!("{}: GPU sharing off, back to copied pixels: {e}", def.id));
                            header.gpu_refused.store(1, Ordering::Release);
                        }
                    }
                }
            }
        } else if due && header.writing.load(Ordering::Relaxed) == 0 {
            let frame = header.frame.load(Ordering::Acquire);
            let rects = xphfbw::plan_upload(header.changed_since(bs.last_uploaded, frame), width, height);
            // What this upload actually costs, which is what the frame's
            // budget has to be charged.
            //
            // The budget used to be spent against the screen's *whole* area
            // instead, however little of it had changed. Six 768x1024
            // display units are charged 18.9 MB of the 24 MB that way before
            // the MFD's turn, so its 1646x1024 never fitted in what was left
            // and it only ever uploaded when UPLOAD_MAX_WAIT let it past --
            // about six frames a second, alone among the screens, which is
            // exactly how it looked. Its dirty rects are a scratchpad line
            // and a softkey or two; they always fitted.
            let area: usize = rects.iter().map(|r| r[2] as usize * r[3] as usize * 4).sum();
            // Out of budget with time still on the clock: leave
            // `last_uploaded`/`last_upload_at` untouched so this same
            // upload is reconsidered next frame rather than dropped, and
            // fall through to draw what is already on the GPU.
            if area <= self.upload_budget || waited >= UPLOAD_MAX_WAIT {
                bs.last_upload_at = Some(now);
                bs.uploads += 1;
                bs.upload_bytes += area as u64;
                if !rects.is_empty() {
                    self.upload_budget = self.upload_budget.saturating_sub(area);
                    crate::perf::screen_bytes(def.id, area as u64);
                    let pixels = bs.block.pixels();
                    if let Some(Ok(renderer)) = self.renderer.as_mut() {
                        renderer.bridge_upload(&mut bs.gpu, width, height, pixels, &rects);
                    }
                }
                // A paint landing during the copy is in the next upload's
                // history, so this one counts as caught up to `frame`.
                bs.last_uploaded = frame;
            }
        }
        Some((width, height))
    }

    /// Whether `index` is a screen the native displays draw.
    #[cfg(feature = "native-displays")]
    fn is_native(&self, index: usize) -> bool {
        self.native.as_ref().is_some_and(|n| n.screens.get(index).is_some_and(|s| s.is_some()))
    }

    /// Another screen's picture routed onto `dest` (a CDS reconfiguration).
    /// A native `dest` takes only a native picture; a Chromium one draws black.
    fn draw_routed_source(&mut self, dest: usize, src: usize, crop: Option<[f32; 4]>, target: gl::Target, brightness: Vec<f32>, dimming: Vec<[f32; 4]>) -> bool {
        #[cfg(feature = "native-displays")]
        if must_draw_black(self.is_native(dest), self.is_native(src)) {
            self.draw_native_black(dest, target);
            return true;
        }
        #[cfg(not(feature = "native-displays"))]
        let _ = dest;
        self.draw_bridge_source(src, crop, target, brightness, dimming)
    }

    #[cfg(feature = "native-displays")]
    fn draw_native_black(&mut self, index: usize, target: gl::Target) {
        let id = self.screens[index].def.id;
        let (Some(route), Some(Ok(renderer))) = (self.native.as_mut(), self.renderer.as_mut()) else { return };
        if route.black.texture() == 0 {
            let pixels = crate::native_displays::screen::black_frame(BLACK_SIDE, BLACK_SIDE);
            renderer.bridge_upload(&mut route.black, BLACK_SIDE, BLACK_SIDE, &pixels, &[]);
        }
        renderer.draw_bridge(id, &mut route.black, None, BLACK_SIDE, BLACK_SIDE, None, target, &[], &[], None);
    }

    /// The native route for one screen: upload the newest frame the render
    /// thread published, within the frame's upload budget and the same
    /// dirty-rectangle planner as the bridge, and draw it like a bridge
    /// frame. A screen picked native never shows its Chromium picture: until
    /// its first native frame lands it draws black. `false` only for a
    /// screen that is not native.
    #[cfg(feature = "native-displays")]
    fn draw_native_source(&mut self, index: usize, crop: Option<[f32; 4]>, target: gl::Target, brightness: &[f32], dimming: &[[f32; 4]]) -> bool {
        if !self.is_native(index) {
            return false;
        }
        let started = std::time::Instant::now();
        if !self.draw_native_frame(index, crop, target, brightness, dimming) {
            self.draw_native_black(index, target);
        }
        crate::native_displays::cost::main_since(started);
        true
    }

    /// `false` while the screen has no native frame to draw.
    #[cfg(feature = "native-displays")]
    fn draw_native_frame(&mut self, index: usize, crop: Option<[f32; 4]>, target: gl::Target, brightness: &[f32], dimming: &[[f32; 4]]) -> bool {
        let def = self.screens[index].def;
        let pointer = self.screen_cursor(index);
        let Some(NativeRoute { source, screens, inline, .. }) = self.native.as_mut() else { return false };
        let Some(ns) = screens[index].as_mut() else { return false };
        let now = std::time::Instant::now();
        if let Some(inline) = inline.as_mut() {
            let dark = !brightness.is_empty() && brightness.iter().all(|b| *b <= 0.);
            if !(dark && ns.last_upload_at.is_some()) {
                if let Some(Ok(renderer)) = self.renderer.as_mut() {
                    let (w, h) = inline.size(ns.slot);
                    match inline.draw(ns.slot) {
                        Ok(Some(texture)) => match renderer.after_native_draw(&mut ns.gpu, texture, w, h) {
                            Ok(()) => {
                                ns.size = (w, h);
                                ns.last_upload_at = Some(now);
                                ns.uploads += 1;
                            }
                            Err(e) => log(&format!("native displays: {} copy failed: {e}", self.screens[index].def.id)),
                        },
                        Ok(None) => {}
                        Err(_) => {
                            let _ = renderer.after_native_draw(&mut ns.gpu, 0, 0, 0);
                        }
                    }
                }
            }
            return self.draw_native_texture(index, crop, target, brightness, dimming, pointer);
        }
        let waited = ns.last_upload_at.map_or(UPLOAD_MAX_WAIT, |t| now - t);
        let dark = !brightness.is_empty() && brightness.iter().all(|b| *b <= 0.);
        if !(dark && ns.last_upload_at.is_some()) {
            if !self.pace_budget.is_zero() {
                let waited = std::time::Instant::now();
                source.wait_due(ns.slot, self.pace_budget);
                self.pace_budget = self.pace_budget.saturating_sub(waited.elapsed());
            }
            if let Some(mut pending) = source.pending(ns.slot, ns.uploaded) {
                let (width, height) = pending.size();
                if let Some((texture, ready)) = pending.gpu() {
                    let seq = pending.seq();
                    if let Some(Ok(renderer)) = self.renderer.as_mut() {
                        match renderer.native_gpu_copy(&mut ns.gpu, texture, ready, width, height) {
                            Ok(release) => {
                                let old = pending.released(release);
                                renderer.delete_sync(old);
                                ns.size = (width, height);
                                ns.uploaded = seq;
                                ns.last_upload_at = Some(now);
                                ns.uploads += 1;
                            }
                            Err(e) => {
                                drop(pending);
                                log(&format!("{}: {e}", def.id));
                            }
                        }
                    }
                } else {
                let rects = pending.rects();
                let area: usize = rects.iter().map(|r| r[2] as usize * r[3] as usize * 4).sum();
                if area <= self.upload_budget || waited >= UPLOAD_MAX_WAIT {
                    if let Some(Ok(renderer)) = self.renderer.as_mut() {
                        renderer.bridge_upload(&mut ns.gpu, width, height, pending.bgra(), &rects);
                    }
                    self.upload_budget = self.upload_budget.saturating_sub(area);
                    crate::perf::screen_bytes(def.id, area as u64);
                    crate::native_displays::cost::uploaded(area as u64);
                    ns.size = (width, height);
                    ns.uploaded = pending.seq();
                    ns.last_upload_at = Some(now);
                    ns.uploads += 1;
                    ns.upload_bytes += area as u64;
                    pending.finish();
                }
                }
            }
        }
        self.draw_native_texture(index, crop, target, brightness, dimming, pointer)
    }

    #[cfg(feature = "native-displays")]
    fn draw_native_texture(&mut self, index: usize, crop: Option<[f32; 4]>, target: gl::Target, brightness: &[f32], dimming: &[[f32; 4]], pointer: Option<(c_int, c_int, [u8; 4])>) -> bool {
        let def = self.screens[index].def;
        let Some(NativeRoute { screens, .. }) = self.native.as_mut() else { return false };
        let Some(ns) = screens[index].as_mut() else { return false };
        let (width, height) = ns.size;
        if ns.gpu.texture() == 0 || width == 0 || height == 0 {
            return false;
        }
        let (fx, fy) = (width as f32 / def.width.max(1) as f32, height as f32 / def.height.max(1) as f32);
        let cursor = crop.is_none().then_some(()).and(pointer).map(|(x, y, rgba)| gl::Cursor { x: x as f32 + 0.5, y: height as f32 - (y as f32 + 0.5), scale: fx, rgba });
        let crop = crop.map(|[x0, y0, x1, y1]| [x0 * fx, y0 * fy, x1 * fx, y1 * fy]);
        let dimming: Vec<[f32; 4]> = dimming.iter().map(|[x0, y0, x1, y1]| [x0 * fx, y0 * fy, x1 * fx, y1 * fy]).collect();
        if let Some(Ok(renderer)) = self.renderer.as_mut() {
            renderer.draw_bridge(def.id, &mut ns.gpu, None, width, height, crop, target, brightness, &dimming, cursor);
        }
        true
    }

    fn draw_bridge_source(&mut self, index: usize, crop: Option<[f32; 4]>, target: gl::Target, brightness: Vec<f32>, dimming: Vec<[f32; 4]>) -> bool {
        #[cfg(feature = "native-displays")]
        if self.draw_native_source(index, crop, target, &brightness, &dimming) {
            return true;
        }
        if !self.bridge_active(index) {
            return false;
        }
        let def = self.screens[index].def;
        let pointer = self.screen_cursor(index);
        let Some((width, height)) = self.refresh_bridge(index, &brightness) else { return false };
        let Some(bridge) = self.bridge.as_mut() else { return false };
        let Some(Some(bs)) = bridge.screens.get_mut(index) else { return false };
        let (fx, fy) = (width as f32 / def.width.max(1) as f32, height as f32 / def.height.max(1) as f32);
        let cursor = crop.is_none().then_some(()).and(pointer).map(|(x, y, rgba)| gl::Cursor { x: x as f32 + 0.5, y: height as f32 - (y as f32 + 0.5), scale: fx, rgba });
        let crop = crop.map(|[x0, y0, x1, y1]| [x0 * fx, y0 * fy, x1 * fx, y1 * fy]);
        let dimming: Vec<[f32; 4]> = dimming.iter().map(|[x0, y0, x1, y1]| [x0 * fx, y0 * fy, x1 * fx, y1 * fy]).collect();
        // The NDs: FlyByWire's terrain gauge (or the weather radar) under
        // nd.html, whose background is transparent (mapdata::plugin::
        // terrain_layer's compositing contract).
        let side = match def.id {
            "SCREEN_DU_NDL" => Some(crate::mapdata::terrain::types::Side::Left),
            "SCREEN_DU_NDR" => Some(crate::mapdata::terrain::types::Side::Right),
            _ => None,
        };
        let layer = side.and_then(|s| crate::wxr::layer(s).or_else(|| crate::mapdata::plugin::terrain_layer(s)));
        let mut has_underlay = false;
        if let Some((lw, lh, generation, rgba)) = layer {
            let bytes = lw as usize * lh as usize * 4;
            if generation != bs.underlay_generation && (bytes <= self.upload_budget || bs.underlay_generation == 0) {
                if let Some(Ok(renderer)) = self.renderer.as_mut() {
                    renderer.bridge_underlay_upload(&mut bs.underlay, lw, lh, &rgba);
                    bs.underlay_generation = generation;
                    self.upload_budget = self.upload_budget.saturating_sub(bytes);
                    crate::perf::screen_bytes(def.id, bytes as u64);
                }
            }
            has_underlay = true;
        }
        // `writing` = 1: draw whatever was uploaded last tick and try the
        // upload again next frame, rather than skip the draw outright.
        if let Some(Ok(renderer)) = self.renderer.as_mut() {
            let underlay = has_underlay.then_some(&bs.underlay);
            renderer.draw_bridge(def.id, &mut bs.gpu, underlay, width, height, crop, target, &brightness, &dimming, cursor);
        }
        true
    }

    fn screen_cursor(&mut self, index: usize) -> Option<(c_int, c_int, [u8; 4])> {
        let side = match self.screens[index].def.id {
            "SCREEN_DU_NDL" => Some(0),
            "SCREEN_DU_NDR" => Some(1),
            "SCREEN_DU_MFD" => None,
            _ => return None,
        };
        let s = &self.screens[index];
        let (x, y) = s.pointer?;
        if s.hover_moved.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(5)) {
            return None;
        }
        let pressed = s.pressed;
        let oans = side.is_some_and(|i| self.oans_shown(i));
        if oans && pressed {
            return None;
        }
        Some((x, y, if oans { [255, 148, 255, 255] } else { [255, 255, 0, 255] }))
    }

    fn oans_shown(&mut self, side: usize) -> bool {
        let Some(xplm) = self.xplm else { return false };
        let refs = self.efis_refs.get_or_insert_with(|| {
            ["fbw/A32NX_EFIS_L_ND_RANGE", "fbw/A32NX_EFIS_L_ND_MODE", "fbw/A32NX_EFIS_R_ND_RANGE", "fbw/A32NX_EFIS_R_ND_MODE"].map(|n| xplm.find(n))
        });
        let (Some(range), Some(mode)) = (refs[side * 2], refs[side * 2 + 1]) else { return false };
        let mode = xplm.get_f(mode).round() as i32;
        xplm.get_f(range).round() as i32 == 0 && (2..=4).contains(&mode)
    }

    /// A device's texel (origin bottom-left) as a point on its screen.
    fn to_screen(&self, screen: usize, x: c_int, y: c_int) -> (f64, f64) {
        let s = &self.screens[screen];
        let (sx, sy) = s.scale;
        let height = s.def.height as f64 * sy;
        xphfbw::device_to_css(x as f64, y as f64, height, (sx, sy))
    }

    /// Whether XPHFBW should be drawing `index` (rule 7, per screen: S08,
    /// built on S10's `SlotHeader::view_loaded` mirror). `false` with no
    /// session attached, so every screen keeps the QuickJS drawing path and
    /// every pointer event keeps going to the scripts' `ScreenEvent`s.
    /// Otherwise a screen switches over the moment *its own* view has
    /// reported `Loaded { ok: true }` (`view_loaded[view] == 1`, S10;
    /// `view_for_screen` maps this screen to that view, [`Bridge`]'s doc
    /// comment), independent of every other screen: one view stuck
    /// loading, crashed, or never created no longer blanks a screen whose
    /// own view is fine. The EFB (`EFB_SCREEN`) has no view of its own --
    /// its gauge is excluded from `view_list`
    /// (`xphfbw_bridge_views::EXCLUDED_GAUGES`, screens.rs's doc comment)
    /// -- so `view_for_screen` has no entry for it; it counts as active
    /// once its own `ScreenBlock` has painted at least one frame, the same
    /// signal `draw_bridge_screen` already uses to decide there is
    /// something worth uploading.
    fn bridge_active(&self, index: usize) -> bool {
        let Some(bridge) = self.bridge.as_ref() else { return false };
        if SCREENS.get(index).is_some_and(|s| s.id == EFB_SCREEN) {
            return bridge.screens.get(index).and_then(|s| s.as_ref()).is_some_and(|bs| bs.block.header().frame.load(Ordering::Relaxed) != 0);
        }
        let Some(Some(view)) = bridge.view_for_screen.get(index) else { return false };
        bridge.session.slots.header().view_loaded.get(*view as usize).is_some_and(|c| c.load(Ordering::Relaxed) == 1)
    }

    fn push_event(&mut self, screen: usize, kind: &'static str, x: c_int, y: c_int, button: i32, delta: f64) {
        let (cx, cy) = self.to_screen(screen, x, y);
        if self.events.len() == EVENT_LIMIT {
            self.events.pop_front();
        }
        let id = self.screens[screen].def.id;
        self.events.push_back(ScreenEvent { screen: id, kind, x: cx, y: cy, button, delta });
    }

    /// A pointer event on a screen: XPHFBW's `Input` ring while that
    /// screen's own bridge switch-over is active (rule 7, per screen: S08;
    /// CSS pixel coordinates, same mapping as `push_event`'s `ScreenEvent`s;
    /// the ring has no room for a right-click distinction, so every button
    /// maps to the same `InputKind`), the scripts' `ScreenEvent` queue
    /// otherwise.
    fn dispatch_pointer(&mut self, screen: usize, screen_event_kind: &'static str, input_kind: InputKind, x: c_int, y: c_int, button: i32, delta: f64) {
        #[cfg(feature = "native-displays")]
        if self.native_pointer(screen, input_kind, x, y, button, delta) {
            return;
        }
        if self.bridge_active(screen) {
            let (cx, cy) = self.to_screen(screen, x, y);
            if input_kind != InputKind::Move {
                static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20 {
                    log(&format!("input: {} {screen_event_kind} at device ({x},{y}) -> page ({cx:.0},{cy:.0}), to XPHFBW", self.screens[screen].def.id));
                }
            }
            if let Some(bridge) = &self.bridge {
                let delta = if input_kind == InputKind::Wheel { bridge_wheel_clicks(delta) } else { delta };
                let record = Input { screen: screen as u32, kind: input_kind, x: cx as f32, y: cy as f32, delta: delta as f32 };
                bridge.session.input.push(&record.encode());
            }
        } else {
            self.push_event(screen, screen_event_kind, x, y, button, delta);
        }
    }

    /// A native-owned screen's pointer event goes to its native display;
    /// `true` when nothing is left for the page.
    #[cfg(feature = "native-displays")]
    fn native_pointer(&mut self, screen: usize, input_kind: InputKind, x: c_int, y: c_int, button: i32, delta: f64) -> bool {
        use crate::native_displays::input::{self, Pointer, Route};
        let id = self.screens[screen].def.id;
        if !input::is_native(id) {
            return false;
        }
        let (cx, cy) = self.to_screen(screen, x, y);
        let (kind, button) = match input_kind {
            InputKind::Down => (Pointer::Down, button),
            InputKind::Up => (Pointer::Up, button),
            InputKind::Wheel => (Pointer::Wheel, delta.round() as i32),
            _ if self.screens[screen].pressed => (Pointer::Drag, button),
            _ => (Pointer::Move, button),
        };
        if id == "SCREEN_EFB" && crate::efb_native::host::pointer(cx, cy, kind as i32, button) {
            return true;
        }
        if id == MFD_SCREEN && matches!(kind, Pointer::Move) && crate::native_displays::input::is_native(MFD_SCREEN) {
            crate::mfd_native_host::hover(cx, cy);
        }
        if id == MFD_SCREEN && matches!(kind, Pointer::Down) && crate::mfd_native_host::pointer(cx, cy, &crate::mfd_native_host::cached_var) {
            return true;
        }
        if matches!(id, "SCREEN_OIT_LEFT" | "SCREEN_OIT_RIGHT") && matches!(kind, Pointer::Down) && crate::oit_native_host::pointer(id, cx, cy) {
            return true;
        }
        input::pointer(id, cx, cy, kind, button) == Route::Native
    }

    /// One keystroke from a device's `XPLMAvionicsKeyboard_f` callback (the
    /// KCCU typing into the MFD, [`InputKind::Key`]'s doc comment): only
    /// meaningful while XPHFBW is drawing this screen (rule 7, per screen:
    /// S08), since the QuickJS path has no keyboard route of its own.
    /// `false` (dropped silently) with no bridge attached or this screen's
    /// own switch-over still not active.
    fn dispatch_key(&mut self, screen: usize, ch: u32, vkey: u32, flags: u32) -> bool {
        #[cfg(feature = "native-displays")]
        if self.screens[screen].def.id == "SCREEN_EFB" && crate::native_displays::input::is_native("SCREEN_EFB") && crate::efb_native::host::key(ch, vkey, flags) {
            return true;
        }
        #[cfg(feature = "native-displays")]
        if self.screens[screen].def.id == MFD_SCREEN && flags & 16 == 0 && crate::native_displays::input::is_native(MFD_SCREEN) {
            let db = crate::native_fms_shadow::navdb();
            if crate::mfd_native_host::key(ch, vkey, db.as_deref().map(|d| d as &dyn fms::navdb::NavDatabase), &crate::mfd_native_host::cached_var) {
                return true;
            }
        }
        #[cfg(feature = "native-displays")]
        if matches!(self.screens[screen].def.id, "SCREEN_OIT_LEFT" | "SCREEN_OIT_RIGHT") && flags & 16 == 0 && crate::native_displays::input::is_native(self.screens[screen].def.id) && crate::oit_native_host::key(self.screens[screen].def.id, ch, vkey) {
            return true;
        }
        #[cfg(feature = "native-displays")]
        if crate::native_displays::input::key(self.screens[screen].def.id, if ch != 0 { ch } else { vkey }, flags & 7, flags & 16 == 0) == crate::native_displays::input::Route::Native {
            return true;
        }
        if !self.bridge_active(screen) {
            return false;
        }
        let Some(bridge) = &self.bridge else { return false };
        let record = Input { screen: screen as u32, kind: InputKind::Key, x: ch as f32, y: vkey as f32, delta: flags as f32 };
        bridge.session.input.push(&record.encode());
        true
    }

    /// One keystroke from [`key_sniffer`], for a screen still drawn in the
    /// 3D cockpit. Declined if that screen has been popped out and given
    /// real keyboard focus since the click that aimed us at it, because
    /// then [`keyboard_callback`] is already delivering the same keystroke
    /// and the scratchpad would take every character twice.
    fn sniff_key(&mut self, screen: usize, ch: u32, vkey: u32, flags: u32) -> bool {
        let popped = match (self.api, self.screens.get(screen)) {
            (Some(api), Some(s)) => !s.handle.is_null() && unsafe { (api.has_keyboard_focus)(s.handle) } != 0,
            _ => false,
        };
        if popped {
            return false;
        }
        self.dispatch_key(screen, ch, vkey, flags)
    }

    /// Clicking a screen aims the keyboard: a click on one that can be
    /// typed into ([`HAS_KEYBOARD`]) sends typing there from then on, a
    /// click on any other screen gives the keyboard back to X-Plane. This
    /// is the whole focus gesture -- there is no callback for a click
    /// anywhere else in the cockpit, so the only other way back is Escape
    /// ([`key_sniffer`]).
    fn aim_keyboard(&self, screen: usize) {
        if let Some(api) = self.api {
            for (i, other) in self.screens.iter().enumerate() {
                if i != screen && !other.handle.is_null() && unsafe { (api.has_keyboard_focus)(other.handle) } != 0 {
                    unsafe { api.release_keyboard_focus(other.handle) };
                    log(&format!("{}: popup closed, {} was clicked to type into", other.def.id, self.screens[screen].def.id));
                }
            }
        }
        let id = self.screens[screen].def.id;
        if !HAS_KEYBOARD.contains(&id) {
            release_keyboard("a screen with no keyboard was clicked");
            return;
        }
        if COCKPIT_KEYBOARD.swap(screen as isize, Ordering::Relaxed) != screen as isize {
            log(&format!("{id}: typing goes here now (Escape, or a click on another screen, hands the keyboard back to X-Plane)"));
        }
    }

    /// The KCCU gesture: right-click a keyboard screen (the MFD, or either
    /// OIT) to pop it out and give it keyboard focus (so typing reaches it,
    /// [`keyboard_callback`]'s doc comment), right-click again to give the
    /// focus back and hide the popup. `has_keyboard_focus` (not local
    /// bookkeeping) decides which way to go, so this stays correct even if
    /// the popup was closed some other way (X-Plane's own popup chrome,
    /// Alt+F4, focus taken by another device) since the last toggle.
    fn toggle_keyboard(&mut self, index: usize) {
        let Some(api) = self.api else { return };
        let handle = self.screens[index].handle;
        if handle.is_null() {
            return;
        }
        let id = self.screens[index].def.id;
        let now = std::time::Instant::now();
        if !toggle_allowed(self.screens[index].toggled_at, now) {
            return;
        }
        self.screens[index].toggled_at = Some(now);
        unsafe {
            if (api.has_keyboard_focus)(handle) != 0 {
                api.release_keyboard_focus(handle);
                log(&format!("{id}: keyboard focus released, popup hidden"));
            } else {
                (api.set_popup_visible)(handle, 1);
                (api.take_keyboard_focus)(handle);
                // The popup now gets the keystrokes directly (sniff_key).
                release_keyboard("the screen was popped out instead");
                log(&format!("{id}: popped up and given keyboard focus"));
            }
        }
    }

    /// The popup lost keyboard focus some other way (the user clicked
    /// outside it, or X-Plane's own window manager moved focus elsewhere):
    /// hide it too, so a stray popup does not linger with no way to type
    /// into it. Safe to call whether or not the popup is even visible.
    fn hide_popup(&mut self, index: usize) {
        let Some(api) = self.api else { return };
        let handle = self.screens[index].handle;
        if handle.is_null() {
            return;
        }
        unsafe { api.release_keyboard_focus(handle) };
    }

    fn mouse(&mut self, screen: usize, x: c_int, y: c_int, status: c_int, button: i32) {
        self.redraw_now(screen);
        if status != xp::MOUSE_UP {
            self.screens[screen].track_pointer(x, y);
        }
        // X-Plane gives no position with a release on an avionics device: the
        // up always arrives at (0,0), whatever the press and drags said.
        // Forwarded as it comes, the page sees the press on a button and the
        // release in its top-left corner, so no click is ever synthesised --
        // the cursor tracks the mouse and nothing can be pressed. A release
        // therefore happens where the pointer last was.
        let (x, y) = match (status, self.screens[screen].hover) {
            (xp::MOUSE_UP, Some((hx, hy))) if x == 0 && y == 0 && (hx, hy) != (0, 0) => (hx, hy),
            _ => (x, y),
        };
        if status == xp::MOUSE_DOWN && self.screens[screen].pressed && self.screens[screen].hover == Some((x, y)) {
            return;
        }
        let (kind, input_kind) = match status {
            xp::MOUSE_DOWN => {
                self.screens[screen].pressed = true;
                self.aim_keyboard(screen);
                ("down", InputKind::Down)
            }
            xp::MOUSE_DRAG => ("move", InputKind::Move),
            xp::MOUSE_UP => {
                self.screens[screen].pressed = false;
                ("up", InputKind::Up)
            }
            _ => return,
        };
        self.screens[screen].hover = Some((x, y));
        self.dispatch_pointer(screen, kind, input_kind, x, y, button, 0.);
    }

    fn redraw_now(&self, screen: usize) {
        let handle = self.screens[screen].handle;
        if let (Some(needs_drawing), false) = (self.api.and_then(|a| a.needs_drawing), handle.is_null()) {
            unsafe { needs_drawing(handle) };
        }
    }

    fn cursor(&mut self, screen: usize, x: c_int, y: c_int) {
        self.redraw_now(screen);
        let s = &mut self.screens[screen];
        s.track_pointer(x, y);
        if !s.pressed && s.hover != Some((x, y)) {
            s.hover = Some((x, y));
            self.dispatch_pointer(screen, "move", InputKind::Move, x, y, 0, 0.);
        }
    }

    /// `FBW_SCREEN_STATS=1`: every 10 s, one line per bridged screen giving
    /// how fast XPHFBW is *publishing* it against how fast this plugin is
    /// *uploading* it.
    ///
    /// The two answer different questions, which is the point. A screen that
    /// looks like it is stepping can be starved on either side: the app may
    /// not be drawing it any faster, or the app may be drawing it fine and
    /// the upload path may be dropping frames. Guessing between those is how
    /// an afternoon disappears; this prints which.
    fn log_screen_stats(&mut self) {
        use std::sync::atomic::AtomicBool;
        use std::sync::OnceLock;
        static ON: OnceLock<bool> = OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("FBW_SCREEN_STATS").is_ok_and(|v| v.trim() != "0" && !v.trim().is_empty())) {
            return;
        }
        static WARNED: AtomicBool = AtomicBool::new(false);
        let _ = &WARNED;
        let now = std::time::Instant::now();
        let since = self.stats_at.map_or(std::time::Duration::from_secs(0), |t| now - t);
        if self.stats_at.is_some() && since < std::time::Duration::from_secs(10) {
            return;
        }
        let seconds = since.as_secs_f64();
        self.stats_at = Some(now);
        let Some(bridge) = self.bridge.as_ref() else { return };
        if seconds <= 0.0 {
            // First call: just take the baseline.
            for (i, bs) in bridge.screens.iter().enumerate() {
                if let Some(bs) = bs {
                    self.stats_baseline[i] = (bs.block.header().frame.load(Ordering::Relaxed), bs.uploads);
                }
            }
            return;
        }
        let mut line = String::from("screens (published/s -> uploaded/s):");
        for (i, bs) in bridge.screens.iter().enumerate() {
            let Some(bs) = bs else { continue };
            let frame = bs.block.header().frame.load(Ordering::Relaxed);
            let (last_frame, last_uploads) = self.stats_baseline[i];
            let published = frame.saturating_sub(last_frame) as f64 / seconds;
            let uploaded = bs.uploads.saturating_sub(last_uploads) as f64 / seconds;
            self.stats_baseline[i] = (frame, bs.uploads);
            line.push_str(&format!(" {}={published:.0}->{uploaded:.0}", SCREENS[i].id.trim_start_matches("SCREEN_")));
        }
        log(&line);
    }

    /// Tells XPHFBW which views to stop painting and, every 10 s, logs what
    /// the native screens and the Chromium ones cost. Nothing runs without a
    /// native route.
    #[cfg(feature = "native-displays")]
    fn native_tick(&mut self) {
        let started = std::time::Instant::now();
        let Some(route) = self.native.as_mut() else { return };
        route.source.frame();
        route.cost_frames += 1;
        if route.cost_at.elapsed() >= crate::native_displays::cost::REPORT_EVERY {
            let report = crate::native_displays::cost::take(route.cost_frames, route.cost_at.elapsed().as_secs_f64());
            crate::log(&report.line());
            route.cost_frames = 0;
            route.cost_at = std::time::Instant::now();
        }
        self.native_tick_reports();
        crate::native_displays::cost::main_since(started);
    }

    #[cfg(feature = "native-displays")]
    fn native_tick_reports(&mut self) {
        let Some(route) = self.native.as_mut() else { return };
        let NativeRoute { source, screens, reported_at, paint_off, .. } = route;
        let landed = |id: &str| screens.iter().flatten().any(|ns| ns.uploaded != 0 && source.slot_of(id) == Some(ns.slot));
        let mask = native_paint_off(crate::native_displays::screen::selection(), landed);
        if let Some(bridge) = self.bridge.as_ref() {
            let header = bridge.session.slots.header();
            if header.native_paint_off.swap(mask, Ordering::Relaxed) != mask || *paint_off != mask {
                log(&format!("native displays: Chromium paint off for {} screens (mask {mask:#x})", mask.count_ones()));
            }
            *paint_off = mask;
        }
        if reported_at.elapsed() < NATIVE_REPORT_EVERY {
            return;
        }
        let seconds = reported_at.elapsed().as_secs_f64();
        *reported_at = std::time::Instant::now();
        for (index, ns) in screens.iter_mut().enumerate() {
            let id = SCREENS[index].id;
            if let Some(ns) = ns {
                let frames = source.frames_drawn(ns.slot);
                let (uploads, bytes, drawn) = (ns.uploads - ns.reported.0, ns.upload_bytes - ns.reported.1, frames - ns.reported.2);
                ns.reported = (ns.uploads, ns.upload_bytes, frames);
                if drawn > 0 || uploads > 0 {
                    log(&format!(
                        "native displays: {id} native {drawn} frames, {uploads} uploads, {:.1} MB ({:.2} MB/s), chromium paint {}",
                        bytes as f64 / 1e6,
                        bytes as f64 / 1e6 / seconds,
                        if crate::xphfbw_bridge_views::paint_off_bit(id) & mask != 0 { "off" } else { "on" }
                    ));
                }
            } else if let Some(Some(bs)) = self.bridge.as_mut().map(|b| b.screens.get_mut(index)).and_then(|s| s.map(|s| s.as_mut())) {
                let (uploads, bytes) = (bs.uploads - bs.reported.0, bs.upload_bytes - bs.reported.1);
                bs.reported = (bs.uploads, bs.upload_bytes);
                if uploads > 0 {
                    log(&format!("native displays: {id} chromium {uploads} uploads, {:.1} MB ({:.2} MB/s)", bytes as f64 / 1e6, bytes as f64 / 1e6 / seconds));
                }
            }
        }
    }

    /// Set each dimming region's brightness from the variables `read` gives.
    pub fn update_brightness(&mut self, mut read: impl FnMut(&str) -> Option<f64>) {
        self.log_screen_stats();
        popout::tend(self);
        // Once per X-Plane frame, from the plugin's tick: a fresh upload budget.
        self.upload_budget = UPLOAD_BUDGET_BYTES;
        self.pace_budget = PACE_BUDGET;
        if let Some(xplm) = self.xplm {
            self.view_scale.refresh(xplm);
        }
        #[cfg(feature = "native-displays")]
        self.native_tick();
        let auto = self.ambient_factor();
        for s in &mut self.screens {
            let now = std::time::Instant::now();
            for ((value, d), seen) in s.brightness.iter_mut().zip(s.def.dimming).zip(s.powered_at.iter_mut()) {
                let live = d.buses.iter().any(|bus| read(bus).is_some_and(|v| v != 0.));
                if live {
                    *seen = Some(now);
                }
                let powered = live || seen.is_some_and(|t| now.duration_since(t) < POWER_RIDE_THROUGH);
                *value = screens::brightness(read(&format!("LIGHT POTENTIOMETER:{}", d.potentiometer)), powered) * auto;
            }
            if !s.brightness.is_empty() && crate::browser_screens::shows(s.def.id) {
                if screen_breaker_open(s.def.id) {
                    s.brightness.iter_mut().for_each(|v| *v = 0.);
                }
                crate::browser_screens::publish_dimming(s.def.id, &s.brightness);
            }
        }
        self.request_redraws();
    }

    fn daylight(&mut self) -> Option<f64> {
        let xplm = self.xplm?;
        let [light, sun, clouds] = *self.ambient_refs.get_or_insert_with(|| {
            ["sim/graphics/misc/outside_light_level", "sim/graphics/scenery/sun_pitch_degrees", "sim/weather/aircraft/cloud_coverage_percent"].map(|n| xplm.find(n))
        });
        if let Some(level) = light.map(|d| xplm.get_f(d) as f64).filter(|v| v.is_finite()) {
            return Some(level.clamp(0., 1.));
        }
        let sun = xplm.get_f(sun?) as f64;
        let mut layers = [0f32; 3];
        let cover = clouds.map_or(0., |d| {
            xplm.get_vf(d, &mut layers);
            layers.iter().copied().fold(0f32, f32::max) as f64
        });
        Some(ambient::daylight_from_sun(sun, cover))
    }

    fn ambient_factor(&mut self) -> f32 {
        let now = Instant::now();
        let dt = self.ambient_at.map_or(0., |t| (now - t).as_secs_f64());
        self.ambient_at = Some(now);
        let daylight = self.daylight();
        self.ambient.update(daylight, dt)
    }

    fn sources_of(&self, index: usize, route: &crate::cds_reconf::Route) -> Vec<usize> {
        let id = self.screens[index].def.id;
        let mut sources = vec![index];
        if id == MFD_SCREEN {
            for (du, _) in MFD_HALVES.iter() {
                if let Some((src, _)) = route[du.id()].filter(|f| f != du).and_then(Self::picture) {
                    sources.push(src);
                }
            }
        } else if let Some(&(du, _)) = UNIT_SCREENS.iter().find(|(_, s)| *s == id) {
            if let Some((src, _)) = route[du.id()].filter(|f| *f != du).and_then(Self::picture) {
                sources.push(src);
            }
        }
        sources
    }

    fn redraw_needed(&mut self, index: usize, route: &crate::cds_reconf::Route) -> (bool, u64) {
        use std::hash::{Hash, Hasher};
        let mut pending = false;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        format!("{route:?}").hash(&mut h);
        for src in self.sources_of(index, route) {
            if let Some(Some(bs)) = self.bridge.as_ref().and_then(|b| b.screens.get(src)) {
                let header = bs.block.header();
                let frame = header.frame.load(Ordering::Relaxed);
                pending |= frame != bs.last_uploaded;
                frame.hash(&mut h);
                bs.underlay_generation.hash(&mut h);
            }
            self.screens[src].stream.hash(&mut h);
            self.bridge_active(src).hash(&mut h);
            #[cfg(feature = "native-displays")]
            if let Some(NativeRoute { source, screens, .. }) = self.native.as_ref() {
                if let Some(ns) = screens.get(src).and_then(|s| s.as_ref()) {
                    let seq = source.seq(ns.slot);
                    pending |= seq != ns.uploaded;
                    seq.hash(&mut h);
                }
            }
        }
        let side = match self.screens[index].def.id {
            "SCREEN_DU_NDL" => Some(crate::mapdata::terrain::types::Side::Left),
            "SCREEN_DU_NDR" => Some(crate::mapdata::terrain::types::Side::Right),
            _ => None,
        };
        if let Some(side) = side {
            let generation = crate::wxr::layer(side).or_else(|| crate::mapdata::plugin::terrain_layer(side)).map(|l| l.2);
            generation.hash(&mut h);
        }
        for b in &self.screens[index].brightness {
            b.to_bits().hash(&mut h);
        }
        screen_breaker_open(self.screens[index].def.id).hash(&mut h);
        self.screen_cursor(index).hash(&mut h);
        (pending, h.finish())
    }

    fn request_redraws(&mut self) {
        let Some(api) = self.api else { return };
        let Some(needs_drawing) = api.needs_drawing else { return };
        let route = crate::cds_reconf::route();
        let now = std::time::Instant::now();
        for index in 0..self.screens.len() {
            let handle = self.screens[index].handle;
            if handle.is_null() {
                continue;
            }
            let (pending, key) = self.redraw_needed(index, &route);
            let s = &mut self.screens[index];
            let stale = s.redrawn_at.is_none_or(|t| now - t > std::time::Duration::from_secs(1));
            if pending || stale || key != s.redraw_key {
                s.redraw_key = key;
                s.redrawn_at = Some(now);
                unsafe { needs_drawing(handle) };
            }
        }
    }

    /// See [`set_bridge`].
    fn set_bridge(&mut self, tag: Option<&'static str>, session: Option<&'static Session>, views: &[ViewDef]) {
        let Some((tag, session)) = tag.zip(session) else {
            if self.bridge.take().is_some() {
                log("XPHFBW bridge detached");
            }
            return;
        };
        let screens: Vec<Option<BridgeScreen>> = SCREENS
            .iter()
            .map(|def| {
                let (dw, dh) = crate::xphfbw_bridge_views::device_size(def.id, def.width, def.height);
                if crate::browser_screens::shows(def.id) {
                    return None;
                }
                let block = ScreenBlock::create(tag, def.id, dw, dh);
                if block.is_none() {
                    log(&format!("could not create the XPHFBW ScreenBlock for {}", def.id));
                }
                block.map(|block| BridgeScreen { block, gpu: gl::BridgeGpu::default(), last_uploaded: 0, underlay: gl::BridgeGpu::default(), underlay_generation: 0, last_upload_at: None, uploads: 0, upload_bytes: 0, reported: (0, 0) })
            })
            .collect();
        // S08, per-screen switch-over: which view (if any) governs each
        // screen, worked out once from panel.cfg's own view list rather
        // than a second shared-memory field -- `bridge_active` reads S10's
        // `view_loaded` straight through this.
        let mut view_for_screen: Vec<Option<u32>> = vec![None; SCREENS.len()];
        for v in views.iter().filter(|v| v.layer == 0) {
            if let Some(i) = screens::find(&v.screen) {
                view_for_screen[i] = Some(v.index);
            }
        }
        log(&format!("XPHFBW bridge attached ({tag}), {}/{} screens", screens.iter().filter(|s| s.is_some()).count(), SCREENS.len()));
        self.bridge = Some(Bridge { session, screens, view_for_screen });
    }
}

/// S08: a screen must switch to the bridge on its own view's `view_loaded`
/// reading (S10), independent of every other screen's, and the EFB (which
/// has no view of its own) must switch on its own `ScreenBlock`'s paint
/// instead.
#[cfg(test)]
mod bridge_active_tests {
    use super::{screens, Displays, Session, ViewDef, EFB_SCREEN};
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    fn tag() -> String {
        format!("test_bridge_active_{}_{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos())
    }

    /// A minimal view list, the same shape `xphfbw_host.rs`'s `view_list`
    /// produces from panel.cfg: the PFD is view 0, the ND is view 1.
    fn views() -> Vec<ViewDef> {
        vec![
            ViewDef { index: 0, section: "VCockpit01".into(), gauge_url: "A380X/PFD/pfd.html".into(), width: 768, height: 1024, screen: "SCREEN_DU_PFDL".into(), layer: 0 },
            ViewDef { index: 1, section: "VCockpit02".into(), gauge_url: "A380X/ND/nd.html".into(), width: 768, height: 1024, screen: "SCREEN_DU_NDL".into(), layer: 0 },
        ]
    }

    fn attached(tag: &str) -> (Displays, &'static Session) {
        let session: &'static Session = Box::leak(Box::new(Session::create(tag).expect("session creates")));
        let tag: &'static str = Box::leak(tag.to_string().into_boxed_str());
        let mut d = Displays::new(None, PathBuf::new());
        d.set_bridge(Some(tag), Some(session), &views());
        (d, session)
    }

    #[test]
    fn a_screen_activates_on_its_own_views_loaded_flag_independent_of_the_others() {
        let (d, session) = attached(&tag());
        let pfdl = screens::find("SCREEN_DU_PFDL").unwrap();
        let ndl = screens::find("SCREEN_DU_NDL").unwrap();
        assert!(!d.bridge_active(pfdl));
        assert!(!d.bridge_active(ndl));
        // View 0 (the PFD) reports `Loaded { ok: true }` -- S10's encoding
        // for `SlotHeader::view_loaded[0]`.
        session.slots.header().view_loaded[0].store(1, Ordering::Relaxed);
        assert!(d.bridge_active(pfdl), "the PFD's own view has loaded");
        assert!(!d.bridge_active(ndl), "the ND's view has not, and must not follow the PFD's");
    }

    #[test]
    fn a_failed_view_does_not_activate_its_screen() {
        let (d, session) = attached(&tag());
        let pfdl = screens::find("SCREEN_DU_PFDL").unwrap();
        session.slots.header().view_loaded[0].store(2, Ordering::Relaxed); // reported ok: false
        assert!(!d.bridge_active(pfdl), "a failed Loaded report must not switch the screen over");
    }

    #[test]
    fn the_efb_activates_on_its_own_screen_block_not_on_view_loaded() {
        let (mut d, _session) = attached(&tag());
        let efb = screens::find(EFB_SCREEN).unwrap();
        assert!(!d.bridge_active(efb), "no frame painted yet");
        let bs = d.bridge.as_mut().unwrap().screens[efb].as_ref().expect("the EFB's ScreenBlock was created");
        bs.block.header().frame.store(1, Ordering::Relaxed);
        // `view_loaded` (still all zero here) must not matter for the EFB.
        assert!(d.bridge_active(efb), "the EFB's own ScreenBlock has painted");
    }
}

/// The instruments' root: `html_ui` in the aircraft folder, two above the
/// plugin's (`<aircraft>/plugins/<plugin>`).
fn html_ui() -> Option<PathBuf> {
    let plugin = crate::js_bridge::plugin_dir()?;
    Some(plugin.parent()?.parent().unwrap_or(Path::new(".")).join(HTML_UI_DIR))
}

fn with<R>(f: impl FnOnce(&mut Displays) -> R) -> Option<R> {
    let mut guard = DISPLAYS.lock().ok()?;
    if guard.is_none() {
        *guard = Some(Displays::new(None, html_ui()?));
    }
    guard.as_mut().map(f)
}

fn with_tess<R>(f: impl FnOnce(&mut Tess) -> R) -> Option<R> {
    let mut guard = TESSELLATION.lock().ok()?;
    if guard.is_none() {
        *guard = Some(Tess::new(html_ui()?));
    }
    guard.as_mut().map(f)
}

/// Hand new meshes to the draw side, with the atlas and images they use.
fn hand_over(tess: &mut Tess, made: Vec<Made>) {
    if made.is_empty() {
        return;
    }
    let atlas = tess.atlas_update();
    let pictures: Vec<_> = tess.res.images.pictures[tess.sent_pictures..].to_vec();
    tess.sent_pictures = tess.res.images.pictures.len();
    let _ = with(|d| d.install(made, atlas, pictures));
}

/// For X-Plane's callbacks, which can arrive while the displays are in use
/// on the same thread (a device made while starting up): skipped then.
fn with_from_callback<R>(f: impl FnOnce(&mut Displays) -> R) -> Option<R> {
    let mut guard = DISPLAYS.try_lock().ok()?;
    guard.as_mut().map(f)
}

// The host functions the scripts call (docs/display-stream.md).

/// A screen's new stream, tessellated here and handed to the draw side.
/// One HUD's composite for this frame: the page `screen` mapped by `triangles` (window pixels,
/// page UVs and fade `[x, y, u, v, a]`) in `viewport` (left, bottom, right, top).
pub struct HudFrame {
    pub screen: &'static str,
    pub triangles: Vec<[f32; 5]>,
    pub color: [f32; 3],
    pub glow: f32,
    pub viewport: [i32; 4],
}

pub fn draw_hud(frame: &HudFrame) {
    let _ = with_from_callback(|d| d.draw_hud(frame));
}

pub fn submit(screen: &str, ops: &[f64], strings: Vec<String>) -> Result<(), String> {
    with_tess(|t| {
        let made = t.submit(screen, ops, strings)?;
        hand_over(t, made);
        Ok(())
    })
    .unwrap_or_else(|| Err("the displays are not running".into()))
}

/// Make again the meshes of screens whose devices changed scale. Called
/// where the streams are submitted, once a tick.
pub fn service() {
    let requests = RESCALED.lock().map(|mut r| std::mem::take(&mut *r)).unwrap_or_default();
    if requests.is_empty() {
        return;
    }
    let _ = with_tess(|t| {
        for (index, scale) in requests {
            let made = t.rescale(index, scale);
            hand_over(t, made);
        }
    });
}

pub fn measure_text(screen: Option<&str>, family: &str, size: f64, text: &str) -> f64 {
    with_tess(|t| t.measure_text(screen, family, size, text)).unwrap_or(0.)
}

pub fn font_metrics(screen: Option<&str>, family: &str, size: f64) -> (f64, f64) {
    with_tess(|t| t.font_metrics(screen, family, size)).unwrap_or((0., 0.))
}

pub fn screen_size(screen: &str) -> Option<(u32, u32)> {
    screens::find(screen).map(|i| (SCREENS[i].width, SCREENS[i].height))
}

pub fn type_on(screen_id: &str, ch: u32, vkey: u32) -> bool {
    let Some(i) = screens::find(screen_id) else { return false };
    with(|d| d.dispatch_key(i, ch, vkey, 0)).unwrap_or(false)
}

/// Mouse events since the last call, oldest first.
pub fn take_events() -> Vec<ScreenEvent> {
    with(|d| d.events.drain(..).collect()).unwrap_or_default()
}

/// Read each screen's potentiometers and buses. `read` gives a variable's
/// value, or `None` if nothing has made the variable.
pub fn update_brightness(read: impl FnMut(&str) -> Option<f64>) {
    let _ = DISPLAYS.try_lock().map(|mut guard| guard.as_mut().map(|d| d.update_brightness(read)));
}

/// The viewport for this draw: `dataref` is `Some` reading already taken
/// from `sim/graphics/view/viewport` (4 ints), `None` when that dataref was
/// not found or answered with anything else; `fallback` (the old
/// `glGetIntegerv(GL_VIEWPORT)` path, `Renderer::viewport`, which needs a
/// live GL context) only runs then, so the normal case never calls glGet.
fn resolve_viewport(dataref: Option<[c_int; 4]>, fallback: impl FnOnce() -> [c_int; 4]) -> [c_int; 4] {
    dataref.unwrap_or_else(fallback)
}

#[cfg(test)]
mod resolve_viewport_tests {
    use super::resolve_viewport;

    #[test]
    fn prefers_the_dataref_reading_over_the_gl_get_fallback() {
        let mut fallback_called = false;
        let v = resolve_viewport(Some([1, 2, 3, 4]), || {
            fallback_called = true;
            [0, 0, 0, 0]
        });
        assert_eq!(v, [1, 2, 3, 4]);
        assert!(!fallback_called, "the glGetIntegerv fallback must not run when the dataref answered");
    }

    #[test]
    fn falls_back_to_gl_get_when_the_dataref_is_unavailable() {
        let v = resolve_viewport(None, || [5, 6, 7, 8]);
        assert_eq!(v, [5, 6, 7, 8]);
    }
}


/// Attach (or detach) XPHFBW's session for this plugin run: agent C/A calls
/// this once the plugin's `Session` opens (`xphfbw_host.rs`, not merged at
/// the time this module was written — until it is, nothing calls this and
/// every screen simply keeps the QuickJS drawing path, rule 7), and again
/// with `None` when it closes (XPHFBW's process exits, or the session
/// restarts under a new tag, rule 9 — the caller must pass the new tag and
/// session, not reuse the old ones).
///
/// Creates this session's `ScreenBlock`s, one per [`SCREENS`] entry, sized
/// from panel.cfg: agent C's `xphfbw_bridge_views::view_list` once merged
/// (its sizes come from the same panel.cfg parse `SCREENS` is written
/// against, so no behaviour change is expected either way), `SCREENS`
/// itself until then.
pub fn set_bridge(tag: Option<&'static str>, session: Option<&'static Session>, views: &[ViewDef]) {
    let _ = with(|d| d.set_bridge(tag, session, views));
}

/// S08: called from `xphfbw_host.rs`'s `check_gone` the moment XPHFBW's
/// process is found to have exited, alongside the existing
/// `displays_active`/`view_loaded` clears there. Without this, a screen
/// whose own view had already loaded would keep reading that stale
/// "loaded" state (S10's `view_loaded` mirror is not something `check_gone`
/// can reach into on its own -- it lives behind `display::with`) and stay
/// on the bridge, drawing whatever was last uploaded into a `ScreenBlock`
/// nothing is updating any more. Most visibly this matters for the EFB,
/// whose `bridge_active` check has no `view_loaded` entry to fall back on
/// at all and depends entirely on its `ScreenBlock`'s `frame` counter
/// (`bridge_active`'s doc comment): zeroing every screen's counter here is
/// what lets the EFB notice XPHFBW is gone and return to the QuickJS
/// placeholder like every other screen.
pub fn mark_bridge_gone() {
    let _ = with(|d| {
        if let Some(bridge) = d.bridge.as_mut() {
            for bs in bridge.screens.iter_mut().flatten() {
                bs.block.header().frame.store(0, Ordering::Relaxed);
            }
        }
    });
}

static DEVICE_GRABS: Mutex<Option<(PathBuf, HashSet<&'static str>)>> = Mutex::new(None);

pub fn request_device_grabs(dir: PathBuf) {
    if let Ok(mut grabs) = DEVICE_GRABS.lock() {
        *grabs = Some((dir, HashSet::new()));
    }
}

/// `FBW_DEVICE_GRABS=<folder>`: what every device holds, once a minute, in a folder named by Unix
/// time, to compare with `screen_grabs.ps1`'s pictures of the same moments.
pub fn device_grabs_on_request() {
    use std::sync::OnceLock;
    static FOLDER: OnceLock<Option<PathBuf>> = OnceLock::new();
    static NEXT: Mutex<Option<Instant>> = Mutex::new(None);
    let Some(folder) = FOLDER.get_or_init(|| std::env::var_os("FBW_DEVICE_GRABS").map(PathBuf::from)) else { return };
    let Ok(mut next) = NEXT.lock() else { return };
    let now = Instant::now();
    if next.is_some_and(|due| now < due) {
        return;
    }
    *next = Some(now + std::time::Duration::from_secs(60));
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    request_device_grabs(folder.join(stamp.to_string()));
}

pub fn snapshot_screens() -> Vec<(String, u32, u32, Vec<u8>)> {
    let Ok(guard) = DISPLAYS.lock() else { return Vec::new() };
    let Some(bridge) = guard.as_ref().and_then(|d| d.bridge.as_ref()) else { return Vec::new() };
    bridge
        .screens
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let bs = s.as_ref()?;
            let header = bs.block.header();
            let (w, h) = (header.width.load(Ordering::Relaxed), header.height.load(Ordering::Relaxed));
            if w == 0 || h == 0 || header.frame.load(Ordering::Relaxed) == 0 {
                return None;
            }
            let n = w as usize * h as usize * 4;
            Some((SCREENS.get(i)?.id.to_string(), w, h, bs.block.pixels().get(..n)?.to_vec()))
        })
        .collect()
}

/// Create the cockpit devices, one per screen. Called once the plugin is
/// enabled.
pub fn start(xplm: &'static Xplm) {
    let Some(api) = xplm.avionics() else {
        log("this X-Plane has no cockpit device API (12.1 or later needed); the screens are not shown");
        return;
    };
    let Some(root) = html_ui() else { return };
    {
        let Ok(mut guard) = DISPLAYS.lock() else { return };
        let mut displays = guard.take().unwrap_or_else(|| Displays::new(Some(xplm), root));
        displays.xplm = Some(xplm);
        displays.api = Some(api);
        displays.fbo = xplm.find("sim/graphics/view/current_gl_fbo");
        displays.viewport = xplm.find("sim/graphics/view/viewport");
        #[cfg(feature = "native-displays")]
        if displays.native.is_none() {
            displays.native = native_route();
        }
        *guard = Some(displays);
    }
    crate::browser_screens::note_avionics(&api);
    // Made without holding the lock: X-Plane may call back straight away.
    let mut handles = Vec::new();
    for (index, def) in SCREENS.iter().enumerate() {
        if crate::browser_screens::shows(def.id) {
            let handle = crate::browser_screens::create_device(&api, def, index as *mut c_void);
            if handle.is_null() {
                log(&format!("X-Plane refused the browser device {}", def.id));
            }
            handles.push(handle);
            continue;
        }
        let id = CString::new(def.id).expect("no nul");
        let title = CString::new(def.title).expect("no nul");
        let (dw, dh) = crate::xphfbw_bridge_views::device_size(def.id, def.width, def.height);
        let (w, h) = (dw as c_int, dh as c_int);
        let mut spec = xp::CreateAvionics {
            struct_size: std::mem::size_of::<xp::CreateAvionics>() as c_int,
            screen_width: w,
            screen_height: h,
            bezel_width: w,
            bezel_height: h,
            screen_offset_x: 0,
            screen_offset_y: 0,
            draw_on_demand: c_int::from(api.needs_drawing.is_some()),
            bezel_draw: None,
            draw: Some(draw_callback),
            bezel_click: None,
            bezel_right_click: None,
            bezel_scroll: None,
            bezel_cursor: None,
            screen_touch: Some(touch_callback),
            screen_right_touch: Some(right_touch_callback),
            screen_scroll: Some(scroll_callback),
            screen_cursor: Some(cursor_callback),
            // Typed-into screens only (keyboard_callback's doc comment): the
            // MFD through the KCCU, and the two OITs, which are
            // keyboard-and-trackball terminals rather than touchscreens.
            keyboard: HAS_KEYBOARD.contains(&def.id).then_some(keyboard_callback as xp::AvionicsKey),
            brightness: Some(brightness_callback),
            device_id: id.as_ptr(),
            device_name: title.as_ptr(),
            refcon: index as *mut c_void,
        };
        let handle = unsafe { (api.create)(&mut spec) };
        if handle.is_null() {
            log(&format!("X-Plane refused the device {}", def.id));
        }
        handles.push(handle);
    }
    let _ = with(|d| {
        for (screen, handle) in d.screens.iter_mut().zip(handles) {
            screen.handle = handle;
        }
        log(&format!("{} cockpit devices made", d.screens.iter().filter(|s| !s.handle.is_null()).count()));
    });
    if xplm.register_key_sniffer(key_sniffer, false, std::ptr::null_mut()) {
        log("key sniffer registered: click the MCDU or an OIT and type into it where it sits in the cockpit");
    } else {
        log("X-Plane refused the key sniffer: the MCDU can only be typed into popped out (right-click it)");
    }
    // Pop-out: a command per screen, and a menu so the commands can be
    // found without binding anything first.
    let commands = popout::Commands::register(xplm, popout_command);
    log(&format!("{} display pop-out commands registered (fbw/display/popout/<screen>)", commands.refs.len()));
    let menu = xplm.menu("A380X Displays", popout_menu);
    if !menu.is_null() {
        for (i, def) in SCREENS.iter().enumerate() {
            let sub = xplm.submenu(menu, &popout::pretty(def.id), popout_menu);
            if sub.is_null() {
                continue;
            }
            xplm.menu_item(sub, "Own window (second monitor)", popout::refcon_for(i, popout::Act::PopOut));
            xplm.menu_item(sub, "Float in X-Plane", popout::refcon_for(i, popout::Act::PopUp));
            xplm.menu_item(sub, "Put away", popout::refcon_for(i, popout::Act::Close));
        }
    }
}

/// X-Plane pressed one of the pop-out commands. Only the press matters; a
/// release would toggle it straight back.
unsafe extern "C" fn popout_command(_: xp::CommandRef, phase: c_int, refcon: *mut c_void) -> c_int {
    if phase == 0 {
        let (screen, act) = popout::unpack(refcon);
        let _ = with_from_callback(|d| popout::apply(d, screen, act));
    }
    popout::HANDLED
}

/// A pop-out menu entry was chosen.
unsafe extern "C" fn popout_menu(_: *mut c_void, item: *mut c_void) {
    let (screen, act) = popout::unpack(item);
    let _ = with_from_callback(|d| popout::apply(d, screen, act));
}

/// Destroy the devices and free what was made on the GPU.
pub fn stop(xplm: &'static Xplm) {
    crate::browser_screens::stop(xplm.avionics().as_ref());
    xplm.unregister_key_sniffer(key_sniffer, false, std::ptr::null_mut());
    release_keyboard("the displays are shutting down");
    let taken = DISPLAYS.lock().ok().and_then(|mut g| g.take());
    let Some(mut d) = taken else { return };
    if let Some(api) = xplm.avionics() {
        for s in &d.screens {
            if !s.handle.is_null() {
                unsafe { (api.destroy)(s.handle) };
            }
        }
    }
    if let Some(Ok(renderer)) = d.renderer.as_mut() {
        let mut gpus: Vec<&mut gl::ScreenGpu> = d.screens.iter_mut().map(|s| &mut s.gpu).collect();
        let mut bridges: Vec<&mut gl::BridgeGpu> =
            d.bridge.iter_mut().flat_map(|b| b.screens.iter_mut()).filter_map(|s| s.as_mut()).map(|bs| &mut bs.gpu).collect();
        renderer.release(&mut gpus, &mut bridges);
    }
}

/// Which screen typing currently goes to, or -1 for X-Plane's own. Read
/// by [`key_sniffer`] on every keystroke the sim sees, so it is a plain
/// atomic rather than something behind the `DISPLAYS` lock: the overwhelming
/// case is nothing focused, and that has to cost nothing.
static COCKPIT_KEYBOARD: AtomicIsize = AtomicIsize::new(-1);

static EFB_TEXT_FOCUS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static MFD_TEXT_FOCUS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub const BREAKER_SCREENS: [(&str, &str); 5] =
    [("SCREEN_DU_PFDL", "capt-pfd-du"), ("SCREEN_DU_NDL", "capt-nd-du"), ("SCREEN_DU_EWD", "capt-ewd-du"), ("SCREEN_DU_PFDR", "fo-pfd-du"), ("SCREEN_DU_NDR", "fo-nd-du")];

static BREAKER_OPEN: [std::sync::atomic::AtomicBool; 5] = [const { std::sync::atomic::AtomicBool::new(false) }; 5];

pub fn set_screen_breaker_open(index: usize, open: bool) {
    if let Some(flag) = BREAKER_OPEN.get(index) {
        if flag.swap(open, Ordering::Relaxed) != open {
            log(&format!("{}: its display unit breaker is {}", BREAKER_SCREENS[index].0, if open { "open, so it goes dark" } else { "closed again" }));
        }
    }
}

fn screen_breaker_open(id: &str) -> bool {
    BREAKER_SCREENS.iter().position(|(screen, _)| *screen == id).is_some_and(|i| BREAKER_OPEN[i].load(Ordering::Relaxed))
}

pub fn set_efb_text_focus(focused: bool) {
    if EFB_TEXT_FOCUS.swap(focused, Ordering::Relaxed) != focused {
        log(&format!("EFB: a text field {} focus", if focused { "has" } else { "lost" }));
    }
}

pub fn set_mfd_text_focus(focused: bool) {
    if MFD_TEXT_FOCUS.swap(focused, Ordering::Relaxed) != focused {
        log(&format!("MFD: a text field {} focus", if focused { "has" } else { "lost" }));
    }
}

pub(crate) fn efb_text_focus() -> bool {
    EFB_TEXT_FOCUS.load(Ordering::Relaxed)
}

pub(crate) fn mfd_text_focus() -> bool {
    MFD_TEXT_FOCUS.load(Ordering::Relaxed)
}

fn screen_ignores_keys(screen: isize) -> bool {
    let Some(def) = (screen >= 0).then(|| SCREENS.get(screen as usize)).flatten() else { return false };
    match def.id {
        EFB_SCREEN => !EFB_TEXT_FOCUS.load(Ordering::Relaxed) && !crate::browser_screens::page_text_focus(EFB_SCREEN),
        "SCREEN_DU_MFD" => !MFD_TEXT_FOCUS.load(Ordering::Relaxed) && !crate::browser_screens::page_text_focus("SCREEN_DU_MFD"),
        _ => false,
    }
}

/// Typing goes back to X-Plane. `why` is logged, so the user can always
/// find out where their keystrokes went.
fn release_keyboard(why: &str) {
    let was = COCKPIT_KEYBOARD.swap(-1, Ordering::Relaxed);
    if was >= 0 {
        let id = SCREENS.get(was as usize).map_or("a screen", |d| d.id);
        log(&format!("{id}: typing goes back to X-Plane ({why})"));
    }
}

/// Whether a keystroke belongs to the focused screen rather than to the
/// sim. An MCDU scratchpad takes printable characters, and Backspace for
/// CLR; Tab, Return and Delete are the OITs' terminal keys. Everything
/// else -- the function keys, the arrows, anything with Ctrl or Alt held --
/// is left to X-Plane, so the views, the pause key and the developer
/// shortcuts keep working with a scratchpad focused.
pub(crate) fn typed_into_cockpit(ch: u32, vkey: u32, flags: c_int) -> bool {
    if flags & (xp::CONTROL_FLAG | xp::ALT_FLAG) != 0 {
        return false;
    }
    matches!(vkey, xp::VK_BACK | xp::VK_TAB | xp::VK_RETURN | xp::VK_DELETE) || (0x20..0x7f).contains(&ch)
}

/// Every keystroke the sim sees, registered after windows so nothing is
/// taken from a text field that already wanted it
/// ([`Xplm::register_key_sniffer`]). With a screen focused
/// ([`Displays::aim_keyboard`]) the keys an MCDU can use are swallowed and
/// forwarded to it; everything else, and every keystroke with nothing
/// focused, travels on untouched.
unsafe extern "C" fn key_sniffer(key: c_char, flags: c_int, vkey: c_char, _refcon: *mut c_void) -> c_int {
    const PASS_ON: c_int = 1;
    const SWALLOW: c_int = 0;
    if let Some(taken) = crate::browser_screens::sniff_key(key as u8 as u32, vkey as u8 as u32, flags) {
        return if taken { SWALLOW } else { PASS_ON };
    }
    let screen = COCKPIT_KEYBOARD.load(Ordering::Relaxed);
    if screen < 0 {
        return PASS_ON;
    }
    let (ch, vkey) = (key as u8 as u32, vkey as u8 as u32);
    if vkey == xp::VK_ESCAPE {
        // Passed on as well: Escape is how X-Plane leaves its own screens.
        if flags & xp::KEY_DOWN_FLAG != 0 {
            release_keyboard("Escape was pressed");
        }
        return PASS_ON;
    }
    if !typed_into_cockpit(ch, vkey, flags) || screen_ignores_keys(screen) {
        return PASS_ON;
    }
    if flags & xp::KEY_DOWN_FLAG == 0 {
        // The release of a key whose press we took. Swallowed as well, so
        // the sim never sees half a keystroke, but there is nothing to
        // deliver: the screens hear about presses and auto-repeats only.
        return SWALLOW;
    }
    let sent = std::panic::catch_unwind(|| with_from_callback(|d| d.sniff_key(screen as usize, ch, vkey, flags as u32)))
        .ok()
        .flatten()
        .unwrap_or(false);
    if sent { SWALLOW } else { PASS_ON }
}

fn screen_of(refcon: *mut c_void) -> usize {
    refcon as usize
}

unsafe extern "C" fn draw_callback(refcon: *mut c_void) {
    let _ = std::panic::catch_unwind(|| crate::perf::time("screens (draw callbacks)", || with_from_callback(|d| d.draw_screen_timed(screen_of(refcon)))));
}

/// The first few pointer callbacks X-Plane makes, whatever happens to them
/// next: whether 3D-cockpit clicks reach the plugin at all.
fn log_callback(what: &str, refcon: *mut c_void, x: c_int, y: c_int, status: c_int, handled: bool) {
    static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if LOGGED.fetch_add(1, Ordering::Relaxed) < 40 {
        let id = SCREENS.get(screen_of(refcon)).map_or("?", |d| d.id);
        log(&format!("input callback: {what} on {id} at ({x},{y}) status {status}, displays lock taken: {handled}"));
    }
}

unsafe extern "C" fn touch_callback(x: c_int, y: c_int, status: c_int, refcon: *mut c_void) -> c_int {
    let handled = std::panic::catch_unwind(|| with_from_callback(|d| d.mouse(screen_of(refcon), x, y, status, 0)).is_some()).unwrap_or(false);
    log_callback("touch", refcon, x, y, status, handled);
    1
}

unsafe extern "C" fn right_touch_callback(x: c_int, y: c_int, status: c_int, refcon: *mut c_void) -> c_int {
    let index = screen_of(refcon);
    if !right_click_is_ours(index) {
        return 0;
    }
    // The keyboard gesture (toggle_keyboard's doc comment): only a screen
    // that takes keystrokes, and it does not also forward as a click. Right
    // dragging is how X-Plane looks around, so only a press that came straight
    // back counts.
    let _ = std::panic::catch_unwind(|| {
        with_from_callback(|d| {
            let press = &mut d.screens[index].right_press;
            if status == xp::MOUSE_DOWN {
                *press = Some(RightPress::new(x, y, std::time::Instant::now()));
            } else if status == xp::MOUSE_DRAG {
                if let Some(p) = press {
                    p.moved_to(x, y);
                }
            } else if status == xp::MOUSE_UP && press.take().is_some_and(|p| p.is_click(std::time::Instant::now())) {
                d.toggle_keyboard(index);
            }
        })
    });
    1
}

/// A right button press on a keyboard screen, until it comes back up.
#[derive(Clone, Copy)]
pub(crate) struct RightPress {
    at: std::time::Instant,
    x: c_int,
    y: c_int,
    dragged: bool,
}

impl RightPress {
    pub(crate) fn new(x: c_int, y: c_int, at: std::time::Instant) -> Self {
        Self { at, x, y, dragged: false }
    }

    pub(crate) fn moved_to(&mut self, x: c_int, y: c_int) {
        self.dragged |= (x - self.x).abs() + (y - self.y).abs() > 8;
    }

    pub(crate) fn is_click(&self, now: std::time::Instant) -> bool {
        !self.dragged && now.duration_since(self.at) < std::time::Duration::from_millis(400)
    }
}

pub(crate) fn bridge_wheel_clicks(dom_delta_y: f64) -> f64 {
    -dom_delta_y / 100.
}

pub(crate) fn toggle_allowed(last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    last.is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_millis(400))
}

pub(crate) fn right_click_is_ours(index: usize) -> bool {
    SCREENS.get(index).is_some_and(|d| HAS_KEYBOARD.contains(&d.id))
}

unsafe extern "C" fn scroll_callback(x: c_int, y: c_int, wheel: c_int, clicks: c_int, refcon: *mut c_void) -> c_int {
    // Vertical only: the contract's wheel event has one delta. A DOM wheel
    // event's deltaY is positive scrolling down, towards the user, about
    // 100 CSS pixels a notch; X-Plane counts clicks positive away from the
    // user.
    if wheel != 0 {
        return 0;
    }
    let _ = std::panic::catch_unwind(|| {
        with_from_callback(|d| d.dispatch_pointer(screen_of(refcon), "wheel", InputKind::Wheel, x, y, 0, -100. * clicks as f64))
    });
    1
}

unsafe extern "C" fn cursor_callback(x: c_int, y: c_int, refcon: *mut c_void) -> c_int {
    let handled = std::panic::catch_unwind(|| with_from_callback(|d| d.cursor(screen_of(refcon), x, y)).is_some()).unwrap_or(false);
    static FIRST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if FIRST.fetch_add(1, Ordering::Relaxed) < 5 {
        log_callback("cursor", refcon, x, y, 0, handled);
    }
    #[cfg(all(windows, feature = "native-displays"))]
    {
        let touch = with_from_callback(|d| {
            let id = d.screens[screen_of(refcon)].def.id;
            matches!(id, "SCREEN_OIT_LEFT" | "SCREEN_OIT_RIGHT" | "SCREEN_EFB") && crate::native_displays::input::is_native(id)
        })
        .unwrap_or(false);
        if touch {
            #[link(name = "user32")]
            extern "system" {
                fn LoadCursorW(instance: *mut c_void, name: *const u16) -> *mut c_void;
                fn SetCursor(cursor: *mut c_void) -> *mut c_void;
            }
            const IDC_HAND: usize = 32649;
            unsafe { SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_HAND as *const u16)) };
            return 3;
        }
    }
    xp::CURSOR_DEFAULT
}

/// Full brightness: the dimming FlyByWire's model does (potentiometer and
/// bus power) is drawn into the texture per region, which X-Plane's single
/// per-device brightness could not do for the two MFDs on one texture.
unsafe extern "C" fn brightness_callback(_rheo: f32, _ambient: f32, _bus: f32, _refcon: *mut c_void) -> f32 {
    1.
}

/// The screens a physical keyboard types into, and so the only ones given
/// an `XPLMAvionicsKeyboard_f` ([`start`]) and the right-click focus gesture
/// ([`right_touch_callback`]): the MFD, whose keyboard is the KCCU, and the
/// two OITs, which the real aircraft drives with a keyboard and trackball on
/// the lateral console rather than by touch (docs/oit.md). Every other
/// screen is a display, not an input device.
const HAS_KEYBOARD: &[&str] = &["SCREEN_DU_MFD", "SCREEN_OIT_LEFT", "SCREEN_OIT_RIGHT", "SCREEN_EFB"];

/// The KCCU types into the MFD — and the console keyboard into an OIT —
/// through this: `XPLMAvionicsKeyboard_f`
/// (XPLMDisplay.h) fires "when your device is popped up and you've
/// requested to capture the keyboard", i.e. only once
/// `XPLMTakeAvionicsKeyboardFocus` has been called for this device's popup
/// window — XPLM410 has no way to capture the keyboard for a device
/// embedded in the 3D cockpit alone. [`right_touch_callback`] gives the
/// screen that popup and focus (right-click it; right-click again, or click
/// away, gives the focus back — [`Displays::toggle_keyboard`]).
///
/// Only registered on the devices in [`HAS_KEYBOARD`] ([`start`]), and
/// only reached once that device has been popped out. Typing into a screen
/// still in the 3D cockpit goes through [`key_sniffer`] instead.
unsafe extern "C" fn keyboard_callback(key: c_char, flags: c_int, vkey: c_char, refcon: *mut c_void, losing_focus: c_int) -> c_int {
    if losing_focus != 0 {
        // hide_popup's doc comment: close the popup along with the
        // focus, however it was lost, so it never lingers unfocused.
        let _ = std::panic::catch_unwind(|| with_from_callback(|d| d.hide_popup(screen_of(refcon))));
        return 0;
    }
    let ch = key as u8 as u32;
    let handled = flags & xp::KEY_DOWN_FLAG == 0
        || std::panic::catch_unwind(|| {
            with_from_callback(|d| d.dispatch_key(screen_of(refcon), ch, vkey as u8 as u32, flags as u32)).unwrap_or(false)
        })
        .unwrap_or(false);
    static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if LOGGED.fetch_add(1, Ordering::Relaxed) < 40 {
        log(&format!(
            "input callback: key {:?} (vkey {}) flags {:#x} on {}, handled: {handled}",
            char::from_u32(ch).filter(|_| ch != 0),
            vkey as u8,
            flags,
            SCREENS.get(screen_of(refcon)).map_or("?", |d| d.id)
        ));
    }
    handled as c_int
}

#[cfg(all(test, feature = "native-displays"))]
mod native_paint_tests {
    use super::native_paint_off;
    use crate::native_displays::screen::parse;
    use crate::xphfbw_bridge_views::paint_off_bit;

    #[test]
    fn only_selected_screens_with_a_native_frame_stop_painting() {
        let selection = parse("nd,mfd");
        let landed = |id: &str| id != "SCREEN_DU_NDR";
        let mask = native_paint_off(&selection, landed);
        assert_eq!(mask, paint_off_bit("SCREEN_DU_NDL") | paint_off_bit("SCREEN_DU_MFD"));
        assert_eq!(mask & paint_off_bit("SCREEN_DU_PFDL"), 0);
    }

    #[test]
    fn nothing_selected_or_nothing_landed_paints_everything() {
        assert_eq!(native_paint_off(&parse("off"), |_| true), 0);
        assert_eq!(native_paint_off(&parse("all"), |_| false), 0);
    }
}

#[cfg(all(test, feature = "native-displays"))]
mod native_route_tests {
    use super::*;
    use crate::native_displays::screen::{parse, Native};

    const TARGET: gl::Target = gl::Target { framebuffer: 0, viewport: [0, 0, 768, 1024] };

    fn displays_with(spec: &str) -> Displays {
        let mut d = Displays::new(None, PathBuf::new());
        d.native = Native::idle(&parse(spec)).map(native_route_for);
        d
    }

    #[test]
    fn a_native_screen_never_falls_through_before_its_first_frame() {
        let mut d = displays_with("ndl");
        let (ndl, ndr) = (screens::find("SCREEN_DU_NDL").unwrap(), screens::find("SCREEN_DU_NDR").unwrap());
        assert!(d.is_native(ndl) && !d.is_native(ndr));
        assert!(d.draw_bridge_source(ndl, None, TARGET, vec![1.], vec![]), "a native screen is drawn (black) with no frame and no Chromium");
        assert!(d.draw_bridge_screen(ndl, TARGET));
        assert!(!d.draw_bridge_source(ndr, None, TARGET, vec![1.], vec![]), "a screen not picked still falls back");
    }

    #[test]
    fn a_chromium_picture_routed_onto_a_native_screen_is_black() {
        assert!(must_draw_black(true, false));
        assert!(!must_draw_black(true, true));
        assert!(!must_draw_black(false, false));
        assert!(!must_draw_black(false, true));
        let mut d = displays_with("ndl");
        let (ndl, ndr) = (screens::find("SCREEN_DU_NDL").unwrap(), screens::find("SCREEN_DU_NDR").unwrap());
        assert!(d.draw_routed_source(ndl, ndr, None, TARGET, vec![1.], vec![]));
        assert!(!d.draw_routed_source(ndr, ndr, None, TARGET, vec![1.], vec![]));
    }

    #[test]
    fn no_native_route_means_no_native_screen() {
        let d = Displays::new(None, PathBuf::new());
        assert!((0..SCREENS.len()).all(|i| !d.is_native(i)));
    }
}
