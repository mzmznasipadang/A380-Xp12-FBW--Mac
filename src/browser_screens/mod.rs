//! Experimental: the cockpit screens with no XPHFBW process, off unless the
//! `browserDisplays` setting is on. The systems run on a thread of the plugin
//! (`FBW_SYSTEMS_IN_PROCESS=0` puts them in `fbw_a380_systems_server.exe`) and
//! `server.rs` stands in for the rest of XPHFBW. The pages draw in our own browsers inside X-Plane's Chromium
//! (`own_cef.rs`, Windows and macOS); `FBW_BROWSER_ROUTE=device` uses X-Plane's browser devices
//! instead (`xplm_WindowContentTypeBrowser`), which X-Plane runs at about 30 Hz.

mod dead_band;
pub mod server;
#[cfg(any(windows, target_os = "macos"))]
pub mod own_cef;
/// Elsewhere the pages draw on X-Plane's browser devices: `own_route` is false.
#[cfg(not(any(windows, target_os = "macos")))]
pub mod own_cef {
    use crate::xphfbw_bridge::Session;

    pub struct Wanted {
        pub key: String,
        pub screen: Option<String>,
        pub width: u32,
        pub height: u32,
    }

    pub struct Browsers;

    impl Browsers {
        pub fn tick(&mut self, _: &Session) {}
        pub fn reload_all(&self) {}
        pub fn close(self) {}
    }

    pub fn open_all(_: &str, _: u16, _: &[Wanted], _: std::ffi::c_int) -> Option<Browsers> {
        None
    }

    pub fn prepare_chromium_start() {}

    pub fn chromium_switches_applied() -> bool {
        false
    }
}
#[cfg(windows)]
mod main_thread_profile;
#[cfg(any(windows, target_os = "macos", test))]
mod paint_copy;
#[cfg(any(windows, target_os = "macos"))]
mod paint_thread;

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::display::screens::ScreenDef;
use crate::efb_web::Paths;
use crate::xp::{self, AvionicsApi, AvionicsId};
use crate::xphfbw_bridge::Session;
use crate::xphfbw_bridge_views::{self as views, ViewDef, EFB_HEIGHT, EFB_SCREEN, EFB_WIDTH};

const CONTENT_BROWSER: c_int = 2;
/// How long a hidden device gets to connect before its page moves onto a shown one.
const HIDDEN_DEVICE_WAIT: Duration = Duration::from_secs(45);
const STATS_EVERY: Duration = Duration::from_secs(30);

type LoadFinished = unsafe extern "C" fn(AvionicsId, *const c_char, *mut c_void);
type LoadError = unsafe extern "C" fn(AvionicsId, *const c_char, *const c_char, *mut c_void);

/// XPLMCreateAvionics_t as SDK 4.4.0 extends it.
#[repr(C)]
struct CreateBrowser {
    base: xp::CreateAvionics,
    content_type: c_int,
    window_with_chrome: c_int,
    load_finished: Option<LoadFinished>,
    load_error: Option<LoadError>,
}

#[derive(Clone, Copy)]
struct Api {
    set_url: unsafe extern "C" fn(AvionicsId, *const c_char),
    refresh: unsafe extern "C" fn(AvionicsId, c_int),
    inject: unsafe extern "C" fn(AvionicsId, *const c_char),
}

fn api() -> Option<Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    *API.get_or_init(|| unsafe {
        Some(Api {
            set_url: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(AvionicsId, *const c_char)>(xp::Xplm::xplm_symbol("XPLMAvionicsSetURL")?),
            refresh: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(AvionicsId, c_int)>(xp::Xplm::xplm_symbol("XPLMAvionicsRefresh")?),
            inject: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(AvionicsId, *const c_char)>(xp::Xplm::xplm_symbol("XPLMAvionicsInjectScript")?),
        })
    })
}

pub fn own_route() -> bool {
    static OWN: OnceLock<bool> = OnceLock::new();
    *OWN.get_or_init(|| cfg!(any(windows, target_os = "macos")) && std::env::var("FBW_BROWSER_ROUTE").map_or(true, |v| v != "device"))
}

/// The setting (or `FBW_BROWSER_DISPLAYS=1`), on an X-Plane that has what the route needs.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let wanted = std::env::var("FBW_BROWSER_DISPLAYS").map_or_else(|_| !cfg!(windows) || crate::app_settings::load_now().browser_displays, |v| v == "1");
        // Windows' own route still asks a browser device to start Chromium; macOS's
        // X-Plane has it running already and needs none.
        if wanted && api().is_none() && !(cfg!(target_os = "macos") && own_route()) {
            log("this X-Plane has no browser devices (12.4 or later needed); using the app");
            return false;
        }
        #[cfg(any(windows, target_os = "macos"))]
        if wanted && own_route() {
            if let Err(e) = xp::system_path().ok_or_else(|| "no X-Plane folder".to_string()).and_then(|root| own_cef::available(&root)) {
                // macOS: X-Plane may load its Chromium after this plugin starts; `chromium_up` checks again.
                let later = cfg!(target_os = "macos") && !own_cef::loaded();
                log(&format!("{}: {e}", if later { "checking again once the screens start" } else { "using the app" }));
                if !later {
                    return false;
                }
            }
        }
        if wanted {
            log("experimental: screens in X-Plane's browser, XPHFBW stays off");
        }
        wanted
    })
}

pub(crate) fn log(message: &str) {
    if let Ok(mut q) = logs().lock() {
        if q.len() < 256 {
            q.push(message.to_string());
        }
    }
}

fn logs() -> &'static Mutex<Vec<String>> {
    static L: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    L.get_or_init(Mutex::default)
}

fn panel_views() -> Vec<ViewDef> {
    let panel = crate::sound_pack::plugin_dir()
        .and_then(|d| Some(d.parent()?.parent()?.join("panel").join("panel.cfg")))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    views::views_to_open(views::view_list(&panel), crate::native_page_state::stopped_pages_mask())
}

fn page_screens() -> &'static [String] {
    static LIST: OnceLock<Vec<String>> = OnceLock::new();
    LIST.get_or_init(|| {
        let mut list: Vec<String> = panel_views().into_iter().filter(|v| !v.screen.is_empty()).map(|v| v.screen).collect();
        list.push(EFB_SCREEN.to_string());
        list.sort();
        list.dedup();
        list
    })
}

/// Whether this screen is a browser device here. Screens with no page stay on the plugin's own drawing.
pub fn shows(screen: &str) -> bool {
    enabled() && !own_route() && page_screens().iter().any(|s| s.eq_ignore_ascii_case(screen))
}

