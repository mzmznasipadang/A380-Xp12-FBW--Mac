//! Drawing a screen's mesh with OpenGL, inside X-Plane's avionics callback.
//!
//! X-Plane 12 draws with Vulkan and gives plugins an OpenGL context of their
//! own, bridged to its textures. The rules this code keeps
//! (developer.x-plane.com, "Plugin Guidance for OpenGL Drawing"):
//!
//! - Blending, texturing, alpha test, depth test and depth writes go through
//!   `XPLMSetGraphicsState`, and 2-D textures bind through
//!   `XPLMBindTexture2d`, never glEnable or glBindTexture directly.
//! - Any other state is put back as found before returning: shaders, VAOs,
//!   VBOs and non-2-D textures unbound, only the fixed-function vertex array
//!   left enabled.
//! - After drawing into an FBO of our own, the FBO X-Plane had bound is
//!   bound again, read from `sim/graphics/view/current_gl_fbo` rather than
//!   glGet; framebuffer completeness is checked once, when it is made.
//! - No glGetError in the per-draw hot path (it stalls the pipeline once a
//!   frame is enough to notice, every triangle is not). It is checked after
//!   the rare, state-establishing uploads in [`Renderer::upload`] --
//!   texture and vertex buffer creation, which only run when an atlas
//!   generation, image or mesh actually changed -- since a silently
//!   rejected texture format or buffer upload there (GL_INVALID_ENUM,
//!   GL_INVALID_VALUE, GL_OUT_OF_MEMORY) is exactly the kind of failure
//!   that would leave geometry undrawn while everything sharing a
//!   different, unaffected call still works.
//!
//! Anti-aliasing is 4x multisampling. The mesh is drawn into a multisampled
//! framebuffer with a stencil buffer (for clip paths and translucent
//! strokes), in tiles of at most `TILE` pixels so one framebuffer serves
//! every screen, and each tile is resolved into the device's texture with
//! glBlitFramebuffer. Without framebuffer objects it draws straight into
//! X-Plane's target, without anti-aliasing and without clip paths.

use std::ffi::{c_char, c_int, c_uint, c_void, CString};

use std::collections::HashMap;
use std::sync::Arc;

use super::plan::{Step, StencilTest, LEVELS, MARK};
use super::tessellate::{Mesh, Paint, Resources, Vertex, RAMP};
use crate::mapdata::terrain::terronnd::NativeImage;
use crate::xp::AvionicsApi;

/// The multisampled framebuffer's size: screens larger than this draw in
/// tiles. 1024 covers every CDS screen in one or two tiles.
pub const TILE: i32 = 1024;
/// Samples per pixel. See docs/screens.md for the measurements behind four.
pub const SAMPLES: i32 = 4;

const GL_TRIANGLES: c_uint = 0x0004;
const GL_SCISSOR_TEST: c_uint = 0x0C11;
const GL_STENCIL_TEST: c_uint = 0x0B90;
const GL_VIEWPORT: c_uint = 0x0BA2;
const GL_COLOR_BUFFER_BIT: c_uint = 0x4000;
const GL_STENCIL_BUFFER_BIT: c_uint = 0x0400;
const GL_PROJECTION: c_uint = 0x1701;
const GL_MODELVIEW: c_uint = 0x1700;
const GL_TEXTURE: c_uint = 0x1702;
const GL_VERTEX_ARRAY: c_uint = 0x8074;
const GL_COLOR_ARRAY: c_uint = 0x8076;
const GL_TEXTURE_COORD_ARRAY: c_uint = 0x8078;
const GL_FLOAT: c_uint = 0x1406;
const GL_UNSIGNED_BYTE: c_uint = 0x1401;
const GL_UNSIGNED_INT_8_8_8_8_REV: c_uint = 0x8367;
const GL_ARRAY_BUFFER: c_uint = 0x8892;
const GL_DYNAMIC_DRAW: c_uint = 0x88E8;
const GL_TEXTURE_2D: c_uint = 0x0DE1;
const GL_ALPHA: c_uint = 0x1906;
const GL_RGBA: c_uint = 0x1908;
const GL_ALPHA8: c_int = 0x803C;
const GL_RGBA8: c_int = 0x8058;
const GL_TEXTURE_MIN_FILTER: c_uint = 0x2801;
const GL_TEXTURE_MAG_FILTER: c_uint = 0x2800;
const GL_LINEAR: c_int = 0x2601;
const GL_NEAREST: c_uint = 0x2600;
const GL_LINEAR_MIPMAP_LINEAR: c_int = 0x2703;
const GL_TEXTURE_WRAP_S: c_uint = 0x2802;
const GL_TEXTURE_WRAP_T: c_uint = 0x2803;
const GL_CLAMP_TO_EDGE: c_int = 0x812F;
const GL_TEXTURE_MAX_LEVEL: c_uint = 0x813D;
const GL_UNPACK_ALIGNMENT: c_uint = 0x0CF5;
const GL_PACK_ALIGNMENT: c_uint = 0x0D05;
const GL_TEXTURE_ENV: c_uint = 0x2300;
const GL_TEXTURE_ENV_MODE: c_uint = 0x2200;
const GL_MODULATE: c_int = 0x2100;
const GL_FRAMEBUFFER: c_uint = 0x8D40;
const GL_READ_FRAMEBUFFER: c_uint = 0x8CA8;
const GL_DRAW_FRAMEBUFFER: c_uint = 0x8CA9;
const GL_RENDERBUFFER: c_uint = 0x8D41;
const GL_COLOR_ATTACHMENT0: c_uint = 0x8CE0;
const GL_DEPTH_STENCIL_ATTACHMENT: c_uint = 0x821A;
const GL_DEPTH24_STENCIL8: c_uint = 0x88F0;
const GL_FRAMEBUFFER_COMPLETE: c_uint = 0x8CD5;
const GL_MAX_SAMPLES: c_uint = 0x8D57;
const GL_STENCIL_BITS: c_uint = 0x0D57;
const GL_SAMPLES: c_uint = 0x80A9;
const GL_KEEP: c_uint = 0x1E00;
const GL_INCR: c_uint = 0x1E02;
const GL_INVERT: c_uint = 0x150A;
const GL_EQUAL: c_uint = 0x0202;
const GL_SRC_ALPHA: c_uint = 0x0302;
const GL_ONE_MINUS_SRC_ALPHA: c_uint = 0x0303;
const GL_ONE: c_uint = 1;
const GL_BGRA: c_uint = 0x80E1;
const GL_UNPACK_ROW_LENGTH: c_uint = 0x0CF2;
const GL_NO_ERROR: c_uint = 0;
const GL_INVALID_ENUM: c_uint = 0x0500;
const GL_INVALID_VALUE: c_uint = 0x0501;
const GL_INVALID_OPERATION: c_uint = 0x0502;
const GL_STACK_OVERFLOW: c_uint = 0x0503;
const GL_STACK_UNDERFLOW: c_uint = 0x0504;
const GL_OUT_OF_MEMORY: c_uint = 0x0505;
const GL_INVALID_FRAMEBUFFER_OPERATION: c_uint = 0x0506;

/// A human name for a `glGetError` code, for the log; the numeric value too,
/// since a code this list does not recognise is still worth reporting.
fn gl_error_name(code: c_uint) -> String {
    match code {
        GL_INVALID_ENUM => "GL_INVALID_ENUM".into(),
        GL_INVALID_VALUE => "GL_INVALID_VALUE".into(),
        GL_INVALID_OPERATION => "GL_INVALID_OPERATION".into(),
        GL_STACK_OVERFLOW => "GL_STACK_OVERFLOW".into(),
        GL_STACK_UNDERFLOW => "GL_STACK_UNDERFLOW".into(),
        GL_OUT_OF_MEMORY => "GL_OUT_OF_MEMORY".into(),
        GL_INVALID_FRAMEBUFFER_OPERATION => "GL_INVALID_FRAMEBUFFER_OPERATION".into(),
        other => format!("0x{other:04X}"),
    }
}

