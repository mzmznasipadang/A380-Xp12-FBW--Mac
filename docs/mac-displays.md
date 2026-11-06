# Cockpit screens on macOS: why they are black, and a porting plan

Found on a real Mac install (2026-11-05): beta installer `tools/beta/mac.sh`,
release 1.0.5 + `main` at `cc77493`, X-Plane 12.4.3-r2, macOS 27.0.1, Apple M4 Pro.

## Summary

On a Mac every DU (PFD, ND, EWD, SD, MFD, ...) is solid black even with all AC
buses powered and the brightness pots at 1.0. Nothing is broken in the systems,
power logic or GL drawing. **No display route works on macOS today:**

| Route | Where it runs | Why it does not work on this Mac |
|---|---|---|
| XPHFBW.exe (`app/`) | Windows default | CEF 152 + D3D11 shared textures + Win32; Windows-only |
| Browser devices (`browser_screens`, default on non-Windows) | `XPLMAvionicsSetURL` / `XPLMAvionicsRefresh` / `XPLMAvionicsInjectScript` | **Not exported by X-Plane 12.4.3** (checked with `nm` on `XPLM.framework`; it only has `XPLMCreateAvionicsEx` and the draw/hook calls). `enabled()` falls back. |
| Own CEF (`browser_screens/own_cef.rs`) | Windows only (`#[cfg(windows)]`, `libcef.dll`) | Not built for macOS |
| Built-in QuickJS runtime (`src/js/msfs`) | The fallback that actually runs | Cannot run the shipped pages (below) |

### Why the fallback draws nothing

`src/js/msfs/instrument.js` `importPage()` only runs MSFS-style
`<script import-script="...">` tags and `<script type="text/html" id>` templates.
The shipped pages (release zip and `fbw/xplane/xplane/build.mjs` output alike)
are the xp-ui format (`xp-ui/spec/DATA-SPEC.md` 7.1):

```html
<template id="A380X_PFD">...</template>
<script src="pfd.js"></script>
```

So no gauge script runs, `registerInstrument`/`defineInstrument` is never
called, and the renderer clears each screen to black. The log shows it:

```
js: VCockpit05 A380X/PFD/pfd.html?Index=1&duID=0: loaded in 2 ms, 2.3 MB
js: instruments            <- empty: no instrument identified itself
```

Red herrings ruled out: the `GL_INVALID_OPERATION` lines after the atlas/VBO
uploads come from `glBindVertexArray(0)` (core name) in the legacy 2.1 context
X-Plane gives plugins on a Mac. A standalone replay of the renderer's calls on
the same driver ("2.1 Metal - 91.7") showed that is the only failing call, and
the GL_ALPHA8 atlas, VBO, 4x MSAA framebuffer and blit all work. Harmless; the call
is now skipped on macOS (`display/gl.rs`) to quiet the log.

## Porting options (cheapest first)

### A. Own CEF on macOS (recommended)

X-Plane for Mac already runs Chromium in-process:
`X-Plane.app/Contents/Frameworks/Chromium Embedded Framework.framework`,
**CEF 117.2.5 / Chromium 117.0.5938.152**. That is the CEF generation
`own_cef.rs` was written for (117.1.4). Measured on the Mac:

- `cef_api_hash(0)` = `35b5f3657fe9f203a7852235b85b5f2a7501d59e` (platform),
  `cef_api_hash(1)` = `133bd59744d82162591d93434cb6ff146717a3f7` (universal).
  `API_HASH_WINDOWS` differs, as platform hashes always do; the hand-written
  structs need checking against the 117.2.5 macOS headers, not just the hash.

Work:
1. Build `own_cef` for macOS: `dlopen(..., RTLD_NOLOAD)` / `dlsym` on the
   framework instead of `GetModuleHandleA`/`GetProcAddress`; a per-platform
   hash constant; the macOS `cef_window_info_t` layout for windowless rendering
   (`parent_view`, `windowless_rendering_enabled`).
2. Un-gate `paint_copy` / `paint_thread` (they have no Windows calls) and feed
   `OnPaint` BGRA buffers to the existing GL upload path
   (`display::gl::Renderer::bridge_upload`, already plain GL 2.1).
3. Keep `server.rs` (loopback `coui://` server) as is; it is not
   Windows-gated.
4. In `browser_screens::enabled()`, take the own-CEF route on macOS when the
   framework is loaded and its hash matches, before the
   "no browser devices" fallback.

