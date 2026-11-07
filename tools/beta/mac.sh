#!/bin/bash
# A380 for X-Plane 12: beta install, built from source on this computer.
# Run it with: bash mac.sh or bash linux.sh (the same script)
# Asks for what it needs, builds, and installs into your X-Plane. Close X-Plane first.

set -u

RUST_VERSION=1.97.1
NODE_VERSION=22.23.1
AIRCRAFT_NAME="A380 for X-Plane 12"
# A380_SOURCE_REPO=<git URL> builds from another fork.
SOURCE_REPO="${A380_SOURCE_REPO:-https://github.com/mzmznasipadang/A380-Xp12-FBW--Mac.git}"

say() { printf '%s\n' "$*"; }
line() { say "------------------------------------------------------------"; }
stop() {
  say ""
  say "STOPPED: $*"
  exit 1
}

ANSWER=""
ask() {
  # ask "question" "default": the reply goes in ANSWER
  if [ -n "$2" ]; then
    printf '%s [%s]: ' "$1" "$2"
  else
    printf '%s: ' "$1"
  fi
  if ! read -r ANSWER; then
    ANSWER=""
    if [ -z "$2" ]; then
      NO_MORE_INPUT=1
    fi
  fi
  if [ -z "$ANSWER" ]; then
    ANSWER="$2"
  fi
}
NO_MORE_INPUT=0

# Dragging a folder into Terminal gives quotes, backslashes or a trailing space.
clean_path() {
  p="$1"
  while [ "${p% }" != "$p" ]; do p="${p% }"; done
  case "$p" in
    \"*\") p="${p#\"}"; p="${p%\"}" ;;
    \'*\') p="${p#\'}"; p="${p%\'}" ;;
  esac
  if [ "$OS" = windows ]; then
    p="$(cygpath -u "$p" 2>/dev/null || printf '%s' "$p")"
  else
    p="$(printf '%s' "$p" | sed 's/\\\(.\)/\1/g')"
  fi
  case "$p" in
    "~") p="$HOME" ;;
    "~/"*) p="$HOME/${p#\~/}" ;;
  esac
  while [ "${p%/}" != "$p" ] && [ "$p" != "/" ]; do p="${p%/}"; done
  printf '%s' "$p"
}

xplane_running() {
  case "$OS" in
    windows) tasklist 2>/dev/null | grep -qi '^X-Plane.exe' ;;
    *) pgrep -x X-Plane > /dev/null 2>&1 ;;
  esac
}

case "$(uname -s)" in
  Darwin) OS=mac; XPL=mac.xpl; LIB=libfbw_a380_systems.dylib; ENGINE_NAME=a380x-xp-engine ;;
  Linux) OS=linux; XPL=lin.xpl; LIB=libfbw_a380_systems.so; ENGINE_NAME=a380x-xp-engine ;;
  MINGW*|MSYS*) OS=windows; XPL=win.xpl; LIB=fbw_a380_systems.dll; ENGINE_NAME=a380x-xp-engine.exe ;;
  *) stop "this system ($(uname -s)) is not supported." ;;
esac
ARCH="$(uname -m)"

line
say "$AIRCRAFT_NAME: beta install from source"
say "This computer: $OS $ARCH"
line
say ""

# ---- what this computer needs ------------------------------------------------

case "$OS" in
  mac)
    if ! xcode-select -p > /dev/null 2>&1; then
      say "Apple's Command Line Tools are needed. A window will ask to install them."
      xcode-select --install > /dev/null 2>&1
      stop "run this script again when that install has finished."
    fi
    ;;
  linux)
    for tool in cc c++ git curl tar; do
      command -v "$tool" > /dev/null 2>&1 || stop "$tool is missing. On Ubuntu or Debian: sudo apt install build-essential git curl"
    done
    ;;
  windows)
    for tool in gcc git curl; do
      command -v "$tool" > /dev/null 2>&1 || stop "$tool is missing. Install MinGW-w64 (gcc) and Git for Windows, then run this again from Git Bash."
    done
    ;;
