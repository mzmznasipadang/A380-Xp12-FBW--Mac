//! Compiles FlyByWire's generated A380 computers (their Simulink C++, used
//! unchanged from the FlyByWire checkout next to this crate) and the small C
//! ABI shims in `src/fbw_cpp` into one static library:
//!
//! - the FADEC computer (`A380FadecComputer`), with `shim.cpp`;
//! - the three PRIMs, three SECs and two FCUs, run through FlyByWire's own
//!   wrappers `prim/Prim.cpp`, `sec/Sec.cpp` and `fcu/Fcu.cpp` (self tests,
//!   power monitoring, the order the PRIM's four model parts step in), with
//!   `prim_shim.cpp`.
//!
//! The wrappers are copied byte for byte into OUT_DIR so that Prim.cpp's
//! `#include "../interface/SimConnectInterface.h"` finds the stand-in in
//! `src/fbw_cpp/stubs` rather than FlyByWire's MSFS SimConnect header; their
//! other relative includes (`../model/...`, `../utils/...`, `../Arinc429.h`)
//! fall through to FlyByWire's tree through the include paths, as in their
//! own build (fbw_a380/build.sh, which adds `-I src/prim`, `-I src/sec`, ...).

use std::path::{Path, PathBuf};

fn main() {
    wasm_link_args();
    if std::env::var_os("CARGO_FEATURE_NATIVE_DISPLAYS").is_some() {
        native_displays();
    }

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // FlyByWire's tree: inside the public repo, or beside the working plugin repo.
    let fbw_tree = Some(manifest.join("fbw/xplane")).filter(|p| p.join("fbw-a380x").is_dir()).unwrap_or_else(|| manifest.join("../fbw-aircraft"));
    println!("cargo:rustc-env=FBW_TREE={}", fbw_tree.display());
    let fbw = fbw_tree.join("fbw-a380x/src/wasm/fbw_a380/src");
    let model = fbw.join("model");
    let shim = manifest.join("src/fbw_cpp");

    // The wrappers, copied unchanged, beside the SimConnectInterface stand-in.
    let wrappers = out.join("fbw_a380");
    for (dir, files) in [
        ("prim", ["Prim.cpp", "Prim.h"]),
        ("sec", ["Sec.cpp", "Sec.h"]),
        ("fcu", ["Fcu.cpp", "Fcu.h"]),
    ] {
        std::fs::create_dir_all(wrappers.join(dir)).unwrap();
        for file in files {
            copy(&fbw.join(dir).join(file), &wrappers.join(dir).join(file));
        }
    }
    std::fs::create_dir_all(wrappers.join("interface")).unwrap();
    copy(
        &shim.join("stubs/interface/SimConnectInterface.h"),
        &wrappers.join("interface/SimConnectInterface.h"),
    );

    let fadec_sources = [
        model.join("A380FadecComputer.cpp"),
        model.join("A380FadecComputer_data.cpp"),
        shim.join("shim.cpp"),
    ];
    // As listed in fbw_a380/build.sh for the PRIM, SEC and FCU and what they
    // call.
    let mut computer_sources: Vec<PathBuf> = [
        "A380PrimComputerGeneralLogic_data",
        "A380PrimComputerGeneralLogic",
        "A380PrimComputerFctl_data",
        "A380PrimComputerFctl",
        "A380PrimComputerFe_data",
        "A380PrimComputerFe",
        "A380FgOuterLoops",
        "A380PrimComputerFg_data",
        "A380PrimComputerFg",
        "A380SecComputer_data",
        "A380SecComputer",
        "A380PitchNzLaw",
        "A380PitchDirectLaw",
        "A380LateralYStarLaw",
        "A380LateralDirectLaw",
        "A380FcuComputer_data",
        "A380FcuComputer",
        "intrp3d_l_pw",
        "look1_binlxpw",
        "look1_binlcpw",
        "look1_iflf_binlxpw",
        "look2_binlxpw",
        "look2_iflf_binlxpw",
        "plook_binx",
        "binsearch_u32d",
        "rt_modd",
        "rt_remd",
    ]
    .iter()
    .map(|name| model.join(format!("{name}.cpp")))
    .collect();
    computer_sources.extend([
        wrappers.join("prim/Prim.cpp"),
        wrappers.join("sec/Sec.cpp"),
        wrappers.join("fcu/Fcu.cpp"),
        fbw.join("utils/PulseNode.cpp"),
        fbw.join("utils/SRFlipFLop.cpp"),
        shim.join("prim_shim.cpp"),
    ]);

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", shim.display());
    println!("cargo:rerun-if-changed={}", model.display());
    for dir in ["prim", "sec", "fcu", "utils"] {
        println!("cargo:rerun-if-changed={}", fbw.join(dir).display());
    }

    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .files(&fadec_sources)
        .files(&computer_sources)
        // The copied wrappers first, then FlyByWire's tree as their own build
        // has it.
        .include(&wrappers)
        .include(&model)
        .include(fbw.join("prim"))
        .include(fbw.join("sec"))
        .include(fbw.join("fcu"))
        .include(fbw.join("utils"))
        .include(&shim)
        // As FlyByWire builds it: no exceptions. No RTTI is needed either.
        .flag_if_supported("-fno-exceptions")
        .flag_if_supported("-fno-rtti")
        // Keep multiply-add pairs as written, so results do not depend on
        // whether the compiler fuses them.
        .flag_if_supported("-ffp-contract=off")
        // Keep sin and cos as the separate libm calls the code makes, rather
        // than fused into one `sincos` call.
        .flag_if_supported("-fno-builtin-sin")
        .flag_if_supported("-fno-builtin-cos")
        // The computers are placed into zeroed memory to start from the same
        // zeros as FlyByWire's global instance; keep that zeroing.
        .flag_if_supported("-fno-lifetime-dse")
        .opt_level(2)
        .warnings(false)
        // The models only need <cmath>; nothing from libstdc++ is linked.
        .cpp_link_stdlib(None)
        // The library is linked by `#[link]` on the extern blocks in
        // src/fbw_controllers.rs and src/fbw_computers.rs, not by a
        // `rustc-link-lib` line here: Cargo hands build-script link libraries
        // only to the library target when there is one, and the integration
        // tests compile those modules themselves. The search path does reach
        // every target.
        .cargo_metadata(false)
        .compile("fbw_controllers");
    println!("cargo:rustc-link-search=native={}", out.display());
    // A Mac dylib must resolve everything when it is linked, unlike a Linux one.
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos") {
        println!("cargo:rustc-link-lib=c++");
    }
}

