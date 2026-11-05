//! Our own browsers inside the Chromium X-Plane has already started: the same
//! `libcef.dll` (Windows) or `Chromium Embedded Framework.framework` (macOS), so no
//! separate app, with our frame rate and resolution. Hand-written C API of CEF 117
//! (117.1.4 on Windows, 117.2.5 on macOS), checked against the library's API hash
//! before anything is called. Everything here runs on X-Plane's main thread, which
//! is the thread that pumps CEF (on macOS the only one it can be: CEF has no
//! multi-threaded message loop there, so its UI thread is the AppKit main thread).

use std::ffi::{c_char, c_int, c_void, CStr};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
#[cfg(windows)]
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use super::paint_thread::{self, PaintSlot, PaintThread};
use crate::xphfbw_bridge::{Input, InputKind, ScreenBlock, Session, MAX_DIRTY_RECTS};
use crate::xphfbw_bridge_views::{self as views, SCREEN_ORDER};

#[cfg(windows)]
const API_HASH: &str = "36384f3e3dc2ac64bdad2db177725caebaebf02e";
/// CEF 117.2.5+gda4c36a, `cef_api_hash(0)` of X-Plane 12.4.3's framework.
#[cfg(target_os = "macos")]
const API_HASH: &str = "35b5f3657fe9f203a7852235b85b5f2a7501d59e";

#[cfg(windows)]
extern "system" {
    fn GetModuleHandleA(name: *const c_char) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn VirtualProtect(address: *mut c_void, size: usize, protection: u32, old: *mut u32) -> i32;
    fn VirtualAlloc(address: *mut c_void, size: usize, kind: u32, protection: u32) -> *mut c_void;
}

#[repr(C)]
struct Base {
    size: usize,
    add_ref: Option<unsafe extern "system" fn(*mut c_void)>,
    release: Option<unsafe extern "system" fn(*mut c_void) -> c_int>,
    has_one_ref: Option<unsafe extern "system" fn(*mut c_void) -> c_int>,
    has_at_least_one_ref: Option<unsafe extern "system" fn(*mut c_void) -> c_int>,
}

type Slot = usize;

#[repr(C)]
struct CefString {
    str_: *const u16,
    length: usize,
    dtor: Slot,
}

const NO_STRING: CefString = CefString { str_: std::ptr::null(), length: 0, dtor: 0 };

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Rect {
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
}

#[repr(C)]
struct ScreenInfo {
    device_scale_factor: f32,
    depth: c_int,
    depth_per_component: c_int,
    is_monochrome: c_int,
    rect: Rect,
    available_rect: Rect,
}

#[repr(C)]
struct MouseEvent {
    x: c_int,
    y: c_int,
    modifiers: u32,
}

#[repr(C)]
struct KeyEvent {
    type_: c_int,
    modifiers: u32,
    windows_key_code: c_int,
    native_key_code: c_int,
    is_system_key: c_int,
    character: u16,
    unmodified_character: u16,
    focus_on_editable_field: c_int,
}

#[cfg(windows)]
#[repr(C)]
struct WindowInfo {
    ex_style: u32,
    window_name: CefString,
    style: u32,
    bounds: Rect,
    parent_window: *mut c_void,
    menu: *mut c_void,
    windowless_rendering_enabled: c_int,
    shared_texture_enabled: c_int,
    external_begin_frame_enabled: c_int,
    window: *mut c_void,
}

/// `cef_types_mac.h`.
#[cfg(target_os = "macos")]
#[repr(C)]
struct WindowInfo {
    window_name: CefString,
    bounds: Rect,
    hidden: c_int,
    parent_view: *mut c_void,
    windowless_rendering_enabled: c_int,
    shared_texture_enabled: c_int,
    external_begin_frame_enabled: c_int,
    view: *mut c_void,
}

impl WindowInfo {
    fn windowless() -> Self {
        #[cfg(windows)]
        return WindowInfo {
            ex_style: 0,
            window_name: NO_STRING,
            style: 0,
            bounds: Rect::default(),
            parent_window: std::ptr::null_mut(),
            menu: std::ptr::null_mut(),
            windowless_rendering_enabled: 1,
            shared_texture_enabled: 0,
            external_begin_frame_enabled: 0,
            window: std::ptr::null_mut(),
        };
        #[cfg(target_os = "macos")]
        return WindowInfo {
            window_name: NO_STRING,
            bounds: Rect::default(),
            hidden: 0,
            parent_view: std::ptr::null_mut(),
            windowless_rendering_enabled: 1,
            shared_texture_enabled: 0,
            external_begin_frame_enabled: 0,
            view: std::ptr::null_mut(),
        };
    }
}

/// Offsets from CEF 117.2.5's own headers (`offsetof` on macOS, both slices LP64),
/// so a wrong hand-written field fails the build rather than X-Plane.
#[cfg(target_os = "macos")]
const _: () = {
    use std::mem::{offset_of as at, size_of as size};
    assert!(size::<Base>() == 40 && size::<CefString>() == 24 && size::<Rect>() == 16);
    assert!(size::<ScreenInfo>() == 48 && at!(ScreenInfo, available_rect) == 32);
    assert!(size::<MouseEvent>() == 12 && size::<KeyEvent>() == 28 && at!(KeyEvent, character) == 20 && at!(KeyEvent, focus_on_editable_field) == 24);
    assert!(size::<WindowInfo>() == 80 && at!(WindowInfo, bounds) == 24 && at!(WindowInfo, parent_view) == 48 && at!(WindowInfo, windowless_rendering_enabled) == 56 && at!(WindowInfo, view) == 72);
    assert!(size::<BrowserSettings>() == 288 && at!(BrowserSettings, windowless_frame_rate) == 8 && at!(BrowserSettings, default_font_size) == 160);
    assert!(at!(BrowserSettings, default_encoding) == 176 && at!(BrowserSettings, background_color) == 248 && at!(BrowserSettings, accept_language_list) == 256 && at!(BrowserSettings, chrome_status_bubble) == 280);
    assert!(at!(Browser, get_host) == 48 && at!(Browser, reload) == 96 && at!(Browser, reload_ignore_cache) == 104);
    assert!(at!(Host, close_browser) == 48 && at!(Host, set_focus) == 64 && at!(Host, was_hidden) == 272 && at!(Host, send_key_event) == 304);
    assert!(at!(Host, send_mouse_click_event) == 312 && at!(Host, send_mouse_move_event) == 320 && at!(Host, send_mouse_wheel_event) == 328 && at!(Host, set_windowless_frame_rate) == 368);
    assert!(size::<ClientVt>() == 192 && at!(ClientVt, get_life_span_handler) == 144 && at!(ClientVt, get_load_handler) == 152 && at!(ClientVt, get_render_handler) == 168);
    assert!(size::<RenderVt>() == 176 && at!(RenderVt, get_view_rect) == 56 && at!(RenderVt, get_screen_info) == 72 && at!(RenderVt, on_paint) == 96);
    assert!(size::<LifeVt>() == 72 && at!(LifeVt, on_after_created) == 48 && at!(LifeVt, on_before_close) == 64 && size::<LoadVt>() == 72);
};