esac

# ---- X-Plane -----------------------------------------------------------------

found_xplane=""
case "$OS" in
  mac) record="$HOME/Library/Preferences/x-plane_install_12.txt" ;;
  linux) record="$HOME/.x-plane/x-plane_install_12.txt" ;;
  windows) record="${LOCALAPPDATA:-}/x-plane_install_12.txt" ;;
esac
if [ -f "$record" ]; then
  while IFS= read -r candidate || [ -n "$candidate" ]; do
    candidate="$(clean_path "$(printf '%s' "$candidate" | tr -d '\r')")"
    if [ -d "$candidate/Aircraft" ]; then
      found_xplane="$candidate"
      break
    fi
  done < "$record"
fi

say "1. Your X-Plane 12 folder (the one holding Aircraft and Resources)."
say "   You can drag the folder into this window."
XPLANE=""
for try in 1 2 3 4 5; do
  ask "   X-Plane folder" "$found_xplane"
  [ "$NO_MORE_INPUT" = 1 ] && stop "no X-Plane folder given."
  XPLANE="$(clean_path "$ANSWER")"
  if [ -d "$XPLANE/Aircraft" ] && [ -d "$XPLANE/Resources" ]; then
    break
  fi
  say "   That folder has no Aircraft and Resources in it: $XPLANE"
  XPLANE=""
done
[ -n "$XPLANE" ] || stop "no X-Plane folder given."
AIRCRAFT="$XPLANE/Aircraft/$AIRCRAFT_NAME"

if [ -f "$XPLANE/Log.txt" ]; then
  xp_minor="$(grep -m1 -oE 'X-Plane 12\.[0-9]+' "$XPLANE/Log.txt" | head -1 | sed 's/.*12\.//')"
  if [ -n "$xp_minor" ] && [ "$xp_minor" -lt 4 ]; then
    say "   WARNING: this looks like X-Plane 12.$xp_minor. The screens need 12.4 or later here."
  fi
fi
if [ "$OS" = mac ] && [ ! -d "$XPLANE/Resources/plugins/XPLM.framework" ]; then
  say "   WARNING: no Resources/plugins/XPLM.framework in that folder. Is it the right one?"
fi
if xplane_running; then
  stop "X-Plane is running. Close it and run this again."
fi
say ""

# ---- work folder -------------------------------------------------------------

say "2. A folder for the source, the tools and the build. It needs about 20 GB."
ask "   Work folder" "$HOME/A380-beta"
WORK="$(clean_path "$ANSWER")"
mkdir -p "$WORK" || stop "could not make $WORK"
WORK="$(cd "$WORK" && pwd)"
LOG="$WORK/install.log"
: > "$LOG"
free_gb="$(df -k "$WORK" | awk 'NR==2 {print int($4/1048576)}')"
if [ -n "$free_gb" ] && [ "$free_gb" -lt 20 ]; then
  say "   WARNING: only $free_gb GB free there."
fi
say ""

# ---- source and branch -------------------------------------------------------

SRC="$WORK/source"
describe_branch() {
  case "$1" in
    main) say "the source of the last public release" ;;
    dev) say "everything being worked on" ;;
    mac-test) say "for testing on a Mac" ;;
    linux-test) say "for testing on Linux" ;;
    systems-fix) say "aircraft systems fixes" ;;
    displays-fix) say "screens and performance fixes" ;;
    *) say "" ;;
  esac
}

say "3. The source comes from the project's public GitHub repo."
if [ -d "$SRC/.git" ]; then
  # The checkout below is forced: never over someone's uncommitted work.
  [ -z "$(git -C "$SRC" status --porcelain 2>/dev/null)" ] || stop "$SRC has uncommitted changes. Commit or move them, or pick another work folder."
  git -C "$SRC" remote set-url origin "$SOURCE_REPO" >> "$LOG" 2>&1
  say "   Pulling the latest source..."
  git -C "$SRC" fetch --prune origin '+refs/heads/*:refs/remotes/origin/*' >> "$LOG" 2>&1 || stop "could not pull from $SOURCE_REPO. Check your connection. See $LOG"