/// Device pixels per page pixel, from the "Screen resolution" setting. X-Plane's
/// browser paints at a device pixel ratio of 2 (measured, 12.4.4), so half the
/// setting gives the sharpness the setting gives XPHFBW, at the same raster size.
fn device_scale(screen: &str) -> f64 {
    static BASE: OnceLock<f64> = OnceLock::new();
    let base = *BASE.get_or_init(|| crate::app_settings::load_now().display_scale * 0.5);
    match screen.to_ascii_uppercase().as_str() {
        "FCU" | "SCREEN_DU_RMP_1" | "SCREEN_DU_RMP_2" | "SCREEN_DU_RMP_3" => base * 0.5,
        "SCREEN_OIT_LEFT" | "SCREEN_OIT_RIGHT" => base * 0.75,
        _ => base,
    }
}

fn device_size(screen: &str, width: u32, height: u32) -> (u32, u32) {
    let s = device_scale(screen);
    ((width as f64 * s).round() as u32, (height as f64 * s).round() as u32)
}

/// The host page each device loads: a screen's views stacked, as XPHFBW's layers stack them.
#[derive(Default)]
pub struct Layout {
    pages: HashMap<String, String>,
}

impl Layout {
    pub fn page(&self, key: &str) -> Option<String> {
        self.pages.get(&key.to_ascii_uppercase()).cloned()
    }
}