Risk: CEF calls must run on the thread that pumps X-Plane's CEF; on macOS that
is the AppKit main thread, which is X-Plane's flight-loop thread. Confirm
before relying on it.

#### Prototype status (branch `claude/mac-own-cef-8e3070`)

Done, in `src/browser_screens/`:

- `own_cef.rs` builds for macOS. It finds CEF with
  `xp::library_symbol("Chromium Embedded Framework.framework/", ...)`, which
  does `dlopen(RTLD_NOLOAD)` over the loaded dyld images, then `dlsym`. It never
  loads or links the framework itself. The hash constant is per platform (Mac:
  `35b5f365...`). `WindowInfo` uses the `cef_types_mac.h` layout.
- Structs were checked against the CEF source at `da4c36a` (117.2.5+gda4c36a,
  the build X-Plane's binary names). Its `cef_api_hash.h` has exactly the
  measured platform hash and universal hash. A C program compiled against
  those headers printed `sizeof`/`offsetof` for every struct and vtable slot
  the plugin touches. They are now `const` asserts in `own_cef.rs`
  (`cfg(target_os = "macos")`), so a wrong field fails the build. Only
  `cef_window_info_t` differs from the Windows code. Everything else
  (browser, host, client, render/life/load handlers, browser settings, key,
  mouse and screen info) has the same layout.
- `paint_copy.rs` and `paint_thread.rs` build on macOS. The paint thread
  reads with `mach_vm_read_overwrite` where Windows uses
  `ReadProcessMemory`. Both fail rather than fault. On macOS our Chromium
  switches are never applied, because X-Plane started CEF before we load and
  its `cef_initialize` is not ours to hook. So `paints_on_their_thread()` is
  false and `OnPaint` copies on the main thread, straight into the
  ScreenBlocks that `display/mod.rs` already creates (in-process emulation)
  and uploads with `bridge_upload`. No new GL code was needed.
- `enabled()`: on macOS the own route needs no browser-device API. It checks
  that the framework is loaded and `cef_api_hash(0)` matches.
  `tick()`/`tick_own()` no longer need `XPLMAvionicsSetURL`. Windows still
  boots Chromium with a blank device (`chromium_up`, Windows body).
- Thread: on macOS CEF has no `multi_threaded_message_loop`, so its UI
  thread is the thread that called `cef_initialize`, the AppKit main thread,
  which is where XPLM callbacks run. The plugin checks this at runtime
  before the first call: `chromium_up` (macOS body) calls
  `cef_currently_on(TID_UI)` once a second and opens browsers only when it
  returns 1. Before CEF is up, that call returns 0 with no harm (the
  task-runner manager is null; each such check logs one CEF warning).
- Static evidence that X-Plane has CEF running: X-Plane's binary carries the
  string "Failed to initialize the required Chromium Embedded Framework."
  and the `CefMessageRouter` wrapper. It loads the framework at runtime (it
  is not in `otool -L`).

Not tested inside X-Plane (nothing was installed). To test: back up the
installed `plugins/<plugin>/64/mac.xpl`, copy `target/mac-own-cef/mac.xpl`
from this worktree over it, leave `FBW_BROWSER_ROUTE` unset
(`browserDisplays` is on by default on macOS), load the A380, then read
`Log.txt`. What to look for, in order:

1. `browser screens: using the app: ...`: the framework is not loaded, or
   the hash differs. On macOS a framework that is not loaded yet when the plugin
   starts no longer turns the route off: `enabled()` logs "checking again once
   the screens start" and `chromium_up` repeats the load-and-hash check once a
   second, logging the reason every 30 s.
2. `X-Plane's Chromium is not up on this thread ...` repeating: CEF never
   came up, or plugins do not run on its UI thread.
3. `X-Plane's Chromium is up and this is its UI thread`, then
   `N browsers opened`.
4. `X-Plane's Chromium refused the browser for ...`: in 117.2.5,
   `CreateBrowser` returns false only for an invalid context, a wrong
   `cef_browser_settings_t` size (asserted at 288), or a missing render
   handler. So this is unexpected; read CEF's own log.
5. `paints a second, N open: ...` every 30 s: 0 paints means the browsers
   exist but nothing reaches `OnPaint`. Either X-Plane's message pump does not
   drive them, or X-Plane's `CefSettings.windowless_rendering_enabled` is
   off. CEF only logs an error for that ("Creating a windowless browser
   without setting ...") and creates the browser anyway. If it is that
   setting, option A is dead without Laminar's help.

Left to do:

- Run it in X-Plane and work through the list above.
- Our Chromium switches (`disable-gpu-compositing`) do not apply on macOS, so
  Chromium composites as X-Plane set it up. With GPU compositing, `OnPaint`
  still delivers a CPU buffer, but measure its cost on the main thread.
- Keyboard: MFD typing did nothing in the first run. CEF on macOS
  (`TranslateWebKeyEvent`) ignores `windows_key_code`. It builds an NSEvent
  from `native_key_code` and the characters, and reads a key with no
  characters as a modifier change. `send_key` now does on macOS what CEF's
  cefclient does: KEYDOWN, CHAR, KEYUP, each with the US-layout `kVK_*` code
  and the NSEvent character (`mac_key`; Backspace 0x7F, arrows 0xF700...).
  Not yet retested in X-Plane. Non-US layouts send their character with the
  US key code, so `event.code` may not match the keycap.
- First run in X-Plane (2026-11-06, `e77a7c7`): 22 browsers open, about
  36 fps, about 3 ms per frame in the plugin, browser paints 0.03-0.25 ms per
  frame, paint copies 0.1-0.4 ms each with no failed reads. Copying on the
  main thread is cheap enough.
- `Browsers::close()` pumps `cef_do_message_loop_work` itself for up to 5 s
  while the plugin stops. That was proven on Windows only; an aircraft
  unload or reload on macOS may hang or crash there.
- Only the arm64 slice could be dlopen-tested here (no Rosetta on this Mac).
  The universal `target/mac-own-cef/mac.xpl` loads with `dlopen(RTLD_NOW)`.
  `nm -u` shows no `cef_*` or Windows symbols in either slice.
- `mac.sh` now pads both slices (`tools/beta/align_strtab.py`). In this build the
  arm64 string pool happened to be aligned and the x86_64 one was 4 bytes off.
- Windows was not `cargo check`ed: only the two darwin targets are installed in
  the toolchain used here. The Windows code paths were moved behind `cfg`, not
  changed, apart from `cef_symbol()` replacing the inline
  `GetModuleHandleA`/`GetProcAddress` pairs and `chromium_up()` holding the
  boot-device step that used to be inline in `tick_own`.

### B. Teach the QuickJS runtime the xp-ui page format

Make `importPage()` also run `<script src>` and clone `<template id>`, and load
`xplane-js-data.js` (the xp-ui page runtime the browser route injects) instead
of / alongside the MSFS shims. This needs no Chromium, but the pages were built
for a real browser: expect missing DOM/CSS features (the log already lists
unsupported `@media`, `:host`, `font-feature-settings`), and the whole
instrument stack running on QuickJS on the frame thread. Larger and riskier
than A.

### C. Wait for X-Plane's browser-device API

The plugin's own message says "12.4 or later needed", but 12.4.3 does not
export it. Worth asking Laminar which build adds `XPLMAvionicsSetURL`. Needs no
work here, but nothing is shown until it ships.

## Two Mac fixes found on the way

1. **HFS paths** (fixed, `src/lib.rs` `XPluginStart`): `remember_main_thread()`
   cached `XPLMGetSystemPath()` before `use_native_paths()` ran, so the whole
   session saw `Macintosh HD:Users:...`. Effects: "navdata: ... is not an
   X-Plane folder", "the FMS's flight plans could not be read", and
   `xphfbw.json` (e.g. `displaySafeMode`) never read. `use_native_paths()` now
   runs first.
2. **Misaligned Mach-O string table** (fixed in `mac.sh`): the arm64
   slice `cargo` produces with the macOS 27 toolchain (ld-27106.1) has
   `LC_SYMTAB.stroff` 4 bytes off an 8-byte boundary (0x014AF8C4), and
   dyld refuses it ("mis-aligned LINKEDIT string pool"). `strip -x` keeps the
   misalignment. Worked around after linking by inserting 4 zero bytes before
   the string pool, bumping `stroff` and `__LINKEDIT.filesize`, then `lipo` and
   an ad-hoc `codesign`. `mac.sh` now does this to each slice, on an unsigned copy,
   with `tools/beta/align_strtab.py` (a no-op on an aligned slice). Either slice
   can come out misaligned. A linker flag that avoids it would be cleaner; none
   was found yet.