else
  say "   Pulling the source. This is a few hundred MB the first time..."
  rm -rf "$SRC"
  git clone --no-checkout "$SOURCE_REPO" "$SRC" >> "$LOG" 2>&1 || stop "could not pull the source from $SOURCE_REPO. Check your connection. See $LOG"
fi
branches="$(git -C "$SRC" for-each-ref --format='%(refname:lstrip=3)' refs/remotes/origin | grep -v '^HEAD$')"
[ -n "$branches" ] || stop "the repo has no branches."
# mac-fixes carries the releases (v1.0.6 on) for every platform.
wanted=mac-fixes
say ""
say "   Branches:"
n=0
default_number=1
for b in $branches; do
  n=$((n + 1))
  printf '   %2d - %-16s %s\n' "$n" "$b" "$(describe_branch "$b")"
  [ "$b" = "$wanted" ] && default_number=$n
done
ask "   Branch number" "$default_number"
n=0
BRANCH=""
for b in $branches; do
  n=$((n + 1))
  [ "$n" = "$ANSWER" ] && BRANCH="$b"
done
[ -n "$BRANCH" ] || stop "there is no branch $ANSWER."
git -C "$SRC" checkout -q -f -B "$BRANCH" "origin/$BRANCH" >> "$LOG" 2>&1 || stop "could not check out $BRANCH. See $LOG"
[ -f "$SRC/tools/beta/mac.sh" ] || stop "branch $BRANCH is older than this script and does not build from source. Run this again and pick another branch."
[ -f "$SRC/Cargo.toml" ] && [ -d "$SRC/installer/engine" ] || stop "$SRC is not the aircraft's source."
say ""

# ---- Rust and Node -----------------------------------------------------------

TOOLS="$WORK/toolchain"
cargo_is_new_enough() {
  command -v cargo > /dev/null 2>&1 || return 1
  minor="$(cargo --version 2>/dev/null | sed -n 's/^cargo 1\.\([0-9]*\).*/\1/p')"
  [ -n "$minor" ] && [ "$minor" -ge 88 ]
}
node_is_new_enough() {
  command -v node > /dev/null 2>&1 || return 1
  major="$(node --version 2>/dev/null | sed -n 's/^v\([0-9]*\).*/\1/p')"
  [ -n "$major" ] && [ "$major" -ge 20 ]
}

if [ -x "$TOOLS/cargo/bin/cargo" ]; then
  export RUSTUP_HOME="$TOOLS/rustup" CARGO_HOME="$TOOLS/cargo"
  PATH="$TOOLS/cargo/bin:$PATH"
fi
if ! cargo_is_new_enough; then
  [ "$OS" = windows ] && stop "Rust is missing. Install it from rustup.rs with the x86_64-pc-windows-gnu toolchain, then run this again."
  say "4. Rust $RUST_VERSION is needed. It goes in $TOOLS and changes nothing else on this computer."
  ask "   Download it now? (y/n)" "y"
  case "$ANSWER" in y|Y|yes) ;; *) stop "Rust is needed to build the aircraft." ;; esac
  export RUSTUP_HOME="$TOOLS/rustup" CARGO_HOME="$TOOLS/cargo"
  mkdir -p "$TOOLS"
  curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain "$RUST_VERSION" >> "$LOG" 2>&1 || stop "could not install Rust. See $LOG"
  PATH="$TOOLS/cargo/bin:$PATH"
fi
[ "$OS" = windows ] && export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable-x86_64-pc-windows-gnu}"