/// Drain and log every pending `glGetError` (there can be more than one),
/// once per distinct `site` (`warned` is [`Renderer::gl_error_warned`]).
/// Only called after the rare, state-establishing uploads in
/// [`Renderer::upload`] -- never in the per-triangle draw loop.
fn check_upload_errors(gl: &Fns, warned: &mut std::collections::HashSet<&'static str>, site: &'static str) {
    loop {
        let code = unsafe { (gl.get_error)() };
        if code == GL_NO_ERROR {
            return;
        }
        if warned.insert(site) {
            crate::log(&format!("display: GL error {} after {site}", gl_error_name(code)));
        }
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryA(name: *const c_char) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
}
#[cfg(not(windows))]
use crate::xp::{GetProcAddress, LoadLibraryA};

const GL_HANDLE_TYPE_D3D11_IMAGE_EXT: c_uint = 0x958B;
const GL_DEDICATED_MEMORY_OBJECT_EXT: c_uint = 0x9581;
const GL_TEXTURE_SWIZZLE_R: c_uint = 0x8E42;
const GL_TEXTURE_SWIZZLE_B: c_uint = 0x8E44;
const GL_RED: c_int = 0x1903;
const GL_BLUE: c_int = 0x1905;

#[derive(Clone, Copy)]
struct Interop {
    create_memory_objects: unsafe extern "system" fn(c_int, *mut c_uint),
    delete_memory_objects: unsafe extern "system" fn(c_int, *const c_uint),
    memory_object_parameteriv: unsafe extern "system" fn(c_uint, c_uint, *const c_int),
    import_memory_win32_name: unsafe extern "system" fn(c_uint, u64, c_uint, *const c_void),
    tex_storage_mem_2d: unsafe extern "system" fn(c_uint, c_int, c_uint, c_int, c_int, c_uint, u64),
    acquire_keyed_mutex: unsafe extern "system" fn(c_uint, u64, c_uint) -> u8,
    release_keyed_mutex: unsafe extern "system" fn(c_uint, u64) -> u8,
    copy_image_sub_data: unsafe extern "system" fn(c_uint, c_uint, c_int, c_int, c_int, c_int, c_uint, c_uint, c_int, c_int, c_int, c_int, c_int, c_int, c_int),
}

impl Interop {
    unsafe fn load() -> Option<Self> {
        let dll = CString::new("opengl32.dll").ok()?;
        let module = LoadLibraryA(dll.as_ptr());
        if module.is_null() {
            return None;
        }
        let wgl = GetProcAddress(module, c"wglGetProcAddress".as_ptr());
        if wgl.is_null() {
            return None;
        }
        let wgl: unsafe extern "system" fn(*const c_char) -> *mut c_void = std::mem::transmute(wgl);
        let find = |name: &std::ffi::CStr| -> Option<*mut c_void> {
            let p = wgl(name.as_ptr());
            ((p as isize) < -1 || (p as isize) > 3).then_some(p)
        };
        Some(Self {
            create_memory_objects: std::mem::transmute(find(c"glCreateMemoryObjectsEXT")?),
            delete_memory_objects: std::mem::transmute(find(c"glDeleteMemoryObjectsEXT")?),
            memory_object_parameteriv: std::mem::transmute(find(c"glMemoryObjectParameterivEXT")?),
            import_memory_win32_name: std::mem::transmute(find(c"glImportMemoryWin32NameEXT")?),
            tex_storage_mem_2d: std::mem::transmute(find(c"glTexStorageMem2DEXT")?),
            acquire_keyed_mutex: std::mem::transmute(find(c"glAcquireKeyedMutexWin32EXT")?),
            release_keyed_mutex: std::mem::transmute(find(c"glReleaseKeyedMutexWin32EXT")?),
            copy_image_sub_data: std::mem::transmute(find(c"glCopyImageSubData")?),
        })
    }
}

#[derive(Clone, Copy)]
struct NativeSync {
    fence_sync: unsafe extern "system" fn(c_uint, c_uint) -> *mut c_void,
    wait_sync: unsafe extern "system" fn(*mut c_void, c_uint, u64),
    delete_sync: unsafe extern "system" fn(*mut c_void),
    copy_image_sub_data: unsafe extern "system" fn(c_uint, c_uint, c_int, c_int, c_int, c_int, c_uint, c_uint, c_int, c_int, c_int, c_int, c_int, c_int, c_int),
}

impl NativeSync {
    unsafe fn load() -> Option<Self> {
        let dll = CString::new("opengl32.dll").ok()?;
        let module = LoadLibraryA(dll.as_ptr());
        if module.is_null() {
            return None;
        }
        let wgl = GetProcAddress(module, c"wglGetProcAddress".as_ptr());
        if wgl.is_null() {
            return None;
        }
        let wgl: unsafe extern "system" fn(*const c_char) -> *mut c_void = std::mem::transmute(wgl);
        let find = |name: &std::ffi::CStr| -> Option<*mut c_void> {
            let p = wgl(name.as_ptr());
            ((p as isize) < -1 || (p as isize) > 3).then_some(p)
        };
        Some(Self {
            fence_sync: std::mem::transmute(find(c"glFenceSync")?),
            wait_sync: std::mem::transmute(find(c"glWaitSync")?),
            delete_sync: std::mem::transmute(find(c"glDeleteSync")?),
            copy_image_sub_data: std::mem::transmute(find(c"glCopyImageSubData")?),
        })
    }
}

struct SharedImport {
    generation: u64,
    memory: c_uint,
    texture: c_int,
    size: (u32, u32),
}

macro_rules! gl_functions {
    ($($field:ident: $name:literal => fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        #[allow(non_snake_case)]
        struct Fns {
            $($field: unsafe extern "system" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Fns {
            /// Every function, from opengl32.dll or, for those it does not
            /// export (anything past OpenGL 1.1), from the current context.
            unsafe fn load() -> Result<Self, String> {
                let dll = CString::new("opengl32.dll").expect("no nul");
                let module = LoadLibraryA(dll.as_ptr());
                if module.is_null() {
                    return Err("opengl32.dll is not loaded".into());
                }
                let wgl = GetProcAddress(module, c"wglGetProcAddress".as_ptr());
                let wgl: Option<unsafe extern "system" fn(*const c_char) -> *mut c_void> =
                    (!wgl.is_null()).then(|| std::mem::transmute(wgl));
                let find = |name: &str| -> Result<*mut c_void, String> {
                    let c = CString::new(name).expect("no nul");
                    let mut p = GetProcAddress(module, c.as_ptr());
                    if p.is_null() {
                        if let Some(wgl) = wgl {
                            p = wgl(c.as_ptr());
                        }
                    }
                    // wglGetProcAddress may answer 1, 2, 3 or -1 for "no".
                    if (p as isize) >= -1 && (p as isize) <= 3 {
                        return Err(format!("OpenGL has no {name}"));
                    }
                    Ok(p)
                };
                Ok(Fns { $($field: std::mem::transmute::<*mut c_void, unsafe extern "system" fn($($arg),*) $(-> $ret)?>(find($name)?),)* })
            }
        }
    };
}

gl_functions! {
    gen_queries: "glGenQueries" => fn(c_int, *mut c_uint);
    query_counter: "glQueryCounter" => fn(c_uint, c_uint);
    get_query_object_iv: "glGetQueryObjectiv" => fn(c_uint, c_uint, *mut c_int);
    get_query_object_ui64v: "glGetQueryObjectui64v" => fn(c_uint, c_uint, *mut u64);
    enable: "glEnable" => fn(c_uint);
    disable: "glDisable" => fn(c_uint);
    is_enabled: "glIsEnabled" => fn(c_uint) -> u8;
    get_integer: "glGetIntegerv" => fn(c_uint, *mut c_int);
    viewport: "glViewport" => fn(c_int, c_int, c_int, c_int);
    scissor: "glScissor" => fn(c_int, c_int, c_int, c_int);
    clear: "glClear" => fn(c_uint);
    clear_color: "glClearColor" => fn(f32, f32, f32, f32);
    clear_stencil: "glClearStencil" => fn(c_int);
    color_mask: "glColorMask" => fn(u8, u8, u8, u8);
    stencil_func: "glStencilFunc" => fn(c_uint, c_int, c_uint);
    stencil_op: "glStencilOp" => fn(c_uint, c_uint, c_uint);
    stencil_mask: "glStencilMask" => fn(c_uint);
    blend_func: "glBlendFunc" => fn(c_uint, c_uint);
    blend_func_separate: "glBlendFuncSeparate" => fn(c_uint, c_uint, c_uint, c_uint);
    matrix_mode: "glMatrixMode" => fn(c_uint);
    push_matrix: "glPushMatrix" => fn();
    pop_matrix: "glPopMatrix" => fn();
    load_identity: "glLoadIdentity" => fn();
    ortho: "glOrtho" => fn(f64, f64, f64, f64, f64, f64);
    scale: "glScaled" => fn(f64, f64, f64);
    color4f: "glColor4f" => fn(f32, f32, f32, f32);
    enable_client_state: "glEnableClientState" => fn(c_uint);
    disable_client_state: "glDisableClientState" => fn(c_uint);
    vertex_pointer: "glVertexPointer" => fn(c_int, c_uint, c_int, *const c_void);
    tex_coord_pointer: "glTexCoordPointer" => fn(c_int, c_uint, c_int, *const c_void);
    color_pointer: "glColorPointer" => fn(c_int, c_uint, c_int, *const c_void);
    draw_arrays: "glDrawArrays" => fn(c_uint, c_int, c_int);
    tex_image: "glTexImage2D" => fn(c_uint, c_int, c_int, c_int, c_int, c_int, c_uint, c_uint, *const c_void);
    tex_sub_image: "glTexSubImage2D" => fn(c_uint, c_int, c_int, c_int, c_int, c_int, c_uint, c_uint, *const c_void);
    tex_parameter: "glTexParameteri" => fn(c_uint, c_uint, c_int);
    tex_env: "glTexEnvi" => fn(c_uint, c_uint, c_int);
    pixel_store: "glPixelStorei" => fn(c_uint, c_int);
    delete_textures: "glDeleteTextures" => fn(c_int, *const c_uint);
    use_program: "glUseProgram" => fn(c_uint);
    bind_vertex_array: "glBindVertexArray" => fn(c_uint);
    gen_buffers: "glGenBuffers" => fn(c_int, *mut c_uint);
    bind_buffer: "glBindBuffer" => fn(c_uint, c_uint);
    buffer_data: "glBufferData" => fn(c_uint, isize, *const c_void, c_uint);
    delete_buffers: "glDeleteBuffers" => fn(c_int, *const c_uint);
    gen_framebuffers: "glGenFramebuffers" => fn(c_int, *mut c_uint);
    bind_framebuffer: "glBindFramebuffer" => fn(c_uint, c_uint);
    delete_framebuffers: "glDeleteFramebuffers" => fn(c_int, *const c_uint);
    gen_renderbuffers: "glGenRenderbuffers" => fn(c_int, *mut c_uint);
    bind_renderbuffer: "glBindRenderbuffer" => fn(c_uint, c_uint);
    delete_renderbuffers: "glDeleteRenderbuffers" => fn(c_int, *const c_uint);
    renderbuffer_storage_multisample: "glRenderbufferStorageMultisample" => fn(c_uint, c_int, c_uint, c_int, c_int);
    framebuffer_renderbuffer: "glFramebufferRenderbuffer" => fn(c_uint, c_uint, c_uint, c_uint);
    check_framebuffer_status: "glCheckFramebufferStatus" => fn(c_uint) -> c_uint;
    blit_framebuffer: "glBlitFramebuffer" => fn(c_int, c_int, c_int, c_int, c_int, c_int, c_int, c_int, c_uint, c_uint);
    get_error: "glGetError" => fn() -> c_uint;
    // Core since GL 3.0 / ARB_framebuffer_object, the same feature that
    // brought glGenFramebuffers and glBlitFramebuffer above: a driver that
    // loaded those (this renderer already requires them for the
    // multisampled framebuffer) has this too.
    generate_mipmap: "glGenerateMipmap" => fn(c_uint);
    read_pixels: "glReadPixels" => fn(c_int, c_int, c_int, c_int, c_uint, c_uint, *mut c_void);
    create_shader: "glCreateShader" => fn(c_uint) -> c_uint;
    shader_source: "glShaderSource" => fn(c_uint, c_int, *const *const c_char, *const c_int);
    compile_shader: "glCompileShader" => fn(c_uint);
    get_shader_iv: "glGetShaderiv" => fn(c_uint, c_uint, *mut c_int);
    create_program: "glCreateProgram" => fn() -> c_uint;
    attach_shader: "glAttachShader" => fn(c_uint, c_uint);
    link_program: "glLinkProgram" => fn(c_uint);
    get_program_iv: "glGetProgramiv" => fn(c_uint, c_uint, *mut c_int);
    get_uniform_location: "glGetUniformLocation" => fn(c_uint, *const c_char) -> c_int;
    uniform1i: "glUniform1i" => fn(c_int, c_int);
    uniform1f: "glUniform1f" => fn(c_int, f32);
    uniform2f: "glUniform2f" => fn(c_int, f32, f32);
    uniform3f: "glUniform3f" => fn(c_int, f32, f32, f32);
    framebuffer_texture_2d: "glFramebufferTexture2D" => fn(c_uint, c_uint, c_uint, c_uint, c_int);
}

const GL_VERTEX_SHADER: c_uint = 0x8B31;
const GL_FRAGMENT_SHADER: c_uint = 0x8B30;
const GL_COMPILE_STATUS: c_uint = 0x8B81;
const GL_LINK_STATUS: c_uint = 0x8B82;

const SHARPEN_VERTEX: &str = "#version 120
void main() {
    gl_TexCoord[0] = gl_MultiTexCoord0;
    gl_FrontColor = gl_Color;
    gl_Position = ftransform();
}
";

const SHARPEN_FRAGMENT: &str = "#version 120
uniform sampler2D tex;
uniform vec2 texel;
uniform float amount;
uniform float glow;
uniform float weight;
uniform float sheen;
uniform float lod;
uniform vec2 pixel;
void main() {
    vec2 uv = gl_TexCoord[0].xy;
    vec2 reach = texel;
    float tap = 0.0;
    vec4 c;
    if (lod > 0.0) {
        reach = pixel * 0.5;
        tap = max(lod - 0.5, 0.0);
        c = (texture2D(tex, uv + pixel * vec2(-0.125, -0.375), tap) + texture2D(tex, uv + pixel * vec2(0.375, -0.125), tap)
            + texture2D(tex, uv + pixel * vec2(0.125, 0.375), tap) + texture2D(tex, uv + pixel * vec2(-0.375, 0.125), tap)) * 0.25;
    } else {
        c = texture2D(tex, uv);
    }
    vec4 n = texture2D(tex, uv - vec2(0.0, reach.y), tap);
    vec4 s = texture2D(tex, uv + vec2(0.0, reach.y), tap);
    vec4 e = texture2D(tex, uv + vec2(reach.x, 0.0), tap);
    vec4 w = texture2D(tex, uv - vec2(reach.x, 0.0), tap);
    vec4 lo = min(c, min(min(n, s), min(e, w)));
    vec4 hi = max(c, max(max(n, s), max(e, w)));
    vec4 blur = (n + s + e + w) * 0.25;
    vec4 sharp = clamp(c + (c - blur) * (amount * 1.5), lo, hi);
    sharp = max(sharp, hi * weight);
    vec2 d = texel * 6.0;
    vec3 lw = vec3(0.299, 0.587, 0.114);
    vec4 wide = texture2D(tex, uv, 4.0);
    vec4 halo = (texture2D(tex, uv + vec2(d.x, d.y), 2.5) + texture2D(tex, uv + vec2(-d.x, d.y), 2.5)
        + texture2D(tex, uv + vec2(d.x, -d.y), 2.5) + texture2D(tex, uv + vec2(-d.x, -d.y), 2.5)) * 0.15
        + (wide + texture2D(tex, uv + vec2(d.x * 2.0, 0.0), 4.0) + texture2D(tex, uv - vec2(d.x * 2.0, 0.0), 4.0)) * 0.1333;
    halo *= 1.0 - smoothstep(0.2, 0.5, dot(wide.rgb, lw));
    vec4 outc = sharp + halo * glow * (1.0 - dot(sharp.rgb, lw));
    float lum = dot(outc.rgb, lw);
    float edge = 1.0 - smoothstep(0.0, 0.12, min(min(uv.x, 1.0 - uv.x), min(uv.y, 1.0 - uv.y)));
    float dark = 1.0 - smoothstep(0.0, 0.2, lum);
    outc.rgb += vec3(1.0, 0.985, 0.955) * (0.006 + 0.006 * edge) * dark * sheen * 2.0;
    outc.a = max(outc.a, max(outc.r, max(outc.g, outc.b)));
    outc = clamp(outc, 0.0, 1.0);
    outc.rgb = min(outc.rgb, vec3(outc.a));
    gl_FragColor = outc * gl_Color;
}
";

/// An owned colour target the HUD page is drawn into before it is composited onto the scene.
#[derive(Default)]
pub struct HudTarget {
    framebuffer: c_uint,
    texture: c_int,
    size: (u32, u32),
}

#[derive(Clone, Copy)]
struct HudProgram {
    program: c_uint,
    tex: c_int,
    texel: c_int,
    color: c_int,
    glow: c_int,
}

const HUD_VERTEX: &str = "#version 120
void main() {
    gl_TexCoord[0] = gl_MultiTexCoord0;
    gl_FrontColor = gl_Color;
    gl_Position = ftransform();
}
";

const HUD_FRAGMENT: &str = "#version 120
uniform sampler2D tex;
uniform vec2 texel;
uniform vec3 color;
uniform float glow;
void main() {
    vec2 uv = gl_TexCoord[0].xy;
    float core = texture2D(tex, uv).g;
    float halo = 0.0;
    for (int i = 0; i < 12; i++) {
        float a = float(i) * 0.5235988;
        vec2 d = vec2(cos(a), sin(a)) * texel;
        halo += texture2D(tex, uv + d * 1.6).g + texture2D(tex, uv + d * 3.4).g * 0.5;
    }
    float v = core + glow * halo / 18.0;
    gl_FragColor = vec4(color * v * gl_Color.a, 1.0);
}
";

/// The sharpening pass [`Renderer::draw_bridge`] draws XPHFBW's pixels with:
/// a clamped unsharp mask, so text edges steepen without ringing past their
/// neighbours' range, on CEF's premultiplied pixels.
#[derive(Clone, Copy)]
struct Sharpen {
    program: c_uint,
    tex: c_int,
    texel: c_int,
    amount: c_int,
    glow: c_int,
    weight: c_int,
    sheen: c_int,
    lod: c_int,
    pixel: c_int,
}

/// The multisampled framebuffer every screen draws through.
struct Multisample {
    framebuffer: c_uint,
    colour: c_uint,
    depth_stencil: c_uint,
}

/// A screen's GPU copy of its mesh.
#[derive(Default)]
pub struct ScreenGpu {
    buffer: c_uint,
    /// The mesh (by its stream number) the buffer holds.
    uploaded: u64,
    gradients: c_int,
    gradient_rows: usize,
}

/// A screen's GPU copy of XPHFBW's pixels (docs/briefs/xphfbw-js-bridge.md
/// rule 6), drawn as a textured quad in place of the tessellated mesh while
/// this screen's own bridge switch-over is active (`Displays::bridge_active`,
/// per screen since S08).
impl BridgeGpu {
    pub fn texture(&self) -> c_int {
        self.texture
    }
}

#[derive(Default)]
pub struct BridgeGpu {
    texture: c_int,
    shared: Option<SharedImport>,
    swizzled: bool,
    /// The quad's vertex buffer (rebuilt each draw: the full screen, then
    /// one quad per dimmed region).
    buffer: c_uint,
    /// The texture's current size, so a size change is caught and uploaded
    /// whole even if the caller thinks it only has a dirty rect.
    size: (u32, u32),
}

/// A device's picture of where it is drawing, from X-Plane's state on entry.
#[derive(Clone, Copy)]
pub struct Target {
    pub framebuffer: c_uint,
    pub viewport: [c_int; 4],
}

/// GPU time of one screen's draw callbacks, from GL timestamp pairs read back a few frames later.
#[derive(Default)]
struct GpuTimes {
    pairs: Vec<(c_uint, c_uint, bool)>,
    open: Option<usize>,
    ns: u64,
    n: u32,
}

pub struct Renderer {
    /// Mip bias for the next `draw_bridge`: `view_scale.rs`, how far this screen is minified on the monitor.
    pub prefilter_lod: f32,
    gpu_times: HashMap<usize, GpuTimes>,
    gl: Fns,
    xp: AvionicsApi,
    multisample: Option<Multisample>,
    atlas_texture: c_int,
    atlas_size: u32,
    atlas_generation: u64,
    /// X-Plane texture ids for images, by image index.
    images: Vec<c_int>,
    /// Native images by id: texture, and the generation and size it holds.
    natives: HashMap<String, (c_int, u64, u32, u32)>,
    /// This frame's texture for each of the mesh's native images, 0 for
    /// one with nothing to draw.
    native_textures: Vec<c_int>,
    /// Draw calls in the last frame, for the statistics.
    pub draw_calls: usize,
    stencil_warned: bool,
    /// Call sites `check_upload_errors` has already logged a GL error for,
    /// so a state that keeps failing (e.g. every frame's mesh re-upload on
    /// a driver that rejects it) is reported once, not forever.
    gl_error_warned: std::collections::HashSet<&'static str>,
    /// Whether X-Plane's device target is itself multisampled, asked once:
    /// a multisampled framebuffer cannot be resolved into one, so the mesh
    /// is then drawn straight into it with its own samples.
    target_multisampled: Option<bool>,
    target_stencil: Option<(c_uint, bool)>,
    /// Screens [`Renderer::draw_bridge`] has already logged the "no texture
    /// yet" warning for, so a screen stuck that way is reported once, not
    /// every frame.
    bridge_no_texture_warned: std::collections::HashSet<&'static str>,
    /// Built on first use; `Some(None)` once building it failed.
    sharpen: Option<Option<Sharpen>>,
    hud_program: Option<Option<HudProgram>>,
    interop: Option<Option<Interop>>,
    native_sync: Option<Option<NativeSync>>,
}

/// Where in the target a region of the screen lands.
#[derive(Clone, Copy)]
struct Frame {
    /// The region, in device pixels with y down: x0, y0, x1, y1.
    region: [i32; 4],
    /// The target pixel (bottom-left origin) the region's bottom-left maps to.
    origin: [i32; 2],
    /// Scissoring has to stay on, because the target is shared.
    always_scissor: bool,
    /// Target pixels per region pixel.
    scale: (f64, f64),
}

impl Frame {
    fn scissor_box(&self, rect: Option<[i32; 4]>) -> [c_int; 4] {
        let [rx0, ry0, rx1, ry1] = self.region;
        let [x0, y0, x1, y1] = rect.unwrap_or(self.region);
        let (x0, y0, x1, y1) = (x0.max(rx0), y0.max(ry0), x1.min(rx1), y1.min(ry1));
        if x0 >= x1 || y0 >= y1 {
            return [self.origin[0], self.origin[1], 0, 0];
        }
        let (sx, sy) = self.scale;
        let left = (self.origin[0] as f64 + (x0 - rx0) as f64 * sx).round() as c_int;
        let right = (self.origin[0] as f64 + (x1 - rx0) as f64 * sx).round() as c_int;
        let bottom = (self.origin[1] as f64 + (ry1 - y1) as f64 * sy).round() as c_int;
        let top = (self.origin[1] as f64 + (ry1 - y0) as f64 * sy).round() as c_int;
        [left, bottom, right - left, top - bottom]
    }
}

impl Renderer {
    /// Bind OpenGL. Must run with X-Plane's plugin context current, which is
    /// the case inside drawing callbacks.
    ///
    /// Before this, `xphfbw.displaySafeMode` (the app's "Use display safe
    /// mode" checkbox, app/ui/index.html) was read by nobody on the plugin
    /// side: a pilot who ticked it because their screens stayed black or
    /// flickered saw no change at all, since nothing in this module ever
    /// looked at the setting. The multisampled framebuffer this constructor
    /// makes is the one hardware-dependent piece of the display path (an
    /// FBO with a 4x `GL_MAX_SAMPLES` renderbuffer, [`make_multisample`]);
    /// safe mode now skips it, falling back to the plain, always-available
    /// path already in [`Renderer::draw`] (the `_ =>` arm, straight into
    /// X-Plane's target with no anti-aliasing and no clip paths) -- exactly
    /// what the checkbox's own tooltip promises.
    pub fn new(xp: AvionicsApi) -> Result<Self, String> {
        let gl = unsafe { Fns::load()? };
        let safe_mode = display_safe_mode();
        let mut r = Renderer {
            gl,
            xp,
            multisample: None,
            atlas_texture: 0,
            atlas_size: 0,
            atlas_generation: 0,
            images: Vec::new(),
            natives: HashMap::new(),
            native_textures: Vec::new(),
            draw_calls: 0,
            stencil_warned: false,
            gl_error_warned: std::collections::HashSet::new(),
            target_multisampled: None,
            gpu_times: HashMap::new(),
            target_stencil: None,
            bridge_no_texture_warned: std::collections::HashSet::new(),
            prefilter_lod: 0.,
            sharpen: None,
            hud_program: None,
            interop: None,
            native_sync: None,
        };
        if safe_mode {
            crate::log("display: xphfbw.displaySafeMode is on; skipping the multisampled framebuffer (no anti-aliasing, no clip paths)");
        } else {
            r.multisample = unsafe { r.make_multisample() };
        }
        Ok(r)
    }

    /// The viewport X-Plane set for the device being drawn.
    pub fn viewport(&self) -> [c_int; 4] {
        let mut v = [0; 4];
        unsafe { (self.gl.get_integer)(GL_VIEWPORT, v.as_mut_ptr()) };
        v
    }

    pub fn anti_aliased(&self) -> bool {
        self.multisample.is_some()
    }

    pub fn read_target(&mut self, target: Target) -> Result<(u32, u32, Vec<u8>), String> {
        let [vx, vy, vw, vh] = target.viewport;
        if vw <= 0 || vh <= 0 {
            return Err(format!("viewport {vw}x{vh}"));
        }
        let row = vw as usize * 4;
        let mut rgba = vec![0u8; row * vh as usize];
        unsafe {
            let gl = &self.gl;
            for _ in 0..16 {
                if (gl.get_error)() == GL_NO_ERROR {
                    break;
                }
            }
            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, target.framebuffer);
            (gl.pixel_store)(GL_PACK_ALIGNMENT, 4);
            (gl.read_pixels)(vx, vy, vw, vh, GL_RGBA, GL_UNSIGNED_BYTE, rgba.as_mut_ptr().cast());
            let code = (gl.get_error)();
            if code != GL_NO_ERROR {
                return Err(gl_error_name(code));
            }
        }
        let flipped = rgba.chunks_exact(row).rev().flatten().copied().collect();
        Ok((vw as u32, vh as u32, flipped))
    }

    unsafe fn make_multisample(&self) -> Option<Multisample> {
        let gl = &self.gl;
        let mut max = 0;
        (gl.get_integer)(GL_MAX_SAMPLES, &mut max);
        if max < SAMPLES {
            return None;
        }
        let (mut framebuffer, mut colour, mut depth_stencil) = (0, 0, 0);
        (gl.gen_framebuffers)(1, &mut framebuffer);
        (gl.gen_renderbuffers)(1, &mut colour);
        (gl.gen_renderbuffers)(1, &mut depth_stencil);
        (gl.bind_renderbuffer)(GL_RENDERBUFFER, colour);
        (gl.renderbuffer_storage_multisample)(GL_RENDERBUFFER, SAMPLES, GL_RGBA8 as c_uint, TILE, TILE);
        (gl.bind_renderbuffer)(GL_RENDERBUFFER, depth_stencil);
        (gl.renderbuffer_storage_multisample)(GL_RENDERBUFFER, SAMPLES, GL_DEPTH24_STENCIL8, TILE, TILE);
        (gl.bind_renderbuffer)(GL_RENDERBUFFER, 0);
        (gl.bind_framebuffer)(GL_FRAMEBUFFER, framebuffer);
        (gl.framebuffer_renderbuffer)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, colour);
        (gl.framebuffer_renderbuffer)(GL_FRAMEBUFFER, GL_DEPTH_STENCIL_ATTACHMENT, GL_RENDERBUFFER, depth_stencil);
        let complete = (gl.check_framebuffer_status)(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE;
        let ms = Multisample { framebuffer, colour, depth_stencil };
        if complete {
            Some(ms)
        } else {
            Self::delete_multisample(gl, &ms);
            None
        }
    }

    fn delete_multisample(gl: &Fns, ms: &Multisample) {
        unsafe {
            (gl.delete_framebuffers)(1, &ms.framebuffer);
            (gl.delete_renderbuffers)(1, &ms.colour);
            (gl.delete_renderbuffers)(1, &ms.depth_stencil);
        }
    }

    /// Free everything this renderer made.
    pub fn release(&mut self, screens: &mut [&mut ScreenGpu], bridges: &mut [&mut BridgeGpu]) {
        let gl = &self.gl;
        unsafe {
            if let Some(ms) = self.multisample.take() {
                Self::delete_multisample(gl, &ms);
            }
            let mut textures: Vec<c_uint> = self.images.drain(..).map(|t| t as c_uint).collect();
            textures.extend(self.natives.drain().map(|(_, (t, ..))| t as c_uint));
            if self.atlas_texture != 0 {
                textures.push(self.atlas_texture as c_uint);
            }
            for s in screens.iter_mut() {
                if s.buffer != 0 {
                    (gl.delete_buffers)(1, &s.buffer);
                }
                if s.gradients != 0 {
                    textures.push(s.gradients as c_uint);
                }
                **s = ScreenGpu::default();
            }
            for b in bridges.iter_mut() {
                if b.buffer != 0 {
                    (gl.delete_buffers)(1, &b.buffer);
                }
                if b.texture != 0 {
                    textures.push(b.texture as c_uint);
                }
                **b = BridgeGpu::default();
            }
            if !textures.is_empty() {
                (gl.delete_textures)(textures.len() as c_int, textures.as_ptr());
            }
        }
        self.atlas_texture = 0;
    }

    fn new_texture(&self) -> c_int {
        let mut id = 0;
        unsafe { (self.xp.generate_textures)(&mut id, 1) };
        id
    }

    unsafe fn texture_parameters(&self, filter: c_int, levels: c_int) {
        let gl = &self.gl;
        (gl.tex_parameter)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, filter);
        (gl.tex_parameter)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        (gl.tex_parameter)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        (gl.tex_parameter)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        (gl.tex_parameter)(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, levels - 1);
    }

    /// Bring the atlas, the images the mesh uses and the mesh itself up to
    /// date on the GPU.
    unsafe fn upload(&mut self, gpu: &mut ScreenGpu, stream: u64, mesh: &Mesh, res: &mut Resources, natives: &[Option<Arc<NativeImage>>]) {
        let gl = &self.gl;
        (gl.pixel_store)(GL_UNPACK_ALIGNMENT, 1);
        let atlas = &mut res.atlas;
        if self.atlas_texture == 0 || self.atlas_size != atlas.size || self.atlas_generation != atlas.generation {
            if self.atlas_texture == 0 {
                self.atlas_texture = self.new_texture();
            }
            (self.xp.bind_texture)(self.atlas_texture, 0);
            (gl.tex_image)(GL_TEXTURE_2D, 0, GL_ALPHA8, atlas.size as c_int, atlas.size as c_int, 0, GL_ALPHA, GL_UNSIGNED_BYTE, atlas.pixels.as_ptr().cast());
            self.texture_parameters(GL_LINEAR, 1);
            self.atlas_size = atlas.size;
            self.atlas_generation = atlas.generation;
            atlas.dirty = None;
            // The call most worth checking in this file: GL_ALPHA / GL_ALPHA8
            // is a single-channel legacy format removed from core-profile
            // OpenGL. If X-Plane ever hands a plugin a core context instead
            // of the compatibility one this whole renderer assumes, this is
            // where it would first show -- as a rejected atlas upload, while
            // the images and native textures below (GL_RGBA8, never legacy)
            // keep working. That alone would not explain text surviving
            // (glyphs share this same atlas texture), but a texture that
            // fails here can be left holding whatever the driver had bound
            // before it, so partial/stale sampling is not ruled out either;
            // this makes that visible instead of silent.
            check_upload_errors(gl, &mut self.gl_error_warned, "atlas texture upload (GL_ALPHA8)");
        } else if let Some((first, last)) = atlas.dirty.take() {
            (self.xp.bind_texture)(self.atlas_texture, 0);
            let row = (first * atlas.size) as usize;
            (gl.tex_sub_image)(
                GL_TEXTURE_2D,
                0,
                0,
                first as c_int,
                atlas.size as c_int,
                (last - first) as c_int,
                GL_ALPHA,
                GL_UNSIGNED_BYTE,
                atlas.pixels[row..].as_ptr().cast(),
            );
            check_upload_errors(gl, &mut self.gl_error_warned, "atlas texture sub-upload (GL_ALPHA)");
        }
        for batch in &mesh.batches {
            if let Paint::Image(i) = batch.paint {
                if self.images.len() <= i {
                    self.images.resize(i + 1, 0);
                }
                if self.images[i] == 0 {
                    let picture = &res.images.pictures[i];
                    self.images[i] = self.new_texture();
                    (self.xp.bind_texture)(self.images[i], 0);
                    for (level, (w, h, pixels)) in picture.levels.iter().enumerate() {
                        (gl.tex_image)(GL_TEXTURE_2D, level as c_int, GL_RGBA8, *w as c_int, *h as c_int, 0, GL_RGBA, GL_UNSIGNED_BYTE, pixels.as_ptr().cast());
                    }
                    self.texture_parameters(GL_LINEAR_MIPMAP_LINEAR, picture.levels.len() as c_int);
                    check_upload_errors(gl, &mut self.gl_error_warned, "picture texture upload");
                }
            }
        }
        self.native_textures.clear();
        for (id, image) in mesh.natives.iter().zip(natives) {
            let Some(image) = image.as_ref().filter(|i| i.width > 0 && i.height > 0 && i.rgba.len() >= (i.width * i.height * 4) as usize) else {
                self.native_textures.push(0);
                continue;
            };
            let (texture, generation, w, h) = match self.natives.get(id) {
                Some(e) => *e,
                None => (self.new_texture(), u64::MAX, 0, 0),
            };
            if generation != image.generation || w != image.width || h != image.height {
                (self.xp.bind_texture)(texture, 0);
                let (iw, ih) = (image.width as c_int, image.height as c_int);
                if w == image.width && h == image.height {
                    (gl.tex_sub_image)(GL_TEXTURE_2D, 0, 0, 0, iw, ih, GL_RGBA, GL_UNSIGNED_BYTE, image.rgba.as_ptr().cast());
                } else {
                    (gl.tex_image)(GL_TEXTURE_2D, 0, GL_RGBA8, iw, ih, 0, GL_RGBA, GL_UNSIGNED_BYTE, image.rgba.as_ptr().cast());
                    self.texture_parameters(GL_LINEAR, 1);
                }
                self.natives.insert(id.clone(), (texture, image.generation, image.width, image.height));
                check_upload_errors(gl, &mut self.gl_error_warned, "native image texture upload");
            }
            self.native_textures.push(texture);
        }
        if gpu.uploaded != stream {
            if gpu.buffer == 0 {
                (gl.gen_buffers)(1, &mut gpu.buffer);
            }
            (gl.bind_buffer)(GL_ARRAY_BUFFER, gpu.buffer);
            (gl.buffer_data)(
                GL_ARRAY_BUFFER,
                std::mem::size_of_val(&mesh.vertices[..]) as isize,
                mesh.vertices.as_ptr().cast(),
                GL_DYNAMIC_DRAW,
            );
            // Every vertex the mesh has -- text glyph quads and flat-colour
            // fill/stroke triangles alike -- lands in this one buffer.
            // GL_OUT_OF_MEMORY or a rejected size here would drop both
            // together, not one selectively; still worth knowing.
            check_upload_errors(gl, &mut self.gl_error_warned, "vertex buffer upload");
            (gl.bind_buffer)(GL_ARRAY_BUFFER, 0);
            gpu.gradient_rows = mesh.gradients.len();
            if !mesh.gradients.is_empty() {
                if gpu.gradients == 0 {
                    gpu.gradients = self.new_texture();
                }
                (self.xp.bind_texture)(gpu.gradients, 0);
                let rows = mesh.gradients.concat();
                (gl.tex_image)(GL_TEXTURE_2D, 0, GL_RGBA8, RAMP as c_int, mesh.gradients.len() as c_int, 0, GL_RGBA, GL_UNSIGNED_BYTE, rows.as_ptr().cast());
                self.texture_parameters(GL_LINEAR, 1);
                check_upload_errors(gl, &mut self.gl_error_warned, "gradient ramp texture upload");
            }
            gpu.uploaded = stream;
        }
        (gl.pixel_store)(GL_UNPACK_ALIGNMENT, 4);
    }

    /// Draw `region` of a screen's mesh (device pixels, y down) into the
    /// viewport X-Plane gave the device, each dimming region at its
    /// `brightness`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        gpu: &mut ScreenGpu,
        stream: u64,
        mesh: &Mesh,
        steps: &[Step],
        region: [i32; 4],
        target: Target,
        brightness: &[f32],
        natives: &[Option<Arc<NativeImage>>],
        res: &mut Resources,
    ) {
        self.draw_calls = 0;
        unsafe {
            let gl = &self.gl;
            let scissor_was_on = (gl.is_enabled)(GL_SCISSOR_TEST) != 0;
            (gl.use_program)(0);
            // macOS gives plugins a legacy 2.1 context (X-Plane draws with Metal), where the
            // core glBindVertexArray only raises GL_INVALID_OPERATION and there is no VAO to undo.
            if !cfg!(target_os = "macos") {
                (gl.bind_vertex_array)(0);
            }
            self.upload(gpu, stream, mesh, res, natives);
            let gl = &self.gl;
            (self.xp.set_graphics_state)(0, 0, 0, 0, 1, 0, 0);
            (gl.blend_func_separate)(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA, GL_ONE, GL_ONE_MINUS_SRC_ALPHA);
            (gl.tex_env)(GL_TEXTURE_ENV, GL_TEXTURE_ENV_MODE, GL_MODULATE);
            for mode in [GL_PROJECTION, GL_MODELVIEW, GL_TEXTURE] {
                (gl.matrix_mode)(mode);
                (gl.push_matrix)();
                (gl.load_identity)();
            }
            (gl.bind_buffer)(GL_ARRAY_BUFFER, gpu.buffer);
            let stride = std::mem::size_of::<Vertex>() as c_int;
            (gl.enable_client_state)(GL_VERTEX_ARRAY);
            (gl.enable_client_state)(GL_COLOR_ARRAY);
            (gl.enable_client_state)(GL_TEXTURE_COORD_ARRAY);
            (gl.vertex_pointer)(2, GL_FLOAT, stride, std::ptr::null());
            (gl.tex_coord_pointer)(2, GL_FLOAT, stride, 8 as *const c_void);
            (gl.color_pointer)(4, GL_UNSIGNED_BYTE, stride, 16 as *const c_void);

            let [vx, vy, vw, vh] = target.viewport;
            let target_multisampled = *self.target_multisampled.get_or_insert_with(|| {
                let mut samples = 0;
                (gl.get_integer)(GL_SAMPLES, &mut samples);
                samples > 1
            });
            let [rx0, ry0, rx1, ry1] = region;
            let (sx, sy) = (vw as f64 / (rx1 - rx0).max(1) as f64, vh as f64 / (ry1 - ry0).max(1) as f64);
            match &self.multisample {
                Some(ms) if !target_multisampled && (sx - 1.).abs() < 1e-9 && (sy - 1.).abs() < 1e-9 => {
                    let framebuffer = ms.framebuffer;
                    (gl.bind_framebuffer)(GL_FRAMEBUFFER, framebuffer);
                    let mut ty = ry0;
                    while ty < ry1 {
                        let th = TILE.min(ry1 - ty);
                        let mut tx = rx0;
                        while tx < rx1 {
                            let tw = TILE.min(rx1 - tx);
                            let frame = Frame { region: [tx, ty, tx + tw, ty + th], origin: [0, 0], always_scissor: false, scale: (1., 1.) };
                            let gl = &self.gl;
                            (gl.bind_framebuffer)(GL_FRAMEBUFFER, framebuffer);
                            (gl.viewport)(0, 0, tw, th);
                            (gl.disable)(GL_SCISSOR_TEST);
                            (gl.clear_color)(0., 0., 0., 1.);
                            (gl.clear_stencil)(0);
                            (gl.stencil_mask)(0xff);
                            (gl.clear)(GL_COLOR_BUFFER_BIT | GL_STENCIL_BUFFER_BIT);
                            self.run(gpu, steps, frame, true, brightness);
                            let gl = &self.gl;
                            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, framebuffer);
                            (gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, target.framebuffer);
                            (gl.disable)(GL_SCISSOR_TEST);
                            let (dx, dy) = (vx + (tx - rx0), vy + (ry1 - (ty + th)));
                            (gl.blit_framebuffer)(0, 0, tw, th, dx, dy, dx + tw, dy + th, GL_COLOR_BUFFER_BIT, GL_NEAREST);
                            tx += tw;
                        }
                        ty += th;
                    }
                    (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, target.framebuffer);
                }
                _ => {
                    // Straight into X-Plane's target, scaled to its viewport.
                    let frame = Frame { region, origin: [vx, vy], always_scissor: true, scale: (sx, sy) };
                    (gl.viewport)(vx, vy, vw, vh);
                    (gl.enable)(GL_SCISSOR_TEST);
                    (gl.scissor)(vx, vy, vw, vh);
                    (gl.clear_color)(0., 0., 0., 1.);
                    (gl.clear)(GL_COLOR_BUFFER_BIT);
                    let stencil = match self.target_stencil {
                        Some((framebuffer, stencil)) if framebuffer == target.framebuffer => stencil,
                        _ => {
                            let mut bits = 0;
                            (gl.get_integer)(GL_STENCIL_BITS, &mut bits);
                            self.target_stencil = Some((target.framebuffer, bits >= 8));
                            bits >= 8
                        }
                    };
                    if !stencil && !self.stencil_warned {
                        self.stencil_warned = true;
                        crate::log("display: the screen target has no stencil buffer; clip paths are not applied");
                    }
                    // The projection maps the region onto the whole viewport,
                    // so a texture of another size scales the drawing with it.
                    self.run(gpu, steps, frame, stencil, brightness);
                }
            }

            let gl = &self.gl;
            (gl.viewport)(vx, vy, vw, vh);
            if scissor_was_on {
                (gl.enable)(GL_SCISSOR_TEST);
            } else {
                (gl.disable)(GL_SCISSOR_TEST);
            }
            (gl.disable)(GL_STENCIL_TEST);
            (gl.stencil_mask)(0xff);
            (gl.color_mask)(1, 1, 1, 1);
            (gl.blend_func)(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
            for mode in [GL_TEXTURE, GL_MODELVIEW, GL_PROJECTION] {
                (gl.matrix_mode)(mode);
                (gl.pop_matrix)();
            }
            (gl.matrix_mode)(GL_MODELVIEW);
            (gl.disable_client_state)(GL_COLOR_ARRAY);
            (gl.disable_client_state)(GL_TEXTURE_COORD_ARRAY);
            (gl.enable_client_state)(GL_VERTEX_ARRAY);
            (gl.bind_buffer)(GL_ARRAY_BUFFER, 0);
        }
    }

    /// Bring a bridge screen's texture up to date with XPHFBW's pixels
    /// (docs/briefs/xphfbw-js-bridge.md rule 6): `rects` are the regions to
    /// copy, already planned and clamped by [`super::xphfbw::plan_upload`].
    /// `pixels` is the screen's full BGRA buffer, top row first
    /// ([`crate::xphfbw_bridge::ScreenBlock::pixels`]); a size change (first
    /// use, or a resized screen) always uploads the whole thing regardless
    /// of `rects`.
    ///
    /// The texture carries a full mip chain (`GL_LINEAR_MIPMAP_LINEAR`,
    /// [`bridge_mip_levels`]), rebuilt with `glGenerateMipmap` whenever this
    /// call actually changed a pixel -- not every frame: the caller
    /// (`display::mod::draw_bridge_screen`) already only reaches this
    /// function when there is something to upload, rate-limited by the same
    /// 24 MB/frame `upload_budget` `UPLOADED_BYTES` below counts against, so
    /// gating the mipmap rebuild on that call is free.
    unsafe fn release_shared(&self, gpu: &mut BridgeGpu) {
        let (Some(shared), Some(Some(interop))) = (gpu.shared.take(), self.interop) else { return };
        let texture = shared.texture as c_uint;
        (self.gl.delete_textures)(1, &texture);
        (interop.delete_memory_objects)(1, &shared.memory);
    }

    unsafe fn import_shared(&mut self, gpu: &mut BridgeGpu, name: &str, generation: u64, width: u32, height: u32) -> Result<(), String> {
        let interop = (*self.interop.get_or_insert_with(|| Interop::load())).ok_or("this OpenGL has no GL_EXT_memory_object_win32 / GL_EXT_win32_keyed_mutex")?;
        self.release_shared(gpu);
        for _ in 0..8 {
            if (self.gl.get_error)() == 0 {
                break;
            }
        }
        let mut memory = 0;
        (interop.create_memory_objects)(1, &mut memory);
        let dedicated: c_int = 1;
        (interop.memory_object_parameteriv)(memory, GL_DEDICATED_MEMORY_OBJECT_EXT, &dedicated);
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let (pw, ph) = crate::xphfbw_bridge::ScreenBlock::gpu_padded(width, height);
        (interop.import_memory_win32_name)(memory, pw as u64 * ph as u64 * 4, GL_HANDLE_TYPE_D3D11_IMAGE_EXT, wide.as_ptr().cast());
        let texture = self.new_texture();
        (self.xp.bind_texture)(texture, 0);
        (interop.tex_storage_mem_2d)(GL_TEXTURE_2D, 1, GL_RGBA8 as c_uint, pw as c_int, ph as c_int, memory, 0);
        let error = (self.gl.get_error)();
        if error != 0 {
            let t = texture as c_uint;
            (self.gl.delete_textures)(1, &t);
            (interop.delete_memory_objects)(1, &memory);
            return Err(format!("importing {name} failed with GL error 0x{error:x}"));
        }
        gpu.shared = Some(SharedImport { generation, memory, texture, size: (width, height) });
        Ok(())
    }

    pub fn bridge_gpu_copy(&mut self, gpu: &mut BridgeGpu, name: &str, generation: u64, width: u32, height: u32) -> Result<bool, String> {
        if width == 0 || height == 0 {
            return Ok(false);
        }
        unsafe {
            if gpu.shared.as_ref().is_none_or(|s| s.generation != generation || s.size != (width, height)) {
                self.import_shared(gpu, name, generation, width, height)?;
            }
            let Some(Some(interop)) = self.interop else { return Err("no interop".into()) };
            let gl = &self.gl;
            if gpu.texture == 0 || gpu.size != (width, height) {
                if gpu.texture == 0 {
                    gpu.texture = self.new_texture();
                }
                (self.xp.bind_texture)(gpu.texture, 0);
                (gl.tex_image)(GL_TEXTURE_2D, 0, GL_RGBA8, width as c_int, height as c_int, 0, GL_BGRA, GL_UNSIGNED_BYTE, std::ptr::null());
                self.texture_parameters(GL_LINEAR_MIPMAP_LINEAR, bridge_mip_levels(width, height));
                gpu.size = (width, height);
                gpu.swizzled = false;
            }
            if !gpu.swizzled {
                (self.xp.bind_texture)(gpu.texture, 0);
                (gl.tex_parameter)(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_R, GL_BLUE);
                (gl.tex_parameter)(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_B, GL_RED);
                gpu.swizzled = true;
            }
            let shared = gpu.shared.as_ref().expect("imported above");
            if (interop.acquire_keyed_mutex)(shared.memory, 0, 0) == 0 {
                return Ok(false);
            }
            (interop.copy_image_sub_data)(
                shared.texture as c_uint,
                GL_TEXTURE_2D,
                0,
                0,
                0,
                0,
                gpu.texture as c_uint,
                GL_TEXTURE_2D,
                0,
                0,
                0,
                0,
                width as c_int,
                height as c_int,
                1,
            );
            (interop.release_keyed_mutex)(shared.memory, 0);
            (self.xp.bind_texture)(gpu.texture, 0);
            (gl.generate_mipmap)(GL_TEXTURE_2D);
        }
        Ok(true)
    }

    /// Copies a native render thread's shared texture into `gpu`'s own once
    /// `ready` has passed on the GPU, and returns the GLsync after the copy
    /// for the render thread to wait on before drawing into `source` again.
    pub fn native_gpu_copy(&mut self, gpu: &mut BridgeGpu, source: u32, ready: usize, width: u32, height: u32) -> Result<usize, String> {
        let sync = (*self.native_sync.get_or_insert_with(|| unsafe { NativeSync::load() })).ok_or("this OpenGL has no glFenceSync / glCopyImageSubData")?;
        unsafe {
            self.release_shared(gpu);
            if ready != 0 {
                (sync.wait_sync)(ready as *mut c_void, 0, u64::MAX);
                (sync.delete_sync)(ready as *mut c_void);
            }
            let gl = &self.gl;
            let fresh = gpu.texture == 0 || gpu.size != (width, height);
            if fresh {
                if gpu.texture == 0 {
                    gpu.texture = self.new_texture();
                }
                (self.xp.bind_texture)(gpu.texture, 0);
                (gl.tex_image)(GL_TEXTURE_2D, 0, GL_RGBA8, width as c_int, height as c_int, 0, GL_RGBA, GL_UNSIGNED_BYTE, std::ptr::null());
                self.texture_parameters(GL_LINEAR_MIPMAP_LINEAR, bridge_mip_levels(width, height));
                (gl.generate_mipmap)(GL_TEXTURE_2D);
                gpu.size = (width, height);
            }
            if gpu.swizzled {
                (self.xp.bind_texture)(gpu.texture, 0);
                (gl.tex_parameter)(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_R, GL_RED);
                (gl.tex_parameter)(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_B, GL_BLUE);
                gpu.swizzled = false;
            }
            (sync.copy_image_sub_data)(source, GL_TEXTURE_2D, 0, 0, 0, 0, gpu.texture as c_uint, GL_TEXTURE_2D, 0, 0, 0, 0, width as c_int, height as c_int, 1);
            if fresh {
                let error = (gl.get_error)();
                if error != 0 {
                    return Err(format!("copying native texture {source} failed with GL error 0x{error:x}"));
                }
            }
            (self.xp.bind_texture)(gpu.texture, 0);
            (gl.generate_mipmap)(GL_TEXTURE_2D);
            Ok((sync.fence_sync)(0x9117, 0) as usize)
        }
    }

    /// After a native display drew on this context: X-Plane's cached graphics
    /// state and texture bindings agree with GL again, and `gpu`
    /// shows `texture` (the display's own, with its mips built) when it drew one.
    pub fn after_native_draw(&mut self, gpu: &mut BridgeGpu, texture: u32, width: u32, height: u32) -> Result<(), String> {
        unsafe {
            self.release_shared(gpu);
            (self.xp.set_graphics_state)(1, 1, 1, 1, 1, 1, 1);
            (self.xp.set_graphics_state)(0, 0, 0, 0, 0, 0, 0);
            for unit in 0..4 {
                (self.xp.bind_texture)(1, unit);
                (self.xp.bind_texture)(0, unit);
            }
        }
        if texture != 0 {
            gpu.texture = texture as c_int;
            gpu.size = (width, height);
            gpu.swizzled = false;
        }
        Ok(())
    }

    /// Marks the start of screen `slot`'s GPU work this callback.
    pub fn gpu_begin(&mut self, slot: usize) {
        const GL_TIMESTAMP: c_uint = 0x8E28;
        const AVAILABLE: c_uint = 0x8867;
        const RESULT: c_uint = 0x8866;
        let gl = &self.gl;
        let t = self.gpu_times.entry(slot).or_default();
        for p in t.pairs.iter_mut().filter(|p| p.2) {
            let mut ready = 0;
            unsafe { (gl.get_query_object_iv)(p.1, AVAILABLE, &mut ready) };
            if ready != 0 {
                let (mut a, mut b) = (0u64, 0u64);
                unsafe {
                    (gl.get_query_object_ui64v)(p.0, RESULT, &mut a);
                    (gl.get_query_object_ui64v)(p.1, RESULT, &mut b);
                }
                t.ns += b.saturating_sub(a);
                t.n += 1;
                p.2 = false;
            }
        }
        let free = match t.pairs.iter().position(|p| !p.2) {
            Some(i) => i,
            None if t.pairs.len() < 4 => {
                let mut ids = [0 as c_uint; 2];
                unsafe { (gl.gen_queries)(2, ids.as_mut_ptr()) };
                t.pairs.push((ids[0], ids[1], false));
                t.pairs.len() - 1
            }
            None => {
                t.open = None;
                return;
            }
        };
        unsafe { (gl.query_counter)(t.pairs[free].0, GL_TIMESTAMP) };
        t.open = Some(free);
    }

    /// Marks the end of screen `slot`'s GPU work this callback.
    pub fn gpu_end(&mut self, slot: usize) {
        const GL_TIMESTAMP: c_uint = 0x8E28;
        let Some(t) = self.gpu_times.get_mut(&slot) else { return };
        let Some(i) = t.open.take() else { return };
        unsafe { (self.gl.query_counter)(t.pairs[i].1, GL_TIMESTAMP) };
        t.pairs[i].2 = true;
    }

    /// Each screen's mean GPU ms per measured callback and count since the last call.
    pub fn gpu_report(&mut self) -> Vec<(usize, f64, u32)> {
        let mut out: Vec<(usize, f64, u32)> = self
            .gpu_times
            .iter_mut()
            .filter(|(_, t)| t.n > 0)
            .map(|(k, t)| {
                let r = (*k, t.ns as f64 / 1e6 / t.n as f64, t.n);
                t.ns = 0;
                t.n = 0;
                r
            })
            .collect();
        out.sort_by(|a, b| (b.1 * b.2 as f64).total_cmp(&(a.1 * a.2 as f64)));
        out
    }

    pub fn delete_sync(&mut self, fence: usize) {
        if fence == 0 {
            return;
        }
        if let Some(Some(sync)) = self.native_sync {
            unsafe { (sync.delete_sync)(fence as *mut c_void) };
        }
    }

    pub fn bridge_upload(&mut self, gpu: &mut BridgeGpu, width: u32, height: u32, pixels: &[u8], rects: &[[u32; 4]]) {
        if width == 0 || height == 0 || pixels.len() < (width as usize * height as usize * 4) {
            return;
        }
        unsafe {
            self.release_shared(gpu);
        }
        let gl = &self.gl;
        unsafe {
            if gpu.texture == 0 {
                gpu.texture = self.new_texture();
            }
            (self.xp.bind_texture)(gpu.texture, 0);
            if gpu.swizzled {
                (gl.tex_parameter)(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_R, GL_RED);
                (gl.tex_parameter)(GL_TEXTURE_2D, GL_TEXTURE_SWIZZLE_B, GL_BLUE);
                gpu.swizzled = false;
            }
            (gl.pixel_store)(GL_UNPACK_ALIGNMENT, 4);
            if gpu.size != (width, height) {
                crate::perf::UPLOADED_BYTES.fetch_add(width as u64 * height as u64 * 4, std::sync::atomic::Ordering::Relaxed);
                (gl.tex_image)(GL_TEXTURE_2D, 0, GL_RGBA8, width as c_int, height as c_int, 0, GL_BGRA, GL_UNSIGNED_BYTE, pixels.as_ptr().cast());
                self.texture_parameters(GL_LINEAR_MIPMAP_LINEAR, bridge_mip_levels(width, height));
                (gl.generate_mipmap)(GL_TEXTURE_2D);
                gpu.size = (width, height);
                return;
            }
            (gl.pixel_store)(GL_UNPACK_ROW_LENGTH, width as c_int);
            let upload_started = std::time::Instant::now();
            let mut updated = false;
            for &[x, y, w, h] in rects {
                if w == 0 || h == 0 || x + w > width || y + h > height {
                    continue;
                }
                let offset = (y as usize * width as usize + x as usize) * 4;
                crate::perf::UPLOADED_BYTES.fetch_add(w as u64 * h as u64 * 4, std::sync::atomic::Ordering::Relaxed);
                (gl.tex_sub_image)(GL_TEXTURE_2D, 0, x as c_int, y as c_int, w as c_int, h as c_int, GL_BGRA, GL_UNSIGNED_INT_8_8_8_8_REV, pixels[offset..].as_ptr().cast());
                updated = true;
            }
            (gl.pixel_store)(GL_UNPACK_ROW_LENGTH, 0);
            crate::perf::note("of the draw callbacks: uploads", upload_started.elapsed());
            // Sub-image writes don't touch the mip levels above 0 by
            // themselves (unlike glTexImage2D, which reallocates and leaves
            // them stale); regenerate only when a rect actually landed, so a
            // due-but-empty call (all rects clipped away by the bounds check
            // above) doesn't pay for a no-op rebuild.
            if updated {
                let mips_started = std::time::Instant::now();
                (gl.generate_mipmap)(GL_TEXTURE_2D);
                crate::perf::note("of the draw callbacks: mipmaps", mips_started.elapsed());
            }
        }
    }

    /// Upload a whole straight-alpha RGBA layer (the ND's terrain or weather
    /// radar picture) into `gpu` for [`Renderer::draw_bridge`]'s underlay.
    pub fn bridge_underlay_upload(&mut self, gpu: &mut BridgeGpu, width: u32, height: u32, rgba: &[u8]) {
        if width == 0 || height == 0 || rgba.len() < width as usize * height as usize * 4 {
            return;
        }
        unsafe {
            let fresh = gpu.texture == 0;
            if fresh {
                gpu.texture = self.new_texture();
            }
            (self.xp.bind_texture)(gpu.texture, 0);
            (self.gl.pixel_store)(GL_UNPACK_ALIGNMENT, 4);
            crate::perf::UPLOADED_BYTES.fetch_add(width as u64 * height as u64 * 4, std::sync::atomic::Ordering::Relaxed);
            if !fresh && gpu.size == (width, height) {
                // Same size as last time, which is the normal case: the ND's
                // terrain and weather pictures are a fixed 768x1024 canvas
                // and only their contents change. `glTexImage2D` would
                // reallocate the texture's storage on every update -- twice
                // a picture with both NDs drawn -- and drivers orphan the old
                // allocation rather than reuse it, which churns and fragments
                // video memory for a picture whose shape never changed.
                (self.gl.tex_sub_image)(
                    GL_TEXTURE_2D,
                    0,
                    0,
                    0,
                    width as c_int,
                    height as c_int,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    rgba.as_ptr().cast(),
                );
            } else {
                (self.gl.tex_image)(GL_TEXTURE_2D, 0, GL_RGBA8, width as c_int, height as c_int, 0, GL_RGBA, GL_UNSIGNED_BYTE, rgba.as_ptr().cast());
                self.texture_parameters(GL_LINEAR, 1);
                gpu.size = (width, height);
            }
        }
    }

    /// Draw a bridge screen's texture as a full-screen quad into the
    /// viewport X-Plane gave the device, with the same per-region brightness
    /// dimming [`Renderer::draw`] applies to a tessellated mesh (screen
    /// pixel space, not device pixels: GL's viewport transform does the
    /// scaling).
    ///
    /// Layers, bottom to top, as MSFS stacks an ND's two gauges on one
    /// texture (panel.cfg: the terrain gauge, then nd.html): opaque black,
    /// `underlay` (straight alpha) when given, then Chromium's pixels, which
    /// CEF paints with premultiplied alpha.
    #[allow(clippy::too_many_arguments)]
    fn sharpen_program(&mut self) -> Option<Sharpen> {
        if let Some(built) = self.sharpen {
            return built;
        }
        let built = unsafe { self.build_sharpen() };
        if built.is_none() {
            crate::log("display: the sharpening shader did not build; screens are drawn unsharpened");
        }
        self.sharpen = Some(built);
        built
    }

    unsafe fn build_sharpen(&self) -> Option<Sharpen> {
        let gl = &self.gl;
        let shader = |kind: c_uint, text: &str| -> Option<c_uint> {
            let id = (gl.create_shader)(kind);
            let src = CString::new(text).ok()?;
            let ptr = src.as_ptr();
            (gl.shader_source)(id, 1, &ptr, std::ptr::null());
            (gl.compile_shader)(id);
            let mut ok = 0;
            (gl.get_shader_iv)(id, GL_COMPILE_STATUS, &mut ok);
            (ok != 0).then_some(id)
        };
        let vertex = shader(GL_VERTEX_SHADER, SHARPEN_VERTEX)?;
        let fragment = shader(GL_FRAGMENT_SHADER, SHARPEN_FRAGMENT)?;
        let program = (gl.create_program)();
        (gl.attach_shader)(program, vertex);
        (gl.attach_shader)(program, fragment);
        (gl.link_program)(program);
        let mut ok = 0;
        (gl.get_program_iv)(program, GL_LINK_STATUS, &mut ok);
        if ok == 0 {
            return None;
        }
        let uniform = |name: &std::ffi::CStr| (gl.get_uniform_location)(program, name.as_ptr());
        Some(Sharpen {
            program,
            tex: uniform(c"tex"),
            texel: uniform(c"texel"),
            amount: uniform(c"amount"),
            glow: uniform(c"glow"),
            weight: uniform(c"weight"),
            sheen: uniform(c"sheen"),
            lod: uniform(c"lod"),
            pixel: uniform(c"pixel"),
        })
    }

    /// A popped up screen is drawn over the scene with the alpha left in its target, which
    /// the cockpit never looks at. The glass is opaque.
    pub fn make_opaque(&mut self, target: Target) {
        unsafe {
            let gl = &self.gl;
            let scissor_was_on = (gl.is_enabled)(GL_SCISSOR_TEST) != 0;
            let [x, y, w, h] = target.viewport;
            (gl.bind_framebuffer)(GL_FRAMEBUFFER, target.framebuffer);
            (gl.enable)(GL_SCISSOR_TEST);
            (gl.scissor)(x, y, w, h);
            (gl.color_mask)(0, 0, 0, 1);
            (gl.clear_color)(0., 0., 0., 1.);
            (gl.clear)(GL_COLOR_BUFFER_BIT);
            (gl.color_mask)(1, 1, 1, 1);
            if !scissor_was_on {
                (gl.disable)(GL_SCISSOR_TEST);
            }
        }
    }

    pub fn draw_bridge(&mut self, screen_id: &'static str, gpu: &mut BridgeGpu, underlay: Option<&BridgeGpu>, width: u32, height: u32, crop: Option<[f32; 4]>, target: Target, brightness: &[f32], dimming: &[[f32; 4]], cursor: Option<Cursor>) {
        let settings = crate::app_settings::current();
        let amount = settings.display_sharpen as f32;
        let finish = [settings.display_glow as f32, settings.display_line_weight as f32, settings.display_sheen as f32];
        let any = amount > 0. || self.prefilter_lod > 0. || finish.iter().any(|v| *v > 0.);
        let sharpen = if any && gpu.size.0 > 0 && gpu.size.1 > 0 { self.sharpen_program() } else { None };
        if gpu.texture == 0 {
            // Distinct from the width/height == 0 case below (a ScreenBlock
            // that simply has not published a size yet, normal for a
            // frame or two after attach): this is a screen the bridge is
            // supposed to be drawing that has never had a single
            // bridge_upload land a texture for it, so there is nothing to
            // show and nothing will ever appear until that changes. Logged
            // once, not every frame (60+ times a second while it persists).
            if self.bridge_no_texture_warned.insert(screen_id) {
                crate::log(&format!("display: {screen_id}: draw_bridge has no texture yet (no bridge_upload has landed); drawing nothing"));
            }
            return;
        }
        if width == 0 || height == 0 {
            return;
        }
        unsafe {
            let gl = &self.gl;
            let scissor_was_on = (gl.is_enabled)(GL_SCISSOR_TEST) != 0;
            (gl.use_program)(0);
            // macOS gives plugins a legacy 2.1 context (X-Plane draws with Metal), where the
            // core glBindVertexArray only raises GL_INVALID_OPERATION and there is no VAO to undo.
            if !cfg!(target_os = "macos") {
                (gl.bind_vertex_array)(0);
            }
            (gl.bind_framebuffer)(GL_FRAMEBUFFER, target.framebuffer);
            let [vx, vy, vw, vh] = target.viewport;
            (gl.viewport)(vx, vy, vw, vh);
            (gl.disable)(GL_SCISSOR_TEST);
            (gl.blend_func)(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
            (gl.tex_env)(GL_TEXTURE_ENV, GL_TEXTURE_ENV_MODE, GL_MODULATE);
            for mode in [GL_PROJECTION, GL_MODELVIEW, GL_TEXTURE] {
                (gl.matrix_mode)(mode);
                (gl.push_matrix)();
                (gl.load_identity)();
            }
            (gl.matrix_mode)(GL_PROJECTION);
            let [cx0, cy0, cx1, cy1] = crop.unwrap_or([0., 0., width as f32, height as f32]);
            (gl.ortho)(cx0 as f64, cx1 as f64, cy1 as f64, cy0 as f64, -1., 1.);
            (gl.matrix_mode)(GL_MODELVIEW);

            if gpu.buffer == 0 {
                (gl.gen_buffers)(1, &mut gpu.buffer);
            }
            let stride = std::mem::size_of::<Vertex>() as c_int;
            (gl.bind_buffer)(GL_ARRAY_BUFFER, gpu.buffer);
            (gl.enable_client_state)(GL_VERTEX_ARRAY);
            (gl.enable_client_state)(GL_COLOR_ARRAY);
            (gl.enable_client_state)(GL_TEXTURE_COORD_ARRAY);
            (gl.vertex_pointer)(2, GL_FLOAT, stride, std::ptr::null());
            (gl.tex_coord_pointer)(2, GL_FLOAT, stride, 8 as *const c_void);
            (gl.color_pointer)(4, GL_UNSIGNED_BYTE, stride, 16 as *const c_void);

            // Opaque black under everything: the screen's glass is black
            // wherever the pages paint nothing.
            (gl.disable_client_state)(GL_COLOR_ARRAY);
            let base = plain_quad(0., 0., width as f32, height as f32);
            (gl.buffer_data)(GL_ARRAY_BUFFER, std::mem::size_of_val(&base) as isize, base.as_ptr().cast(), GL_DYNAMIC_DRAW);
            (self.xp.set_graphics_state)(0, 0, 0, 0, 0, 0, 0);
            (gl.color4f)(0., 0., 0., 1.);
            (gl.draw_arrays)(GL_TRIANGLES, 0, 6);
            (gl.color4f)(1., 1., 1., 1.);
            (gl.enable_client_state)(GL_COLOR_ARRAY);

            let quad = textured_quad(width as f32, height as f32);
            (gl.buffer_data)(GL_ARRAY_BUFFER, std::mem::size_of_val(&quad) as isize, quad.as_ptr().cast(), GL_DYNAMIC_DRAW);
            self.draw_calls = 1;
            if let Some(under) = underlay.filter(|u| u.texture != 0) {
                (self.xp.set_graphics_state)(0, 1, 0, 0, 1, 0, 0);
                (gl.blend_func)(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
                (self.xp.bind_texture)(under.texture, 0);
                (gl.draw_arrays)(GL_TRIANGLES, 0, 6);
                self.draw_calls += 1;
            }
            // CEF's off-screen pixels are premultiplied.
            (self.xp.set_graphics_state)(0, 1, 0, 0, 1, 0, 0);
            (gl.blend_func)(GL_ONE, GL_ONE_MINUS_SRC_ALPHA);
            (self.xp.bind_texture)(gpu.texture, 0);
            if let Some(s) = sharpen {
                (gl.use_program)(s.program);
                (gl.uniform1i)(s.tex, 0);
                (gl.uniform2f)(s.texel, 1. / gpu.size.0 as f32, 1. / gpu.size.1 as f32);
                (gl.uniform1f)(s.amount, amount);
                (gl.uniform1f)(s.glow, finish[0]);
                (gl.uniform1f)(s.weight, finish[1]);
                (gl.uniform1f)(s.sheen, finish[2]);
                (gl.uniform1f)(s.lod, self.prefilter_lod);
                let seen = self.prefilter_lod.exp2();
                (gl.uniform2f)(s.pixel, seen * (cx1 - cx0) / (width as f32 * vw.max(1) as f32), seen * (cy1 - cy0) / (height as f32 * vh.max(1) as f32));
            }
            (gl.draw_arrays)(GL_TRIANGLES, 0, 6);
            #[cfg(feature = "native-displays")]
            {
                static SEEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if crate::native_displays::screen::inline_mode() && SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 24 {
                    let (mut bound, mut unit, mut program, mut fb) = (0, 0, 0, 0);
                    (gl.get_integer)(0x8069, &mut bound);
                    (gl.get_integer)(0x84E0, &mut unit);
                    (gl.get_integer)(0x8B8D, &mut program);
                    (gl.get_integer)(0x8CA6, &mut fb);
                    let mut vp = [0; 4];
                    (gl.get_integer)(GL_VIEWPORT, vp.as_mut_ptr());
                    let error = (gl.get_error)();
                    let mut pixel = [0u8; 4];
                    (gl.read_pixels)(vp[0] + vp[2] / 2, vp[1] + vp[3] / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel.as_mut_ptr().cast());
                    let read_error = (gl.get_error)();
                    crate::log(&format!(
                        "display: {screen_id} inline draw: texture {} ({}x{}), GL bound {bound} unit {}, program {program} (sharpen {}), tex2d {}, fb {fb} (target {}), viewport {vp:?}, error 0x{error:x}, centre pixel {pixel:?} (read error 0x{read_error:x})",
                        gpu.texture,
                        gpu.size.0,
                        gpu.size.1,
                        unit - 0x84C0,
                        sharpen.is_some(),
                        (gl.is_enabled)(GL_TEXTURE_2D),
                        target.framebuffer
                    ));
                }
            }
            if sharpen.is_some() {
                (gl.use_program)(0);
            }
            self.draw_calls += 1;
            if screen_id == "SCREEN_EFB" {
                let gain = efb_gain();
                if gain > 0. {
                    (gl.disable_client_state)(GL_COLOR_ARRAY);
                    (gl.blend_func)(GL_ONE, GL_ONE);
                    (gl.color4f)(gain, gain, gain, gain);
                    (gl.draw_arrays)(GL_TRIANGLES, 0, 6);
                    (gl.color4f)(1., 1., 1., 1.);
                    (gl.enable_client_state)(GL_COLOR_ARRAY);
                    self.draw_calls += 1;
                }
            }
            (gl.blend_func)(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);

            if let Some(c) = cursor {
                let verts = cursor_vertices(&c);
                (gl.buffer_data)(GL_ARRAY_BUFFER, std::mem::size_of_val(verts.as_slice()) as isize, verts.as_ptr().cast(), GL_DYNAMIC_DRAW);
                (self.xp.set_graphics_state)(0, 0, 0, 0, 1, 0, 0);
                (gl.draw_arrays)(GL_TRIANGLES, 0, verts.len() as c_int);
                self.draw_calls += 1;
            }

            // The dimming regions on top, same darkening as Step::Dim.
            (gl.disable_client_state)(GL_COLOR_ARRAY);
            for (i, region) in dimming.iter().enumerate() {
                let b = brightness.get(i).copied().unwrap_or(1.).clamp(0., 1.);
                if b >= 1. {
                    continue;
                }
                let [x0, y0, x1, y1] = *region;
                let quad = plain_quad(x0, y0, x1, y1);
                (gl.buffer_data)(GL_ARRAY_BUFFER, std::mem::size_of_val(&quad) as isize, quad.as_ptr().cast(), GL_DYNAMIC_DRAW);
                (self.xp.set_graphics_state)(0, 0, 0, 0, 1, 0, 0);
                (gl.color4f)(0., 0., 0., 1. - screens_dim_srgb(b));
                (gl.draw_arrays)(GL_TRIANGLES, 0, 6);
                self.draw_calls += 1;
            }
            (gl.color4f)(1., 1., 1., 1.);
            (gl.enable_client_state)(GL_COLOR_ARRAY);

            (gl.viewport)(vx, vy, vw, vh);
            if scissor_was_on {
                (gl.enable)(GL_SCISSOR_TEST);
            } else {
                (gl.disable)(GL_SCISSOR_TEST);
            }
            for mode in [GL_TEXTURE, GL_MODELVIEW, GL_PROJECTION] {
                (gl.matrix_mode)(mode);
                (gl.pop_matrix)();
            }
            (gl.matrix_mode)(GL_MODELVIEW);
            // Cleanup matches Renderer::draw's tail (~713-717). This used to
            // turn GL_TEXTURE_COORD_ARRAY back off and stop there, leaving
            // GL_COLOR_ARRAY enabled for whatever drew next -- another
            // screen's plain-mesh draw() call, or X-Plane's own 3-D drawing
            // -- in violation of this module's own rule ("only the
            // fixed-function vertex array left enabled", see the file
            // header).
            (gl.disable_client_state)(GL_COLOR_ARRAY);
            (gl.disable_client_state)(GL_TEXTURE_COORD_ARRAY);
            (gl.enable_client_state)(GL_VERTEX_ARRAY);
            (gl.bind_buffer)(GL_ARRAY_BUFFER, 0);
        }
    }

    /// Carry out the steps for one frame: a tile of the multisampled
    /// framebuffer, or the whole region straight in X-Plane's target.
    unsafe fn run(&mut self, gpu: &ScreenGpu, steps: &[Step], frame: Frame, stencil: bool, brightness: &[f32]) {
        let gl = &self.gl;
        let [rx0, ry0, rx1, ry1] = frame.region;
        (gl.matrix_mode)(GL_PROJECTION);
        (gl.load_identity)();
        (gl.ortho)(rx0 as f64, rx1 as f64, ry1 as f64, ry0 as f64, -1., 1.);
        (gl.matrix_mode)(GL_MODELVIEW);
        let mut current: Option<[i32; 4]> = None;
        let apply_scissor = |gl: &Fns, rect: Option<[i32; 4]>| {
            if rect.is_none() && !frame.always_scissor {
                (gl.disable)(GL_SCISSOR_TEST);
            } else {
                let [x, y, w, h] = frame.scissor_box(rect);
                (gl.enable)(GL_SCISSOR_TEST);
                (gl.scissor)(x, y, w, h);
            }
        };
        let mut textured: Option<Paint> = None;
        for step in steps {
            match *step {
                Step::Scissor(rect) => {
                    current = rect;
                    apply_scissor(gl, rect);
                }
                Step::ClearStencil if stencil => {
                    apply_scissor(gl, None);
                    (gl.stencil_mask)(0xff);
                    (gl.clear)(GL_STENCIL_BUFFER_BIT);
                    apply_scissor(gl, current);
                }
                Step::ClearMark if stencil => {
                    (gl.stencil_mask)(MARK as c_uint);
                    (gl.clear)(GL_STENCIL_BUFFER_BIT);
                }
                Step::ClipPath { level, first, count } if stencil => {
                    (self.xp.set_graphics_state)(0, 0, 0, 0, 1, 0, 0);
                    textured = None;
                    (gl.color_mask)(0, 0, 0, 0);
                    (gl.enable)(GL_STENCIL_TEST);
                    (gl.stencil_mask)(LEVELS as c_uint);
                    (gl.stencil_func)(GL_EQUAL, level as c_int, LEVELS as c_uint);
                    (gl.stencil_op)(GL_KEEP, GL_KEEP, GL_INCR);
                    (gl.draw_arrays)(GL_TRIANGLES, first as c_int, count as c_int);
                    (gl.color_mask)(1, 1, 1, 1);
                    self.draw_calls += 1;
                }
                Step::ClearStencil | Step::ClearMark | Step::ClipPath { .. } => {}
                Step::Dim { region, first, count } => {
                    let b = brightness.get(region).copied().unwrap_or(1.).clamp(0., 1.);
                    if b >= 1. {
                        continue;
                    }
                    (gl.disable)(GL_STENCIL_TEST);
                    (self.xp.set_graphics_state)(0, 0, 0, 0, 1, 0, 0);
                    textured = None;
                    (gl.disable_client_state)(GL_COLOR_ARRAY);
                    // MSFS scales the screen's emitted light by the knob;
                    // the texture holds sRGB, so the same fraction of light
                    // darkens its values by b^(1/2.2), not b.
                    (gl.color4f)(0., 0., 0., 1. - screens_dim_srgb(b));
                    (gl.draw_arrays)(GL_TRIANGLES, first as c_int, count as c_int);
                    (gl.color4f)(1., 1., 1., 1.);
                    (gl.enable_client_state)(GL_COLOR_ARRAY);
                    self.draw_calls += 1;
                }
                Step::Draw { paint, first, count, stencil: test } => {
                    match test {
                        StencilTest::Equal { reference, mask, mark } if stencil => {
                            (gl.enable)(GL_STENCIL_TEST);
                            (gl.stencil_func)(GL_EQUAL, reference as c_int, mask as c_uint);
                            if mark {
                                (gl.stencil_mask)(MARK as c_uint);
                                (gl.stencil_op)(GL_KEEP, GL_KEEP, GL_INVERT);
                            } else {
                                (gl.stencil_mask)(0);
                                (gl.stencil_op)(GL_KEEP, GL_KEEP, GL_KEEP);
                            }
                        }
                        _ => (gl.disable)(GL_STENCIL_TEST),
                    }
                    let native = match paint {
                        Paint::Native(i) => self.native_textures.get(i).copied().unwrap_or(0),
                        _ => 0,
                    };
                    if matches!(paint, Paint::Native(_)) && native == 0 {
                        continue;
                    }
                    if textured != Some(paint) {
                        match paint {
                            Paint::Atlas => {
                                (self.xp.set_graphics_state)(0, 1, 0, 0, 1, 0, 0);
                                (self.xp.bind_texture)(self.atlas_texture, 0);
                            }
                            Paint::Image(i) => {
                                (self.xp.set_graphics_state)(0, 1, 0, 0, 1, 0, 0);
                                (self.xp.bind_texture)(self.images.get(i).copied().unwrap_or(0), 0);
                            }
                            Paint::Gradients => {
                                (self.xp.set_graphics_state)(0, 1, 0, 0, 1, 0, 0);
                                (self.xp.bind_texture)(gpu.gradients, 0);
                            }
                            Paint::Native(_) => {
                                (self.xp.set_graphics_state)(0, 1, 0, 0, 1, 0, 0);
                                (self.xp.bind_texture)(native, 0);
                            }
                        }
                        (gl.matrix_mode)(GL_TEXTURE);
                        (gl.load_identity)();
                        if paint == Paint::Gradients {
                            (gl.scale)(1., 1. / gpu.gradient_rows.max(1) as f64, 1.);
                        }
                        (gl.matrix_mode)(GL_MODELVIEW);
                        textured = Some(paint);
                    }
                    (gl.draw_arrays)(GL_TRIANGLES, first as c_int, count as c_int);
                    self.draw_calls += 1;
                }
            }
        }
        (gl.disable)(GL_STENCIL_TEST);
    }
}

/// `xphfbw.displaySafeMode` out of a parsed xphfbw.json map: `true`/`"true"`
/// only (matching `app_settings.rs`'s `as_bool`, which this intentionally
/// duplicates rather than importing -- `app_settings.rs` is outside this
/// module's ownership, and `AppSettings` does not carry this key today).
/// Anything else (missing, malformed, a stray number) is "off", the
/// checkbox's own default (`app/ui/index.html`'s `DEFAULTS`).
fn parse_display_safe_mode(map: &serde_json::Map<String, serde_json::Value>) -> bool {
    match map.get("displaySafeMode") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s == "true",
        _ => false,
    }
}

/// Reads `xphfbw.json` fresh (this runs once, in [`Renderer::new`], not per
/// frame): `false` with no X-Plane folder known yet (unit tests, or called
/// before `xp::system_path` can resolve) or no readable/parsable file, same
/// fallback `app_settings.rs::load_from_path` uses for a missing install.
fn display_safe_mode() -> bool {
    let Some(root) = crate::xp::system_path() else { return false };
    let path = crate::settings_files::app_settings_path(&root);
    let map = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    parse_display_safe_mode(&map)
}

#[cfg(test)]
mod safe_mode_tests {
    use super::parse_display_safe_mode;
    use serde_json::{json, Value};

    #[test]
    fn off_by_default_when_the_key_is_missing() {
        let map = json!({}).as_object().cloned().unwrap();
        assert!(!parse_display_safe_mode(&map));
    }

    #[test]
    fn on_when_the_app_wrote_a_json_bool() {
        let map = json!({"displaySafeMode": true}).as_object().cloned().unwrap();
        assert!(parse_display_safe_mode(&map));
    }

    #[test]
    fn on_when_the_app_wrote_a_string_true_like_its_other_booleans() {
        let map = json!({"displaySafeMode": "true"}).as_object().cloned().unwrap();
        assert!(parse_display_safe_mode(&map));
    }

    #[test]
    fn off_for_anything_else() {
        for v in [Value::Bool(false), Value::String("false".into()), Value::String("nonsense".into()), Value::Null] {
            let map = json!({"displaySafeMode": v}).as_object().cloned().unwrap();
            assert!(!parse_display_safe_mode(&map));
        }
    }
}

/// The factor on sRGB-encoded values that scales their light by `b`.
pub fn screens_dim_srgb(b: f32) -> f32 {
    b.clamp(0., 1.).powf(1. / 2.2)
}

/// Mip levels a `width`x`height` bridge screen texture needs for a full
/// chain down to 1x1: `floor(log2(max(width, height))) + 1`, the count
/// [`Renderer::texture_parameters`]'s `levels` argument expects (it sets
/// `GL_TEXTURE_MAX_LEVEL` to `levels - 1`). The EFB (1430 px) covers only
/// ~700 screen px at the pilot's eye (docs/screens.md) -- minified ~2:1 --
/// and `GL_LINEAR` with a single level samples that straight, aliasing text;
/// a full chain plus `GL_LINEAR_MIPMAP_LINEAR` lets the driver blend down
/// instead.
fn efb_gain() -> f32 {
    static GAIN: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *GAIN.get_or_init(|| std::env::var("FBW_EFB_GAIN").ok().and_then(|v| v.parse().ok()).unwrap_or(0.3f32).clamp(0., 1.))
}

fn bridge_mip_levels(width: u32, height: u32) -> c_int {
    (width.max(height).max(1).ilog2() + 1) as c_int
}

#[cfg(test)]
mod bridge_mip_levels_tests {
    use super::bridge_mip_levels;

    #[test]
    fn one_by_one_needs_one_level() {
        assert_eq!(bridge_mip_levels(1, 1), 1);
    }

    #[test]
    fn a_power_of_two_matches_its_own_log2_plus_one() {
        // 1024 = 2^10, so the chain is 1024,512,...,2,1: 11 levels.
        assert_eq!(bridge_mip_levels(1024, 1024), 11);
    }

    #[test]
    fn the_larger_side_drives_the_count() {
        // The EFB: 1430x1000. floor(log2(1430)) = 10, so 11 levels.
        assert_eq!(bridge_mip_levels(1430, 1000), 11);
        assert_eq!(bridge_mip_levels(1000, 1430), 11);
    }

    #[test]
    fn a_du_screen() {
        // 768x1024: floor(log2(1024)) = 10, so 11 levels.
        assert_eq!(bridge_mip_levels(768, 1024), 11);
    }

    #[test]
    fn zero_never_underflows_the_log2() {
        // A malformed size should not panic (ilog2(0) would); bridge_upload
        // already returns before this runs for a real zero width/height, but
        // the helper stays safe on its own.
        assert_eq!(bridge_mip_levels(0, 0), 1);
    }
}

/// A full-screen quad (device pixels, y down, matching [`Vertex`]'s
/// convention), textured over its whole extent.
fn textured_quad(width: f32, height: f32) -> [Vertex; 6] {
    let rgba = [255, 255, 255, 255];
    let v = |x: f32, y: f32, u: f32, v: f32| Vertex { x, y, u, v, rgba };
    [v(0., 0., 0., 0.), v(width, 0., 1., 0.), v(width, height, 1., 1.), v(0., 0., 0., 0.), v(width, height, 1., 1.), v(0., height, 0., 1.)]
}

#[derive(Clone, Copy, Debug)]
pub struct Cursor {
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    pub rgba: [u8; 4],
}

fn cursor_vertices(c: &Cursor) -> Vec<Vertex> {
    let at = |px: f32, py: f32| (c.x + (px - 40.) * c.scale, c.y + (py - 40.) * c.scale);
    let segments: [((f32, f32), (f32, f32), f32); 6] = [
        ((0., 0.), (40., 35.), 3.),
        ((40., 35.), (80., 0.), 3.),
        ((0., 80.), (40., 45.), 3.),
        ((40., 45.), (80., 80.), 3.),
        ((40., 39.), (40., 41.), 2.),
        ((39., 40.), (41., 40.), 2.),
    ];
    let mut out = Vec::with_capacity(segments.len() * 6);
    for (a, b, w) in segments {
        let (ax, ay) = at(a.0, a.1);
        let (bx, by) = at(b.0, b.1);
        let (dx, dy) = (bx - ax, by - ay);
        let len = (dx * dx + dy * dy).sqrt().max(1e-3);
        let (nx, ny) = (-dy / len * w * c.scale / 2., dx / len * w * c.scale / 2.);
        let v = |x: f32, y: f32| Vertex { x, y, u: 0., v: 0., rgba: c.rgba };
        out.extend([v(ax + nx, ay + ny), v(bx - nx, by - ny), v(bx + nx, by + ny), v(ax + nx, ay + ny), v(ax - nx, ay - ny), v(bx - nx, by - ny)]);
    }
    out
}

/// An untextured black quad over one region, for the dimming pass
/// ([`Renderer::draw_bridge`]); its alpha is set with `glColor4f` instead of
/// a per-vertex colour, as [`Step::Dim`] does for the tessellated path.
fn plain_quad(x0: f32, y0: f32, x1: f32, y1: f32) -> [Vertex; 6] {
    let rgba = [0, 0, 0, 255];
    let v = |x: f32, y: f32| Vertex { x, y, u: 0., v: 0., rgba };
    [v(x0, y0), v(x1, y0), v(x1, y1), v(x0, y0), v(x1, y1), v(x0, y1)]
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    fn area(t: &[Vertex]) -> f32 {
        (t[1].x - t[0].x) * (t[2].y - t[0].y) - (t[2].x - t[0].x) * (t[1].y - t[0].y)
    }

    #[test]
    fn the_cursor_winds_like_the_screen_quads() {
        let quad = plain_quad(0., 0., 10., 10.);
        let front = area(&quad[..3]).signum();
        let verts = cursor_vertices(&Cursor { x: 500., y: 500., scale: 1.5, rgba: [255, 255, 0, 255] });
        assert!(verts.chunks(3).all(|t| area(t).signum() == front));
    }
}

impl Renderer {
    unsafe fn build_hud_program(&self) -> Option<HudProgram> {
        let gl = &self.gl;
        let shader = |kind: c_uint, text: &str| -> Option<c_uint> {
            let id = (gl.create_shader)(kind);
            let src = CString::new(text).ok()?;
            let ptr = src.as_ptr();
            (gl.shader_source)(id, 1, &ptr, std::ptr::null());
            (gl.compile_shader)(id);
            let mut ok = 0;
            (gl.get_shader_iv)(id, GL_COMPILE_STATUS, &mut ok);
            (ok != 0).then_some(id)
        };
        let vertex = shader(GL_VERTEX_SHADER, HUD_VERTEX)?;
        let fragment = shader(GL_FRAGMENT_SHADER, HUD_FRAGMENT)?;
        let program = (gl.create_program)();
        (gl.attach_shader)(program, vertex);
        (gl.attach_shader)(program, fragment);
        (gl.link_program)(program);
        let mut ok = 0;
        (gl.get_program_iv)(program, GL_LINK_STATUS, &mut ok);
        if ok == 0 {
            return None;
        }
        let uniform = |name: &std::ffi::CStr| (gl.get_uniform_location)(program, name.as_ptr());
        Some(HudProgram { program, tex: uniform(c"tex"), texel: uniform(c"texel"), color: uniform(c"color"), glow: uniform(c"glow") })
    }

    fn hud_program(&mut self) -> Option<HudProgram> {
        if let Some(built) = self.hud_program {
            return built;
        }
        let built = unsafe { self.build_hud_program() };
        if built.is_none() {
            crate::log("hud: the composite shader did not build; the HUD is not drawn");
        }
        self.hud_program = Some(built);
        built
    }

    /// Draws a screen's mesh into `target` (made or resized to `width` x `height`) and returns its texture.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_offscreen(
        &mut self,
        target: &mut HudTarget,
        gpu: &mut ScreenGpu,
        stream: u64,
        mesh: &Mesh,
        steps: &[Step],
        width: u32,
        height: u32,
        restore_framebuffer: u32,
        restore_viewport: [i32; 4],
        natives: &[Option<Arc<NativeImage>>],
        res: &mut Resources,
    ) -> Option<c_int> {
        unsafe {
            if target.framebuffer == 0 || target.size != (width, height) {
                if target.framebuffer == 0 {
                    (self.gl.gen_framebuffers)(1, &mut target.framebuffer);
                }
                if target.texture == 0 {
                    target.texture = self.new_texture();
                }
                (self.xp.bind_texture)(target.texture, 0);
                (self.gl.tex_image)(GL_TEXTURE_2D, 0, GL_RGBA8, width as c_int, height as c_int, 0, GL_RGBA, GL_UNSIGNED_BYTE, std::ptr::null());
                self.texture_parameters(GL_LINEAR, 1);
                (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, target.framebuffer);
                (self.gl.framebuffer_texture_2d)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, target.texture as c_uint, 0);
                let status = (self.gl.check_framebuffer_status)(GL_FRAMEBUFFER);
                (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, restore_framebuffer);
                if status != GL_FRAMEBUFFER_COMPLETE {
                    crate::log(&format!("hud: offscreen target incomplete (0x{status:x})"));
                    return None;
                }
                target.size = (width, height);
            }
        }
        let region = [0, 0, width as i32, height as i32];
        let viewport = [0, 0, width as i32, height as i32];
        self.draw(gpu, stream, mesh, steps, region, Target { framebuffer: target.framebuffer, viewport }, &[], natives, res);
        unsafe {
            (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, restore_framebuffer);
            let [x0, y0, x1, y1] = restore_viewport;
            (self.gl.viewport)(x0, y0, x1 - x0, y1 - y0);
        }
        Some(target.texture)
    }

    /// Composites `texture` additively over the scene: `triangles` are window pixels, page UVs and an
    /// alpha `[x, y, u, v, a]`, drawn in `color` with the phosphor glow.
    pub fn draw_hud(&mut self, texture: c_int, size: (u32, u32), triangles: &[[f32; 5]], color: [f32; 3], glow: f32, viewport: [i32; 4]) {
        let Some(program) = self.hud_program() else { return };
        if triangles.is_empty() {
            return;
        }
        let [x0, y0, x1, y1] = viewport;
        let mut verts: Vec<f32> = Vec::with_capacity(triangles.len() * 2);
        let mut uvs: Vec<f32> = Vec::with_capacity(triangles.len() * 2);
        let mut colors: Vec<u8> = Vec::with_capacity(triangles.len() * 4);
        for t in triangles {
            verts.extend_from_slice(&[t[0], t[1]]);
            uvs.extend_from_slice(&[t[2], t[3]]);
            colors.extend_from_slice(&[255, 255, 255, (t[4].clamp(0., 1.) * 255.) as u8]);
        }
        unsafe {
            let gl = &self.gl;
            (self.xp.set_graphics_state)(0, 1, 0, 0, 1, 0, 0);
            (self.xp.bind_texture)(texture, 0);
            (gl.blend_func)(GL_ONE, GL_ONE);
            for mode in [GL_PROJECTION, GL_MODELVIEW] {
                (gl.matrix_mode)(mode);
                (gl.push_matrix)();
                (gl.load_identity)();
            }
            (gl.matrix_mode)(GL_PROJECTION);
            (gl.ortho)(0., (x1 - x0) as f64, 0., (y1 - y0) as f64, -1., 1.);
            (gl.viewport)(x0, y0, x1 - x0, y1 - y0);
            (gl.use_program)(program.program);
            (gl.uniform1i)(program.tex, 0);
            (gl.uniform2f)(program.texel, 1. / size.0.max(1) as f32, 1. / size.1.max(1) as f32);
            (gl.uniform3f)(program.color, color[0], color[1], color[2]);
            (gl.uniform1f)(program.glow, glow);
            (gl.bind_buffer)(GL_ARRAY_BUFFER, 0);
            (gl.enable_client_state)(GL_VERTEX_ARRAY);
            (gl.enable_client_state)(GL_TEXTURE_COORD_ARRAY);
            (gl.enable_client_state)(GL_COLOR_ARRAY);
            (gl.vertex_pointer)(2, GL_FLOAT, 0, verts.as_ptr().cast());
            (gl.tex_coord_pointer)(2, GL_FLOAT, 0, uvs.as_ptr().cast());
            (gl.color_pointer)(4, GL_UNSIGNED_BYTE, 0, colors.as_ptr().cast());
            (gl.draw_arrays)(GL_TRIANGLES, 0, triangles.len() as c_int);
            (gl.disable_client_state)(GL_COLOR_ARRAY);
            (gl.disable_client_state)(GL_TEXTURE_COORD_ARRAY);
            (gl.use_program)(0);
            (gl.blend_func)(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
            for mode in [GL_MODELVIEW, GL_PROJECTION] {
                (gl.matrix_mode)(mode);
                (gl.pop_matrix)();
            }
            (gl.matrix_mode)(GL_MODELVIEW);
        }
    }
}