fn host_page(screen: &str, css: (u32, u32), scale: f64, frames: &[(String, String)], dimming: &[[u32; 4]], underlay: bool) -> String {
    let (w, h) = css;
    // The NDs' terrain and weather radar are drawn under the page.
    let is_nd = matches!(screen.to_ascii_uppercase().as_str(), "SCREEN_DU_NDL" | "SCREEN_DU_NDR");
    let background = if own_route() && is_nd { "transparent" } else { "#000" };
    let mut body = String::new();
    if underlay {
        body.push_str(&format!(r#"<canvas id="under" style="width:{w}px;height:{h}px;display:none"></canvas>"#));
    }
    for (i, (name, url)) in frames.iter().enumerate() {
        let events = if i > 0 { "pointer-events:none;" } else { "" };
        body.push_str(&format!(r#"<iframe name="{name}" src="{url}" style="width:{w}px;height:{h}px;{events}"></iframe>"#));
    }
    if has_kccu_cursor(screen) && !own_route() {
        body.push_str(r##"<svg id="kccu" viewBox="0 0 80 80" style="width:120px;height:120px;display:none;pointer-events:none;overflow:visible"><path d="M0 0L40 35L80 0M0 80L40 45L80 80M40 39V41M39 40H41" fill="none" stroke-width="3"/></svg>"##);
    }
    for [x, y, rw, rh] in dimming {
        body.push_str(&format!(r#"<div class="dim" style="left:{x}px;top:{y}px;width:{rw}px;height:{rh}px;opacity:0"></div>"#));
    }
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><style>
html,body{{margin:0;padding:0;width:100%;height:100%;overflow:hidden;background:{background}}}
#w{{position:absolute;left:0;top:0;width:{w}px;height:{h}px;transform-origin:0 0;transform:scale({scale})}}
#w>*{{position:absolute;left:0;top:0;border:0;background:transparent}}
.dim{{background:#000;pointer-events:none}}
</style></head><body><div id="w">{body}</div><script>
const S={screen:?};const dims=[...document.querySelectorAll('.dim')];const under=document.getElementById('under');let gen=-1;
const kccu=document.getElementById('kccu');let kccuHide=0;
window.__kccu=(x,y,color)=>{{if(!kccu)return;kccu.style.left=(x-60)+'px';kccu.style.top=(y-60)+'px';kccu.firstChild.setAttribute('stroke',color);kccu.style.display='block';clearTimeout(kccuHide);kccuHide=setTimeout(()=>{{kccu.style.display='none';}},5000);}};
function addView(name,url){{const f=document.createElement('iframe');f.name=name;f.src=url;f.style.cssText='width:1px;height:1px;opacity:0;pointer-events:none';document.getElementById('w').append(f);}}
async function tick(){{try{{const r=await(await fetch('/__b/state?s='+S)).json();
dims.forEach((d,i)=>{{const b=r.d[i];d.style.opacity=b===undefined?0:1-b;}});
if(under){{if(!r.u){{under.style.display='none';gen=-1;}}else if(r.u.g!==gen){{gen=r.u.g;
const px=new Uint8ClampedArray(await(await fetch('/__b/underlay?s='+S)).arrayBuffer());
if(px.length===r.u.w*r.u.h*4){{under.width=r.u.w;under.height=r.u.h;under.getContext('2d').putImageData(new ImageData(px,r.u.w,r.u.h),0,0);under.style.display='block';}}}}}}
}}catch(e){{}}setTimeout(tick,200);}}if(dims.length||under)tick();
</script></body></html>"#
    )
}

fn has_kccu_cursor(screen: &str) -> bool {
    matches!(screen.to_ascii_uppercase().as_str(), "SCREEN_DU_MFD" | "SCREEN_DU_NDL" | "SCREEN_DU_NDR")
}

fn layout(defs: &[ViewDef], screens: &[&'static ScreenDef]) -> Layout {
    let mut pages = HashMap::new();
    let mut seen: Vec<&str> = Vec::new();
    for v in defs.iter().filter(|v| !v.screen.is_empty()) {
        if seen.iter().any(|s| s.eq_ignore_ascii_case(&v.screen)) {
            continue;
        }
        seen.push(&v.screen);
        let mut stack: Vec<&ViewDef> = defs.iter().filter(|o| o.screen.eq_ignore_ascii_case(&v.screen)).collect();
        stack.sort_by_key(|o| o.layer);
        let frames: Vec<(String, String)> = stack.iter().map(|o| (format!("xphfbw:{}:{}", o.index, o.screen), format!("/Pages/VCockpit/Instruments/{}", o.gauge_url))).collect();
        let scale = if own_route() { 1. } else { device_scale(&v.screen) };
        let def = screens.iter().find(|d| d.id.eq_ignore_ascii_case(&v.screen));
        let dimming: Vec<[u32; 4]> = if own_route() { Vec::new() } else { def.map(|d| d.dimming.iter().map(|r| r.region).collect()).unwrap_or_default() };
        let underlay = !own_route() && matches!(v.screen.to_ascii_uppercase().as_str(), "SCREEN_DU_NDL" | "SCREEN_DU_NDR");
        pages.insert(v.screen.to_ascii_uppercase(), host_page(&v.screen, (v.width, v.height), scale, &frames, &dimming, underlay));
    }
    let efb = host_page(EFB_SCREEN, (EFB_WIDTH, EFB_HEIGHT), if own_route() { 1. } else { device_scale(EFB_SCREEN) }, &[("xphfbw:efb".to_string(), "/".to_string())], &[], false);
    pages.insert(EFB_SCREEN.to_string(), efb);
    for v in defs.iter().filter(|v| v.screen.is_empty()) {
        let frames = [(format!("xphfbw:{}:", v.index), format!("/Pages/VCockpit/Instruments/{}", v.gauge_url))];
        pages.insert(format!("@{}", v.index), host_page("", (1, 1), 1., &frames, &[], false));
    }
    Layout { pages }
}

struct Device {
    key: String,
    handle: usize,
    url_set: bool,
}

#[derive(Default)]
struct State {
    avionics: Option<AvionicsApi>,
    devices: Vec<Device>,
    hidden: Vec<(u32, String)>,
    hidden_made: bool,
    moved: Vec<u32>,
    port: u16,
    attached_at: Option<Instant>,
    spawns_seen: u32,
    stats_at: Option<Instant>,
    tag: String,
    wanted: Vec<own_cef::Wanted>,
    browsers: Option<own_cef::Browsers>,
    boot: usize,
    #[cfg(target_os = "macos")]
    cef_checked_at: Option<Instant>,
    #[cfg(target_os = "macos")]
    cef_waits: u32,
}

// The handles are only ever used on X-Plane's main thread.
unsafe impl Send for State {}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Mutex::default)
}

static CHROMIUM_UP: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn load_finished(_: AvionicsId, url: *const c_char, _: *mut c_void) {
    CHROMIUM_UP.store(true, Ordering::SeqCst);
    log(&format!("loaded {}", text(url)));
}

unsafe extern "C" fn load_error(_: AvionicsId, url: *const c_char, error: *const c_char, _: *mut c_void) {
    log(&format!("could not load {}: {}", text(url), text(error)));
}

unsafe fn text(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
}

unsafe extern "C" fn no_brightness_change(_rheo: f32, _ambient: f32, _bus: f32, _refcon: *mut c_void) -> f32 {
    1.
}

fn hovers() -> &'static Mutex<HashMap<u32, (f64, f64)>> {
    static H: OnceLock<Mutex<HashMap<u32, (f64, f64)>>> = OnceLock::new();
    H.get_or_init(Mutex::default)
}

pub(crate) fn take_hover(view: u32) -> Option<(f64, f64)> {
    hovers().lock().ok()?.remove(&view)
}

fn screen_view(screen: &str) -> Option<u32> {
    static VIEWS: OnceLock<Vec<ViewDef>> = OnceLock::new();
    VIEWS.get_or_init(panel_views).iter().find(|v| v.layer == 0 && v.screen.eq_ignore_ascii_case(screen)).map(|v| v.index)
}

/// Where the mouse is over a screen with a KCCU cursor, which a page may never
/// hear of: X-Plane tells the device while the mouse hovers.
unsafe extern "C" fn hover_callback(x: c_int, y: c_int, refcon: *mut c_void) -> c_int {
    if let Some(def) = crate::display::screens::SCREENS.get(refcon as usize) {
        let scale = device_scale(def.id);
        if let (Some(view), Ok(mut h)) = (screen_view(def.id), hovers().lock()) {
            h.insert(view, (x as f64 / scale, def.height as f64 - y as f64 / scale));
        }
    }
    xp::CURSOR_DEFAULT
}

fn create(avionics: &AvionicsApi, id: &str, title: &str, w: c_int, h: c_int, refcon: *mut c_void) -> AvionicsId {
    let hover = has_kccu_cursor(id);
    let (id, title) = (CString::new(id).expect("no nul"), CString::new(title).expect("no nul"));
    let mut spec = CreateBrowser {
        base: xp::CreateAvionics {
            struct_size: std::mem::size_of::<CreateBrowser>() as c_int,
            screen_width: w,
            screen_height: h,
            bezel_width: w,
            bezel_height: h,
            screen_offset_x: 0,
            screen_offset_y: 0,
            draw_on_demand: 0,
            bezel_draw: None,
            draw: None,
            bezel_click: None,
            bezel_right_click: None,
            bezel_scroll: None,
            bezel_cursor: None,
            screen_touch: None,
            screen_right_touch: None,
            screen_scroll: None,
            screen_cursor: hover.then_some(hover_callback as xp::AvionicsCursor),
            keyboard: None,
            brightness: Some(no_brightness_change),
            device_id: id.as_ptr(),
            device_name: title.as_ptr(),
            refcon,
        },
        content_type: CONTENT_BROWSER,
        window_with_chrome: 0,
        load_finished: Some(load_finished),
        load_error: Some(load_error),
    };
    unsafe { (avionics.create)(&mut spec as *mut CreateBrowser as *mut xp::CreateAvionics) }
}

/// The cockpit device for a screen, from `display::start`. Touches go straight
/// to the page, so none of the plugin's click callbacks are registered.
pub fn note_avionics(avionics: &AvionicsApi) {
    if let Ok(mut s) = state().lock() {
        s.avionics = Some(*avionics);
    }
}

pub fn create_device(avionics: &AvionicsApi, def: &ScreenDef, refcon: *mut c_void) -> AvionicsId {
    let (w, h) = device_size(def.id, def.width, def.height);
    let handle = create(avionics, def.id, def.title, w as c_int, h as c_int, refcon);
    if let Ok(mut s) = state().lock() {
        s.avionics = Some(*avionics);
        if !handle.is_null() {
            s.devices.push(Device { key: def.id.to_ascii_uppercase(), handle: handle as usize, url_set: false });
        }
    }
    handle
}

/// The session is up: serve it and point the devices at it.
pub fn attach(tag: &str, session: &Session, defs: &[ViewDef]) {
    let Some(port) = server::start() else {
        crate::log("browser screens: could not open a local port");
        return;
    };
    let Some(plugin_dir) = crate::sound_pack::plugin_dir() else { return };
    let Some(aircraft) = plugin_dir.parent().and_then(|p| p.parent()).map(|p| p.to_path_buf()) else { return };
    let Some(root) = xp::system_path() else { return };
    let paths = Paths { xplane_root: root, aircraft_dir: aircraft, ui_dir: plugin_dir.join("XPHFBW").join("ui") };
    let open = views::views_to_open(defs.to_vec(), session.slots.header().native_views_off.load(Ordering::Acquire));
    let screens: Vec<&'static ScreenDef> = crate::display::screens::SCREENS.iter().collect();
    let hidden: Vec<(u32, String)> = open.iter().filter(|v| v.screen.is_empty()).map(|v| (v.index, format!("@{}", v.index))).collect();
    if !server::attach(tag, paths, crate::static_source_patches(), layout(&open, &screens)) {
        crate::log("browser screens: could not open the session");
        return;
    }
    if let Ok(mut s) = state().lock() {
        let old = s.browsers.take();
        s.port = port;
        s.attached_at = Some(Instant::now());
        s.spawns_seen = session.slots.header().ready.load(Ordering::Acquire);
        s.hidden = hidden;
        s.tag = tag.to_string();
        s.wanted = wanted_browsers(&open);
        drop(s);
        if let Some(old) = old {
            old.close();
        }
    }
    if let Ok(mut s) = state().lock() {
        s.moved.clear();
        for d in &mut s.devices {
            d.url_set = false;
        }
    }
    crate::log(&format!("browser screens: serving {} views on port {port}", open.len()));
}

fn wanted_browsers(open: &[ViewDef]) -> Vec<own_cef::Wanted> {
    let mut out: Vec<own_cef::Wanted> = Vec::new();
    for v in open {
        if v.screen.is_empty() {
            out.push(own_cef::Wanted { key: format!("@{}", v.index), screen: None, width: 1, height: 1 });
        } else if !out.iter().any(|w| w.screen.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&v.screen))) {
            out.push(own_cef::Wanted { key: v.screen.to_ascii_uppercase(), screen: Some(v.screen.clone()), width: v.width, height: v.height });
        }
    }
    out.push(own_cef::Wanted { key: EFB_SCREEN.to_string(), screen: Some(EFB_SCREEN.to_string()), width: EFB_WIDTH, height: EFB_HEIGHT });
    out
}

fn fps_cap() -> c_int {
    let root = xp::system_path();
    let cap = root.and_then(|r| crate::efb_settings::load(&r).get("xphfbw.displayFps").and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))));
    cap.filter(|v| *v > 0).unwrap_or(60) as c_int
}

/// While the plugin loads: X-Plane can start Chromium before the plugin's first tick, and
/// Chromium takes its switches when it starts.
pub fn prepare_chromium() {
    if enabled() && own_route() {
        own_cef::prepare_chromium_start();
    }
}

/// Our browsers open once X-Plane's Chromium is up.
fn tick_own(session: &Session) {
    let Ok(mut s) = state().lock() else { return };
    let s = &mut *s;
    if s.attached_at.is_none() {
        return;
    }
    if s.browsers.is_none() {
        if !chromium_up(s) {
            return;
        }
        s.browsers = own_cef::open_all(&s.tag, s.port, &s.wanted, fps_cap());
        crate::log(&format!("browser screens: {} browsers opened in X-Plane's Chromium, our switches {}", s.wanted.len(), if own_cef::chromium_switches_applied() { "applied" } else { "not applied" }));
    }
    let spawns = session.slots.header().ready.load(Ordering::Acquire);
    let spawned = spawns != s.spawns_seen;
    s.spawns_seen = spawns;
    if let Some(b) = s.browsers.as_mut() {
        b.tick(session);
        if spawned || REFRESH.swap(false, Ordering::SeqCst) {
            b.reload_all();
            crate::log("browser screens: pages reloaded");
        }
    }
}

/// Chromium starts when X-Plane first needs it, so a blank browser device asks for
/// it, and is up once that page has loaded.
#[cfg(windows)]
fn chromium_up(s: &mut State) -> bool {
    if s.boot == 0 {
        let (Some(avionics), Some(api)) = (s.avionics, api()) else { return false };
        own_cef::prepare_chromium_start();
        let handle = create(&avionics, "A380X_BROWSER_BOOT", "A380X browser start", 16, 16, std::ptr::null_mut());
        if handle.is_null() {
            crate::log("browser screens: X-Plane refused the start device");
            return false;
        }
        let url = CString::new("about:blank").expect("no nul");
        unsafe { (api.set_url)(handle, url.as_ptr()) };
        s.boot = handle as usize;
    }
    CHROMIUM_UP.load(Ordering::SeqCst)
}

/// macOS: X-Plane starts its Chromium itself, and CEF's UI thread there can only be
/// the AppKit main thread this runs on. Checked once a second, as CEF logs each
/// check made before it is up.
#[cfg(target_os = "macos")]
fn chromium_up(s: &mut State) -> bool {
    if s.cef_checked_at.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
        return false;
    }
    s.cef_checked_at = Some(Instant::now());
    // The framework may have loaded since `enabled()` looked (macOS ignores the folder).
    let ready = own_cef::available(std::path::Path::new("")).and_then(|_| if own_cef::on_ui_thread() { Ok(()) } else { Err("cef_currently_on(UI) is false".to_string()) });
    if ready.is_ok() {
        crate::log(&format!("browser screens: X-Plane's Chromium is up and this is its UI thread (after {} checks)", s.cef_waits + 1));
        return true;
    }
    if s.cef_waits % 30 == 0 {
        crate::log(&format!("browser screens: X-Plane's Chromium is not ready on this thread ({}, {} s); the screens stay dark until it is", ready.unwrap_err(), s.cef_waits));
    }
    s.cef_waits += 1;
    false
}