#[repr(C)]
struct BrowserSettings {
    size: usize,
    windowless_frame_rate: c_int,
    fonts: [CefString; 6],
    default_font_size: c_int,
    default_fixed_font_size: c_int,
    minimum_font_size: c_int,
    minimum_logical_font_size: c_int,
    default_encoding: CefString,
    states: [c_int; 12],
    background_color: u32,
    accept_language_list: CefString,
    chrome_status_bubble: c_int,
}

#[repr(C)]
struct Browser {
    base: Base,
    is_valid: Slot,
    get_host: unsafe extern "system" fn(*mut Browser) -> *mut Host,
    before_reload: [Slot; 5],
    reload: unsafe extern "system" fn(*mut Browser),
    reload_ignore_cache: unsafe extern "system" fn(*mut Browser),
}

#[repr(C)]
struct Host {
    base: Base,
    get_browser: Slot,
    close_browser: unsafe extern "system" fn(*mut Host, c_int),
    try_close: Slot,
    set_focus: unsafe extern "system" fn(*mut Host, c_int),
    before_was_hidden: [Slot; 25],
    was_hidden: unsafe extern "system" fn(*mut Host, c_int),
    notify_screen: Slot,
    invalidate: Slot,
    external_begin_frame: Slot,
    send_key_event: unsafe extern "system" fn(*mut Host, *const KeyEvent),
    send_mouse_click_event: unsafe extern "system" fn(*mut Host, *const MouseEvent, c_int, c_int, c_int),
    send_mouse_move_event: unsafe extern "system" fn(*mut Host, *const MouseEvent, c_int),
    send_mouse_wheel_event: unsafe extern "system" fn(*mut Host, *const MouseEvent, c_int, c_int),
    send_touch: Slot,
    capture_lost: Slot,
    notify_move: Slot,
    get_frame_rate: Slot,
    set_windowless_frame_rate: unsafe extern "system" fn(*mut Host, c_int),
}

#[repr(C)]
struct ClientVt {
    base: Base,
    before_life_span: [Slot; 13],
    get_life_span_handler: unsafe extern "system" fn(*mut ClientVt) -> *mut LifeVt,
    get_load_handler: unsafe extern "system" fn(*mut ClientVt) -> *mut LoadVt,
    get_print_handler: Slot,
    get_render_handler: unsafe extern "system" fn(*mut ClientVt) -> *mut RenderVt,
    get_request_handler: Slot,
    on_process_message_received: Slot,
}

#[repr(C)]
struct RenderVt {
    base: Base,
    get_accessibility_handler: Slot,
    get_root_screen_rect: Slot,
    get_view_rect: unsafe extern "system" fn(*mut RenderVt, *mut Browser, *mut Rect),
    get_screen_point: Slot,
    get_screen_info: unsafe extern "system" fn(*mut RenderVt, *mut Browser, *mut ScreenInfo) -> c_int,
    on_popup_show: Slot,
    on_popup_size: Slot,
    on_paint: unsafe extern "system" fn(*mut RenderVt, *mut Browser, c_int, usize, *const Rect, *const u8, c_int, c_int),
    on_accelerated_paint: Slot,
    rest: [Slot; 8],
}

#[repr(C)]
struct LifeVt {
    base: Base,
    on_before_popup: Slot,
    on_after_created: unsafe extern "system" fn(*mut LifeVt, *mut Browser),
    do_close: Slot,
    on_before_close: unsafe extern "system" fn(*mut LifeVt, *mut Browser),
}

#[repr(C)]
struct LoadVt {
    base: Base,
    on_loading_state_change: Slot,
    on_load_start: Slot,
    on_load_end: Slot,
    on_load_error: Slot,
}

type CreateBrowser = unsafe extern "C" fn(*const WindowInfo, *mut ClientVt, *const CefString, *const BrowserSettings, *mut c_void, *mut c_void) -> c_int;
type ApiHash = unsafe extern "C" fn(c_int) -> *const c_char;
type DoWork = unsafe extern "C" fn();

/// One browser and everything its callbacks need; the handler structs point back at it.
#[repr(C)]
struct View {
    client: ClientVt,
    client_view: *mut View,
    render: RenderVt,
    render_view: *mut View,
    life: LifeVt,
    life_view: *mut View,
    load: LoadVt,
    load_view: *mut View,
    key: String,
    screen_slot: Option<usize>,
    width: c_int,
    height: c_int,
    scale: f32,
    slot: Option<Arc<PaintSlot>>,
    browser: *mut Browser,
    closed: bool,
    paints: u64,
}

// The handler structs are reference counted by CEF, and all of them live as long
// as the view: nothing is freed until every browser has closed.
unsafe extern "system" fn add_ref(_: *mut c_void) {
    REFS.fetch_add(1, Ordering::Relaxed);
}

unsafe extern "system" fn release(_: *mut c_void) -> c_int {
    REFS.fetch_sub(1, Ordering::Relaxed);
    0
}

unsafe extern "system" fn has_one_ref(_: *mut c_void) -> c_int {
    0
}

unsafe extern "system" fn has_at_least_one_ref(_: *mut c_void) -> c_int {
    1
}

static REFS: AtomicU64 = AtomicU64::new(0);
static OPEN_BROWSERS: AtomicI32 = AtomicI32::new(0);

fn base<T>() -> Base {
    Base { size: std::mem::size_of::<T>(), add_ref: Some(add_ref), release: Some(release), has_one_ref: Some(has_one_ref), has_at_least_one_ref: Some(has_at_least_one_ref) }
}

/// The view a handler belongs to: the pointer stored right after its table.
unsafe fn view_of<T>(handler: *mut T) -> *mut View {
    *(handler.add(1) as *mut *mut View)
}

unsafe fn release_browser(browser: *mut Browser) {
    if !browser.is_null() {
        if let Some(f) = (*browser).base.release {
            f(browser as *mut c_void);
        }
    }
}

unsafe fn host_of(browser: *mut Browser) -> Option<*mut Host> {
    if browser.is_null() {
        return None;
    }
    let host = ((*browser).get_host)(browser);
    (!host.is_null()).then_some(host)
}

unsafe fn release_host(host: *mut Host) {
    if let Some(f) = (*host).base.release {
        f(host as *mut c_void);
    }
}