node_folder=""
case "$OS-$ARCH" in
  mac-arm64) node_folder="node-v$NODE_VERSION-darwin-arm64" ;;
  mac-x86_64) node_folder="node-v$NODE_VERSION-darwin-x64" ;;
  linux-x86_64) node_folder="node-v$NODE_VERSION-linux-x64" ;;
  linux-aarch64) node_folder="node-v$NODE_VERSION-linux-arm64" ;;
esac
if [ -n "$node_folder" ] && [ -x "$TOOLS/$node_folder/bin/node" ]; then
  PATH="$TOOLS/$node_folder/bin:$PATH"
fi
if ! node_is_new_enough; then
  [ -n "$node_folder" ] || stop "Node.js 20 or later is missing. Install it and run this again."
  say "   Node.js $NODE_VERSION is needed. It goes in $TOOLS too."
  ask "   Download it now? (y/n)" "y"
  case "$ANSWER" in y|Y|yes) ;; *) stop "Node.js is needed to build the screens." ;; esac
  mkdir -p "$TOOLS"
  curl -fsSL "https://nodejs.org/dist/v$NODE_VERSION/$node_folder.tar.gz" | tar -xz -C "$TOOLS" >> "$LOG" 2>&1 || stop "could not install Node.js. See $LOG"
  PATH="$TOOLS/$node_folder/bin:$PATH"
fi
say "   Using $(cargo --version) and Node $(node --version)"
say ""

# ---- the install tool, then the rest of the questions ------------------------

export CARGO_TARGET_DIR="$WORK/target"
say "Building the install tool. This takes a few minutes the first time..."
(cd "$SRC/installer/engine" && cargo build --release) >> "$LOG" 2>&1 || stop "the install tool did not build. See $LOG"
ENGINE="$CARGO_TARGET_DIR/release/$ENGINE_NAME"
[ -x "$ENGINE" ] || stop "the install tool is missing: $ENGINE"
say ""

say "5. Textures. 2K suits most computers, and 8K needs a lot of memory."
say "    1 - 1K"
say "    2 - 2K"
say "    3 - 4K"
say "    4 - 8K"
ask "   Textures" "2"
case "$ANSWER" in
  1) TEXTURES=1024; TEXTURE_NAME=1K ;;
  2) TEXTURES=2048; TEXTURE_NAME=2K ;;
  3) TEXTURES=4096; TEXTURE_NAME=4K ;;
  4) TEXTURES=8192; TEXTURE_NAME=8K ;;
  *) stop "there is no texture choice $ANSWER." ;;
esac
say ""

RELEASE_MODE=full
if [ -f "$AIRCRAFT/fbw_xp_installed_version.txt" ]; then
  say "6. The aircraft is already installed (version $(tr -d '\r\n' < "$AIRCRAFT/fbw_xp_installed_version.txt"))."
  say "    1 - Only rebuild the code (fastest)"
  say "    2 - Update from the latest release, then rebuild"
  say "    3 - Clean reinstall, then rebuild (keeps your liveries)"
  ask "   Choice" "1"
  case "$ANSWER" in
    1) RELEASE_MODE=none ;;
    2) RELEASE_MODE=full ;;
    3) RELEASE_MODE=clean ;;
    *) stop "there is no choice $ANSWER." ;;
  esac
else
  say "6. The aircraft is not installed yet, so its models and textures download from the latest release."
fi
say ""

