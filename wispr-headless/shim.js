// Runs in Wispr's Electron main process before the app's own code
// (package.json's main points here; see wispr-flow.nix).
//
// Phase 1: pass everything through unchanged, and record the shape of what
// the app does with its windows, so the headless mode can be built from facts:
// which page each window loads, which IPC channels flow in each direction, and
// which store keys change. Only names and key paths are recorded, never values,
// so no transcripts, tokens or settings end up in the log.
//
// The log is ~/.cache/wispr-flow/shim-trace.jsonl, capped at TRACE_MAX bytes.
"use strict";

const path = require("path");
const fs = require("fs");

const TRACE_MAX = 20 * 1024 * 1024;

function openTrace() {
  try {
    const dir = path.join(process.env.XDG_CACHE_HOME || path.join(process.env.HOME || "/tmp", ".cache"), "wispr-flow");
    fs.mkdirSync(dir, { recursive: true });
    const file = path.join(dir, "shim-trace.jsonl");
    try {
      if (fs.statSync(file).size > TRACE_MAX) fs.renameSync(file, file + ".old");
    } catch {}
    const fd = fs.openSync(file, "a", 0o600);
    let written = 0;
    const t0 = Date.now();
    return (o) => {
      if (written > TRACE_MAX) return;
      const line = JSON.stringify({ t: Date.now() - t0, ...o }) + "\n";
      written += line.length;
      try { fs.writeSync(fd, line); } catch {}
    };
  } catch {
    return () => {};
  }
}

// Key paths of a value, down to `depth` levels, without any values.
function shape(v, depth = 2) {
  if (v === null || typeof v !== "object") return typeof v;
  if (v instanceof ArrayBuffer || ArrayBuffer.isView(v)) return "bytes";
  if (Array.isArray(v)) return "array";
  if (depth === 0) return "object";
  const out = {};
  for (const k of Object.keys(v).slice(0, 40)) out[k] = shape(v[k], depth - 1);
  return out;
}

// Paths whose value differs between two states, without the values.
function changedPaths(a, b, prefix = "", depth = 3, out = []) {
  if (out.length > 60) return out;
  if (a === b) return out;
  const objA = a !== null && typeof a === "object" && !Array.isArray(a);
  const objB = b !== null && typeof b === "object" && !Array.isArray(b);
  if (!objA || !objB || depth === 0) {
    try { if (JSON.stringify(a) === JSON.stringify(b)) return out; } catch {}
    out.push(prefix || "<root>");
    return out;
  }
  for (const k of new Set([...Object.keys(a), ...Object.keys(b)])) {
    changedPaths(a[k], b[k], prefix ? `${prefix}.${k}` : k, depth - 1, out);
  }
  return out;
}

const pageOf = (url) => String(url).replace(/^.*\/\.webpack\/renderer\//, "").replace(/\/index\.html.*$/, "");

function install(trace) {
  const Module = require("module");
  const realLoad = Module._load;
  let proxied = null;
  Module._load = function (request) {
    const mod = realLoad.apply(this, arguments);
    if (request !== "electron" || process.type !== "browser") return mod;
    if (proxied) return proxied;

    const { BrowserWindow, ipcMain, Tray, app } = mod;
    const pages = new Map(); // webContents id -> page
    const lastState = new Map(); // webContents id -> last store state sent
    const nameOf = (wc) => pages.get(wc?.id) ?? (wc ? `wc${wc.id}` : "?");

    for (const m of ["show", "showInactive", "hide", "focus", "close", "destroy", "loadURL", "loadFile"]) {
      const orig = BrowserWindow.prototype[m];
      if (typeof orig !== "function") continue;
      BrowserWindow.prototype[m] = function (...a) {
        if (m === "loadURL" || m === "loadFile") pages.set(this.webContents.id, pageOf(a[0]));
        trace({ ev: "win." + m, win: nameOf(this.webContents) });
        return orig.apply(this, a);
      };
    }

    class ShimWindow extends BrowserWindow {
      constructor(opts) {
        super(opts);
        const wc = this.webContents;
        trace({ ev: "win.new", id: wc.id, title: opts?.title, show: opts?.show });
        const send = wc.send.bind(wc);
        wc.send = (ch, ...args) => {
          if (ch === "zubridge:state-update" && args[0]?.state) {
            const prev = lastState.get(wc.id);
            lastState.set(wc.id, args[0].state);
            trace({ ev: "state", win: nameOf(wc), changed: prev ? changedPaths(prev, args[0].state) : ["<first>"] });
          } else {
            trace({ ev: "send", win: nameOf(wc), ch, shape: shape(args[0], 1) });
          }
          return send(ch, ...args);
        };
        wc.once("destroyed", () => lastState.delete(wc.id));
      }
    }

    const realEmit = ipcMain.emit.bind(ipcMain);
    ipcMain.emit = (ch, e, ...args) => {
      if (ch === "zubridge:dispatch") {
        trace({ ev: "dispatch", win: nameOf(e?.sender), type: args[0]?.action?.type ?? args[0]?.type });
      } else if (ch !== "zubridge:state-update-ack" && ch !== "zubridge:track-action-dispatch" && ch !== "log" && !String(ch).startsWith("sentry-ipc")) {
        trace({ ev: "recv", win: nameOf(e?.sender), ch, shape: shape(args[0], 1) });
      }
      return realEmit(ch, e, ...args);
    };

    class ShimTray extends Tray {
      constructor(...a) {
        super(...a);
        trace({ ev: "tray.new" });
        for (const m of ["setImage", "setToolTip", "setContextMenu", "setTitle"]) {
          const o = this[m].bind(this);
          this[m] = (...x) => { trace({ ev: "tray." + m }); return o(...x); };
        }
        for (const ev of ["click", "right-click", "double-click"]) this.on(ev, () => trace({ ev: "tray." + ev }));
      }
    }

    for (const ev of ["second-instance", "activate", "open-url", "before-quit", "window-all-closed"]) {
      app.on(ev, () => trace({ ev: "app." + ev }));
    }

    proxied = new Proxy(mod, {
      get: (t, k) => (k === "BrowserWindow" ? ShimWindow : k === "Tray" ? ShimTray : t[k]),
    });
    return proxied;
  };
}

// Nothing here may stop Wispr from starting.
try {
  install(openTrace());
} catch (e) {
  try { console.error("wispr-shim: not installed:", e); } catch {}
}

const root = path.join(__dirname, "..");
require(path.join(root, require(path.join(root, "package.json")).wisprOriginalMain));