unsafe extern "system" fn get_life(client: *mut ClientVt) -> *mut LifeVt {
    add_ref(std::ptr::null_mut());
    &mut (*view_of(client)).life
}

unsafe extern "system" fn get_load(client: *mut ClientVt) -> *mut LoadVt {
    add_ref(std::ptr::null_mut());
    &mut (*view_of(client)).load
}

unsafe extern "system" fn get_render(client: *mut ClientVt) -> *mut RenderVt {
    add_ref(std::ptr::null_mut());
    &mut (*view_of(client)).render
}

unsafe extern "system" fn view_rect(handler: *mut RenderVt, browser: *mut Browser, rect: *mut Rect) {
    let v = &*view_of(handler);
    *rect = Rect { x: 0, y: 0, width: v.width, height: v.height };
    release_browser(browser);
}

unsafe extern "system" fn screen_info(handler: *mut RenderVt, browser: *mut Browser, info: *mut ScreenInfo) -> c_int {
    (*info).device_scale_factor = (*view_of(handler)).scale;
    release_browser(browser);
    1
}

fn clamped_rects(list: &[Rect], width: c_int, height: c_int) -> Vec<[u32; 4]> {
    list.iter()
        .filter_map(|r| {
            let (x0, y0) = (r.x.clamp(0, width), r.y.clamp(0, height));
            let (x1, y1) = (r.x.saturating_add(r.width).clamp(0, width), r.y.saturating_add(r.height).clamp(0, height));
            (x1 > x0 && y1 > y0).then_some([x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32])
        })
        .collect()
}

fn paints_on_their_thread() -> bool {
    static YES: OnceLock<bool> = OnceLock::new();
    *YES.get_or_init(|| {
        let yes = SWITCHES_APPLIED.load(Ordering::SeqCst) && chromium_switches().contains("disable-gpu-compositing") && std::env::var("FBW_PAINT_THREAD").as_deref() != Ok("0");
        super::log(&format!("paints copied {}", if yes { "on their own thread" } else { "on the main thread" }));
        yes
    })
}

unsafe extern "system" fn on_paint(handler: *mut RenderVt, browser: *mut Browser, kind: c_int, count: usize, rects: *const Rect, buffer: *const u8, width: c_int, height: c_int) {
    release_browser(browser);
    let v = &mut *view_of(handler);
    if kind != 0 {
        return;
    }
    v.paints += 1;
    let Some(slot) = v.slot.as_ref() else { return };
    if !slot.fits(width, height) {
        return;
    }
    let whole = [Rect { x: 0, y: 0, width, height }];
    let given = if rects.is_null() { &[][..] } else { std::slice::from_raw_parts(rects, count) };
    let list = if given.is_empty() || given.len() > MAX_DIRTY_RECTS { &whole[..] } else { given };
    let list = clamped_rects(list, width, height);
    #[cfg(all(test, windows))]
    let probe_started = (Instant::now(), probe::page_faults());
    crate::perf::time("screens (browser paints)", || {
        if paints_on_their_thread() {
            slot.hand_over(buffer as usize, &list);
        } else {
            slot.copy_now(std::slice::from_raw_parts(buffer, width as usize * height as usize * 4), &list);
        }
    });
    #[cfg(all(test, windows))]
    {
        probe::PAINTS.fetch_add(1, Ordering::Relaxed);
        probe::BYTES.fetch_add(list.iter().map(|r| r[2] as u64 * r[3] as u64 * 4).sum(), Ordering::Relaxed);
        probe::NS.fetch_add(probe_started.0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        probe::FAULTS.fetch_add((probe::page_faults() - probe_started.1) as u64, Ordering::Relaxed);
    }
}

unsafe extern "system" fn on_after_created(handler: *mut LifeVt, browser: *mut Browser) {
    let v = &mut *view_of(handler);
    v.browser = browser;
    OPEN_BROWSERS.fetch_add(1, Ordering::SeqCst);
    if let Some(host) = host_of(browser) {
        ((*host).set_focus)(host, 1);
        ((*host).was_hidden)(host, 0);
        release_host(host);
    }
}

unsafe extern "system" fn on_before_close(handler: *mut LifeVt, browser: *mut Browser) {
    let v = &mut *view_of(handler);
    v.closed = true;
    release_browser(browser);
    if !v.browser.is_null() {
        release_browser(v.browser);
        v.browser = std::ptr::null_mut();
    }
    OPEN_BROWSERS.fetch_sub(1, Ordering::SeqCst);
}

fn new_view(key: &str, screen_slot: Option<usize>, width: c_int, height: c_int, scale: f32, slot: Option<Arc<PaintSlot>>) -> *mut View {
    let view = Box::into_raw(Box::new(View {
        client: ClientVt { base: base::<ClientVt>(), before_life_span: [0; 13], get_life_span_handler: get_life, get_load_handler: get_load, get_print_handler: 0, get_render_handler: get_render, get_request_handler: 0, on_process_message_received: 0 },
        client_view: std::ptr::null_mut(),
        render: RenderVt { base: base::<RenderVt>(), get_accessibility_handler: 0, get_root_screen_rect: 0, get_view_rect: view_rect, get_screen_point: 0, get_screen_info: screen_info, on_popup_show: 0, on_popup_size: 0, on_paint, on_accelerated_paint: 0, rest: [0; 8] },
        render_view: std::ptr::null_mut(),
        life: LifeVt { base: base::<LifeVt>(), on_before_popup: 0, on_after_created, do_close: 0, on_before_close },
        life_view: std::ptr::null_mut(),
        load: LoadVt { base: base::<LoadVt>(), on_loading_state_change: 0, on_load_start: 0, on_load_end: 0, on_load_error: 0 },
        load_view: std::ptr::null_mut(),
        key: key.to_string(),
        screen_slot,
        width,
        height,
        scale,
        slot,
        browser: std::ptr::null_mut(),
        closed: false,
        paints: 0,
    }));
    unsafe {
        (*view).client_view = view;
        (*view).render_view = view;
        (*view).life_view = view;
        (*view).load_view = view;
    }
    view
}

struct Lib {
    create: CreateBrowser,
    work: DoWork,
}

/// A function of the CEF X-Plane has loaded; never loads it, and never links to it.
fn cef_symbol(name: &CStr) -> Option<*mut c_void> {
    #[cfg(windows)]
    let p = unsafe {
        let module = GetModuleHandleA(c"libcef.dll".as_ptr());
        if module.is_null() {
            return None;
        }
        GetProcAddress(module, name.as_ptr())
    };
    #[cfg(target_os = "macos")]
    let p = unsafe { crate::xp::library_symbol("Chromium Embedded Framework.framework/", name) };
    (!p.is_null()).then_some(p)
}

fn lib() -> Option<Lib> {
    unsafe {
        Some(Lib {
            create: std::mem::transmute::<*mut c_void, CreateBrowser>(cef_symbol(c"cef_browser_host_create_browser")?),
            work: std::mem::transmute::<*mut c_void, DoWork>(cef_symbol(c"cef_do_message_loop_work")?),
        })
    }
}

/// Whether X-Plane has its CEF loaded at all.
pub fn loaded() -> bool {
    cef_symbol(c"cef_api_hash").is_some()
}

/// Whether CEF is up and this is its UI thread (`cef_currently_on(TID_UI)`): false,
/// with no harm, before X-Plane has initialized it.
#[cfg(target_os = "macos")]
pub fn on_ui_thread() -> bool {
    let Some(f) = cef_symbol(c"cef_currently_on") else { return false };
    unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn(c_int) -> c_int>(f)(0) != 0 }
}