#[cfg(not(any(windows, target_os = "macos")))]
fn chromium_up(_: &mut State) -> bool {
    false
}

pub fn detach() {
    server::detach();
    if let Ok(mut s) = state().lock() {
        s.attached_at = None;
    }
}

fn url_for(port: u16, index: usize, key: &str) -> CString {
    // A host name per device: Chromium allows six requests at a time per host,
    // and a page blocked on a synchronous request must not hold up the others.
    CString::new(format!("http://s{index}.localhost:{port}/__b/host/{key}")).expect("no nul")
}

static REFRESH: AtomicBool = AtomicBool::new(false);
static VIEW_OUTSIDE: AtomicBool = AtomicBool::new(false);

/// Whether the pilot is outside, for the screens' paint rates.
pub fn note_view(xplm: &crate::xp::Xplm) {
    if !enabled() || !own_route() {
        return;
    }
    #[cfg(windows)]
    main_thread_profile::start();
    if let Some(d) = xplm.find("sim/graphics/view/view_is_external") {
        VIEW_OUTSIDE.store(xplm.get_i(d) != 0, Ordering::Relaxed);
    }
}

pub fn request_refresh() {
    REFRESH.store(true, Ordering::SeqCst);
}

/// Once a frame, on the main thread.
pub fn tick(session: &Session) {
    if !enabled() {
        return;
    }
    if let Ok(mut q) = logs().lock() {
        for line in q.drain(..) {
            crate::log(&format!("browser screens: {line}"));
        }
    }
    if own_route() {
        tick_own(session);
        return;
    }
    let Some(api) = api() else { return };
    publish_underlays();
    let Ok(mut s) = state().lock() else { return };
    let s = &mut *s;
    let Some(attached_at) = s.attached_at else { return };
    if !s.hidden_made {
        if let Some(avionics) = s.avionics {
            s.hidden_made = true;
            for (view, key) in s.hidden.clone() {
                let id = format!("A380X_BROWSER_HOST_{view}");
                let handle = create(&avionics, &id, &id, 64, 64, std::ptr::null_mut());
                if !handle.is_null() {
                    s.devices.push(Device { key, handle: handle as usize, url_set: false });
                }
            }
        }
    }
    let port = s.port;
    for (i, d) in s.devices.iter_mut().enumerate().filter(|(_, d)| !d.url_set) {
        let url = url_for(port, i, &d.key);
        unsafe { (api.set_url)(d.handle as AvionicsId, url.as_ptr()) };
        d.url_set = true;
    }
    let spawns = session.slots.header().ready.load(Ordering::Acquire);
    let spawned = spawns != s.spawns_seen;
    s.spawns_seen = spawns;
    if spawned || REFRESH.swap(false, Ordering::SeqCst) {
        for d in &s.devices {
            unsafe { (api.refresh)(d.handle as AvionicsId, 1) };
        }
        crate::log(&format!("browser screens: {} pages reloaded", s.devices.len()));
    }
    if attached_at.elapsed() > HIDDEN_DEVICE_WAIT {
        let shown = s.devices.iter().find(|d| !d.key.starts_with('@') && d.key != EFB_SCREEN).map(|d| d.handle);
        for (view, _) in &s.hidden {
            if !s.moved.contains(view) && !server::view_connected(*view) {
                s.moved.push(*view);
                let Some(shown) = shown else { continue };
                if let Some(url) = panel_views().iter().find(|v| v.index == *view).map(|v| format!("/Pages/VCockpit/Instruments/{}", v.gauge_url)) {
                    crate::log(&format!("browser screens: hidden device for view {view} never connected; moving its page onto a shown screen"));
                    if let Ok(script) = CString::new(format!("addView('xphfbw:{view}:','{url}')")) {
                        unsafe { (api.inject)(shown as AvionicsId, script.as_ptr()) };
                    }
                }
            }
        }
    }
    if s.stats_at.is_none_or(|t| t.elapsed() > STATS_EVERY) {
        s.stats_at = Some(Instant::now());
        let rows: Vec<String> = server::stats().into_iter().map(|(v, fps, bytes)| format!("{v}:{fps}fps/{}KB", bytes / 1024)).collect();
        crate::log(&format!("browser screens: {} pages connected: {}", rows.len(), rows.join(" ")));
    }
}

