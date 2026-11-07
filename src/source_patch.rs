//! A text-level fix applied to FlyByWire's own compiled JS as it loads
//! (`docs/js-build.md`), shared by both engines that can serve that JS: the
//! plugin's in-process QuickJS `Cockpit` (`src/js/msfs/mod.rs`, behind
//! `feature = "js"`) and XPHFBW's separate `coui://` handler
//! (`app/src/scheme.rs`), a different binary (`XPHFBW.exe`) that links this
//! crate WITHOUT that feature. `SourcePatch` and `apply` live here,
//! unconditionally compiled and with no dependency on `rquickjs`/`oxc`, so
//! both can reach them -- before this module existed, every `SourcePatch`
//! in the crate (`ecam_patches.rs`, `wxr/mod.rs`, `oans/plugin.rs`,
//! `deep/ecam/patches.rs`) was defined behind `feature = "js"` purely
//! because this struct was declared inside the `js`-feature module, so
//! XPHFBW could not even name the type, let alone apply the patches -- and
//! XPHFBW, not the QuickJS `Cockpit`, is what the user actually sees almost
//! all the time (`docs/deep/debug_screen_clicks.md`).

/// A change to a script as it loads: in the file at `path` (under
/// html_ui), the text `find`, which must occur exactly once, becomes
/// `replace`. A patch that does not match is reported and the file runs
/// unchanged.
#[derive(Clone, Debug)]
pub struct SourcePatch {
    pub path: String,
    pub find: String,
    pub replace: String,
    /// Why, for the log.
    pub reason: String,
}

/// Applies every patch in `patches` whose `path` case-insensitively equals
/// `clean` (the VFS-relative path both callers already resolve their own
/// way -- `js/msfs/mod.rs`'s `clean`, `app/src/scheme.rs`'s `path`) to
/// `text`, in the order given. A patch applies only when its `find` occurs
/// in `text` exactly once; `log(true, ...)` reports a successful patch,
/// `log(false, ...)` an anchor that matched zero or more-than-one times
/// (the file is left unchanged for that one patch either way) -- callers
/// turn that bool into whatever their own log takes (`js/msfs/mod.rs`:
/// `LogLevel::Info`/`LogLevel::Error`; `app/src/scheme.rs`:
/// `crate::logging::log`, which has no level).
/// The message list and the other dialogs stop 10 px above the footer, and the bottom edges of
/// the page's own buttons showed under them.
pub fn mfd_dialog_height_patch() -> SourcePatch {
    SourcePatch {
        path: "/Pages/VCockpit/Instruments/A380X/MFD/mfd.css".to_string(),
        find: "  width: 768px;\n  height: 852px;\n  overflow: visible;".to_string(),
        replace: "  width: 768px;\n  height: 862px;\n  overflow: visible;".to_string(),
        reason: "MFD dialogs cover the page down to the footer".to_string(),
    }
}

pub fn apply(patches: &[SourcePatch], clean: &str, mut text: String, mut log: impl FnMut(bool, &str)) -> String {
    for patch in patches.iter().filter(|p| p.path.eq_ignore_ascii_case(clean)) {
        let (spans, _) = locate(&text, &patch.find);
        if spans.len() == 1 {
            text.replace_range(spans[0].clone(), &patch.replace);
            log(true, &format!("{}: {}", patch.path, patch.reason));
        } else {
            log(
                false,
                &format!("{}: the patch ({}) matches {} times, not once; the file runs unchanged", patch.path, patch.reason, spans.len()),
            );
        }
    }
    text
}

pub fn count(text: &str, find: &str) -> usize {
    locate(text, find).0.len()
}

pub fn locate(text: &str, find: &str) -> (Vec<std::ops::Range<usize>>, bool) {
    let exact: Vec<_> = text.match_indices(find).map(|(i, m)| i..i + m.len()).collect();
    if !exact.is_empty() {
        return (exact, true);
    }
    let pattern = compile(&short_numbers(&drop_arrow_parens(find)));
    (find_all(text, &pattern), false)
}