#[repr(C)]
#[allow(dead_code)]
#[cfg(windows)]
struct Settings {
    size: usize,
    no_sandbox: c_int,
    browser_subprocess_path: CefString,
    framework_dir_path: CefString,
    main_bundle_path: CefString,
    chrome_runtime: c_int,
    multi_threaded_message_loop: c_int,
    external_message_pump: c_int,
    windowless_rendering_enabled: c_int,
    command_line_args_disabled: c_int,
    cache_path: CefString,
    root_cache_path: CefString,
    persist_session_cookies: c_int,
    persist_user_preferences: c_int,
    user_agent: CefString,
    user_agent_product: CefString,
    locale: CefString,
    log_file: CefString,
    log_severity: c_int,
    log_items: c_int,
    javascript_flags: CefString,
    resources_dir_path: CefString,
    locales_dir_path: CefString,
    pack_loading_disabled: c_int,
    remote_debugging_port: c_int,
    uncaught_exception_stack_size: c_int,
    background_color: u32,
    accept_language_list: CefString,
    cookieable_schemes_list: CefString,
    cookieable_schemes_exclude_defaults: c_int,
}

#[repr(C)]
#[cfg(windows)]
struct CommandLine {
    base: Base,
    before: [Slot; 14],
    append_switch: unsafe extern "system" fn(*mut CommandLine, *const CefString),
    append_switch_with_value: unsafe extern "system" fn(*mut CommandLine, *const CefString, *const CefString),
}

#[cfg(windows)]
type OnCommandLine = unsafe extern "system" fn(*mut AppVt, *const CefString, *mut CommandLine);

#[repr(C)]
#[cfg(windows)]
struct AppVt {
    base: Base,
    on_before_command_line_processing: Option<OnCommandLine>,
    rest: [Slot; 4],
}

#[cfg(windows)]
type CefInitialize = unsafe extern "C" fn(*const *mut c_void, *const c_void, *mut AppVt, *mut c_void) -> c_int;

/// Where Chromium composites, as measured on six 1152x1536 browsers. `Software`:
/// 9 MB of the card instead of 410, and no page faults reading each paint, with the
/// renderers doing the work on their own cores. `Integrated`: as the old app ran,
/// nothing on the card and 420 MB on the Intel. `Card`: X-Plane's own choice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChromiumMode {
    Software,
    Integrated,
    Card,
}

impl ChromiumMode {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "software" => Some(Self::Software),
            "integrated" => Some(Self::Integrated),
            "card" => Some(Self::Card),
            _ => None,
        }
    }

    fn switches(self) -> &'static str {
        match self {
            Self::Software => "disable-gpu-compositing",
            Self::Integrated | Self::Card => "gpu-rasterization-msaa-sample-count=0",
        }
    }
}

pub fn chromium_mode() -> ChromiumMode {
    static MODE: OnceLock<ChromiumMode> = OnceLock::new();
    *MODE.get_or_init(|| {
        std::env::var("FBW_CHROMIUM_MODE").ok().and_then(|v| ChromiumMode::parse(&v)).unwrap_or_else(|| {
            if crate::app_settings::load_now().screens_on_integrated_gpu {
                ChromiumMode::Integrated
            } else {
                ChromiumMode::Software
            }
        })
    })
}

fn chromium_switches() -> String {
    std::env::var("FBW_CHROMIUM_SWITCHES").unwrap_or_else(|_| chromium_mode().switches().to_string())
}

#[cfg(windows)]
fn chromium_helper() -> Option<std::path::PathBuf> {
    Some(crate::xp::system_path()?.join("Resources").join("dlls").join("64").join("cef").join("win").join("cefclient.exe"))
}

#[cfg(windows)]
static IAT_SLOT: AtomicUsize = AtomicUsize::new(0);
#[cfg(windows)]
static IAT_WAS: AtomicUsize = AtomicUsize::new(0);
#[cfg(windows)]
static X_PLANES_ON_COMMAND_LINE: AtomicUsize = AtomicUsize::new(0);
static SWITCHES_APPLIED: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
unsafe fn append_switches(line: *mut CommandLine) {
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
    let string = |v: &Vec<u16>| CefString { str_: v.as_ptr(), length: v.len(), dtor: 0 };
    for switch in chromium_switches().split_whitespace() {
        match switch.split_once('=') {
            Some((name, value)) => {
                let (name, value) = (wide(name), wide(value));
                ((*line).append_switch_with_value)(line, &string(&name), &string(&value));
            }
            None => {
                let name = wide(switch);
                ((*line).append_switch)(line, &string(&name));
            }
        }
    }
}

#[cfg(windows)]
unsafe fn text(s: &CefString) -> String {
    if s.str_.is_null() {
        return String::new();
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(s.str_, s.length))
}

#[cfg(windows)]
unsafe fn log_settings(settings: *const c_void) {
    if settings.is_null() {
        return;
    }
    let s = &*(settings as *const Settings);
    if s.size < std::mem::size_of::<Settings>() {
        super::log(&format!("Chromium settings: struct is {} bytes, ours {}", s.size, std::mem::size_of::<Settings>()));
        return;
    }
    super::log(&format!(
        "Chromium settings: multi_threaded={} external_pump={} windowless={} args_disabled={} no_sandbox={} chrome_runtime={} helper={} cache={} js_flags={} debug_port={} background={:#x}",
        s.multi_threaded_message_loop,
        s.external_message_pump,
        s.windowless_rendering_enabled,
        s.command_line_args_disabled,
        s.no_sandbox,
        s.chrome_runtime,
        text(&s.browser_subprocess_path),
        text(&s.cache_path),
        text(&s.javascript_flags),
        s.remote_debugging_port,
        s.background_color,
    ));
}