type Underlay = (u32, u32, u64, Arc<[u8]>);

fn underlays() -> &'static Mutex<[Option<Underlay>; 2]> {
    static U: OnceLock<Mutex<[Option<Underlay>; 2]>> = OnceLock::new();
    U.get_or_init(|| Mutex::new([None, None]))
}

fn publish_underlays() {
    use crate::mapdata::terrain::types::Side;
    let left = crate::wxr::layer(Side::Left).or_else(|| crate::mapdata::plugin::terrain_layer(Side::Left));
    let right = crate::wxr::layer(Side::Right).or_else(|| crate::mapdata::plugin::terrain_layer(Side::Right));
    if let Ok(mut u) = underlays().lock() {
        *u = [left, right];
    }
}

pub(crate) fn underlay(screen: &str) -> Option<Underlay> {
    let i = match screen.to_ascii_uppercase().as_str() {
        "SCREEN_DU_NDL" => 0,
        "SCREEN_DU_NDR" => 1,
        _ => return None,
    };
    underlays().lock().ok()?[i].clone()
}

fn dims() -> &'static Mutex<HashMap<String, Vec<f32>>> {
    static D: OnceLock<Mutex<HashMap<String, Vec<f32>>>> = OnceLock::new();
    D.get_or_init(Mutex::default)
}

/// A screen's dimming regions this frame, for its host page's overlays.
pub fn publish_dimming(screen: &str, values: &[f32]) {
    if let Ok(mut d) = dims().lock() {
        match d.get_mut(screen) {
            Some(v) => {
                v.clear();
                v.extend_from_slice(values);
            }
            None => {
                d.insert(screen.to_string(), values.to_vec());
            }
        }
    }
}

pub(crate) fn dimming(screen: &str) -> Vec<f32> {
    dims().lock().ok().and_then(|d| d.get(screen).cloned()).unwrap_or_default()
}

/// The view typing goes to: a text field a page reported focused.
static TEXT_FOCUS: AtomicI64 = AtomicI64::new(-1);

pub(crate) fn set_text_focus(view: u32, focused: bool) {
    if focused {
        TEXT_FOCUS.store(view as i64, Ordering::Relaxed);
    } else {
        let _ = TEXT_FOCUS.compare_exchange(view as i64, -1, Ordering::Relaxed, Ordering::Relaxed);
    }
}

fn keys() -> &'static Mutex<Vec<(u32, u32, u32)>> {
    static K: OnceLock<Mutex<Vec<(u32, u32, u32)>>> = OnceLock::new();
    K.get_or_init(Mutex::default)
}

pub(crate) fn take_keys(view: u32) -> Vec<(u32, u32)> {
    let Ok(mut k) = keys().lock() else { return Vec::new() };
    if k.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    k.retain(|(v, ch, vk)| {
        if *v == view {
            out.push((*ch, *vk));
            false
        } else {
            true
        }
    });
    out
}

fn mfd_view() -> Option<u32> {
    static V: OnceLock<Option<u32>> = OnceLock::new();
    *V.get_or_init(|| panel_views().iter().find(|v| v.screen.eq_ignore_ascii_case("SCREEN_DU_MFD")).map(|v| v.index))
}

/// Whether a page on `screen` reported a focused text field ([`set_text_focus`]).
pub(crate) fn page_text_focus(screen: &str) -> bool {
    static SCREENS: OnceLock<Vec<(u32, String)>> = OnceLock::new();
    let Ok(view) = u32::try_from(TEXT_FOCUS.load(Ordering::Relaxed)) else { return false };
    if view == server::EFB_VIEW {
        return screen == EFB_SCREEN;
    }
    SCREENS.get_or_init(|| panel_views().into_iter().map(|v| (v.index, v.screen)).collect()).iter().any(|(i, s)| *i == view && s.eq_ignore_ascii_case(screen))
}

fn typing_target() -> Option<u32> {
    if crate::display::efb_text_focus() {
        return Some(server::EFB_VIEW);
    }
    if crate::display::mfd_text_focus() {
        return mfd_view();
    }
    u32::try_from(TEXT_FOCUS.load(Ordering::Relaxed)).ok()
}

/// The display module's key sniffer, for a screen that is a page: `Some` when
/// the keystroke was for a page. Text fields report their own focus here, since
/// the plugin never sees the click.
pub fn sniff_key(ch: u32, vkey: u32, flags: c_int) -> Option<bool> {
    if !enabled() || own_route() {
        return None;
    }
    let view = typing_target()?;
    if vkey == xp::VK_ESCAPE {
        TEXT_FOCUS.store(-1, Ordering::Relaxed);
        return Some(false);
    }
    if !crate::display::typed_into_cockpit(ch, vkey, flags) {
        return Some(false);
    }
    if flags & xp::KEY_DOWN_FLAG != 0 {
        if let Ok(mut k) = keys().lock() {
            k.push((view, ch, vkey));
        }
    }
    Some(true)
}