#[derive(Debug, PartialEq)]
enum Part {
    Lit(String),
    Ws,
    OptSemi,
}

fn push_lit(parts: &mut Vec<Part>, s: &str) {
    if s.is_empty() {
        return;
    }
    if let Some(Part::Lit(l)) = parts.last_mut() {
        l.push_str(s);
    } else {
        parts.push(Part::Lit(s.to_string()));
    }
}

fn push_ws(parts: &mut Vec<Part>) {
    if !matches!(parts.last(), Some(Part::Ws) | None) {
        parts.push(Part::Ws);
    }
}

fn skip_gap(c: &[char], mut i: usize) -> usize {
    loop {
        while i < c.len() && c[i].is_whitespace() {
            i += 1;
        }
        if i + 1 < c.len() && c[i] == '/' && c[i + 1] == '/' {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if i + 1 < c.len() && c[i] == '/' && c[i + 1] == '*' {
            i += 2;
            while i + 1 < c.len() && !(c[i] == '*' && c[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(c.len());
        } else {
            return i;
        }
    }
}

fn compile(find: &str) -> Vec<Part> {
    let c: Vec<char> = find.chars().collect();
    let mut parts = Vec::new();
    code(&c, 0, &mut parts, false);
    while matches!(parts.last(), Some(Part::Ws)) {
        parts.pop();
    }
    if let Some(Part::Lit(l)) = parts.last_mut() {
        if l.ends_with(';') {
            l.pop();
            if l.is_empty() {
                parts.pop();
            }
            parts.push(Part::OptSemi);
        }
    }
    parts
}

fn code(c: &[char], mut i: usize, parts: &mut Vec<Part>, in_template: bool) -> usize {
    let mut depth = 0usize;
    while i < c.len() {
        let ch = c[i];
        let gap = skip_gap(c, i);
        if gap > i {
            push_ws(parts);
            i = gap;
            continue;
        }
        match ch {
            '\'' | '"' => {
                let start = i;
                i += 1;
                while i < c.len() && c[i] != ch {
                    if c[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(c.len());
                push_lit(parts, &c[start..i].iter().collect::<String>());
            }
            '`' => {
                push_lit(parts, "`");
                i += 1;
                while i < c.len() && c[i] != '`' {
                    if c[i] == '\\' && i + 1 < c.len() {
                        push_lit(parts, &c[i..i + 2].iter().collect::<String>());
                        i += 2;
                    } else if c[i] == '$' && i + 1 < c.len() && c[i + 1] == '{' {
                        push_lit(parts, "${");
                        i = code(c, i + 2, parts, true);
                        push_lit(parts, "}");
                    } else {
                        push_lit(parts, &c[i].to_string());
                        i += 1;
                    }
                }
                push_lit(parts, "`");
                i = (i + 1).min(c.len());
            }
            ';' if c.get(skip_gap(c, i + 1)) == Some(&'}') => {
                parts.push(Part::OptSemi);
                i += 1;
            }
            '{' => {
                depth += 1;
                push_lit(parts, "{");
                i += 1;
            }
            '}' if in_template && depth == 0 => return i + 1,
            '}' => {
                depth = depth.saturating_sub(1);
                push_lit(parts, "}");
                i += 1;
            }
            _ => {
                push_lit(parts, &ch.to_string());
                i += 1;
            }
        }
    }
    i
}

fn skip_text_gap(text: &[u8], mut at: usize) -> usize {
    loop {
        while at < text.len() && text[at].is_ascii_whitespace() {
            at += 1;
        }
        if text.get(at) == Some(&b'/') && text.get(at + 1) == Some(&b'*') {
            at += 2;
            while at + 1 < text.len() && !(text[at] == b'*' && text[at + 1] == b'/') {
                at += 1;
            }
            at = (at + 2).min(text.len());
        } else {
            return at;
        }
    }
}

fn match_at(text: &[u8], mut at: usize, parts: &[Part]) -> Option<usize> {
    for part in parts {
        match part {
            Part::Lit(l) => {
                let lb = l.as_bytes();
                let mut nest: Vec<Option<u8>> = vec![None];
                let mut braces: Vec<u32> = vec![0];
                for (k, &ch) in lb.iter().enumerate() {
                    let in_code = matches!(nest.last(), Some(None));
                    if in_code && k > 0 && !(is_ident(lb[k - 1]) && is_ident(ch)) {
                        at = skip_text_gap(text, at);
                        if ch == b'}' && lb[k - 1] != b';' && text.get(at) == Some(&b';') {
                            at = skip_text_gap(text, at + 1);
                        }
                    }
                    if text.get(at) != Some(&ch) {
                        return None;
                    }
                    at += 1;
                    let prev = lb.get(k.wrapping_sub(1)).copied();
                    match nest.last().copied().flatten() {
                        Some(q) => {
                            if ch == q && prev != Some(b'\\') {
                                nest.pop();
                            } else if q == b'`' && ch == b'{' && prev == Some(b'$') {
                                nest.push(None);
                                braces.push(0);
                            }
                        }
                        None => match ch {
                            b'"' | b'\'' | b'`' => nest.push(Some(ch)),
                            b'{' => *braces.last_mut().unwrap() += 1,
                            b'}' => {
                                if *braces.last().unwrap() == 0 && nest.len() > 1 {
                                    nest.pop();
                                    braces.pop();
                                } else {
                                    let d = braces.last_mut().unwrap();
                                    *d = d.saturating_sub(1);
                                }
                            }
                            _ => {}
                        },
                    }
                }
            }
            Part::Ws => {
                at = skip_text_gap(text, at);
            }
            Part::OptSemi => {
                if text.get(at) == Some(&b';') {
                    at += 1;
                }
            }
        }
    }
    Some(at)
}

fn find_all(text: &str, parts: &[Part]) -> Vec<std::ops::Range<usize>> {
    let Some(Part::Lit(first)) = parts.first() else { return Vec::new() };
    let fb = first.as_bytes();
    let mut cut = 1;
    while cut < fb.len() && is_ident(fb[cut - 1]) && is_ident(fb[cut]) {
        cut += 1;
    }
    let anchor = &first[..first.char_indices().map(|(i, _)| i).find(|&i| i >= cut).unwrap_or(first.len())];
    let bytes = text.as_bytes();
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    for (i, _) in text.match_indices(anchor) {
        if out.last().is_some_and(|r| i < r.end) {
            continue;
        }
        if let Some(end) = match_at(bytes, i, parts) {
            out.push(i..end);
        }
    }
    out
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn drop_arrow_parens(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'(' && (i == 0 || !is_ident(b[i - 1])) {
            let mut j = i + 1;
            while j < b.len() && b[j] == b' ' {
                j += 1;
            }
            let id = j;
            while j < b.len() && is_ident(b[j]) {
                j += 1;
            }
            let id_end = j;
            while j < b.len() && b[j] == b' ' {
                j += 1;
            }
            if id_end > id && !b[id].is_ascii_digit() && j < b.len() && b[j] == b')' {
                let mut k = j + 1;
                while k < b.len() && b[k] == b' ' {
                    k += 1;
                }
                if s[k..].starts_with("=>") {
                    out.push_str(&s[id..id_end]);
                    i = j + 1;
                    continue;
                }
            }
        }
        let ch = s[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn short_numbers(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == b'\\' && i + 1 < b.len() {
                out.push_str(&s[i..i + 2]);
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
        } else if c == b'"' || c == b'\'' {
            quote = Some(c);
        } else if c.is_ascii_digit() && (i == 0 || !(is_ident(b[i - 1]) || b[i - 1] == b'.')) {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.') {
                j += 1;
            }
            if j < b.len() && is_ident(b[j]) {
                out.push_str(&s[i..j]);
                i = j;
                continue;
            }
            let lit = &s[i..j];
            if let Some(frac) = lit.strip_prefix("0.").filter(|f| !f.is_empty() && f.bytes().all(|d| d.is_ascii_digit())) {
                out.push('.');
                out.push_str(frac);
            } else if lit.bytes().all(|d| d.is_ascii_digit()) && lit.len() > 1 {
                let zeros = lit.len() - lit.trim_end_matches('0').len();
                let head = lit.trim_end_matches('0');
                let exp = format!("{head}e{zeros}");
                if zeros > 0 && !head.is_empty() && exp.len() < lit.len() {
                    out.push_str(&exp);
                } else {
                    out.push_str(lit);
                }
            } else {
                out.push_str(lit);
            }
            i = j;
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

pub fn flight_plan_backup_patches() -> Vec<SourcePatch> {
    const MFD: &str = "/Pages/VCockpit/Instruments/A380X/MFD/mfd.js";
    vec![
        SourcePatch {
            path: MFD.to_string(),
            find: "if(!this.master){setTimeout(()=>this.sendEvent(\"flightPlanManager.syncRequest\",void 0),5e3)}else{this._initialized.set(true)}".to_string(),
            replace: "if(!this.master){setTimeout(()=>this.sendEvent(\"flightPlanManager.syncRequest\",void 0),5e3)}else{this._initialized.set(true)}if(this.master){const __xpKey=\"A380X_XP_FPLN_BACKUP\";const __xpSelf=this;this.__xpBorn=Date.now();let __xpLast=\"\";let __xpStamp=0;const __xpLoad=()=>{try{const raw=loadStored(__xpKey);return raw?JSON.parse(raw):null}catch(e){return null}};setTimeout(async()=>{try{const b=__xpLoad();if(!b||!b.plans||Date.now()-b.t>6e5){return}const a=__xpSelf.has(0)?__xpSelf.get(0):void 0;if(a&&a.legCount>0){return}for(const[i,sp]of Object.entries(b.plans)){const ix=parseInt(i);const np=await FlightPlan.fromSerializedFlightPlan(__xpSelf.context,ix,sp,__xpSelf.bus,__xpSelf.performanceDataInit.clone(),__xpSelf.time.get());__xpSelf.set(ix,np)}console.warn(\"A380 X-Plane: flight plan restored from the in-flight backup\")}catch(e){console.error(\"A380 X-Plane: flight plan restore failed\",e)}},3e3);setInterval(()=>{try{const a=__xpSelf.has(0)?__xpSelf.get(0):void 0;if(!a||a.legCount===0){return}const plans={0:a.serialize()};const text=JSON.stringify({plans});const now=Date.now();if(text!==__xpLast||now-__xpStamp>6e4){__xpLast=text;__xpStamp=now;saveStored(__xpKey,JSON.stringify({t:now,plans}))}}catch(e){}},5e3)}".to_string(),
            reason: "the master FMS keeps a backup of the active flight plan and restores it if it restarts empty mid-flight".to_string(),
        },
        SourcePatch {
            path: MFD.to_string(),
            find: "reset(notify=true){const activePlan=this.has(0)?this.get(0):void 0;".to_string(),
            replace: "reset(notify=true){if(this.master&&this.__xpBorn&&Date.now()-this.__xpBorn>3e4){try{saveStored(\"A380X_XP_FPLN_BACKUP\",\"\")}catch(e){}}const activePlan=this.has(0)?this.get(0):void 0;".to_string(),
            reason: "a deliberate flight plan reset clears the in-flight backup".to_string(),
        },
        SourcePatch {
            path: MFD.to_string(),
            find: "getClimbThrustCorrectedN1(parameters,pressureAltitude,outsideTemperature){let loAltRow=0;let hiAltRow=0;".to_string(),
            replace: "getClimbThrustCorrectedN1(parameters,pressureAltitude,outsideTemperature){if(!Number.isFinite(pressureAltitude)){if(!this.__xpWarnedAlt){this.__xpWarnedAlt=true;console.warn(\"A380 X-Plane: climb thrust asked for a non-numeric altitude\",pressureAltitude)}pressureAltitude=0}if(!Number.isFinite(outsideTemperature)){outsideTemperature=15}let loAltRow=0;let hiAltRow=0;".to_string(),
            reason: "EngineModel.getClimbThrustCorrectedN1 read row -1 of its table for an altitude below the first row or not a number, throwing inside VNAV's profile build every recompute; with no profile the distance to destination is unknown, which lets an FCU altitude below the aircraft start a descent thousands of miles out".to_string(),
        },
        SourcePatch {
            path: MFD.to_string(),
            find: "hiAltRow=parameters.cn1ClimbLimit.reduce((acc,val,idx)=>{return val[0]<=pressureAltitude?idx+1:acc},0);loAltRow=hiAltRow-1}".to_string(),
            replace: "hiAltRow=Math.max(1,parameters.cn1ClimbLimit.reduce((acc,val,idx)=>{return val[0]<=pressureAltitude?idx+1:acc},0));loAltRow=hiAltRow-1}".to_string(),
            reason: "the climb thrust table lookup never goes below its first row".to_string(),
        },
        SourcePatch {
            path: MFD.to_string(),
            find: "case Vars.read(\"fbw/GEAR_IS_ON_GROUND\",\"bool\")>0:this.changePhase(0);break;".to_string(),
            replace: "case Vars.read(\"fbw/GEAR_IS_ON_GROUND\",\"bool\")>0||Vars.read(\"fbw/PLANE_ALT_ABOVE_GROUND_MINUS_CG\",\"feet\")<1:this.changePhase(0);break;".to_string(),
            reason: "the spawn phase check read GEAR IS ON GROUND as 0 on a runway start and began in CLIMB; height above ground under 1 ft also counts as on the ground".to_string(),
        },
    ]
}

/// `amdb.ts`'s two Navigraph calls in `nd.js`, sent through `fetch` to XPHFBW's `/__plugin/` route instead of axios,
/// so the plugin answers them from the local AMDB folder (`oans::plugin`).
/// The RMP reads the radios' frequencies once, as the session starts. A page's first read of
/// a variable returns 0 until the host has resolved it on its next tick, so it stored 0 and
/// came up blank. Read them until they answer, for up to ten seconds.
pub fn rmp_start_frequency_patches() -> Vec<SourcePatch> {
    const FIND: &str = "Waits.forValue(SessionState.get(), (v) => v === SessionPhase.flying, true).then(() => {\n          if (this.index === 3) {";
    const REPLACE: &str = "Waits.forValue(SessionState.get(), (v) => v === SessionPhase.flying, true).then(async () => {\n          for (let i = 0; i < 100; i++) {\n            const active = Vars.read(`fbw/COM_ACTIVE_FREQUENCY_${this.index}`, \"Frequency BCD32\");\n            const standby = Vars.read(`fbw/COM_STANDBY_FREQUENCY_${this.index}`, \"Frequency BCD32\");\n            if (active && standby) break;\n            await new Promise((r) => setTimeout(r, 100));\n          }\n          if (this.index === 3) {";
    vec![SourcePatch {
        path: "/Pages/VCockpit/Instruments/A380X/RMP/rmp.js".to_string(),
        find: FIND.to_string(),
        replace: REPLACE.to_string(),
        reason: "the RMP's one read of the radios' frequencies came back 0: it was the page's first read of them".to_string(),
    }]
}

/// `LightSync.ts` sets the RMP knobs at start with `#RMP_ID#` left in the variable's name and
/// the value as percent times 100, so the real knobs stayed at 0 and all three RMPs stayed dark.
pub fn rmp_start_brightness_patch() -> SourcePatch {
    SourcePatch {
        path: "/Pages/VCockpit/Instruments/A380X/ExtrasHost/index.js".to_string(),
        find: "Vars.write(`L:A380X_RMP_#RMP_ID#_BRIGHTNESS_KNOB`, \"number\" /* Number */, brightness);".to_string(),
        replace: "Vars.write(`L:A380X_RMP_${rmp}_BRIGHTNESS_KNOB`, \"number\" /* Number */, brightness / 100);".to_string(),
        reason: "LightSync wrote the RMP knobs under an unsubstituted template name, 100 times too large".to_string(),
    }
}

/// The OANS clears its BTV layer and its map layers with the transform the last draw left
/// (translated to the map centre), and a canvas clears through its transform: everything above
/// and left of the centre stayed, so every redraw of the stop bars added to the old ones.
pub fn oans_clear_patches() -> Vec<SourcePatch> {
    const NDJS: &str = "/Pages/VCockpit/Instruments/A380X/ND/nd.js";
    let patch = |find: &str, replace: &str, why: &str| SourcePatch { path: NDJS.to_string(), find: find.to_string(), replace: replace.to_string(), reason: why.to_string() };
    vec![
        patch(
            "(_b6 = this.canvasRef.target.getContext(\"2d\")) == null ? void 0 : _b6.clearRect(0, 0, this.canvasRef.target.width, this.canvasRef.target.height);",
            "(_b6 = this.canvasRef.target.getContext(\"2d\")) == null ? void 0 : (_b6.resetTransform(), _b6.clearRect(0, 0, this.canvasRef.target.width, this.canvasRef.target.height));",
            "OansBrakeToVacateSelection.drawBtvLayer clears the whole BTV layer",
        ),
        patch(
            "        if (ctx) {\n          ctx.clearRect(0, 0, cw, ch);\n        }",
            "        if (ctx) {\n          ctx.resetTransform();\n          ctx.clearRect(0, 0, cw, ch);\n        }",
            "Oanc.clearMap clears the whole of every map layer",
        ),
    ]
}

pub fn amdb_fetch_patches() -> Vec<SourcePatch> {
    const OLD_LINE: &str = "const response = await navigraphRequest.get(`https://amdb.api.navigraph.com/v1/${query}`);";
    const NEW_LINE: &str = "const response = { data: await (await fetch(`/__plugin/amdb/v1/${query}`)).json() };";
    const NDJS: &str = "/Pages/VCockpit/Instruments/A380X/ND/nd.js";
    let patch = |find: &str, why: &str| SourcePatch {
        path: NDJS.to_string(),
        find: find.to_string(),
        replace: find.replacen(OLD_LINE, NEW_LINE, 1),
        reason: format!(
            "{why}: navigraphRequest needs XMLHttpRequest, which nothing here defines (axios's \
             getDefaultAdapter only tries `typeof XMLHttpRequest !== \"undefined\"`), so it is \
             sent through fetch() to /__plugin/amdb instead, which src/oans/plugin.rs answers locally"
        ),
    };
    vec![
        // amdb.ts:8-17 (searchAmdbAirports).
        patch(
            "    let query = \"search\";\n    query += `?q=${queryString}`;\n    navigraphAuth;\n    const response = await navigraphRequest.get(`https://amdb.api.navigraph.com/v1/${query}`);",
            "amdb.ts searchAmdbAirports",
        ),
        // amdb.ts:19-39 (getAmdbData).
        patch(
            "    query += `&include=${includeString}`;\n    navigraphAuth;\n    const response = await navigraphRequest.get(`https://amdb.api.navigraph.com/v1/${query}`);",
            "amdb.ts getAmdbData",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(find: &str, replace: &str) -> SourcePatch {
        SourcePatch { path: "/a.js".to_string(), find: find.to_string(), replace: replace.to_string(), reason: "test".to_string() }
    }

    #[test]
    fn every_static_patch_matches_the_installed_pages_once() {
        let Some(root) = std::env::var_os("FBW_HTML_UI") else { return };
        let bad: Vec<String> = crate::static_source_patches()
            .into_iter()
            .filter_map(|p| {
                let text = std::fs::read_to_string(std::path::Path::new(&root).join(p.path.trim_start_matches('/'))).unwrap_or_default();
                let n = count(&text, &p.find);
                (n != 1).then(|| format!("{} x{n}: {}", p.path, p.reason))
            })
            .collect();
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    #[test]
    fn writes_every_patched_page() {
        let (Some(root), Some(out)) = (std::env::var_os("FBW_HTML_UI"), std::env::var_os("FBW_PATCHED_OUT")) else { return };
        let patches = crate::static_source_patches();
        let mut paths: Vec<String> = patches.iter().map(|p| p.path.clone()).collect();
        paths.sort();
        paths.dedup();
        for path in paths {
            let rel = path.trim_start_matches('/');
            let text = std::fs::read_to_string(std::path::Path::new(&root).join(rel)).unwrap_or_default();
            let idents = |s: &str| -> std::collections::BTreeSet<String> {
                s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$')).filter(|w| w.chars().next().is_some_and(|c| !c.is_ascii_digit())).map(str::to_string).collect()
            };
            let known = idents(&text);
            for p in patches.iter().filter(|p| p.path == path) {
                let missing: Vec<String> = idents(&p.replace).difference(&known).cloned().collect();
                if !missing.is_empty() {
                    println!("{path}: {}: new names {missing:?}", p.reason.chars().take(60).collect::<String>());
                }
            }
            let mut failed = Vec::new();
            let patched = apply(&patches, &path, text, |ok, msg| if !ok { failed.push(msg.to_string()) });
            assert!(failed.is_empty(), "{}", failed.join("\n"));
            let dest = std::path::Path::new(&out).join(rel);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::write(dest, patched).unwrap();
        }
    }

    #[test]
    fn a_patch_matching_once_is_applied_and_reported_true() {
        let mut ok = None;
        let out = apply(&[patch("x", "y")], "/a.js", "ax".to_string(), |o, _| ok = Some(o));
        assert_eq!(out, "ay");
        assert_eq!(ok, Some(true));
    }

    #[test]
    fn a_patch_matching_zero_or_many_times_is_left_unchanged_and_reported_false() {
        let mut ok = None;
        let out = apply(&[patch("x", "y")], "/a.js", "no match here".to_string(), |o, _| ok = Some(o));
        assert_eq!(out, "no match here");
        assert_eq!(ok, Some(false));

        let mut ok2 = None;
        let out2 = apply(&[patch("x", "y")], "/a.js", "x and x again".to_string(), |o, _| ok2 = Some(o));
        assert_eq!(out2, "x and x again");
        assert_eq!(ok2, Some(false));
    }

    #[test]
    fn a_patch_written_against_readable_output_matches_the_minified_build() {
        let find = "      this.wxr1Failed = Subject.create(true);\n      // radar\n      this.wxr2Failed = Subject.create(true);\n    }";
        let bundle = "x;this.wxr1Failed=Subject.create(true);this.wxr2Failed=Subject.create(true)}init(){}";
        assert_eq!(count(bundle, find), 1);
        let out = apply(&[patch(find, "this.wxr1Failed = Subject.create(false); this.wxr2Failed = Subject.create(false); }")], "/a.js", bundle.to_string(), |o, _| assert!(o));
        assert_eq!(out, "x;this.wxr1Failed = Subject.create(false); this.wxr2Failed = Subject.create(false); }init(){}");
    }

    #[test]
    fn arrow_parens_and_template_expressions_follow_the_minified_build() {
        let find = "bus.getSubscriber().on(`x_${ a + 1 }`).handle((v) => this.pub(\"y\", v));";
        let bundle = "bus.getSubscriber().on(`x_${a+1}`).handle(v=>this.pub(\"y\",v))}";
        assert_eq!(count(bundle, find), 1);
        assert_eq!(count("bus.getSubscriber().on(`x_${a+2}`)", find), 0);
    }

    #[test]
    fn numbers_are_matched_in_their_minified_spelling() {
        assert_eq!(count("[null,null,x.get()<.35]},y=1e3", "[null, null, x.get() < 0.35]"), 1);
        assert_eq!(count("y=1e3;z=100", "y = 1000;"), 1);
        assert_eq!(count("y=1e3;z=100", "z = 100"), 1);
        assert_eq!(count("s=\"0.35\"", "s = \"0.35\""), 1);
    }

    #[test]
    fn a_patch_for_a_different_path_does_not_apply() {
        let out = apply(&[patch("x", "y")], "/b.js", "ax".to_string(), |_, _| panic!("must not log"));
        assert_eq!(out, "ax");
    }
}