#[cfg(windows)]
unsafe fn log_command_line(line: *mut CommandLine) {
    const GET_COMMAND_LINE_STRING: usize = 7;
    let get = std::mem::transmute::<Slot, unsafe extern "system" fn(*mut CommandLine) -> *mut CefString>((*line).before[GET_COMMAND_LINE_STRING]);
    let string = get(line);
    if string.is_null() {
        return;
    }
    super::log(&format!("Chromium command line: {}", text(&*string)));
    if let Some(free) = cef_symbol(c"cef_string_userfree_utf16_free") {
        std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut CefString)>(free)(string);
    }
}

#[cfg(windows)]
unsafe extern "system" fn on_command_line(app: *mut AppVt, process: *const CefString, line: *mut CommandLine) {
    let was = X_PLANES_ON_COMMAND_LINE.load(Ordering::SeqCst);
    if was != 0 {
        std::mem::transmute::<usize, OnCommandLine>(was)(app, process, line);
    }
    if process.is_null() || (*process).length == 0 {
        append_switches(line);
        log_command_line(line);
        SWITCHES_APPLIED.store(true, Ordering::SeqCst);
    }
}

#[cfg(windows)]
unsafe fn write_slot(slot: usize, value: usize) -> bool {
    const PAGE_READWRITE: u32 = 4;
    let mut protection = 0u32;
    if VirtualProtect(slot as *mut c_void, 8, PAGE_READWRITE, &mut protection) == 0 {
        return false;
    }
    std::ptr::write_volatile(slot as *mut usize, value);
    let mut ignored = 0u32;
    VirtualProtect(slot as *mut c_void, 8, protection, &mut ignored);
    true
}

/// An app for a Chromium started without one. CEF keeps it until it shuts down,
/// after this DLL may have unloaded with the aircraft, so nothing in it lives here:
/// two small functions in memory of their own, returning 0 and 1.
#[cfg(windows)]
unsafe fn app_outliving_this_dll() -> Option<(*mut AppVt, Slot)> {
    const MEM_COMMIT_RESERVE: u32 = 0x3000;
    const PAGE_READWRITE: u32 = 4;
    const PAGE_EXECUTE_READ: u32 = 0x20;
    const RETURN_0: [u8; 3] = [0x31, 0xC0, 0xC3];
    const RETURN_1: [u8; 6] = [0xB8, 1, 0, 0, 0, 0xC3];
    let code = VirtualAlloc(std::ptr::null_mut(), 4096, MEM_COMMIT_RESERVE, PAGE_READWRITE) as *mut u8;
    let app = VirtualAlloc(std::ptr::null_mut(), 4096, MEM_COMMIT_RESERVE, PAGE_READWRITE) as *mut AppVt;
    if code.is_null() || app.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(RETURN_0.as_ptr(), code, RETURN_0.len());
    std::ptr::copy_nonoverlapping(RETURN_1.as_ptr(), code.add(16), RETURN_1.len());
    let mut was = 0u32;
    if VirtualProtect(code as *mut c_void, 4096, PAGE_EXECUTE_READ, &mut was) == 0 {
        return None;
    }
    let (zero, one) = (code as Slot, code.add(16) as Slot);
    app.write(AppVt {
        base: Base {
            size: std::mem::size_of::<AppVt>(),
            add_ref: Some(std::mem::transmute::<Slot, unsafe extern "system" fn(*mut c_void)>(zero)),
            release: Some(std::mem::transmute::<Slot, unsafe extern "system" fn(*mut c_void) -> c_int>(zero)),
            has_one_ref: Some(std::mem::transmute::<Slot, unsafe extern "system" fn(*mut c_void) -> c_int>(one)),
            has_at_least_one_ref: Some(std::mem::transmute::<Slot, unsafe extern "system" fn(*mut c_void) -> c_int>(one)),
        },
        on_before_command_line_processing: Some(on_command_line),
        rest: [zero; 4],
    });
    Some((app, zero))
}

/// X-Plane's call to start Chromium, with our switches added to its command line.
#[cfg(windows)]
unsafe extern "C" fn cef_initialize_hook(args: *const *mut c_void, settings: *const c_void, app: *mut AppVt, sandbox: *mut c_void) -> c_int {
    let slot = IAT_SLOT.swap(0, Ordering::SeqCst);
    if slot != 0 {
        write_slot(slot, IAT_WAS.load(Ordering::SeqCst));
    }
    log_settings(settings);
    let Some(real) = cef_symbol(c"cef_initialize") else { return 0 };
    let real = std::mem::transmute::<*mut c_void, CefInitialize>(real);
    if app.is_null() {
        let Some((own, zero)) = app_outliving_this_dll() else {
            super::log("Chromium starts with X-Plane's settings: no memory for our app");
            return real(args, settings, app, sandbox);
        };
        super::log("Chromium is starting, with no app from X-Plane; adding ours");
        let started = real(args, settings, own, sandbox);
        (*own).on_before_command_line_processing = Some(std::mem::transmute::<Slot, OnCommandLine>(zero));
        return started;
    }
    super::log("Chromium is starting, with X-Plane's app");
    let was = (*app).on_before_command_line_processing;
    X_PLANES_ON_COMMAND_LINE.store(was.map_or(0, |f| f as usize), Ordering::SeqCst);
    (*app).on_before_command_line_processing = Some(on_command_line);
    let started = real(args, settings, app, sandbox);
    (*app).on_before_command_line_processing = was;
    started
}

/// The slot of `function` in the delay-load table of `module`, for `dll`.
#[cfg(windows)]
unsafe fn delay_import_slot(module: usize, dll: &str, function: &str) -> Option<usize> {
    let read32 = |at: usize| std::ptr::read_unaligned(at as *const u32) as usize;
    let name_at = |at: usize| CStr::from_ptr(at as *const c_char).to_string_lossy().into_owned();
    let headers = module + read32(module + 0x3C);
    const DELAY_DIRECTORY_AT: usize = 24 + 112 + 13 * 8;
    let directory = read32(headers + DELAY_DIRECTORY_AT);
    if directory == 0 {
        return None;
    }
    let mut descriptor = module + directory;
    loop {
        let name = read32(descriptor + 4);
        if name == 0 {
            return None;
        }
        if name_at(module + name).eq_ignore_ascii_case(dll) {
            let (iat, names) = (module + read32(descriptor + 12), module + read32(descriptor + 16));
            for i in 0.. {
                let entry = std::ptr::read_unaligned((names + i * 8) as *const u64);
                if entry == 0 {
                    return None;
                }
                if entry >> 63 == 0 && name_at(module + (entry as usize & 0x7FFF_FFFF) + 2) == function {
                    return Some(iat + i * 8);
                }
            }
        }
        descriptor += 32;
    }
}