say "7. Liveries. Enter the numbers you want, like 1,3,5. Or enter all, or leave it empty for none."
LIVERIES_FILE="$WORK/liveries.txt"
LIVERY_IDS=""
LIVERY_NAMES=""
LIVERY_EDITION=""
if "$ENGINE" liveries-list > "$LIVERIES_FILE" 2>> "$LOG"; then
  count="$(wc -l < "$LIVERIES_FILE" | tr -d ' ')"
  n=0
  while IFS="$(printf '\t')" read -r id airline name engines editions; do
    n=$((n + 1))
    printf '   %2d - %-20s %-26s %s\n' "$n" "$airline" "$name" "$engines"
  done < "$LIVERIES_FILE"
  ask "   Liveries" ""
  NO_MORE_INPUT=0
  picks="$(printf '%s' "$ANSWER" | tr ',' ' ')"
  case "$picks" in
    all|ALL|All)
      picks="$(n=0; while [ "$n" -lt "$count" ]; do n=$((n + 1)); printf '%s ' "$n"; done)"
      ;;
  esac
  for pick in $picks; do
    case "$pick" in
      *[!0-9]*) stop "$pick is not a number." ;;
    esac
    if [ "$pick" -lt 1 ] || [ "$pick" -gt "$count" ]; then
      stop "there is no livery $pick."
    fi
    row="$(sed -n "${pick}p" "$LIVERIES_FILE")"
    id="$(printf '%s' "$row" | cut -f1)"
    case " $LIVERY_IDS " in
      *" $id "*) ;;
      *)
        LIVERY_IDS="$LIVERY_IDS $id"
        LIVERY_NAMES="$LIVERY_NAMES
     $(printf '%s' "$row" | cut -f2) $(printf '%s' "$row" | cut -f3)"
        ;;
    esac
  done
  if [ -n "$LIVERY_IDS" ]; then
    ask "   Livery textures, 2K or 4K" "2K"
    case "$ANSWER" in
      2K|2k|2) LIVERY_EDITION=2K ;;
      4K|4k|4) LIVERY_EDITION=4K ;;
      *) stop "liveries come in 2K or 4K." ;;
    esac
  fi
else
  say "   Could not fetch the livery list, so none will be installed. See $LOG"
fi
say ""

# ---- check, then go ----------------------------------------------------------

line
say "About to do this:"
say "  X-Plane:   $XPLANE"
say "  Branch:    $BRANCH"
say "  Textures:  $TEXTURE_NAME"
case "$RELEASE_MODE" in
  none) say "  Aircraft:  keep what is installed, rebuild the code" ;;
  full) say "  Aircraft:  download the latest release, then rebuild the code" ;;
  clean) say "  Aircraft:  clean reinstall, then rebuild the code" ;;
esac
if [ -n "$LIVERY_IDS" ]; then
  say "  Liveries ($LIVERY_EDITION):$LIVERY_NAMES"
else
  say "  Liveries:  none"
fi
line
ask "Go ahead? (y/n)" "n"
case "$ANSWER" in y|Y|yes) ;; *) stop "nothing was installed." ;; esac
say ""

STEP=0
STEPS=4
[ "$RELEASE_MODE" = none ] || STEPS=$((STEPS + 1))
[ -z "$LIVERY_IDS" ] || STEPS=$((STEPS + 1))
step() {
  STEP=$((STEP + 1))
  say "[$STEP/$STEPS] $1"
  printf '\n==== %s\n' "$1" >> "$LOG"
}
run() {
  # run "what failed" command...: everything goes in the log, a failure shows its end
  what="$1"
  shift
  "$@" >> "$LOG" 2>&1
  status=$?
  if [ "$status" != 0 ]; then
    say ""
    tail -30 "$LOG"
    stop "$what. The whole log is $LOG"
  fi
}

if [ "$RELEASE_MODE" != none ]; then
  step "Downloading the aircraft's models and textures. This is several GB..."
  release_args="--textures $TEXTURES"
  [ "$RELEASE_MODE" = clean ] && release_args="$release_args --clean"
  export FBW_XP_PLAIN=1
  # shellcheck disable=SC2086
  "$ENGINE" install --xplane "$XPLANE" $release_args 2>&1 | tee -a "$LOG"
  [ "${PIPESTATUS[0]}" = 0 ] || stop "the aircraft did not install. The whole log is $LOG"
  [ -d "$AIRCRAFT/plugins/fbw_a380_systems" ] || stop "the install finished but the aircraft is missing. The whole log is $LOG"
fi

