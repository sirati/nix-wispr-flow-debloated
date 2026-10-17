// Runs in Wispr's Electron main process before the app's own code
// (package.json's main points here; see wispr-flow.nix).
//
// Keeps Wispr's windows without a renderer until their UI is needed. The app
// gets real BrowserWindow objects, so every method it calls works; the shim
// only decides when a window's page is loaded (a renderer process exists) and
// what happens to the IPC messages meant for a window that has none:
//
//   hub     records dictation audio. Here wispr-headless (Rust) records
//           instead: audioStart/audioStop go to it, and its chunks come back
//           as if from the Hub. The page loads when Wispr shows the Hub
//           (settings) or sends it something the shim does not handle, such
//           as meeting capture.
//   status  the floating indicator. While it would only show the speech
//           indicator, wispr-headless draws that. Anything else (notifications,
//           reminders, tips) loads the real page; once it is back to showing
//           nothing, the renderer is stopped again.
//   others  context menu, scratchpad, meeting recorder, ...: load when shown.
//
// A stopped window's renderer is killed; Electron keeps the window object and
// the next load starts a fresh renderer. The 'render-process-gone' that the
// kill causes is kept from Wispr. If wispr-headless cannot run, every window
// loads as usual. Only names and key paths are logged, never values, in
// ~/.cache/wispr-flow/shim-trace.jsonl.
"use strict";

const path = require("path");
const fs = require("fs");
const childProcess = require("child_process");
const readline = require("readline");

// Filled in by the Nix build.
const DAEMON = "@daemon@";
const PW_CAT = "@pwcat@";
const MIC_TARGET = "@micTarget@";

const TRACE_MAX = 20 * 1024 * 1024;
// A loaded window that went back to showing nothing is stopped after this.
const STATUS_IDLE_MS = Number(process.env.WISPR_HEADLESS_TEST_IDLE_MS) || 15_000;
const HIDDEN_IDLE_MS = Number(process.env.WISPR_HEADLESS_TEST_IDLE_MS) || 60_000;

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
const trace = openTrace();

function shape(v, depth = 1) {
  if (v === null || typeof v !== "object") return typeof v;
  if (v instanceof ArrayBuffer || ArrayBuffer.isView(v)) return "bytes";
  if (Array.isArray(v)) return "array";
  if (depth === 0) return "object";
  const out = {};
  for (const k of Object.keys(v).slice(0, 40)) out[k] = shape(v[k], depth - 1);
  return out;
}

const roleOf = (opts) => {
  const m = /renderer[\\/]([^\\/]+)[\\/]preload\.js$/.exec(String(opts?.webPreferences?.preload ?? ""));
  return m ? m[1] : null;
};
const LAZY_ROLES = new Set(["status", "hub", "contextMenu", "scratchpad", "meeting_recorder", "feature_tour"]);

// Messages to a renderer-less window that need no page.
const HUB_DROP = new Set([
  "status:dictationStatus", "hub:hidden", "hub:shown", "history:needsUpdate", "media:initialize",
  "meetingRecorder:weeklyQuotaUpdated", "zubridge:dispatch-ack", "dictation:audioSetup",
]);
const HUB_SOUNDS = {
  "media:playDictationStartSound": "start",
  "media:playDictationStopSound": "stop",
  "media:playAchievementSound": "achievement",
};
const STATUS_DROP = new Set(["status:micActive", "status:monitorChanged", "notification:clear", "zubridge:dispatch-ack"]);
// Store changes that may make the status window show something.
const STATUS_WAKE_KEYS = [/^remoteNotifications\.new/, /^remoteNotifications\.unreadCount/, /^subscription\.(tier|daysLeft|gracePeriodEndsAt)/, /[Ll]imit|[Qq]uota|[Nn]udge|[Rr]eminder/];

function changedPaths(a, b, prefix = "", depth = 3, out = []) {
  if (out.length > 60 || a === b) return out;
  const objA = a !== null && typeof a === "object" && !Array.isArray(a);
  const objB = b !== null && typeof b === "object" && !Array.isArray(b);
  if (!objA || !objB || depth === 0) {
    try { if (JSON.stringify(a) === JSON.stringify(b)) return out; } catch {}
    out.push(prefix || "<root>");
    return out;
  }
  for (const k of new Set([...Object.keys(a), ...Object.keys(b)])) changedPaths(a[k], b[k], prefix ? `${prefix}.${k}` : k, depth - 1, out);
  return out;
}