/// Before anything asks X-Plane for a browser: Chromium takes its switches when it starts.
#[cfg(windows)]
pub fn prepare_chromium_start() {
    static ONCE: AtomicBool = AtomicBool::new(false);
    if ONCE.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(helper) = chromium_helper() {
        crate::set_gpu_preference(&helper, chromium_mode() == ChromiumMode::Integrated);
    }
    super::log(&format!("Chromium mode: {:?}", chromium_mode()));
    unsafe {
        let exe = GetModuleHandleA(std::ptr::null()) as usize;
        let Some(slot) = delay_import_slot(exe, "libcef.dll", "cef_initialize") else {
            super::log("Chromium's start is not reachable, its switches stay as X-Plane sets them");
            return;
        };
        let was = std::ptr::read_volatile(slot as *const usize);
        let image_bytes = std::ptr::read_unaligned((exe + std::ptr::read_unaligned((exe + 0x3C) as *const u32) as usize + 24 + 56) as *const u32) as usize;
        if !(exe..exe + image_bytes).contains(&was) {
            super::log("X-Plane already called Chromium's start, too early for our switches");
            return;
        }
        IAT_WAS.store(was, Ordering::SeqCst);
        IAT_SLOT.store(slot, Ordering::SeqCst);
        if !write_slot(slot, cef_initialize_hook as *const () as usize) {
            IAT_SLOT.store(0, Ordering::SeqCst);
        }
    }
}

/// Before this DLL unloads, if X-Plane never started Chromium.
#[cfg(windows)]
pub fn forget_chromium_start() {
    let slot = IAT_SLOT.swap(0, Ordering::SeqCst);
    if slot != 0 {
        unsafe { write_slot(slot, IAT_WAS.load(Ordering::SeqCst)) };
    }
}

/// X-Plane's Chromium is already running on macOS by the time a plugin loads, and its
/// start is not ours to hook: it keeps X-Plane's switches.
#[cfg(not(windows))]
pub fn prepare_chromium_start() {}

#[cfg(not(windows))]
pub fn forget_chromium_start() {}

pub fn chromium_switches_applied() -> bool {
    SWITCHES_APPLIED.load(Ordering::SeqCst)
}

/// Whether the X-Plane installed here carries the exact Chromium this code was written for.
pub fn available(xplane_root: &Path) -> Result<(), String> {
    unsafe {
        #[cfg(windows)]
        if GetModuleHandleA(c"libcef.dll".as_ptr()).is_null() {
            let path = xplane_root.join("Resources").join("dlls").join("64").join("cef").join("win").join("libcef.dll");
            let wide: Vec<u16> = path.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
            const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 8;
            if LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH).is_null() {
                return Err(format!("cannot load {}", xplane_root.display()));
            }
        }
        #[cfg(not(windows))]
        let _ = xplane_root;
        let Some(hash) = cef_symbol(c"cef_api_hash") else {
            return Err("X-Plane's Chromium is not loaded, or has no cef_api_hash".to_string());
        };
        let hash = std::mem::transmute::<*mut c_void, ApiHash>(hash);
        let found = CStr::from_ptr(hash(0)).to_string_lossy().into_owned();
        if found != API_HASH {
            return Err(format!("X-Plane's Chromium has API hash {found}, this plugin was written for {API_HASH}"));
        }
    }
    Ok(())
}

/// What to open: a key the server's host page answers to, and its screen.
pub struct Wanted {
    pub key: String,
    pub screen: Option<String>,
    pub width: u32,
    pub height: u32,
}

pub struct Browsers {
    views: Vec<*mut View>,
    paints: Option<PaintThread>,
    fps_cap: c_int,
    governed: c_int,
    governed_outside: bool,
    governed_at: Option<(u64, Instant)>,
    stats_at: Instant,
}

const OUTSIDE_RATE: c_int = 5;

/// The big screens need no full rate from outside. Everything else keeps it: the
/// pages that do the aircraft's work run on the same clock.
fn rate_for(key: &str, base: c_int, outside: bool) -> c_int {
    if outside && matches!(key, "SCREEN_DU_PFDL" | "SCREEN_DU_PFDR" | "SCREEN_DU_NDL" | "SCREEN_DU_NDR" | "SCREEN_DU_EWD" | "SCREEN_DU_SD") {
        return OUTSIDE_RATE.min(base);
    }
    base
}

static RATE_WANTED: AtomicBool = AtomicBool::new(false);

fn screen_info_for(screen: &str) -> (f32, usize) {
    (views::supersample(screen) as f32, SCREEN_ORDER.iter().position(|s| s.eq_ignore_ascii_case(screen)).unwrap_or(usize::MAX))
}

pub fn open_all(tag: &str, port: u16, wanted: &[Wanted], fps_cap: c_int) -> Option<Browsers> {
    let lib = lib()?;
    let mut blocks = Vec::new();
    let mut block_of = vec![None; wanted.len()];
    for (i, w) in wanted.iter().enumerate() {
        let Some(screen) = &w.screen else { continue };
        let (dw, dh) = views::device_size(screen, w.width, w.height);
        match ScreenBlock::open(tag, screen, dw, dh) {
            Some(block) => {
                block_of[i] = Some(blocks.len());
                blocks.push(block);
            }
            None => super::log(&format!("no ScreenBlock for {screen}")),
        }
    }
    let (paints, slots) = PaintThread::start(blocks);
    let mut list = Vec::with_capacity(wanted.len());
    for (i, w) in wanted.iter().enumerate() {
        let (scale, slot) = match &w.screen {
            Some(screen) => {
                let (scale, slot) = screen_info_for(screen);
                (scale, (slot != usize::MAX).then_some(slot))
            }
            None => (1., None),
        };
        let view = new_view(&w.key, slot, w.width.max(1) as c_int, w.height.max(1) as c_int, scale, block_of[i].map(|b| slots[b].clone()));
        let url = format!("http://s{i}.localhost:{port}/__b/boot/{}", w.key);
        #[cfg(test)]
        let url = std::env::var("FBW_BENCH_URL").unwrap_or(url);
        let url: Vec<u16> = url.encode_utf16().collect();
        let url_string = CefString { str_: url.as_ptr(), length: url.len(), dtor: 0 };
        let info = WindowInfo::windowless();
        let settings = BrowserSettings {
            size: std::mem::size_of::<BrowserSettings>(),
            windowless_frame_rate: fps_cap,
            fonts: [NO_STRING, NO_STRING, NO_STRING, NO_STRING, NO_STRING, NO_STRING],
            default_font_size: 0,
            default_fixed_font_size: 0,
            minimum_font_size: 0,
            minimum_logical_font_size: 0,
            default_encoding: NO_STRING,
            states: [0; 12],
            background_color: 0,
            accept_language_list: NO_STRING,
            chrome_status_bubble: 0,
        };
        let made = unsafe { (lib.create)(&info, &mut (*view).client, &url_string, &settings, std::ptr::null_mut(), std::ptr::null_mut()) };
        if made == 0 {
            super::log(&format!("X-Plane's Chromium refused the browser for {}", w.key));
        }
        list.push(view);
    }
    RATE_WANTED.store(false, Ordering::Relaxed);
    Some(Browsers { views: list, paints: Some(paints), fps_cap, governed: 0, governed_outside: false, governed_at: None, stats_at: Instant::now() })
}