step "Compiling the plugin. This can take 20 minutes or more the first time..."
PLUGIN_DIR="$AIRCRAFT/plugins/fbw_a380_systems"
BUILT_PLUGIN=""
build_plugin() {
  (cd "$SRC" && cargo build --release --features js --lib "$@")
}
if [ "$OS" = mac ]; then
  case "$ARCH" in
    arm64) this_target=aarch64-apple-darwin; other_target=x86_64-apple-darwin ;;
    *) this_target=x86_64-apple-darwin; other_target=aarch64-apple-darwin ;;
  esac
  rustup target add "$this_target" "$other_target" >> "$LOG" 2>&1
  # The macOS 27 linker can leave either slice's string table 4 bytes off an 8-byte
  # boundary, and dyld then refuses the plugin: pad it, on an unsigned copy.
  align_slice() {
    cp "$CARGO_TARGET_DIR/$1/release/$LIB" "$WORK/$1.dylib" &&
      codesign --remove-signature "$WORK/$1.dylib" &&
      python3 -I "$SRC/tools/beta/align_strtab.py" "$WORK/$1.dylib"
  }
  run "the plugin did not compile" build_plugin --target "$this_target"
  run "the plugin could not be prepared for loading" align_slice "$this_target"
  BUILT_PLUGIN="$WORK/$this_target.dylib"
  say "      Compiling the second half, so X-Plane loads it with or without Rosetta..."
  if build_plugin --target "$other_target" >> "$LOG" 2>&1; then
    run "the second half could not be prepared for loading" align_slice "$other_target"
    run "the two halves of the plugin did not join" lipo -create "$BUILT_PLUGIN" "$WORK/$other_target.dylib" -output "$WORK/mac.xpl"
    BUILT_PLUGIN="$WORK/mac.xpl"
  else
    say "      WARNING: the second half did not compile. X-Plane under Rosetta will not load this plugin."
  fi
else
  run "the plugin did not compile" build_plugin
  BUILT_PLUGIN="$CARGO_TARGET_DIR/release/$LIB"
fi
[ -f "$BUILT_PLUGIN" ] || stop "the plugin is missing after the build: $BUILT_PLUGIN"

step "Building the screens..."
INSTRUMENTS="$WORK/instruments"
run "the screens' packages did not install" sh -c "cd '$SRC/fbw/xplane/xplane' && npm ci --no-audit --no-fund"
rm -rf "$INSTRUMENTS"
run "the screens did not build" sh -c "cd '$SRC/fbw/xplane' && node xplane/build.mjs --out '$INSTRUMENTS' --library '$SRC/xp-ui'"
[ -f "$INSTRUMENTS/html_ui/Pages/VCockpit/Instruments/A380X/PFD/pfd.html" ] || stop "the screens built but the PFD page is missing. The whole log is $LOG"