// ---------------------------------------------------------------- the daemon

function startDaemon(resourcesPath, onEvent, onExit) {
  if (DAEMON.startsWith("@")) return null;
  const args = ["--pw-cat", PW_CAT, "--sounds", path.join(resourcesPath, "assets", "sounds")];
  if (MIC_TARGET && !MIC_TARGET.startsWith("@")) args.push("--target", MIC_TARGET);
  // Tests only: a stand-in microphone.
  if (process.env.WISPR_HEADLESS_TEST_CAPTURE) args.push("--capture-cmd", process.env.WISPR_HEADLESS_TEST_CAPTURE);
  let child;
  try {
    child = childProcess.spawn(DAEMON, args, { stdio: ["pipe", "pipe", "inherit"] });
  } catch (e) {
    trace({ ev: "daemon.spawnFailed", error: String(e) });
    return null;
  }
  let alive = true;
  child.on("error", (e) => { trace({ ev: "daemon.error", error: String(e) }); });
  child.on("exit", (code, signal) => { alive = false; trace({ ev: "daemon.exit", code, signal }); onExit(); });
  child.stdin.on("error", () => {});
  readline.createInterface({ input: child.stdout }).on("line", (line) => {
    let msg;
    try { msg = JSON.parse(line); } catch { return; }
    if (msg.op === "log") trace({ ev: "daemon.log", msg: String(msg.msg).slice(0, 200) });
    else onEvent(msg);
  });
  return {
    send(o) { if (alive) try { child.stdin.write(JSON.stringify(o) + "\n"); } catch {} },
    get alive() { return alive; },
  };
}

// ---------------------------------------------------------------- the hook

function install() {
  const Module = require("module");
  const realLoad = Module._load;
  let proxied = null;
  Module._load = function (request) {
    const mod = realLoad.apply(this, arguments);
    if (request !== "electron" || process.type !== "browser") return mod;
    if (proxied) return proxied;
    proxied = build(mod);
    return proxied;
  };
}