impl Browsers {
    pub fn tick(&mut self, session: &Session) {
        self.drain_input(session);
        self.govern(session);
        if self.stats_at.elapsed() > Duration::from_secs(30) {
            self.stats_at = Instant::now();
            let rows: Vec<String> = self
                .views
                .iter()
                .map(|v| unsafe {
                    let v = &mut **v;
                    let row = format!("{}:{}", v.key, v.paints / 30);
                    v.paints = 0;
                    row
                })
                .collect();
            super::log(&format!("paints a second, {} open: {}", OPEN_BROWSERS.load(Ordering::SeqCst), rows.join(" ")));
            super::log(&format!("paint copies: {}", paint_thread::take_report()));
        }
    }

    pub fn reload_all(&self) {
        for v in &self.views {
            unsafe {
                let b = (**v).browser;
                if !b.is_null() {
                    ((*b).reload_ignore_cache)(b);
                }
            }
        }
    }

    fn set_rate(&self, rate: c_int, outside: bool) {
        for v in &self.views {
            unsafe {
                if let Some(host) = host_of((**v).browser) {
                    ((*host).set_windowless_frame_rate)(host, rate_for(&(**v).key, rate, outside));
                    release_host(host);
                }
            }
        }
    }

    /// No faster than X-Plane shows, as every paint costs its main thread, and never
    /// for a 1-2 fps wobble.
    fn govern(&mut self, session: &Session) {
        let now = Instant::now();
        let frame = session.slots.header().frame.load(Ordering::Acquire);
        let Some((last_frame, at)) = self.governed_at else {
            self.governed_at = Some((frame, now));
            return;
        };
        let seconds = (now - at).as_secs_f64();
        if seconds < 1. {
            return;
        }
        self.governed_at = Some((frame, now));
        let sim_fps = frame.saturating_sub(last_frame) as f64 / 2. / seconds;
        let shown = if sim_fps > 0. { (sim_fps.ceil() as c_int).clamp(10, 60) } else { self.fps_cap };
        let rate = shown.min(self.fps_cap.clamp(1, 60));
        let outside = super::VIEW_OUTSIDE.load(Ordering::Relaxed);
        if self.governed <= 0 || (rate - self.governed).abs() >= 3 || outside != self.governed_outside {
            self.governed = rate;
            self.governed_outside = outside;
            self.set_rate(rate, outside);
        }
    }

    fn drain_input(&self, session: &Session) {
        for record in session.input.drain() {
            let Some(input) = Input::decode(&record) else { continue };
            let slot = input.screen as usize;
            let Some(view) = self.views.iter().find(|v| unsafe { (***v).screen_slot } == Some(slot)) else { continue };
            unsafe {
                let Some(host) = host_of((**view).browser) else { continue };
                send_input(host, &input);
                release_host(host);
            }
        }
    }

    /// Every browser closed, pumping CEF ourselves: its callbacks live in this DLL.
    pub fn close(mut self) {
        if let Some(paints) = self.paints.take() {
            paints.stop();
        }
        let Some(lib) = lib() else { return };
        for v in &self.views {
            unsafe {
                if let Some(host) = host_of((**v).browser) {
                    ((*host).close_browser)(host, 1);
                    release_host(host);
                }
            }
        }
        let until = Instant::now() + Duration::from_secs(5);
        while OPEN_BROWSERS.load(Ordering::SeqCst) > 0 && Instant::now() < until {
            unsafe { (lib.work)() };
            std::thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..40 {
            unsafe { (lib.work)() };
            std::thread::sleep(Duration::from_millis(5));
        }
        let left = OPEN_BROWSERS.load(Ordering::SeqCst);
        if left != 0 {
            super::log(&format!("{left} browsers did not close"));
        }
    }
}

unsafe fn send_input(host: *mut Host, input: &Input) {
    let event = MouseEvent { x: input.x.round() as c_int, y: input.y.round() as c_int, modifiers: 0 };
    match input.kind {
        InputKind::Down => ((*host).send_mouse_click_event)(host, &event, 0, 0, 1),
        InputKind::Up => ((*host).send_mouse_click_event)(host, &event, 0, 1, 1),
        InputKind::Move => ((*host).send_mouse_move_event)(host, &event, 0),
        InputKind::Wheel => ((*host).send_mouse_wheel_event)(host, &event, 0, (input.delta * 120.) as c_int),
        InputKind::Key => send_key(host, input),
    }
}

unsafe fn send_key(host: *mut Host, input: &Input) {
    const RAWKEYDOWN: c_int = 0;
    const KEYUP: c_int = 2;
    const CHAR: c_int = 3;
    let vkey = input.y as c_int;
    let character = char::from_u32(input.x as u32).map_or(0, |c| c as u32 as u16);
    let flags = input.delta as u32;
    let mut modifiers = 0;
    if flags & 1 != 0 {
        modifiers |= 1 << 1;
    }
    if flags & 4 != 0 {
        modifiers |= 1 << 2;
    }
    if flags & 2 != 0 {
        modifiers |= 1 << 3;
    }
    let key = |type_: c_int, ch: u16| KeyEvent { type_, modifiers, windows_key_code: vkey, native_key_code: 0, is_system_key: 0, character: ch, unmodified_character: ch, focus_on_editable_field: 0 };
    ((*host).send_key_event)(host, &key(RAWKEYDOWN, 0));
    if matches!(vkey, 0x09 | 0x0D) {
        ((*host).send_key_event)(host, &key(CHAR, vkey as u16));
    } else if !matches!(vkey, 0x08 | 0x1B | 0x21 | 0x22 | 0x25..=0x28 | 0x2D | 0x2E | 0x70..=0x7B) {
        ((*host).send_key_event)(host, &key(CHAR, character));
    }
    ((*host).send_key_event)(host, &key(KEYUP, 0));
}

#[cfg(all(test, windows))]
pub mod probe {
    use super::*;

    pub static NS: AtomicU64 = AtomicU64::new(0);
    pub static FAULTS: AtomicU64 = AtomicU64::new(0);
    pub static PAINTS: AtomicU64 = AtomicU64::new(0);
    pub static BYTES: AtomicU64 = AtomicU64::new(0);