step "Installing into X-Plane..."
xplane_running && stop "X-Plane was started while building. Close it and run this again; the build is kept."
[ -d "$PLUGIN_DIR" ] || stop "the aircraft is not installed at $AIRCRAFT."
mkdir -p "$PLUGIN_DIR/64" "$PLUGIN_DIR/XPHFBW/ui" "$PLUGIN_DIR/XPHFBW/js" "$AIRCRAFT/html_ui"
[ -f "$PLUGIN_DIR/64/$XPL" ] && cp "$PLUGIN_DIR/64/$XPL" "$WORK/$XPL.previous"
run "the plugin could not be copied" cp "$BUILT_PLUGIN" "$PLUGIN_DIR/64/$XPL"
STAGE="$WORK/stage"
rm -rf "$STAGE"
mkdir -p "$STAGE"
run "the screens could not be staged" cp -R "$INSTRUMENTS/html_ui" "$STAGE/html_ui"
rm -rf "$STAGE/html_ui/Pages/VCockpit/Instruments/A380X/EFB" "$STAGE/html_ui/Pages/VCockpit/Instruments/A380X/OITlegacy" "$STAGE/html_ui/Pages/VCockpit/Instruments/A380X/popup"
ASSETS="$SRC/fbw/xplane/fbw-a380x/src/base/flybywire-aircraft-a380-842/html_ui"
mkdir -p "$STAGE/html_ui/Fonts" "$STAGE/html_ui/Images"
run "the fonts could not be staged" cp -R "$ASSETS/Fonts/." "$STAGE/html_ui/Fonts/"
run "the images could not be staged" cp -R "$ASSETS/Images/." "$STAGE/html_ui/Images/"
# FlyByWire's build copies src/images into Images/fbw-a380x (the OANS map's SVG icons).
mkdir -p "$STAGE/html_ui/Images/fbw-a380x"
run "the images could not be staged" cp -R "$SRC/fbw/xplane/fbw-a380x/src/images/." "$STAGE/html_ui/Images/fbw-a380x/"
run "our pages could not be staged" cp -R "$SRC/html_ui/." "$STAGE/html_ui/"
run "the screens could not be installed" cp -R "$STAGE/html_ui/." "$AIRCRAFT/html_ui/"
for json in "$INSTRUMENTS"/*.json; do
  [ -f "$json" ] && cp "$json" "$AIRCRAFT/"
done
run "the EFB pages could not be installed" cp -R "$SRC/app/ui/." "$PLUGIN_DIR/XPHFBW/ui/"
run "the page runtime could not be installed" cp -R "$SRC/app/js/." "$PLUGIN_DIR/XPHFBW/js/"
run "the page runtime could not be installed" cp "$INSTRUMENTS/runtime/xplane-js-data.js" "$PLUGIN_DIR/XPHFBW/js/xplane-js-data.js"
[ -f "$SRC/distribution/THIRD_PARTY_NOTICES.txt" ] && cp "$SRC/distribution/THIRD_PARTY_NOTICES.txt" "$AIRCRAFT/"

if [ "$OS" = mac ]; then
  xattr -dr com.apple.quarantine "$AIRCRAFT" >> "$LOG" 2>&1
  codesign --force --sign - "$PLUGIN_DIR/64/$XPL" >> "$LOG" 2>&1 || say "      WARNING: could not sign the plugin. See $LOG"
  say "      Plugin holds: $(lipo -archs "$PLUGIN_DIR/64/$XPL" 2>/dev/null)"
  nm -gU "$PLUGIN_DIR/64/$XPL" 2>/dev/null | grep -q '_XPluginStart' || stop "the plugin has no XPluginStart, so X-Plane cannot load it."
elif [ "$OS" = linux ]; then
  nm -D --defined-only "$PLUGIN_DIR/64/$XPL" 2>/dev/null | grep -q 'XPluginStart' || stop "the plugin has no XPluginStart, so X-Plane cannot load it."
fi

commit="$(git -C "$SRC" rev-parse --short HEAD 2>/dev/null || printf 'unknown')"
{
  say "branch $BRANCH"
  say "commit $commit"
  say "built $(date) on $OS $ARCH"
} > "$AIRCRAFT/beta-source.txt"

if [ -n "$LIVERY_IDS" ]; then
  step "Downloading the liveries..."
  export FBW_XP_PLAIN=1
  for id in $LIVERY_IDS; do
    say "      $id"
    run "the livery $id did not install" "$ENGINE" livery-install --xplane "$XPLANE" --id "$id" --edition "$LIVERY_EDITION"
  done
  [ "$OS" = mac ] && xattr -dr com.apple.quarantine "$AIRCRAFT/liveries" >> "$LOG" 2>&1
fi

say ""
line
say "Done: $AIRCRAFT_NAME, branch $BRANCH, commit $commit."
say "Start X-Plane and load the aircraft from Aircraft/$AIRCRAFT_NAME."
say "If something is wrong, send these two files:"
say "  $XPLANE/Log.txt"
say "  $LOG"
line
