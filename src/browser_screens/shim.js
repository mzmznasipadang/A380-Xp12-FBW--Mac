// window.__xphfbw for a page in X-Plane's own browser. Same contract as
// app/src/renderer.rs: variables, calls and events over the plugin's local
// server (src/browser_screens/server.rs), with the synchronous reads as
// synchronous requests.
(function () {
  "use strict";
  const parsed = /^xphfbw:(\d+):(.*)$/.exec(window.name || "");
  const efb = window.name === "xphfbw:efb";
  if (!parsed && !efb) return;
  const view = efb ? 31 : +parsed[1];
  const screen = efb ? "SCREEN_EFB" : parsed[2];

  function sync(method, url, body) {
    const x = new XMLHttpRequest();
    x.open(method, url, false);
    x.send(body === undefined ? null : body);
    return x;
  }
  function q(s) {
    return encodeURIComponent(s === undefined || s === null ? "" : String(s));
  }

  const out = [];
  let ws = null;
  let scheduled = false;
  function flush() {
    scheduled = false;
    if (out.length && ws && ws.readyState === 1) {
      ws.send(JSON.stringify(out));
      out.length = 0;
    }
  }
  function send(op) {
    out.push(op);
    if (!scheduled) {
      scheduled = true;
      queueMicrotask(flush);
    }
  }

  // Keystrokes the plugin's key sniffer took for this page.
  function isText(el) {
    return !!el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable);
  }
  function typeKey(ch, vkey) {
    const el = document.activeElement || document.body;
    const key = vkey === 8 ? "Backspace" : vkey === 13 ? "Enter" : vkey === 9 ? "Tab" : vkey === 46 ? "Delete" : String.fromCharCode(ch);
    const printable = ch >= 0x20 && ch < 0x7f;
    const init = { key, keyCode: printable ? ch : vkey, which: printable ? ch : vkey, charCode: printable ? ch : 0, bubbles: true, cancelable: true };
    const down = el.dispatchEvent(new KeyboardEvent("keydown", init));
    let press = true;
    if (printable || vkey === 13) press = el.dispatchEvent(new KeyboardEvent("keypress", init));
    if (down && press && isText(el)) {
      if (printable) document.execCommand("insertText", false, String.fromCharCode(ch));
      else if (vkey === 8) document.execCommand("delete");
      else if (vkey === 46) document.execCommand("forwardDelete");
    }
    el.dispatchEvent(new KeyboardEvent("keyup", init));
  }
  // The MFD's fields are focusable spans that read keypress themselves, not text inputs.
  const takesKeys = (el) => isText(el) || (!!el && !!el.classList && el.classList.contains("mfd-input-field-text-input"));
  document.addEventListener("focusin", (e) => { if (takesKeys(e.target)) send(["f", 1]); }, true);
  document.addEventListener("focusout", (e) => { if (takesKeys(e.target)) send(["f", 0]); }, true);

  // A page only sees focus when its browser has it, which nothing here gives it.
  const realHasFocus = document.hasFocus.bind(document);
  document.hasFocus = () => true;
  const realFocus = HTMLElement.prototype.focus;
  HTMLElement.prototype.focus = function (options) {
    const before = document.activeElement;
    realFocus.call(this, options);
    if (!realHasFocus() && before !== this && document.activeElement === this) {
      if (before) before.dispatchEvent(new FocusEvent("blur"));
      this.dispatchEvent(new FocusEvent("focus"));
      this.dispatchEvent(new FocusEvent("focusin", { bubbles: true }));
    }
  };

  let onFrame = () => {};
  let onRecords = () => {};
  function connect() {
    const socket = new WebSocket(`ws://${location.host}/__b/ws?v=${view}${efb ? "&lite=1" : ""}`);
    socket.binaryType = "arraybuffer";
    socket.onopen = () => { ws = socket; flush(); };
    socket.onclose = () => { ws = null; setTimeout(connect, 1000); };
    socket.onmessage = (ev) => {
      if (typeof ev.data === "string") {
        for (const r of JSON.parse(ev.data)) {
          if (r[0] === "k") typeKey(r[1], r[2]);
          else onRecords(r);
        }
      } else {
        onFrame(new Float64Array(ev.data));
      }
    };
  }

  if (efb) {
    connect();
    return;
  }

  const hello = JSON.parse(sync("GET", `/__b/hello?v=${view}`).responseText);
  const slots = new Map();
  hello.slots.forEach((pair, i) => slots.set(pair[0] + "\0" + pair[1], i));
  const values = new Float64Array(65536);
  hello.values.forEach((v, i) => { values[i] = v === null ? NaN : v; });
  let frame = hello.frame;
  let resolved = hello.resolved;
  const overlay = new Map();
  const strings = new Map();
  const games = new Map(Object.entries(hello.games));
  const gaveUp = new Set();
  const calls = new Map();
  const polled = [];
  const magCache = new Map();
  let callSeq = 0;

  function slotFor(name, unit) {
    const key = name + "\0" + unit;
    let s = slots.get(key);
    if (s !== undefined) return s;
    const r = JSON.parse(sync("GET", `/__b/slot?n=${q(name)}&u=${q(unit)}`).responseText);
    s = r.s;
    slots.set(key, s);
    if (s >= 0) {
      values[s] = r.v === null ? NaN : r.v;
      if (r.ready) resolved = Math.max(resolved, s + 1);
    }
    return s;
  }

  // rAF stops on a browser that is not being drawn, and the logic in these
  // pages has to keep running, so a published frame stands in for it then.
  const realRaf = window.requestAnimationFrame.bind(window);
  const queued = new Map();
  let rafId = 0;
  let armed = false;
  let lastRaf = 0;
  let frames = 0;
  let framesAt = performance.now();
  function runQueued(t) {
    frames++;
    const now = [...queued.values()];
    queued.clear();
    for (const cb of now) {
      try {
        cb(t);
      } catch (e) {
        setTimeout(() => { throw e; });
      }
    }
  }
  const minFrameMs = hello.free ? 0 : /^(FCU|Clock|BAT|RTPI|SCREEN_IFE|SCREEN_DU_RMP_\d|SCREEN_OIT_\w+)$/.test(screen) ? 98 : 31;
  let lastRun = 0;
  function onRealRaf(t) {
    lastRaf = performance.now();
    if (lastRaf - lastRun < minFrameMs) {
      realRaf(onRealRaf);
      return;
    }
    lastRun = lastRaf;
    armed = false;
    runQueued(t);
  }
  window.requestAnimationFrame = (cb) => {
    const id = ++rafId;
    queued.set(id, cb);
    if (!armed) {
      armed = true;
      realRaf(onRealRaf);
    }
    return id;
  };
  window.cancelAnimationFrame = (id) => { queued.delete(id); };

  const ndSide = /^SCREEN_DU_ND([LR])$/.exec(screen);
  function cursorAt(x, y) {
    if (!parent.__kccu) return;
    let color = "#ffff00";
    if (ndSide) {
      const range = window.__xphfbw.getVar(`L:A32NX_EFIS_${ndSide[1]}_ND_RANGE`, "number");
      const mode = window.__xphfbw.getVar(`L:A32NX_EFIS_${ndSide[1]}_ND_MODE`, "number");
      if (range === 0 && mode >= 2 && mode <= 4) color = "#ff94ff";
    }
    parent.__kccu(x, y, color);
  }
  document.addEventListener("pointermove", (e) => cursorAt(e.clientX, e.clientY), true);

  onFrame = (m) => {
    frame = m[0];
    resolved = Math.max(resolved, m[1]);
    for (let i = 2; i + 1 < m.length; i += 2) values[m[i]] = m[i + 1];
    const now = performance.now();
    if (queued.size && now - lastRaf > 200) runQueued(now);
    if (now - framesAt > 10000) {
      send(["p", Math.round((frames * 1000) / (now - framesAt))]);
      frames = 0;
      framesAt = now;
    }
  };
  onRecords = (r) => {
    switch (r[0]) {
      case "r": {
        const pending = calls.get(r[1]);
        if (pending) {
          calls.delete(r[1]);
          if (r[2]) pending[0](r[3]);
          else pending[1](r[3]);
        }
        break;
      }
      case "c": cursorAt(r[1], r[2]); break;
      case "game": games.set(r[1], r[2]); polled.push(r); break;
      case "string": strings.set(r[1], r[2]); polled.push(r); break;
      default: polled.push(r);
    }
  };

  window.__xphfbw = {
    view,
    screen,
    aircraftDir: hello.aircraftDir,
    snapshot() {
      for (const [slot, w] of overlay) if (frame >= w[1] + 4) overlay.delete(slot);
    },
    getVar(name, unit) {
      const s = slotFor(String(name), unit === undefined || unit === null ? "" : String(unit));
      if (s < 0) return 0;
      const w = overlay.get(s);
      if (w) {
        if (frame < w[1] + 4) return w[0];
        overlay.delete(s);
      }
      return s < resolved ? values[s] : 0;
    },
    setVar(name, unit, value) {
      const s = slotFor(String(name), unit === undefined || unit === null ? "" : String(unit));
      if (s < 0) return;
      const v = +value;
      send(["w", s, Number.isFinite(v) ? v : 0]);
      overlay.set(s, [v, frame]);
    },
    getString(name) {
      const v = strings.get(String(name));
      return v === undefined ? "" : v;
    },
    setString(name, value) {
      strings.set(String(name), String(value));
      send(["s", String(name), String(value)]);
    },
    sendEvent(name, values_) {
      send(["e", String(name), Array.from(values_ || [], Number)]);
    },
    call(name, argsJson) {
      const id = ++callSeq;
      return new Promise((resolve, reject) => {
        calls.set(id, [resolve, reject]);
        send(["c", id, String(name), String(argsJson)]);
      });
    },
    poll() {
      return polled.splice(0, polled.length);
    },
    gameString(name) {
      const n = String(name);
      const known = games.get(n);
      if (known !== undefined) return known;
      if (gaveUp.has(n)) return "";
      flush();
      const x = sync("GET", `/__b/game?v=${view}&n=${q(n)}`);
      if (x.status === 200) {
        games.set(n, x.responseText);
        return x.responseText;
      }
      gaveUp.add(n);
      return "";
    },
    storedData(op, key, value) {
      return sync("POST", "/__b/stored", JSON.stringify([String(op), String(key), value === undefined ? "" : String(value)])).responseText;
    },
    readFile(path) {
      const x = sync("GET", `/__b/file?p=${q(path)}`);
      return x.status === 200 ? x.responseText : null;
    },
    log(level, text) {
      send(["l", level | 0, String(text)]);
    },
    magVar(lat, lon) {
      const key = Math.round(lat * 4) + "," + Math.round(lon * 4);
      let v = magCache.get(key);
      if (v === undefined) {
        v = +sync("GET", `/__b/mag?lat=${+lat}&lon=${+lon}`).responseText;
        magCache.set(key, v);
      }
      return v;
    },
    loaded(ok, text) {
      send(["L", ok ? 1 : 0, String(text === undefined ? "" : text)]);
    },
  };

  // SimBridge's address is the plugin's, whatever else is listening on that port.
  const realFetch = window.fetch.bind(window);
  window.fetch = function (resource, options) {
    const url = String(resource && resource.url !== undefined ? resource.url : resource);
    const m = /^https?:\/\/(?:localhost|127\.0\.0\.1):8380(\/.*)$/i.exec(url);
    return m ? realFetch(`/__b/simbridge?p=${q(m[1])}`, options) : realFetch(resource, options);
  };

  window.addEventListener("error", (e) => send(["l", 2, `${e.message} at ${e.filename}:${e.lineno}`]));
  window.addEventListener("unhandledrejection", (e) => send(["l", 2, `unhandled rejection: ${e.reason && e.reason.message ? e.reason.message : e.reason}`]));
  connect();
})();