    pub fn page_faults() -> u32 {
        extern "system" {
            fn K32GetProcessMemoryInfo(process: *mut c_void, counters: *mut u32, bytes: u32) -> i32;
            fn GetCurrentProcess() -> *mut c_void;
        }
        let mut c = [0u32; 20];
        c[0] = 80;
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), c.as_mut_ptr(), 80) };
        c[1]
    }

    pub fn reset() {
        for a in [&NS, &FAULTS, &PAINTS, &BYTES] {
            a.store(0, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod paint_rect_tests {
    use super::*;

    #[test]
    fn paint_rectangles_are_clamped_to_the_frame() {
        let list = [Rect { x: -5, y: 2, width: 10, height: 3 }, Rect { x: 90, y: 90, width: 50, height: 50 }, Rect { x: 200, y: 0, width: 5, height: 5 }];
        assert_eq!(clamped_rects(&list, 100, 100), vec![[0, 2, 5, 3], [90, 90, 10, 10]]);
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn each_mode_has_its_switches() {
        assert_eq!(ChromiumMode::parse("software"), Some(ChromiumMode::Software));
        assert_eq!(ChromiumMode::parse("integrated"), Some(ChromiumMode::Integrated));
        assert_eq!(ChromiumMode::parse("card"), Some(ChromiumMode::Card));
        assert_eq!(ChromiumMode::parse("nonsense"), None);
        assert!(ChromiumMode::Software.switches().contains("disable-gpu-compositing"));
        assert!(!ChromiumMode::Integrated.switches().contains("disable-gpu-compositing"));
    }
}

#[cfg(all(test, windows))]
mod start_tests {
    use super::*;

    #[test]
    #[ignore = "needs X-Plane.exe, FBW_XPLANE_EXE"]
    fn x_planes_chromium_start_is_found() {
        let exe = std::env::var("FBW_XPLANE_EXE").expect("FBW_XPLANE_EXE");
        let wide: Vec<u16> = exe.encode_utf16().chain(std::iter::once(0)).collect();
        const DONT_RESOLVE_DLL_REFERENCES: u32 = 1;
        unsafe {
            let module = LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), DONT_RESOLVE_DLL_REFERENCES);
            assert!(!module.is_null());
            let module = module as usize & !3;
            assert!(delay_import_slot(module, "libcef.dll", "cef_initialize").is_some());
            assert!(delay_import_slot(module, "libcef.dll", "no_such_function").is_none());
        }
    }
}

#[cfg(test)]
mod rate_tests {
    use super::*;

    #[test]
    fn only_the_big_screens_slow_down_from_outside() {
        for key in ["SCREEN_DU_PFDL", "SCREEN_DU_PFDR", "SCREEN_DU_NDL", "SCREEN_DU_NDR"] {
            assert_eq!(rate_for(key, 20, false), 20);
            assert_eq!(rate_for(key, 20, true), OUTSIDE_RATE);
        }
        assert_eq!(rate_for("SCREEN_DU_EWD", 20, true), OUTSIDE_RATE);
        assert_eq!(rate_for("SCREEN_DU_MFD", 20, true), 20);
        assert_eq!(rate_for("SystemsHost", 20, true), 20);
    }
}

#[cfg(all(test, windows))]
pub mod test_support {
    use super::*;

    /// Starts a copy of X-Plane's Chromium in this process, as X-Plane itself does.
    pub fn start_chromium(dir: &str, cache: &str) {
        let wide = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
        let string = |v: &Vec<u16>| CefString { str_: v.as_ptr(), length: v.len(), dtor: 0 };
        let dll: Vec<u16> = format!("{dir}/libcef.dll").replace('/', "\\").encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let module = LoadLibraryExW(dll.as_ptr(), std::ptr::null_mut(), 8);
            assert!(!module.is_null(), "libcef.dll");
            let helper = wide(&format!("{dir}/cefclient.exe").replace('/', "\\"));
            let cache_w = wide(cache);
            let log = wide(&format!("{cache}\\cef.log"));
            let resources = wide(&dir.replace('/', "\\"));
            let locales = wide(&format!("{dir}/locales").replace('/', "\\"));
            let settings = Settings {
                size: std::mem::size_of::<Settings>(),
                no_sandbox: 1,
                browser_subprocess_path: string(&helper),
                framework_dir_path: NO_STRING,
                main_bundle_path: NO_STRING,
                chrome_runtime: 0,
                multi_threaded_message_loop: 0,
                external_message_pump: 0,
                windowless_rendering_enabled: 1,
                command_line_args_disabled: 1,
                cache_path: string(&cache_w),
                root_cache_path: string(&cache_w),
                persist_session_cookies: 0,
                persist_user_preferences: 0,
                user_agent: NO_STRING,
                user_agent_product: NO_STRING,
                locale: NO_STRING,
                log_file: string(&log),
                log_severity: 0,
                log_items: 0,
                javascript_flags: NO_STRING,
                resources_dir_path: string(&resources),
                locales_dir_path: string(&locales),
                pack_loading_disabled: 0,
                remote_debugging_port: std::env::var("FBW_BENCH_CDP").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
                uncaught_exception_stack_size: 0,
                background_color: 0,
                accept_language_list: NO_STRING,
                cookieable_schemes_list: NO_STRING,
                cookieable_schemes_exclude_defaults: 0,
            };
            let init = GetProcAddress(module, c"cef_initialize".as_ptr());
            let init = std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*const *mut c_void, *const Settings, *mut c_void, *mut c_void) -> c_int>(init);
            let instance = GetModuleHandleA(std::ptr::null());
            let _ = init;
            assert_eq!(cef_initialize_hook(&instance, &settings as *const Settings as *const c_void, std::ptr::null_mut(), std::ptr::null_mut()), 1, "cef_initialize");
            assert!(SWITCHES_APPLIED.load(Ordering::SeqCst), "the switches reach Chromium");
        }
    }

    pub fn pump() {
        if let Some(lib) = lib() {
            unsafe { (lib.work)() };
        }
    }

    pub fn stats(b: &Browsers) -> Vec<(String, u64)> {
        b.views.iter().map(|v| unsafe { ((**v).key.clone(), (**v).paints) }).collect()
    }

    pub fn reset(b: &Browsers) {
        for v in &b.views {
            unsafe {
                (**v).paints = 0;
            }
        }
    }

    pub fn open_count() -> i32 {
        OPEN_BROWSERS.load(Ordering::SeqCst)
    }

    pub fn tga_like_dump(block: &ScreenBlock) -> (u32, u32, Vec<u8>) {
        let h = block.header();
        let (w, ht) = (h.width.load(Ordering::Relaxed), h.height.load(Ordering::Relaxed));
        (w, ht, block.pixels()[..w as usize * ht as usize * 4].to_vec())
    }

}
