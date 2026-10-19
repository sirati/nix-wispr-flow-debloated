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

// Filled in by the Nix build; empty when unused (pw-cat and the PipeWire
// target on Linux, the capture and player commands on macOS).
const DAEMON = "@daemon@";
const PW_CAT = "@pwcat@";
const MIC_TARGET = "@micTarget@";
const CAPTURE_CMD = "@captureCmd@";
const DEVICE_NAME = "@deviceName@";
const PLAYER_CMD = "@playerCmd@";
const FONT_DIR = "@fontDir@";
const given = (v) => Boolean(v) && !v.startsWith("@");

const TRACE_MAX = 20 * 1024 * 1024;
// A loaded window that went back to showing nothing is stopped after this:
// the status page almost at once (time for its exit animation), since the
// Rust bar takes over; a hidden window after a minute.
const STATUS_IDLE_MS = Number(process.env.WISPR_HEADLESS_TEST_IDLE_MS) || 400;
// The Rust bar stays up this long after the status page loads, so the bar
// never blanks while the page paints its first frame.
const HANDOVER_MS = 150;
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
// Windows that load when shown. Not here: windows that show themselves on
// 'ready-to-show' (macOS's accessibility_drop and meeting_ax_inspector), which
// never fires without a load; they are only created when needed anyway.
const LAZY_ROLES = new Set(["status", "hub", "contextMenu", "scratchpad", "meeting_recorder", "feature_tour", "overlay"]);

// Messages to a renderer-less window that need no page.
const HUB_DROP = new Set([
  "status:dictationStatus", "hub:hidden", "hub:shown", "history:needsUpdate", "media:initialize",
  "meetingRecorder:weeklyQuotaUpdated", "zubridge:dispatch-ack", "dictation:audioSetup",
  // macOS, one-way updates for the Hub's own recorder (focused app, whether
  // playing media can be paused), which does not run here.
  "helper:appInfoUpdate", "media:isPlaying",
]);
const HUB_SOUNDS = {
  "media:playDictationStartSound": "start",
  "media:playDictationStopSound": "stop",
  "media:playAchievementSound": "achievement",
};
const STATUS_DROP = new Set(["status:monitorChanged", "notification:clear", "zubridge:dispatch-ack", "helper:appInfoUpdate"]);
// Store changes that may make the status window show something.
const STATUS_WAKE_KEYS = [/^remoteNotifications\.new/, /^remoteNotifications\.unreadCount/, /^subscription\.(tier|daysLeft|gracePeriodEndsAt)/, /[Ll]imit|[Qq]uota|[Nn]udge|[Rr]eminder/];

// Status-window messages the Rust bar follows (see daemon/src/bar.rs), as
// daemon commands; null for anything else.
function barCommand(ch, a) {
  switch (ch) {
    case "status:dictationStatus": return { op: "status", value: String(a) };
    case "status:popo": return { op: "popo" };
    case "status:audioLevel": return { op: "level", value: typeof a === "number" ? a : Number(a?.level ?? a?.value ?? 0) };
    case "status:micActive": return { op: "mic", active: Boolean(a) };
    case "status:micSilence": return { op: "silence", value: Boolean(a) };
    case "status:commandModeStatus": return { op: "commanding", active: Boolean(a) };
    // Other indicator states (polish progress, "Cleaned up!") are the page's.
    case "status:setIndicatorState": return a?.state === "resting" ? { op: "indicator", state: "resting" } : null;
  }
  return null;
}
// Dictation statuses only Wispr's page shows: polish results, instruct mode,
// the hotkey reminder.
const PAGE_STATUSES = /^(polish_|instruct_chat_|toggle_hotkey_reminder)/;