function build(electron) {
  const { BrowserWindow, ipcMain } = electron;
  const realEmit = ipcMain.emit.bind(ipcMain);
  const windows = new Map(); // webContents id -> Lazy
  const byRole = new Map(); // role -> Lazy (first window of that role)
  let headless = true;
  let session = null; // current dictation uuid

  const fakeEvent = (wc) => ({ sender: wc, senderFrame: null, frameId: 0, processId: 0, ports: [], returnValue: undefined, reply: () => {} });
  const emitFrom = (lazy, ch, payload) => {
    if (!lazy || lazy.win.isDestroyed()) return;
    realEmit(ch, fakeEvent(lazy.wc), payload);
  };

  const daemon = startDaemon(process.resourcesPath, onDaemonEvent, () => {
    // Without the recorder, Wispr's own windows have to do the work.
    if (!headless) return;
    headless = false;
    trace({ ev: "fallback", why: "daemon exited" });
    for (const lazy of windows.values()) lazy.materialize("fallback");
  });
  if (!daemon) {
    headless = false;
    trace({ ev: "fallback", why: "no daemon" });
  }

  function onDaemonEvent(msg) {
    const hub = byRole.get("hub");
    const uuid = msg.uuid;
    switch (msg.op) {
      case "ready":
        trace({ ev: "daemon.ready" });
        break;
      case "started":
        emitFrom(hub, "dictation:recordingStarted", { transcriptionEntityUuid: uuid });
        break;
      case "chunk": {
        const buf = Buffer.from(msg.pcm, "base64");
        const packet = buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);
        emitFrom(hub, "dictation:audioChunk", { packet, index: msg.index, volume: msg.volume, duration: msg.duration, encoding: "wav", transcriptionEntityUuid: uuid });
        break;
      }
      case "done":
        emitFrom(hub, "dictation:recordingDone", { ...msg.timing, transcriptionEntityUuid: uuid });
        if (session === uuid) session = null;
        break;
      case "micFailed":
        trace({ ev: "daemon.micFailed", error: String(msg.error).slice(0, 200) });
        emitFrom(hub, "dictation:micAcquisitionFailed", { transcriptionEntityUuid: uuid });
        if (session === uuid) session = null;
        break;
    }
  }

  class Lazy {
    constructor(win, role) {
      this.win = win;
      this.wc = win.webContents;
      this.role = role;
      this.load = null; // [method, args]
      this.materialized = false;
      this.loading = null;
      this.wantVisible = false;
      this.showMethod = "showInactive";
      this.queue = [];
      this.killing = false;
      this.idleTimer = null;
      this.lastState = null;
      this.notificationActive = false;
      this.remindersActive = false;
      this.barHidden = true;
      this.rectsEmpty = true;
      this.meeting = false;
    }

    materialize(why) {
      if (this.materialized || this.win.isDestroyed()) return this.loading ?? Promise.resolve();
      this.materialized = true;
      this.cancelIdle();
      trace({ ev: "materialize", win: this.role, why });
      if (this.role === "status") daemon?.send({ op: "pill", enabled: false });
      if (!this.load) return Promise.resolve();
      const [method, args] = this.load;
      this.loading = new Promise((resolve) => {
        this.wc.once("did-finish-load", () => {
          this.loading = null;
          const queued = this.queue.splice(0);
          for (const [ch, a] of queued) this.realSend(ch, ...a);
          if (this.wantVisible && !this.win.isDestroyed()) this.realWin[this.showMethod].call(this.win);
          resolve();
        });
        // A load started while the renderer we killed is still going away
        // fails, so wait for it to be gone, and try once more on failure.
        const load = (attempt) =>
          Promise.resolve(BrowserWindow.prototype[method].apply(this.win, args)).catch((e) => {
            trace({ ev: "materialize.loadFailed", win: this.role, attempt, error: String(e).slice(0, 200) });
            if (attempt < 3 && !this.win.isDestroyed()) setTimeout(() => load(attempt + 1), 300 * attempt);
          });
        const start = Date.now();
        const whenGone = () => {
          if (this.killing && Date.now() - start < 3_000) return setTimeout(whenGone, 25);
          load(1);
        };
        whenGone();
      });
      return this.loading;
    }

    dematerialize(why) {
      if (!this.materialized || !headless || this.win.isDestroyed() || this.loading) return;
      if (this.role === "hub" && this.meeting) return;
      let pid = 0;
      try { pid = this.wc.getOSProcessId(); } catch {}
      this.materialized = false;
      this.queue = [];
      trace({ ev: "dematerialize", win: this.role, why });
      if (this.realWin.isVisible.call(this.win)) BrowserWindow.prototype.hide.call(this.win);
      if (pid > 0) {
        this.killing = true;
        setTimeout(() => { this.killing = false; }, 5_000);
        try { process.kill(pid, "SIGKILL"); } catch {}
      }
      if (this.role === "status") daemon?.send({ op: "pill", enabled: true });
    }

    scheduleIdle(ms, why) {
      this.cancelIdle();
      this.idleTimer = setTimeout(() => { this.idleTimer = null; this.dematerialize(why); }, ms);
    }

    cancelIdle() {
      if (this.idleTimer) clearTimeout(this.idleTimer);
      this.idleTimer = null;
    }

    // Status window: back to showing nothing?
    checkStatusIdle() {
      if (this.role !== "status" || !this.materialized) return;
      if (!this.notificationActive && !this.remindersActive && (this.barHidden || this.rectsEmpty) && !session) this.scheduleIdle(STATUS_IDLE_MS, "nothing shown");
      else this.cancelIdle();
    }

    ack(update) {
      if (!update?.updateId) return;
      const ackPayload = { updateId: update.updateId, windowId: this.wc.id, thunkId: update.thunkId };
      setImmediate(() => emitFrom(this, "zubridge:state-update-ack", ackPayload));
    }

    // A message for this window while it has no page. Returns true if handled.
    route(ch, args) {
      const a = args[0];
      if (ch === "zubridge:state-update") {
        this.ack(a);
        if (this.role === "status" && a?.state) {
          const changed = this.lastState ? changedPaths(this.lastState, a.state) : [];
          this.lastState = a.state;
          if (changed.some((p) => STATUS_WAKE_KEYS.some((re) => re.test(p)))) {
            trace({ ev: "wake.state", win: "status", keys: changed.slice(0, 10) });
            this.materialize("store change");
          }
        }
        return true;
      }
      if (this.role === "hub") {
        if (ch === "dictation:audioStart") {
          session = a?.transcriptionEntityUuid ?? null;
          daemon.send({ op: "start", uuid: session });
          return true;
        }
        if (ch === "dictation:audioStop") {
          daemon.send({ op: "stop" });
          return true;
        }
        if (HUB_SOUNDS[ch]) {
          daemon.send({ op: "sound", name: HUB_SOUNDS[ch] });
          return true;
        }
        if (HUB_DROP.has(ch)) return true;
        return false;
      }
      if (this.role === "status") {
        if (ch === "status:dictationStatus") {
          daemon.send({ op: "status", value: String(a) });
          return true;
        }
        if (ch === "status:audioLevel") {
          daemon.send({ op: "level", value: typeof a === "number" ? a : Number(a?.level ?? a?.value ?? 0) });
          return true;
        }
        if (ch === "status:playPasteSound") {
          daemon.send({ op: "sound", name: "paste" });
          return true;
        }
        if (ch === "status:setReminders") {
          this.remindersActive = a?.reminders?.length > 0;
          if (!this.remindersActive) return true;
        }
        if (ch === "notification:show") this.notificationActive = true;
        if (STATUS_DROP.has(ch)) return true;
        return false;
      }
      return false; // other windows: queued until shown
    }
  }

  // Dictation keeps going to the recorder even while the Hub's page is up,
  // so there is one recorder; the real Hub never records.
  function routeLoaded(lazy, ch, args) {
    if (lazy.role === "hub" && headless) {
      if (ch === "dictation:audioStart" || ch === "dictation:audioStop" || HUB_SOUNDS[ch] || ch === "dictation:audioSetup") return lazy.route(ch, args);
      if (/^media:(start|stop)Meeting/.test(ch)) lazy.meeting = ch.startsWith("media:start");
    }
    if (lazy.role === "status") {
      if (ch === "status:dictationStatus") daemon?.send({ op: "status", value: String(args[0]) });
      if (ch === "notification:show") { lazy.notificationActive = true; lazy.checkStatusIdle(); }
      if (ch === "notification:clear") { lazy.notificationActive = false; lazy.checkStatusIdle(); }
      if (ch === "status:setReminders") { lazy.remindersActive = args[0]?.reminders?.length > 0; lazy.checkStatusIdle(); }
    }
    return false;
  }

  class ShimWindow extends BrowserWindow {
    constructor(opts) {
      super(opts);
      const wc = this.webContents;
      const role = roleOf(opts);
      trace({ ev: "win.new", id: wc.id, role, title: opts?.title });
      if (!role || !LAZY_ROLES.has(role) || !headless) return;
      const lazy = new Lazy(this, role);
      lazy.realWin = BrowserWindow.prototype;
      windows.set(wc.id, lazy);
      if (!byRole.has(role)) byRole.set(role, lazy);
      const self = this;
      const realSend = wc.send.bind(wc);
      lazy.realSend = realSend;

      for (const method of ["loadURL", "loadFile"]) {
        this[method] = function (...args) {
          if (!headless || lazy.materialized) return BrowserWindow.prototype[method].apply(self, args);
          lazy.load = [method, args];
          trace({ ev: "deferLoad", win: role });
          if (role === "hub") setTimeout(() => virtualHubStartup(lazy), 500);
          return Promise.resolve();
        };
      }
      for (const method of ["show", "showInactive", "focus"]) {
        this[method] = function (...args) {
          lazy.wantVisible = true;
          if (method !== "focus") lazy.showMethod = method;
          lazy.cancelIdle();
          if (!headless || lazy.materialized) return lazy.loading ? undefined : BrowserWindow.prototype[method].apply(self, args);
          if (role === "status") return undefined; // the Rust indicator stands in
          lazy.materialize(method);
          return undefined;
        };
      }
      this.hide = function (...args) {
        lazy.wantVisible = false;
        const r = BrowserWindow.prototype.hide.apply(self, args);
        if (headless && lazy.materialized && role !== "status") lazy.scheduleIdle(HIDDEN_IDLE_MS, "hidden");
        return r;
      };
      this.isVisible = function () {
        if (headless && !lazy.materialized) return lazy.wantVisible;
        return BrowserWindow.prototype.isVisible.call(self);
      };

      wc.send = (ch, ...args) => {
        try {
          if (headless && !lazy.materialized) {
            if (lazy.route(ch, args)) return;
            if (lazy.queue.length < 500) lazy.queue.push([ch, args]);
            trace({ ev: "wake.msg", win: role, ch });
            lazy.materialize(ch);
            return;
          }
          if (headless && routeLoaded(lazy, ch, args)) return;
          if (lazy.loading) {
            if (lazy.queue.length < 500) lazy.queue.push([ch, args]);
            return;
          }
        } catch (e) {
          trace({ ev: "route.error", win: role, ch, error: String(e).slice(0, 200) });
        }
        return realSend(ch, ...args);
      };

      // Keep our own renderer kills from Wispr.
      const realWcEmit = wc.emit.bind(wc);
      wc.emit = (name, ...args) => {
        if (lazy.killing && (name === "render-process-gone" || name === "crashed" || name === "did-fail-load" || name === "unresponsive")) {
          if (name === "render-process-gone") lazy.killing = false;
          return false;
        }
        return realWcEmit(name, ...args);
      };
      wc.once("destroyed", () => windows.delete(wc.id));
    }
  }

  // What a freshly loaded Hub page tells main, without a page.
  function virtualHubStartup(lazy) {
    if (lazy.materialized || lazy.win.isDestroyed()) return;
    for (const ch of ["perms:complete", "helper:startTextboxMonitoring", "history:initializeStatistics", "hub:opened"]) emitFrom(lazy, ch, undefined);
    emitFrom(lazy, "dictation:opusEncoderSupport", { supported: false });
    trace({ ev: "virtualHub.ready" });
  }

  ipcMain.emit = (ch, e, ...args) => {
    const lazy = windows.get(e?.sender?.id);
    if (lazy && headless) {
      // The recorder sends WAV; a loaded Hub must not switch Wispr to Opus.
      if (ch === "dictation:opusEncoderSupport" && lazy.role === "hub") args = [{ ...(args[0] || {}), supported: false }];
      if (lazy.role === "status") {
        if (ch === "status:barHidden") { lazy.barHidden = true; lazy.checkStatusIdle(); }
        if (ch === "status:barVisible") { lazy.barHidden = false; lazy.checkStatusIdle(); }
        if (ch === "status:reportAlphaCatchRects") { lazy.rectsEmpty = !(args[0]?.rects?.length > 0); lazy.checkStatusIdle(); }
      }
      if (lazy.role === "hub" && ch === "hub:close") lazy.scheduleIdle(HIDDEN_IDLE_MS, "closed");
    }
    if (ch === "zubridge:dispatch") trace({ ev: "dispatch", win: lazy?.role, type: args[0]?.action?.type ?? args[0]?.type });
    else if (ch !== "zubridge:state-update-ack" && ch !== "zubridge:track-action-dispatch" && ch !== "log" && ch !== "dictation:audioChunk" && !String(ch).startsWith("sentry-ipc"))
      trace({ ev: "recv", win: lazy?.role, ch, shape: shape(args[0]) });
    return realEmit(ch, e, ...args);
  };

  return new Proxy(electron, { get: (t, k) => (k === "BrowserWindow" ? ShimWindow : t[k]) });
}

// Nothing here may stop Wispr from starting.
try {
  install();
} catch (e) {
  try { console.error("wispr-shim: not installed:", e); } catch {}
}

const root = path.join(__dirname, "..");
require(path.join(root, require(path.join(root, "package.json")).wisprOriginalMain));