fn copy(from: &Path, to: &Path) {
    let bytes = std::fs::read(from).unwrap_or_else(|e| panic!("{}: {e}", from.display()));
    // Only rewrite when changed, so the C++ is not rebuilt for nothing.
    if std::fs::read(to).ok().as_deref() != Some(&bytes[..]) {
        std::fs::write(to, bytes).unwrap();
    }
}

/// The MSFS build. `msfs/deep-wasm` links this crate as a library, but the
/// `cdylib` in `crate-type` is built alongside the rlib whatever depends on
/// it, and for `wasm32-wasip1` that link needs the SDK's C library and the
/// export/undefined flags FlyByWire's own modules use, or it fails on the
/// first `kernel32` symbol. Same list as `msfs/lvar-bench/build.rs`, which
/// documents where each flag comes from. A host build emits nothing.
fn wasm_link_args() {
    println!("cargo:rerun-if-env-changed=MSFS_SDK");
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.starts_with("wasm32") {
        return;
    }
    let sdk = std::env::var("MSFS_SDK")
        .ok()
        .filter(|p| Path::new(p).exists())
        .or_else(|| ["/workdir/MSFS_SDK", r"C:\MSFS SDK"].into_iter().find(|p| Path::new(p).exists()).map(String::from))
        .expect("wasm32 build needs the MSFS SDK: set MSFS_SDK, or build in FlyByWire's dev-env image (msfs/deep-wasm/build.ps1)");
    let sysroot_lib = format!("{sdk}/WASM/wasi-sysroot/lib/wasm32-wasi");
    for arg in [
        "-l".to_string(),
        "c".to_string(),
        format!("{sysroot_lib}/libclang_rt.builtins-wasm32.a"),
        "-L".to_string(),
        sysroot_lib.clone(),
        "--export-table".to_string(),
        "--allow-undefined".to_string(),
        "--export-dynamic".to_string(),
        "--export=__wasm_call_ctors".to_string(),
        "--export=malloc".to_string(),
        "--export=free".to_string(),
        "--export=mark_decommit_pages".to_string(),
        "--export=mallinfo".to_string(),
        "--export=mchunkit_begin".to_string(),
        "--export=mchunkit_next".to_string(),
        "--export=get_pages_state".to_string(),
    ] {
        println!("cargo:rustc-link-arg={arg}");
    }
}