// Notifications the Rust bar draws: Wispr's "condensed" layout (a title and
// up to two buttons) whose strings are known here. The page translates keys,
// so only for an English UI; anything else loads the page.
const NOTICE_TEXT = {
  transcript_cancelled_title: "Transcript cancelled",
  transcript_cancelled_detailed_action_undo: "Undo",
};
function nativeNotice(p, locale) {
  if (!/^en/i.test(locale) || p?.variant !== "condensed" || p.titleWrap || p.selfDismiss === false) return null;
  if (p.icon || p.badge || p.badgeIcon) return null;
  const text = (t) => (typeof t === "string" ? t : t?.values ? undefined : NOTICE_TEXT[t?.key]);
  const title = text(p.title);
  const actions = (p.actions ?? []).map((a) => text(a?.text));
  if (!title || actions.length > 2 || actions.some((a) => !a)) return null;
  return { op: "notify", title, actions, timeout: Number(p.timeout ?? 4000) || 0 };
}

// The value at a dotted path, or undefined. A value that was null or missing
// before is arriving for the first time (Wispr fills placeholders at login).
const valueAt = (obj, p) => p.split(".").reduce((o, k) => (o !== null && typeof o === "object" ? o[k] : undefined), obj);

// The status page reports the areas it draws. The resting bar is about
// 78x18; anything clearly bigger is a notification, tip or similar.
function drawsMoreThanBar(rects) {
  if (!Array.isArray(rects)) return false;
  return rects.some((r) => Number(r?.width ?? r?.w ?? 0) > 140 || Number(r?.height ?? r?.h ?? 0) > 48);
}

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
  if (!given(DAEMON)) return null;
  const args = ["--sounds", path.join(resourcesPath, "assets", "sounds")];
  if (given(PW_CAT)) args.push("--pw-cat", PW_CAT);
  if (given(MIC_TARGET)) args.push("--target", MIC_TARGET);
  if (given(CAPTURE_CMD)) args.push("--capture-cmd", CAPTURE_CMD);
  if (given(DEVICE_NAME)) args.push("--device-name", DEVICE_NAME);
  if (given(PLAYER_CMD)) args.push("--player-cmd", PLAYER_CMD);
  if (given(FONT_DIR)) args.push("--font-dir", FONT_DIR);
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
  let notice = null; // the notification the Rust bar shows (Wispr's payload)

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

  // Wispr's resting bar is shown unless hidden for good or snoozed (its own
  // rule: prefs.user.hideFlowBarPermanently, clickableBubbleSnoozeUntil).
  let barVisible = null;
  let barTimer = null;
  // Wispr's microphone ranking, for the recorder to open the device Wispr
  // would (daemon/src/mics.rs). Device IDs are Chromium's, so names go along,
  // with the raw label of each device Wispr's page last listed.
  let sentMics = null;
  function updateMics(state) {
    const user = state?.prefs?.user;
    if (!Array.isArray(user?.rankedAudioDevices)) return;
    const seen = state?.sharedSettings?.audioDevices ?? state?.audioDevices;
    const label = (id) => (Array.isArray(seen) ? seen.find((d) => d?.deviceId === id)?.label : undefined) ?? null;
    const entry = (d) => ({ name: String(d?.name ?? ""), label: label(d?.deviceId), default: d?.deviceId === "default" });
    const ranked = user.rankedAudioDevices.map(entry);
    // A virtual device the user picked comes first, as in Wispr.
    const picked = user.rankedAudioDevices.find((d) => d?.deviceId === user.overrideAudioDeviceId);
    if (picked?.isVirtual) ranked.unshift(entry(picked));
    const json = JSON.stringify(ranked);
    if (json === sentMics) return;
    sentMics = json;
    trace({ ev: "mics", count: ranked.length, labelled: ranked.filter((r) => r.label).length });
    daemon?.send({ op: "mics", ranked });
  }

  function updateBar(state) {
    const user = state?.prefs?.user;
    if (!user) return;
    const until = Number(user.clickableBubbleSnoozeUntil) || 0;
    const now = Date.now();
    const visible = !user.hideFlowBarPermanently && now >= until;
    if (barTimer) clearTimeout(barTimer);
    barTimer = null;
    if (!user.hideFlowBarPermanently && until > now) barTimer = setTimeout(() => updateBar(state), Math.min(until - now + 50, 2 ** 31 - 1));
    if (visible !== barVisible) {
      barVisible = visible;
      daemon?.send({ op: "bar", visible });
    }
  }

  function onDaemonEvent(msg) {
    const hub = byRole.get("hub");
    const uuid = msg.uuid;
    switch (msg.op) {
      case "ready":
        trace({ ev: "daemon.ready" });
        break;
      case "started":
        trace({ ev: "rec.started" });
        emitFrom(hub, "dictation:recordingStarted", { transcriptionEntityUuid: uuid });
        break;
      case "chunk": {
        if (msg.index === 0) trace({ ev: "rec.firstChunk" });
        const buf = Buffer.from(msg.pcm, "base64");
        const packet = buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);
        emitFrom(hub, "dictation:audioChunk", { packet, index: msg.index, volume: msg.volume, duration: msg.duration, encoding: "wav", transcriptionEntityUuid: uuid });
        break;
      }
      case "done":
        emitFrom(hub, "dictation:recordingDone", { ...msg.timing, transcriptionEntityUuid: uuid });
        if (session === uuid) session = null;
        break;
      case "click": {
        const status = byRole.get("status");
        if (msg.action === "start") emitFrom(status, "status:startClicked", undefined);
        else if (msg.action === "stop") emitFrom(status, "status:stopClicked", undefined);
        else if (msg.action === "cancel") emitFrom(status, "status:cancelClicked", "user_action_cancel_button");
        break;
      }
      case "noticeAction": {
        const action = notice?.actions?.[msg.index];
        if (action) emitFrom(byRole.get("status"), "notification:callback", { callback: action.callback, type: notice.type, callbackOperationId: notice.callbackOperationId, isFirstHit: undefined });
        break;
      }
      case "noticeClosed":
        // The page reports its own timeout or action to main the same way.
        if (notice) emitFrom(byRole.get("status"), "notification:clear", { type: notice.type, reason: String(msg.reason) });
        notice = null;
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
      this.drawsMoreThanBar = false;
      this.meeting = false;
    }

    materialize(why) {
      if (this.materialized || this.win.isDestroyed()) return this.loading ?? Promise.resolve();
      this.materialized = true;
      this.cancelIdle();
      trace({ ev: "materialize", win: this.role, why });
      if (!this.load) {
        if (this.role === "status") daemon?.send({ op: "pill", enabled: false });
        return Promise.resolve();
      }
      const [method, args] = this.load;
      this.loading = new Promise((resolve) => {
        this.wc.once("did-finish-load", () => {
          this.loading = null;
          if (this.role === "status") setTimeout(() => { if (this.materialized) daemon?.send({ op: "pill", enabled: false }); }, HANDOVER_MS);
          const queued = this.queue.splice(0);
          for (const [ch, a] of queued) this.realSend(ch, ...a);
          if (this.wantVisible && !this.win.isDestroyed()) this.realWin[this.showMethod].call(this.win);
          // Loaded for a message but never shown: stop it again later.
          else if (this.role !== "status") this.scheduleIdle(HIDDEN_IDLE_MS, "loaded, not shown");
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
      // A running dictation needs no page: the Rust bar draws all of it.
      if (!this.notificationActive && !this.remindersActive && !this.drawsMoreThanBar) this.scheduleIdle(STATUS_IDLE_MS, "only the bar");
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
        updateMics(a?.state);
        if (this.role === "status" && a?.state) {
          const changed = this.lastState ? changedPaths(this.lastState, a.state) : [];
          this.prevState = this.lastState;
          this.lastState = a.state;
          updateBar(a.state);
          const prev = this.prevState;
          if (changed.some((p) => STATUS_WAKE_KEYS.some((re) => re.test(p)) && valueAt(prev, p) != null)) {
            trace({ ev: "wake.state", win: "status", keys: changed.slice(0, 10) });
            this.materialize("store change");
          }
        }
        return true;
      }
      if (this.role === "hub") {
        if (ch === "dictation:audioStart") {
          session = a?.transcriptionEntityUuid ?? null;
          trace({ ev: "rec.audioStart" });
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
        if (ch === "status:dictationStatus" && a === "initializing") trace({ ev: "rec.initializing" });
        const cmd = barCommand(ch, a);
        if (cmd) daemon.send(cmd);
        // The page plays this one itself when it is up.
        if (ch === "status:popo") daemon.send({ op: "sound", name: "popo" });
        if (ch === "status:dictationStatus") return !PAGE_STATUSES.test(String(a));
        if (ch === "status:instructModeStatus") return !a;
        if (cmd) return true;
        if (ch === "status:playPasteSound") {
          daemon.send({ op: "sound", name: "paste" });
          return true;
        }
        if (ch === "status:setReminders") {
          this.remindersActive = a?.reminders?.length > 0;
          if (!this.remindersActive) return true;
        }
        if (ch === "notification:show") {
          const n = daemon && nativeNotice(a, electron.app.getLocale());
          if (n) {
            notice = a;
            daemon.send(n);
            return true;
          }
          this.notificationActive = true;
        }
        if (ch === "notification:clear" && notice) {
          notice = null;
          daemon.send({ op: "notifyClear" });
        }
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
    if (ch === "zubridge:state-update") updateMics(args[0]?.state);
    if (lazy.role === "status" && ch === "zubridge:state-update" && args[0]?.state) {
      lazy.prevState = lazy.lastState;
      lazy.lastState = args[0].state;
      updateBar(args[0].state);
    }
    if (lazy.role === "status") {
      const cmd = barCommand(ch, args[0]);
      if (cmd) daemon?.send(cmd);
      if (ch === "notification:show") { lazy.notificationActive = true; lazy.checkStatusIdle(); }
      if (ch === "notification:clear") {
        lazy.notificationActive = false;
        lazy.checkStatusIdle();
        if (notice) { notice = null; daemon?.send({ op: "notifyClear" }); }
      }
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
        if (ch === "status:reportAlphaCatchRects") { lazy.drawsMoreThanBar = drawsMoreThanBar(args[0]?.rects); lazy.checkStatusIdle(); }
      }
      if (lazy.role === "hub" && ch === "hub:close") lazy.scheduleIdle(HIDDEN_IDLE_MS, "closed");
    }
    if (ch === "zubridge:dispatch") trace({ ev: "dispatch", win: lazy?.role, type: args[0]?.action?.type ?? args[0]?.type });
    else if (ch !== "zubridge:state-update-ack" && ch !== "zubridge:track-action-dispatch" && ch !== "log" && ch !== "dictation:audioChunk" && !String(ch).startsWith("sentry-ipc"))
      trace({ ev: "recv", win: lazy?.role, ch, shape: shape(args[0]) });
    return realEmit(ch, e, ...args);
  };

  const autoUpdater = pinnedUpdater();
  return new Proxy(electron, {
    get: (t, k) => (k === "BrowserWindow" ? ShimWindow : k === "autoUpdater" ? autoUpdater : t[k]),
  });
}

// Nix pins Wispr's version, so the app never updates itself: every check
// finds nothing (macOS's Squirrel would also refuse our re-signed bundle).
function pinnedUpdater() {
  const { EventEmitter } = require("events");
  const u = new EventEmitter();
  let feed = "";
  u.setFeedURL = (opts) => { feed = typeof opts === "string" ? opts : String(opts?.url ?? ""); };
  u.getFeedURL = () => feed;
  u.checkForUpdates = () => {
    trace({ ev: "update.check" });
    setImmediate(() => { u.emit("checking-for-update"); u.emit("update-not-available"); });
  };
  u.quitAndInstall = () => trace({ ev: "update.install.ignored" });
  return u;
}

// Nothing here may stop Wispr from starting.
try {
  install();
} catch (e) {
  try { console.error("wispr-shim: not installed:", e); } catch {}
}

// As the app's main (Linux), load Wispr's own entry. On macOS a loader in the
// app requires this file from the Nix profile and then loads Wispr itself.
// (Not require.main: Electron may start the main script without setting it.)
if (!globalThis.wisprShimLoader) {
  const root = path.join(__dirname, "..");
  require(path.join(root, require(path.join(root, "package.json")).wisprOriginalMain));
}