/// Before the devices go: the server's threads end here, because the DLL
/// unloads right after.
pub fn stop(avionics: Option<&AvionicsApi>) {
    if !enabled() {
        return;
    }
    #[cfg(windows)]
    main_thread_profile::stop();
    #[cfg(windows)]
    own_cef::forget_chromium_start();
    let browsers = state().lock().ok().and_then(|mut s| s.browsers.take());
    if let Some(b) = browsers {
        b.close();
    }
    server::stop();
    if let Ok(mut s) = state().lock() {
        if let Some(avionics) = avionics {
            if s.boot != 0 {
                unsafe { (avionics.destroy)(s.boot as AvionicsId) };
            }
            for d in s.devices.iter().filter(|d| d.key.starts_with('@')) {
                unsafe { (avionics.destroy)(d.handle as AvionicsId) };
            }
        }
        *s = State::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(index: u32, screen: &str, url: &str, layer: u32) -> ViewDef {
        ViewDef { index, section: String::new(), gauge_url: url.into(), width: 768, height: 1024, screen: screen.into(), layer }
    }

    #[test]
    fn a_screens_views_stack_on_one_page_in_layer_order() {
        let defs = [def(3, "SCREEN_DU_SD", "A380X/SD/sd.html?duID=7", 0), def(4, "SCREEN_DU_SD", "A380X/SDv2/sdv2.html?duID=7", 1), def(9, "", "A380X/SystemsHost/systems-host.html", 0)];
        let screens: Vec<&'static ScreenDef> = crate::display::screens::SCREENS.iter().collect();
        let l = layout(&defs, &screens);
        let sd = l.page("SCREEN_DU_SD").unwrap();
        let (first, second) = (sd.find("xphfbw:3:SCREEN_DU_SD").unwrap(), sd.find("xphfbw:4:SCREEN_DU_SD").unwrap());
        assert!(first < second, "layer 0 is drawn first, under layer 1");
        assert!(sd.contains("/Pages/VCockpit/Instruments/A380X/SDv2/sdv2.html?duID=7"));
        assert_eq!(sd.contains(r#"class="dim""#), !own_route(), "the SD dims on the page only on browser devices");
        assert!(l.page("@9").unwrap().contains("xphfbw:9:"), "a screenless view gets a page of its own");
        assert!(l.page("SCREEN_EFB").unwrap().contains("xphfbw:efb"));
        assert!(l.page("SCREEN_DU_PFDL").is_none());
    }

    /// A running stand-in for the plugin, for loading the real pages in a real
    /// browser offline: FBW_BENCH_AIRCRAFT, FBW_BENCH_XPROOT, FBW_BENCH_UI,
    /// FBW_BENCH_PORTFILE, FBW_BENCH_SECONDS.
    #[test]
    #[ignore]
    fn serve_the_real_pages_for_a_browser() {
        use systems::simulation::{SimulatorReaderWriter, VariableRegistry};
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k}"));
        let aircraft = std::path::PathBuf::from(env("FBW_BENCH_AIRCRAFT"));
        let panel = std::fs::read_to_string(aircraft.join("panel").join("panel.cfg")).unwrap();
        let xplm: &'static crate::xp::Xplm = Box::leak(Box::new(crate::xp::Xplm::dummy()));
        let tag = format!("bench_{}", std::process::id());
        let mut host = crate::xphfbw_host::XphfbwHost::start(&tag, xplm, &panel, None).unwrap();
        let defs = views::view_list(&panel);
        let screens: Vec<&'static ScreenDef> = crate::display::screens::SCREENS.iter().collect();
        let port = server::start().unwrap();
        let paths = Paths { xplane_root: env("FBW_BENCH_XPROOT").into(), aircraft_dir: aircraft, ui_dir: env("FBW_BENCH_UI").into() };
        assert!(server::attach(&tag, paths, crate::static_source_patches(), layout(&defs, &screens)));
        let hosts: Vec<String> = defs.iter().filter(|d| d.layer == 0).map(|d| if d.screen.is_empty() { format!("@{}", d.index) } else { d.screen.to_ascii_uppercase() }).collect();
        std::fs::write(env("FBW_BENCH_PORTFILE"), format!("{port}\n{}\n", hosts.join("\n"))).unwrap();
        let mut vars = crate::Vars::new(xplm);
        for (name, value) in [("A32NX_ELEC_DC_ESS_BUS_IS_POWERED", 1.), ("A32NX_ELEC_DC_1_BUS_IS_POWERED", 1.), ("A32NX_ELEC_DC_2_BUS_IS_POWERED", 1.), ("A32NX_ELEC_AC_ESS_BUS_IS_POWERED", 1.), ("A32NX_ELEC_AC_2_BUS_IS_POWERED", 1.), ("A32NX_ELEC_108PH_BUS_IS_POWERED", 1.), ("A32NX_ELEC_103PP_BUS_IS_POWERED", 1.), ("A32NX_ELEC_202PP_BUS_IS_POWERED", 1.),("A32NX_IS_READY", 1.), ("A380X_BENCH_ROUNDTRIP", 42.)] {
            let id = vars.add(name.to_string(), crate::NAMED);
            vars.write(&id, value);
        }
        for p in 78..100 {
            let id = vars.get(format!("LIGHT POTENTIOMETER:{p}"));
            vars.write(&id, 1.);
        }
        let echo = vars.add("A380X_BENCH_ECHO".to_string(), crate::NAMED);
        if let Ok(path) = std::env::var("FBW_BENCH_VARS_JSON") {
            let reply: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            for (name, value) in reply["vars"].as_object().unwrap() {
                let id = if name.contains(' ') { vars.get(name.clone()) } else { vars.add(name.clone(), crate::NAMED) };
                vars.write(&id, value.as_f64().unwrap_or(0.));
            }
        }
        for pair in std::env::var("FBW_BENCH_SET").unwrap_or_default().split(';').filter(|s| !s.is_empty()) {
            let (name, value) = pair.split_once('=').expect("NAME=value");
            let id = match name.strip_prefix("L:") {
                Some(local) => vars.add(local.to_string(), crate::NAMED),
                None => vars.get(name.to_string()),
            };
            vars.write(&id, value.parse().expect("a number"));
        }
        let noisy: Vec<_> = std::env::var("FBW_BENCH_NOISE")
            .unwrap_or_default()
            .split(';')
            .filter_map(|pair| pair.split_once('='))
            .map(|(name, amplitude)| {
                let id = vars.add(name.to_string(), crate::NAMED);
                let value = vars.read(&id);
                (id, value, amplitude.parse::<f32>().expect("a number"))
            })
            .collect();
        let hevents_file = std::env::var("FBW_BENCH_HEVENTS").ok();
        let mut hevents_done = 0;
        let seconds: f64 = env("FBW_BENCH_SECONDS").parse().unwrap();
        let started = Instant::now();
        let mut printed = 0;
        while started.elapsed().as_secs_f64() < seconds {
            let time = started.elapsed().as_secs_f64();
            let lines: Vec<String> = hevents_file.as_ref().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default().lines().map(str::to_string).collect();
            let mut hevents: Vec<String> = lines.iter().skip(hevents_done).cloned().collect();
            hevents_done = lines.len().max(hevents_done);
            hevents.retain(|h| match h.strip_prefix("set ").and_then(|s| s.split_once('=')) {
                Some((name, value)) => {
                    let id = vars.add(name.to_string(), crate::NAMED);
                    vars.write(&id, value.parse().expect("a number"));
                    false
                }
                None => true,
            });
            for (i, (id, value, amplitude)) in noisy.iter().enumerate() {
                let bits = *value as u64;
                let moved = f32::from_bits(bits as u32) + amplitude * (time as f32 * 37. + i as f32).sin();
                vars.write(id, (((bits >> 32) << 32) | u64::from(moved.to_bits())) as f64);
            }
            host.pre_tick(&mut vars);
            host.post_tick(&mut vars, time, &hevents, &[]);
            if time as u64 / 5 != printed {
                printed = time as u64 / 5;
                let rows: Vec<String> = server::stats().into_iter().map(|(v, fps, bytes)| format!("{v}:{fps}/{}K", bytes / 1024)).collect();
                println!("t={time:.0} loaded={}/{} active={} echo={} pages: {}", host.views_loaded(), host.view_count(), host.displays_active(), vars.read(&echo), rows.join(" "));
            }
            std::thread::sleep(Duration::from_millis(33));
        }
        server::stop();
    }

    /// The real server exe in lockstep, built and rebuilt as a flight does it:
    /// FBW_BENCH_SERVER is its path.
    #[test]
    #[ignore]
    fn the_server_exe_ticks_in_lockstep() {
        use crate::aspects::test_vars::TestVars;
        use systems::simulation::StartState;
        let exe = std::path::PathBuf::from(std::env::var("FBW_BENCH_SERVER").unwrap());
        let mut vars = TestVars::default();
        let mut remote = crate::remote::client::RemoteSystems::start(&exe, StartState::Apron, &mut vars).unwrap();
        println!("variables {}", remote.variable_count());
        let step = |remote: &mut crate::remote::client::RemoteSystems, vars: &mut TestVars, from: usize, n: usize| {
            for i in from..from + n {
                vars.set("AMBIENT PRESSURE", 29.92);
                vars.set("AMBIENT TEMPERATURE", 15.);
                vars.set("AMBIENT DENSITY", 0.002377);
                for n in 1..=4 {
                    vars.set(&format!("A32NX_EXT_PWR_AVAIL:{n}"), 1.);
                    vars.set(&format!("A32NX_OVHD_ELEC_EXT_PWR_{n}_PB_IS_ON"), 1.);
                }
                if let Some(m) = remote.tick(Duration::from_millis(50), i as f64 * 0.05, vars) {
                    println!("tick {i}: {m}");
                }
            }
        };
        step(&mut remote, &mut vars, 0, 200);
        println!("late before rebuild {}", remote.stats.late);
        remote.rebuild(StartState::Runway);
        step(&mut remote, &mut vars, 200, 200);
        println!("ticks {} late {} mean round trip {:.2} ms", remote.stats.ticks, remote.stats.late, remote.stats.round_trip_us as f64 / remote.stats.ticks.max(1) as f64 / 1000.);
        println!("process {:?} systems mean {:.2} ms AC1 powered {}", remote.process_id(), remote.stats.systems_us as f64 / remote.stats.ticks.max(1) as f64 / 1000., vars.value("A32NX_ELEC_AC_1_BUS_IS_POWERED"));
    }

    #[test]
    #[ignore = "offline, with X-Plane's Chromium: FBW_BENCH_CEF, FBW_BENCH_CACHE, FBW_BENCH_URL"]
    #[cfg(windows)]
    fn paint_cost_probe() {
        use own_cef::probe as p;
        use own_cef::test_support as t;
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k}"));
        t::start_chromium(&env("FBW_BENCH_CEF"), &env("FBW_BENCH_CACHE"));
        let tag = format!("probe_{}", std::process::id());
        let keys = ["SCREEN_DU_PFDL", "SCREEN_DU_PFDR", "SCREEN_DU_NDL", "SCREEN_DU_NDR", "SCREEN_DU_EWD", "SCREEN_DU_SD"];
        let wanted: Vec<own_cef::Wanted> = keys.iter().map(|k| own_cef::Wanted { key: k.to_string(), screen: Some(k.to_string()), width: 768, height: 1024 }).collect();
        let mut blocks = Vec::new();
        for k in keys {
            let (dw, dh) = views::device_size(k, 768, 1024);
            blocks.push(crate::xphfbw_bridge::ScreenBlock::create(&tag, k, dw, dh).unwrap());
        }
        let session = crate::xphfbw_bridge::Session::create(&tag).unwrap();
        let number = |k: &str, default: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
        let (rate, frame_ms, pumps) = (number("FBW_BENCH_RATE", 30), number("FBW_BENCH_FRAME_MS", 28), number("FBW_BENCH_PUMPS", 0));
        let mut browsers = own_cef::open_all(&tag, 1, &wanted, rate as i32).expect("browsers");
        let started = Instant::now();
        let mut marked = false;
        while started.elapsed().as_secs_f64() < 30. {
            browsers.tick(&session);
            let until = Instant::now() + Duration::from_millis(frame_ms);
            if pumps > 0 {
                for _ in 0..pumps {
                    t::pump();
                }
                std::thread::sleep(until.saturating_duration_since(Instant::now()));
            }
            while pumps == 0 && Instant::now() < until {
                t::pump();
                std::thread::sleep(Duration::from_millis(1));
            }
            if !marked && started.elapsed().as_secs_f64() > 12. {
                marked = true;
                p::reset();
            }
        }
        let get = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed) as f64;
        let (b, f, n, c) = (get(&p::BYTES), get(&p::FAULTS), get(&p::NS), get(&p::PAINTS));
        println!("PROBE paints/s {:.1}; MB per paint {:.2}; ms per paint {:.3}; GB/s {:.2}; faults per MB {:.0}", c / 18., b / c.max(1.) / 1e6, n / c.max(1.) / 1e6, b / n.max(1.), f / (b / 1e6).max(1.));
        let (w, h, px) = t::tga_like_dump(&blocks[0]);
        std::fs::write(format!("E:/fbw-build/tmp/probe-{w}x{h}.bgra"), &px).unwrap();
        browsers.close();
    }

    /// Our browsers in a copy of X-Plane's Chromium, no windows: FBW_BENCH_CEF is its
    /// folder, FBW_BENCH_AIRCRAFT, FBW_BENCH_XPROOT, FBW_BENCH_UI as above,
    /// FBW_BENCH_SHOTS gets raw BGRA dumps.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn own_browsers_in_x_planes_chromium() {
        use own_cef::test_support as t;
        use systems::simulation::{SimulatorReaderWriter, VariableRegistry};
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k}"));
        let aircraft = std::path::PathBuf::from(env("FBW_BENCH_AIRCRAFT"));
        let panel = std::fs::read_to_string(aircraft.join("panel").join("panel.cfg")).unwrap();
        t::start_chromium(&env("FBW_BENCH_CEF"), r"E:\fbw-build\tmp\oc-cache");
        for line in logs().lock().unwrap().drain(..) {
            println!("log: {line}");
        }
        println!("chromium up, hash check {:?}", own_cef::available(std::path::Path::new("E:/none")));
        let xplm: &'static crate::xp::Xplm = Box::leak(Box::new(crate::xp::Xplm::dummy()));
        let tag = format!("own_{}", std::process::id());
        let mut host = crate::xphfbw_host::XphfbwHost::start(&tag, xplm, &panel, None).unwrap();
        let defs = views::view_list(&panel);
        let screens: Vec<&'static ScreenDef> = crate::display::screens::SCREENS.iter().collect();
        let mut wanted = wanted_browsers(&defs);
        if let Ok(only) = std::env::var("FBW_BENCH_ONLY") {
            wanted.retain(|w| only.split(',').any(|o| o == w.key));
        }
        let mut blocks = Vec::new();
        for w in &wanted {
            if let Some(s) = &w.screen {
                let (dw, dh) = views::device_size(s, w.width, w.height);
                blocks.push((s.clone(), crate::xphfbw_bridge::ScreenBlock::create(&tag, s, dw, dh).unwrap()));
            }
        }
        for (s, b) in &blocks {
            if ["SCREEN_DU_PFDL", "SCREEN_DU_EWD", "SCREEN_DU_MFD"].contains(&s.as_str()) {
                b.header().gpu_refused.store(1, Ordering::Relaxed);
            }
        }
        let port = server::start().unwrap();
        let paths = Paths { xplane_root: env("FBW_BENCH_XPROOT").into(), aircraft_dir: aircraft, ui_dir: env("FBW_BENCH_UI").into() };
        assert!(server::attach(&tag, paths, crate::static_source_patches(), layout(&defs, &screens)));
        let mut vars = crate::Vars::new(xplm);
        for (name, value) in [("A32NX_ELEC_DC_ESS_BUS_IS_POWERED", 1.), ("A32NX_ELEC_DC_1_BUS_IS_POWERED", 1.), ("A32NX_ELEC_DC_2_BUS_IS_POWERED", 1.), ("A32NX_ELEC_AC_ESS_BUS_IS_POWERED", 1.), ("A32NX_ELEC_AC_2_BUS_IS_POWERED", 1.), ("A32NX_ELEC_108PH_BUS_IS_POWERED", 1.), ("A32NX_IS_READY", 1.)] {
            let id = vars.add(name.to_string(), crate::NAMED);
            vars.write(&id, value);
        }
        for p in 78..100 {
            let id = vars.get(format!("LIGHT POTENTIOMETER:{p}"));
            vars.write(&id, 1.);
        }
        let mut browsers = own_cef::open_all(&tag, port, &wanted, 60).expect("browsers");
        let seconds: f64 = env("FBW_BENCH_SECONDS").parse().unwrap();
        let started = Instant::now();
        let session = crate::xphfbw_bridge::Session::open(&tag).unwrap();
        let (mut printed, mut clicked) = (0, false);
        while started.elapsed().as_secs_f64() < seconds {
            let time = started.elapsed().as_secs_f64();
            host.pre_tick(&mut vars);
            host.post_tick(&mut vars, time, &[], &[]);
            browsers.tick(&session);
            let until = Instant::now() + Duration::from_millis(28);
            while Instant::now() < until {
                t::pump();
                std::thread::sleep(Duration::from_millis(1));
            }
            if !clicked && time > seconds - 12. {
                clicked = true;
                t::reset(&browsers);
                println!("measuring the last 12 s");
            }
            if time as u64 / 10 != printed {
                printed = time as u64 / 10;
                println!("t={time:.0} open={} loaded={}/{} active={}", t::open_count(), host.views_loaded(), host.view_count(), host.displays_active());
            }
        }
        for (key, paints) in t::stats(&browsers) {
            println!("{key}: {:.1} paints/s", paints as f64 / 12.);
        }
        let dir = env("FBW_BENCH_SHOTS");
        for (s, b) in &blocks {
            let frame = b.header().frame.load(Ordering::Relaxed);
            let gen = b.header().gpu_generation.load(Ordering::Relaxed);
            println!("{s}: block frame {frame}, gpu generation {gen}");
            if b.header().gpu_refused.load(Ordering::Relaxed) != 0 {
                let (w, h, px) = t::tga_like_dump(b);
                std::fs::write(format!("{dir}/{s}_{w}x{h}.bgra"), px).unwrap();
            }
        }
        browsers.close();
        println!("closed, open={}", t::open_count());
        server::stop();
    }

    /// Raw frames from a running session: FBW_DUMP_TAG, FBW_DUMP_DIR.
    #[test]
    #[ignore]
    fn dump_screens_of_a_running_session() {
        let tag = std::env::var("FBW_DUMP_TAG").unwrap();
        let dir = std::env::var("FBW_DUMP_DIR").unwrap();
        for def in crate::display::screens::SCREENS {
            let (w, h) = views::device_size(def.id, def.width, def.height);
            let Some(block) = crate::xphfbw_bridge::ScreenBlock::open(&tag, def.id, w, h) else { continue };
            let px = &block.pixels()[..w as usize * h as usize * 4];
            std::fs::write(format!("{dir}/{}_{w}x{h}.bgra", def.id), px).unwrap();
            println!("{} {w}x{h} frame {}", def.id, block.header().frame.load(Ordering::Relaxed));
        }
    }

    #[test]
    fn only_the_focused_views_keys_are_taken() {
        keys().lock().unwrap().clear();
        keys().lock().unwrap().extend([(1, 65, 65), (2, 66, 66), (1, 67, 67)]);
        assert_eq!(take_keys(1), vec![(65, 65), (67, 67)]);
        assert_eq!(take_keys(2), vec![(66, 66)]);
        assert!(take_keys(1).is_empty());
    }
}