fn env_or(name: &str, default: &str) -> String {
    println!("cargo:rerun-if-env-changed={name}");
    std::env::var(name).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

/// Feature `native-displays`: builds `a380_displays.dll` (a380-displays, Skia
/// M152 with Ganesh GL linked statically inside it) with CMake, Ninja and MSVC
/// through vcvars in `E:/fbw-build/a380-displays/plugin-<profile>`, and copies
/// it next to the build output. Nothing of Skia is linked into the plugin
/// (the GNU toolchain cannot link MSVC libraries); the plugin loads the DLL at
/// run time (src/native_displays). `A380_DISPLAYS_DLL_BUILT` carries the
/// copy's path to the unit tests.
fn native_displays() {
    if !cfg!(windows) {
        panic!("feature native-displays: only the Windows build is wired so far (Linux and Mac follow, plan Task 7)");
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "release".into());
    let default_src = manifest.join("../a380-displays");
    let src = env_or("A380_DISPLAYS_DIR", &default_src.to_string_lossy());
    let src = std::fs::canonicalize(&src).unwrap_or_else(|e| panic!("a380-displays not found at {src}: {e}"));
    let src = src.to_string_lossy().replace('\\', "/");
    let src = src.trim_start_matches("//?/").to_string();
    let build = env_or("A380_DISPLAYS_BUILD_DIR", &format!("E:/fbw-build/a380-displays/plugin-{profile}"));
    let skia_root = env_or("A380_SKIA_ROOT", "E:/skia-c152");
    let skia_lib = env_or("A380_SKIA_LIB_DIR", &format!("{skia_root}/out/c152v"));
    let vcvars = env_or("A380_VCVARS", "E:/VS/2022/Community/VC/Auxiliary/Build/vcvars64.bat").replace('/', "\\");

    for rel in ["CMakeLists.txt", "core", "displays", "third_party"] {
        println!("cargo:rerun-if-changed={src}/{rel}");
    }

    let script = out.join("build_a380_displays.cmd");
    let build_win = build.replace('/', "\\");
    let lines = [
        "@echo off".to_string(),
        format!("call \"{vcvars}\" >nul"),
        "if errorlevel 1 exit /b 1".to_string(),
        format!("if exist \"{build_win}\\build.ninja\" goto build"),
        format!(
            "cmake -S \"{src}\" -B \"{build}\" -G Ninja \"-DCMAKE_BUILD_TYPE=Release\" \"-DCMAKE_CXX_COMPILER=cl\" \"-DA380_GPU=ON\" \"-DA380_SKIA_ROOT={skia_root}\" \"-DA380_SKIA_LIB_DIR={skia_lib}\""
        ),
        "if errorlevel 1 exit /b 1".to_string(),
        ":build".to_string(),
        format!("cmake --build \"{build}\" --target a380_displays"),
        "exit /b %errorlevel%".to_string(),
    ];
    std::fs::write(&script, lines.join("\r\n") + "\r\n").unwrap();
    let status = std::process::Command::new("cmd")
        .arg("/C")
        .arg(&script)
        .status()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", script.display()));
    if !status.success() {
        panic!("building a380_displays.dll failed ({status}); build dir {build}");
    }

    let built = Path::new(&build).join("a380_displays.dll");
    if !built.is_file() {
        panic!("{} was not produced", built.display());
    }
    let profile_dir = out.ancestors().nth(3).unwrap_or(&out).to_path_buf();
    let copy = profile_dir.join("a380_displays.dll");
    std::fs::copy(&built, &copy).unwrap_or_else(|e| panic!("copying {} to {}: {e}", built.display(), copy.display()));
    println!("cargo:rustc-env=A380_DISPLAYS_DLL_BUILT={}", copy.display());
}
