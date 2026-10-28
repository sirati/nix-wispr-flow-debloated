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
//
// A new Wispr version must still have these names in .webpack/main/index.js:
// dictation:audioStart, dictation:audioChunk, recordingStarted,
// recordingDone, status:dictationStatus, status:audioLevel, status:micActive,
// status:popo, status:startClicked, notification:show, zubridge:state-update,
// hideFlowBarPermanently, clickableBubbleSnoozeUntil, keyboard:isRecording,
// the preload paths renderer/<role>/preload.js, the helper's KeypressEvent
// (payload exactly eventType, key, index, inputType) with prefs.user.shortcuts
// keyed by key codes, zubridge:dispatch with SET_PREFS, and
// helper:updateShortcuts.
"use strict";

const path = require("path");
const fs = require("fs");
const childProcess = require("child_process");
const readline = require("readline");
const pasteContext = new (require("async_hooks").AsyncLocalStorage)();

// Filled in by the Nix build; empty when unused (pw-cat and the PipeWire
// target on Linux, the capture and player commands on macOS, the local
// model's settings without one).
const DAEMON = "@daemon@";
const PW_CAT = "@pwcat@";
const MIC_TARGET = "@micTarget@";
const CAPTURE_CMD = "@captureCmd@";
const DEVICE_NAME = "@deviceName@";
const PLAYER_CMD = "@playerCmd@";
const FONT_DIR = "@fontDir@";
const BACKEND = "@backend@";
const ASR_CONFIG = "@asrConfig@";
const BAR_COLOR = "@barColor@";
const LOCAL_BAR_COLOR = "@localBarColor@";
// More daemon arguments, separated by spaces: its own dictation shortcuts
// (--hotkey) and how it types (--type-with, --wl-copy).
const DAEMON_ARGS = "@daemonArgs@";
// "1": the key events that would start one of Wispr's dictations are kept
// from Wispr (see keyGate).
const SUPPRESS_WISPR_SHORTCUT = "@suppressWisprShortcut@";
// Wispr's key codes for the keys its push to talk and hands-free are bound to
// (F20 and F19 by default, keys nobody presses). wispr-headless's own
// shortcuts press them for Wispr (the daemon's wisprKey), so Wispr runs its
// own push to talk and hands-free. With REBIND_WISPR_SHORTCUTS "1", the shim
// binds them in Wispr's settings itself (see rebindShortcuts).
const WISPR_PTT_CODE = "@wisprPushToTalkCode@";
const WISPR_HANDS_FREE_CODE = "@wisprHandsFreeCode@";
const REBIND_WISPR_SHORTCUTS = "@rebindWisprShortcuts@";
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
  if (p?.component === "voidDictation" && typeof p.body === "string") {
    return { op: "recover", text: p.body, local: false };
  }
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
  if (given(BACKEND)) args.push("--backend", BACKEND);
  if (given(ASR_CONFIG)) args.push("--asr-config", ASR_CONFIG);
  if (given(BAR_COLOR)) args.push("--bar-color", BAR_COLOR);
  if (given(LOCAL_BAR_COLOR)) args.push("--local-bar-color", LOCAL_BAR_COLOR);
  if (given(DAEMON_ARGS)) args.push(...DAEMON_ARGS.split(/\s+/).filter(Boolean));
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
    if (process.env.WISPR_HEADLESS_TEST_HOOKS) globalThis.wisprHeadlessObserveDaemonEvent?.(msg);
    if (msg.op === "log") trace({ ev: "daemon.log", msg: String(msg.msg).slice(0, 200) });
    else onEvent(msg);
  });
  return {
    send(o) { if (alive) try { child.stdin.write(JSON.stringify(o) + "\n"); } catch {} },
    get alive() { return alive; },
  };
}

// Correlate only actual dictations, never unrelated helper pastes. An absent
// broker permits the original paste; uncertain delivery fails closed.
function createPasteRouter(send, alive, timeout = 65_000) {
  const sessions = new Map();
  const requests = new Map();
  let next = 0;
  return {
    start(uuid) {
      if (typeof uuid !== "string") return;
      // Remember retired sessions too. A late result must fail closed rather
      // than becoming an unrelated native paste after its route expires.
      if (!sessions.has(uuid)) sessions.set(uuid, { waiting: [] });
    },
    route(payload, done) {
      const uuid = payload.transcriptEntityUUID ?? payload.logAttributes?.transcriptEntityUuid;
      if (typeof uuid !== "string" || !sessions.has(uuid)) return false;
      const session = sessions.get(uuid);
      if (session.result) { done(session.result); return true; }
      session.waiting.push(done);
      if (session.pending) return true;
      session.pending = true;
      const finish = (result) => {
        session.result = result;
        for (const callback of session.waiting.splice(0)) callback(result);
      };
      if (!alive()) {
        process.nextTick(() => finish({ applied: false, error: "dictation daemon disconnected" }));
        return true;
      }
      const request = `paste-${++next}`;
      const timer = setTimeout(() => {
        requests.delete(request);
        finish({ applied: false, error: "dictation delivery timed out" });
      }, timeout);
      requests.set(request, (result) => { clearTimeout(timer); finish(result); });
      trace({ ev: "broker.route", request, uuid, chars: String(payload.text ?? "").length });
      const submit = payload.submit ?? payload.logAttributes?.dictationSubmit;
      send({ op: "cloudTranscript", request, uuid, text: String(payload.text ?? ""), submit: submit === true || submit === "true" });
      return true;
    },
    delivered(message) {
      trace({ ev: "broker.delivered", request: message.request, applied: message.applied, fallback: message.fallback, error: message.error });
      const done = requests.get(message.request);
      requests.delete(message.request);
      done?.(message);
    },
    disconnected() {
      for (const done of requests.values()) done({ applied: false, error: "dictation daemon disconnected" });
      requests.clear();
    },
  };
}

function createCloudRetry(send, callback, delay = 1000) {
  let active = null, timer = null, attempt = 0, notification = null, retrying = false, started = false;
  const clear = () => { if (timer) clearTimeout(timer); timer = null; };
  const retry = () => {
    if (active === null || timer) return;
    retrying = true;
    send({ op: "cloudState", value: "retrying", id: active });
    timer = setTimeout(() => {
      timer = null;
      if (active === null) return;
      const payload = notification ?? { type: "TranscriptionError", actions: [{ callback: "retry_last_text" }] };
      callback(payload);
      // Wispr may swallow a failed retry without another status event.
      if (retrying) retry();
    }, Math.min(30000, delay * 2 ** Math.min(attempt++, 5)));
  };
  return {
    get active() { return active; },
    start(id) { clear(); active = id; attempt = 0; notification = null; retrying = false; started = false; },
    recording() { started = true; },
    status(value) {
      if (active === null) return;
      if (value === "error") retry();
      else if (["initializing", "listening", "stopping", "processing", "retrying"].includes(value)) { retrying = false; clear(); }
      else if (value === "idle" || value === "dismissed") {
        // A duplicate terminal status from the preceding job can arrive
        // after the next start was requested but before its audioStart.
        if (!started) return;
        const id = active; clear(); active = null; send({ op: "cloudState", value, id });
      }
    },
    notice(payload) {
      if (active === null || !["TranscriptionError", "NoInternet", "TranscriptCurrentlyProcessing", "ProcessingDelayNotification"].includes(payload?.type)) return false;
      if (payload.type === "TranscriptionError" || payload.type === "NoInternet") { notification = payload.type === "TranscriptionError" ? payload : null; retry(); }
      return true;
    },
    close() { clear(); active = null; retrying = false; },
  };
}

// These releases omit the UUID on ordinary helper pastes and on their custom
// copy notification. Add it where Wispr still owns the exact dictation state.
function patchWisprBundle(source) {
  const module = (exportName) => {
    const exports = new RegExp(String.raw`\d+\(e,t,n\)\{"use strict";(?:n\.r\(t\),)?n\.d\(t,\{[^}]*\b${exportName}:\(\)=>(\w+)[^}]*\}\);`).exec(source);
    if (!exports) throw new Error(`missing Wispr export ${exportName}`);
    const after = source.slice(exports.index + exports[0].length);
    const end = /\},\d+\(e,t,n\)\{/.exec(after);
    if (!end) throw new Error(`missing Wispr module boundary ${exportName}`);
    return { start: exports.index, end: exports.index + exports[0].length + end.index, name: exports[1] };
  };
  const notices = module("sendVoidDictationNotification");
  const body = source.slice(notices.start, notices.end);
  const notification = /\{\.\.\.\w+\.L\[\w+\.Eg\.VoidDictation\],body:\w+\}/g;
  const signature = new RegExp(String.raw`${notices.name}=\((\w+),(\w+)\)=>\{`);
  if (!signature.test(body) || [...body.matchAll(notification)].length !== 1) throw new Error("unsupported Wispr copy notification");
  const patchedNotice = body.replace(signature, `${notices.name}=($1,$2,__wisprDictationUUID,__wisprDictationSubmit)=>{`).replace(notification, (value) => value.slice(0, -1) + ",callbackOperationId:__wisprDictationUUID??globalThis.wisprHeadlessPasteUUID?.(),dictationSubmit:!!__wisprDictationSubmit}");
  source = source.slice(0, notices.start) + patchedNotice + source.slice(notices.end);
  // Only this pre-helper call belongs to the final dictation. Extension API
  // pastes must not inherit whichever dictation happened most recently.
  const voidCall = /(\w+)\.ZZ\.textboxContents\.couldNotGetTextBoxInfo\|\|!\1\.ZZ\.textboxContents\.isEditable\)&&[^;]{0,200}?\(\(0,(\w+)\.sendVoidDictationNotification\)\((\w+)\)/g;
  const guards = [...source.matchAll(voidCall)];
  const formatter = /async function \w+\(\w+\)\{(?=const\{transcriptionResult:)/g;
  const formatted = /return\{textToPaste:(\w+),formatterReturnedEmpty:\w+,statusOverride:\w+,didSubstitution:\w+,shouldPressEnterAfterPaste:(\w+)/g;
  if (guards.length !== 1 || [...source.matchAll(formatter)].length !== 1 || [...source.matchAll(formatted)].length !== 1) throw new Error("unsupported Wispr pre-helper copy guard");
  // This guard runs before Wispr interprets the spoken Enter command. Defer
  // its notice until formatting finishes so both the text and intent agree.
  const [, copyState, copyModule] = guards[0];
  source = source.replace(formatter, "$&let __wisprDeferredCopy=false;");
  source = source.replace(voidCall, (call, state, module, text) => call.replace(`(0,${module}.sendVoidDictationNotification)(${text})`, "__wisprDeferredCopy=true"));
  source = source.replace(formatted, (value, text, submit) => `__wisprDeferredCopy&&(0,${copyModule}.sendVoidDictationNotification)(${text},void 0,${copyState}.ZZ.uuid,${submit});${value}`);
  const paste = /(\w+)=\((\w+),(\w+),(\w+),(\w+)\)=>\{\w+\.RA\.lastTranscriptionStatus=\w+/.exec(source);
  if (!paste) throw new Error("unsupported Wispr final paste");
  const owner = paste[4], uuid = paste[5];
  const pasteBody = source.slice(paste.index, paste.index + 4000);
  const pasteState = /app: \$\{(\w+)\.ZZ\.appName\}/.exec(pasteBody)?.[1];
  if (!pasteState) throw new Error("unsupported Wispr paste submit intent");
  const submit = `${pasteState}.ZZ.shouldPressEnterAfterPaste`;
  const correlation = `${uuid}?{transcriptEntityUuid:${uuid}}:void 0`;
  if (source.split(correlation).length !== 2) throw new Error("unsupported Wispr paste correlation");
  // Wispr validates logAttributes as a string-to-string map before writing
  // to the helper. A boolean here rejects the entire paste request.
  source = source.replace(correlation, `${uuid}?{transcriptEntityUuid:${uuid},dictationSubmit:String(!!${submit})}:"dictation"===${owner}.kind?{transcriptEntityUuid:${owner}.transcriptEntityUUID,dictationSubmit:String(!!${submit})}:void 0`);
  const helper = module("getTextBoxInfo");
  source = source.slice(0, helper.end) + `;globalThis.wisprHeadlessTextBoxInfo=${helper.name};` + source.slice(helper.end);
  return source;
}

// ---------------------------------------------------------------- the hook

// Wispr's helper reports every key to the main process; the shim reads the
// same messages so it can start recording on the dictation shortcut itself
// (see onKey in build()), keep some from Wispr (keyGate: the daemon's own
// shortcut keys and, with SUPPRESS_WISPR_SHORTCUT, the ones that would start
// one of Wispr's dictations), and add key presses of its own (helperLink).
// Nothing per key is logged or kept beyond the keys held right now; the trace
// only gets how long before a dictation starts the last key came
// (rec.initializing's sinceKey).
let lastKeyAt = 0;
let onKey = null; // (pressed: boolean, key: number) => void, set by build()
let keyGate = null; // (pressed: boolean, key: number) => boolean, true: Wispr does not get it
let pasteGate = null; // broker delivery for correlated Wispr transcripts
let helperLink = null; // the running helper's { inject(key, pressed) }, see gateHelper

// The helper's framing: messages separated by "|", with "|" escaped as "+2"
// and "+" as "+1" (Wispr's IPC reader).
const unescapeHelper = (s) => s.replaceAll("+2", "|").replaceAll("+1", "+");
const escapeHelper = (s) => s.replaceAll("+", "+1").replaceAll("|", "+2");

// A key event's key as Wispr's keyboard service sees it: mouse buttons 2-9
// become 4098-4105 (mouse_middle .. mouse10), other mouse buttons nothing.
function wisprKey(p) {
  if (typeof p?.key !== "number") return undefined;
  if ((p.inputType || "keyboard") !== "mouse") return p.key;
  return p.key >= 2 && p.key <= 9 ? 4096 + p.key : undefined;
}

function sawKey(p) {
  lastKeyAt = Date.now();
  const key = wisprKey(p);
  const pressed = !/release/.test(String(p?.eventType));
  if (key !== undefined) onKey?.(pressed, key);
  return { key, pressed };
}

function watchHelper() {
  const realSpawn = childProcess.spawn;
  childProcess.spawn = function (command, ...rest) {
    const child = realSpawn.call(this, command, ...rest);
    try {
      // Its messages come on an extra pipe (fd 3); stdout only carries logs.
      if (given(DAEMON) && /swift-helper-app-dist|wispr-flow-linux-helper/.test(String(command)) && child.stdio?.[3]) gateHelper(child);
    } catch (e) {
      trace({ ev: "helper.hookFailed", error: String(e).slice(0, 200) });
    }
    return child;
  };
}

// Wispr reads the helper with stdio[3].on("data"), so every 'data' event of
// that pipe comes through here first and Wispr gets only whole messages. They
// pass unchanged, except key events keyGate keeps from Wispr: those the shim
// acknowledges as Wispr would, and the ones that pass are numbered again
// (payload.index), as Wispr warns about every gap in the helper's count.
// Key events of the shim's own (inject) go to Wispr the same way, numbered in
// the same count; Wispr's ACKs for them are kept from the helper, which never
// sent them.
function gateHelper(child) {
  const fd3 = child.stdio[3];
  const realEmit = fd3.emit;
  const BAR = Buffer.from("|");
  let pending = Buffer.alloc(0);
  let passed = 0;
  let kept = 0;
  const stdin = child.stdin;
  const realWrite = stdin?.write;
  const ack = (uuid) => {
    if (typeof uuid !== "string" || !realWrite) return;
    try { realWrite.call(stdin, escapeHelper(JSON.stringify({ HelperAPIResponse: { uuid, ACK: true } })) + "|"); } catch {}
  };

  // Our events' uuids until Wispr acknowledges them (Wispr drops a message it
  // cannot read without an ACK, hence the bound).
  const ours = new Set();
  const pasteSessions = new Map();
  let injectedCount = 0;
  let suppressPasteKeys = false;
  const emitMessage = (message) => {
    const pasteId = message?.HelperAPIRequest?.PasteOutcome?.payload?.pasteId;
    const session = pasteSessions.get(pasteId);
    if (pasteId) pasteSessions.delete(pasteId);
    return pasteContext.run(session, () => realEmit.call(fd3, "data", Buffer.from(escapeHelper(JSON.stringify(message)) + "|", "utf8")));
  };
  const outcome = (request, payload, result) => {
    emitMessage({ HelperAPIResponse: { uuid: request.uuid, ACK: true } });
    const uuid = `wispr-headless-${process.pid}-${++injectedCount}`;
    ours.add(uuid);
    if (ours.size > 1000) ours.delete(ours.values().next().value);
    // Successful native pastes can trigger cursor moves and Enter in Wispr.
    // Those must never be sent globally after broker delivery.
    suppressPasteKeys = true;
    emitMessage({ HelperAPIRequest: { uuid, PasteOutcome: { payload: {
      pasteId: payload.pasteId, success: Boolean(result.applied),
      reason: result.error || undefined, timeElapsedMs: 0,
    } } } });
    setImmediate(() => { suppressPasteKeys = false; });
  };
  const ackOf = (message) => {
    if (!message.includes("HelperAPIResponse")) return undefined;
    try { return JSON.parse(unescapeHelper(message))?.HelperAPIResponse?.uuid; } catch { return undefined; }
  };
  // Wispr writes each message to the helper's stdin in one call, ending in "|".
  if (stdin && realWrite) stdin.write = function (chunk, ...rest) {
    try {
      const text = typeof chunk === "string" ? chunk : Buffer.isBuffer(chunk) ? chunk.toString("utf8") : null;
      if (text?.endsWith("|")) {
        const messages = text.slice(0, -1).split("|");
        const keep = messages.filter((raw) => {
          if (ours.delete(ackOf(raw))) return false;
          let request;
          try { request = JSON.parse(unescapeHelper(raw))?.HelperAPIRequest; } catch { return true; }
          const payload = request?.PasteText?.payload;
          const session = payload?.transcriptEntityUUID ?? payload?.logAttributes?.transcriptEntityUuid;
          if (payload?.pasteId && typeof session === "string") {
            pasteSessions.set(payload.pasteId, session);
            if (pasteSessions.size > 1000) pasteSessions.delete(pasteSessions.keys().next().value);
          }
          if (payload && pasteGate?.(payload, (result) => {
            if (result.fallback) realWrite.call(stdin, raw + "|");
            else outcome(request, payload, result);
          })) return false;
          if (suppressPasteKeys && request?.SimulateKeyPress) {
            process.nextTick(() => emitMessage({ HelperAPIResponse: { uuid: request.uuid, ACK: true } }));
            return false;
          }
          return true;
        });
        if (keep.length === 0) {
          const done = rest.find((a) => typeof a === "function");
          if (done) process.nextTick(done);
          return true;
        }
        if (keep.length < messages.length) {
          const out = keep.map((m) => m + "|").join("");
          chunk = typeof chunk === "string" ? out : Buffer.from(out, "utf8");
        }
      }
    } catch {}
    return realWrite.call(this, chunk, ...rest);
  };

  const link = {
    inject(key, pressed) {
      const uuid = `wispr-headless-${process.pid}-${++injectedCount}`;
      ours.add(uuid);
      if (ours.size > 1000) ours.delete(ours.values().next().value);
      const payload = { eventType: pressed ? "key_event_press" : "key_event_release", key, index: ++passed, inputType: "keyboard" };
      const message = { HelperAPIRequest: { uuid, KeypressEvent: { payload } } };
      realEmit.call(fd3, "data", Buffer.from(escapeHelper(JSON.stringify(message)) + "|", "utf8"));
    },
  };
  helperLink = link;
  child.once("exit", () => {
    if (helperLink === link) helperLink = null;
  });
  // The message to hand on (raw, still escaped), or null to keep it.
  const gate = (raw) => {
    const text = unescapeHelper(raw.toString("utf8"));
    if (!text.includes("KeypressEvent")) return raw;
    let message;
    try { message = JSON.parse(text); } catch { return raw; }
    const p = message?.HelperAPIRequest?.KeypressEvent?.payload;
    if (!p || typeof p !== "object") return raw;
    const { key, pressed } = sawKey(p);
    if (key !== undefined && keyGate?.(pressed, key)) {
      if (kept++ === 0) trace({ ev: "wisprShortcut.suppressed" });
      ack(message.HelperAPIRequest.uuid);
      return null;
    }
    p.index = ++passed;
    return Buffer.from(escapeHelper(JSON.stringify(message)), "utf8");
  };
  fd3.emit = function (name, chunk, ...rest) {
    if (name !== "data") return realEmit.call(this, name, chunk, ...rest);
    pending = Buffer.concat([pending, Buffer.isBuffer(chunk) ? chunk : Buffer.from(String(chunk), "utf8")]);
    const out = [];
    let start = 0;
    // "|" (0x7c) is one byte in UTF-8 and never part of another character.
    for (let bar; (bar = pending.indexOf(0x7c, start)) !== -1; start = bar + 1) {
      const raw = pending.subarray(start, bar);
      let handOn = raw;
      try { handOn = gate(raw); } catch (e) { trace({ ev: "keyGate.error", error: String(e).slice(0, 200) }); }
      if (handOn) out.push(Buffer.concat([handOn, BAR]));
    }
    pending = Buffer.from(pending.subarray(start));
    if (out.length === 0) return false;
    let emitted = false;
    for (const data of out) {
      let pasteId;
      try { pasteId = JSON.parse(unescapeHelper(data.subarray(0, -1).toString("utf8")))?.HelperAPIRequest?.PasteOutcome?.payload?.pasteId; } catch {}
      const session = pasteSessions.get(pasteId);
      if (pasteId) pasteSessions.delete(pasteId);
      emitted = pasteContext.run(session, () => realEmit.call(this, name, typeof chunk === "string" ? data.toString("utf8") : data, ...rest)) || emitted;
    }
    return emitted;
  };
}

function install() {
  globalThis.wisprHeadlessPasteUUID = () => pasteContext.getStore();
  watchHelper();
  const Module = require("module");
  const realLoad = Module._load;
  const realJs = Module._extensions[".js"];
  Module._extensions[".js"] = function (mod, filename) {
    if (!/[\\/]\.webpack[\\/]main[\\/]index\.js$/.test(filename)) return realJs(mod, filename);
    const source = fs.readFileSync(filename, "utf8");
    let patched = source;
    try {
      patched = patchWisprBundle(source);
      trace({ ev: "broker.bundlePatched" });
    } catch (error) {
      trace({ ev: "broker.bundlePatchFailed", error: String(error) });
    }
    mod._compile(patched, filename);
  };
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
  let cloudStartedAt = 0;
  let cloudReleaseTimer = null;
  let cloudCancelled = false;
  const clearCloudRelease = () => {
    if (cloudReleaseTimer) clearTimeout(cloudReleaseTimer);
    cloudReleaseTimer = null;
  };

  const fakeEvent = (wc) => ({ sender: wc, senderFrame: null, frameId: 0, processId: 0, ports: [], returnValue: undefined, reply: () => {} });
  const emitFrom = (lazy, ch, payload) => {
    if (!lazy || lazy.win.isDestroyed()) return;
    realEmit(ch, fakeEvent(lazy.wc), payload);
  };

  const daemon = startDaemon(process.resourcesPath, onDaemonEvent, () => {
    clearCloudRelease();
    cloudRetry.close();
    brokerPastes.disconnected();
    // Wispr must not keep a key held that nobody will release.
    releaseWisprKeys();
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

  const brokerPastes = createPasteRouter((message) => daemon.send(message), () => Boolean(daemon?.alive));
  const cloudRetry = createCloudRetry((message) => daemon?.send(message), (payload) => {
    const action = payload.actions?.find((action) => action.callback === "retry_last_text");
    trace({ ev: "cloud.autoRetry" });
    emitFrom(byRole.get("status"), "notification:callback", { callback: action?.callback ?? "retry_last_text", type: payload.type, callbackOperationId: payload.callbackOperationId });
    emitFrom(byRole.get("status"), "notification:clear", { type: payload.type, reason: "action" });
  });
  pasteGate = (payload, done) => brokerPastes.route(payload, (result) => {
    if (!result.fallback && !result.applied) daemon?.send({ op: "recover", text: String(payload.text ?? ""), local: false });
    done(result);
  });

  function copyNotice(payload) {
    const recovery = nativeNotice(payload, electron.app.getLocale());
    if (recovery?.op !== "recover" || !daemon?.alive) return false;
    const show = (result) => {
      if (!result.applied) daemon.send(recovery);
      emitFrom(byRole.get("status"), "notification:clear", { type: payload.type, reason: "action" });
    };
    if (!brokerPastes.route({ transcriptEntityUUID: payload.callbackOperationId, text: recovery.text, submit: payload.dictationSubmit }, show)) show({ applied: false });
    return true;
  }

  // Wispr's resting bar is shown unless hidden for good or snoozed (its own
  // rule: prefs.user.hideFlowBarPermanently, clickableBubbleSnoozeUntil).
  let barVisible = null;
  let barTimer = null;
  // Wispr's microphone ranking, for the recorder to open the device Wispr
  // would (daemon/src/mics.rs). Device IDs are Chromium's, so names go along,
  // with the raw label of each device Wispr's page last listed.
  let sentMics = null;
  // Start recording on the dictation shortcut itself, as Wispr starts its
  // dictation on it: Wispr keys its shortcuts (prefs.user.shortcuts) by the
  // key codes held, sorted as strings and joined with "+". If another key
  // joins before Wispr asks to record, the combination is no longer a
  // dictation (fn+F-key, say) and the early recording is dropped; once Wispr
  // has asked, only Wispr stops it. Wispr's own list of the actions that
  // start a dictation: push to talk, hands-free, command mode (lens, command).
  const DICTATION_ACTIONS = new Set(["ptt", "popo", "lens", "command"]);
  // Until Wispr's store arrives, its defaults: Fn, Fn+Space, Fn+Ctrl on the
  // Mac; Ctrl+Win, Ctrl+Win+Space, Ctrl+Win+Alt (Windows key codes) on Linux.
  let dictationKeys = new Set(process.platform === "darwin" ? ["63", "49+63", "59+63"] : ["162+91", "162+32+91", "162+164+91"]);
  let dictationSets = [];
  const held = new Set();
  let early = false;
  let earlyAt = 0;
  function setDictationKeys(keys) {
    dictationKeys = keys;
    dictationSets = [...keys].map((k) => k.split("+").map(Number));
  }
  setDictationKeys(dictationKeys);
  function updateShortcuts(state) {
    const shortcuts = state?.prefs?.user?.shortcuts;
    if (!shortcuts || typeof shortcuts !== "object") return;
    const keys = Object.entries(shortcuts).filter(([, action]) => DICTATION_ACTIONS.has(action)).map(([keys]) => keys);
    if (keys.join() !== [...dictationKeys].join()) setDictationKeys(new Set(keys));
    rebindShortcuts(shortcuts);
  }
  if (!given(SUPPRESS_WISPR_SHORTCUT)) onKey = (pressed, key) => {
    if (!daemon || !headless) return;
    if (!pressed) {
      held.delete(key);
      return;
    }
    held.add(key);
    const combo = [...held].map(String).sort().join("+");
    // An early start counts until Wispr asks to record or the daemon gives
    // up on it (EARLY_START_PATIENCE there).
    const pending = early && !session && Date.now() - earlyAt < 500;
    if (dictationKeys.has(combo)) {
      if (!pending && !session) {
        early = true;
        earlyAt = Date.now();
        daemon.send({ op: "early" });
      }
    } else if (pending) {
      early = false;
      daemon.send({ op: "earlyCancel" });
    }
  };

  // Keys kept from Wispr. The daemon's own shortcut keys (hideKeys): Wispr
  // starts a dictation when a key goes down and the keys it knows to be held
  // are exactly a shortcut, so Right Option held while the shim presses F20
  // for Wispr would make it no shortcut at all. And, with
  // SUPPRESS_WISPR_SHORTCUT, Wispr's own dictation shortcuts: a press that
  // could make the held keys one never reaches Wispr, nor does that key's
  // release. Every other key does, so Escape, paste-last, undo and Wispr's
  // other shortcuts work as before, as do the held modifiers Wispr looks at.
  // Wispr may have dropped keys it thought stuck, so a press counts if it
  // completes a shortcut with any of the keys it was told about. Off while
  // Wispr's settings record a new shortcut, and when wispr-headless is not
  // running. A key whose press Wispr got also gets its release.
  const toldWispr = new Set(); // keys down whose press Wispr got
  const keptFromWispr = new Set(); // keys down whose press it did not
  let hiddenKeys = new Set(); // the daemon's shortcut keys (hideKeys)
  keyGate = (pressed, key) => {
    if (!pressed) {
      toldWispr.delete(key);
      return keptFromWispr.delete(key);
    }
    if (keptFromWispr.has(key)) return true;
    const active = daemon?.alive && headless && !recordingShortcut;
    const after = new Set(toldWispr).add(key);
    const keep = active && (hiddenKeys.has(key) || (given(SUPPRESS_WISPR_SHORTCUT) && dictationSets.some((s) => s.includes(key) && s.every((k) => after.has(k)))));
    if (!keep) toldWispr.add(key);
    // (A repeat of a key Wispr has is dropped too; Wispr does nothing on it.)
    else if (!toldWispr.has(key)) keptFromWispr.add(key);
    return Boolean(keep);
  };
  let recordingShortcut = false; // Wispr's settings record a shortcut

  // The daemon presses Wispr's push to talk and hands-free keys (F20 and F19
  // by default) for its own shortcuts, so Wispr runs its own dictation
  // exactly as on its shortcut. Released again if the daemon goes away.
  const isMac = process.platform === "darwin";
  const wisprKeys = {
    ptt: given(WISPR_PTT_CODE) ? Number(WISPR_PTT_CODE) : isMac ? 90 : 131,
    popo: given(WISPR_HANDS_FREE_CODE) ? Number(WISPR_HANDS_FREE_CODE) : isMac ? 80 : 130,
  };
  const injectedDown = new Set();
  function pressWisprKey(action, down) {
    const key = wisprKeys[action];
    if (key === undefined) return trace({ ev: "wisprKey.unknown", action: String(action).slice(0, 20) });
    if (!helperLink) return trace({ ev: "wisprKey.noHelper", action, down });
    if (down) injectedDown.add(key);
    else injectedDown.delete(key);
    helperLink.inject(key, down);
  }
  function releaseWisprKeys() {
    for (const key of injectedDown) helperLink?.inject(key, false);
    injectedDown.clear();
  }

  // Wispr's push to talk and hands-free bound to exactly those keys, with all
  // its other shortcuts kept: the store's SET_PREFS action, as Wispr's
  // settings page dispatches it (main replaces user.shortcuts as a whole), and
  // then helper:updateShortcuts as the page sends after it. Wispr only adds a
  // default binding back for an action that has none, so Fn stays unbound.
  // Once per shortcut map seen, and only while the daemon runs.
  let rebound = null; // the map last corrected
  function rebindShortcuts(shortcuts) {
    if (!given(REBIND_WISPR_SHORTCUTS) || !daemon?.alive || !headless || recordingShortcut) return;
    const want = { [wisprKeys.ptt]: "ptt", [wisprKeys.popo]: "popo" };
    const entries = Object.entries(shortcuts);
    const right = (action) => shortcuts[wisprKeys[action]] === action && entries.every(([k, a]) => (a === action) === (want[k] === action));
    if (right("ptt") && right("popo")) return;
    const json = JSON.stringify(shortcuts);
    if (json === rebound) return;
    rebound = json;
    const sender = byRole.get("hub") ?? byRole.get("status");
    if (!sender || sender.win.isDestroyed()) return trace({ ev: "wisprShortcut.rebind", skipped: "no window" });
    const corrected = Object.fromEntries(entries.filter(([k, a]) => a !== "ptt" && a !== "popo" && !(k in want)));
    Object.assign(corrected, want);
    trace({ ev: "wisprShortcut.rebind", ptt: wisprKeys.ptt, popo: wisprKeys.popo });
    realEmit("zubridge:dispatch", fakeEvent(sender.wc), {
      action: { type: "SET_PREFS", payload: { user: { shortcuts: corrected } }, __id: `wispr-headless-rebind-${Date.now()}` },
    });
    setTimeout(() => emitFrom(sender, "helper:updateShortcuts", undefined), 200);
  }

  // The bar's buttons, clicked on the Rust bar or by wispr-headless for a
  // dictation of its own (its shortcut). A stop or cancel that comes before
  // Wispr asks to record waits for that request, so a short press on the
  // shortcut does not leave a dictation running that Wispr was still
  // starting; Wispr's dictation ending first forgets it.
  let clickedStart = false;
  let pendingEnd = null; // the click that ends it once Wispr records
  const endClick = (ch) => emitFrom(byRole.get("status"), ch, ch === "status:cancelClicked" ? "user_action_cancel_button" : undefined);
  function onClick(action) {
    if (action === "start") {
      clickedStart = !session;
      pendingEnd = null;
      emitFrom(byRole.get("status"), "status:startClicked", undefined);
      return;
    }
    const ch = { stop: "status:stopClicked", cancel: "status:cancelClicked" }[action];
    if (!ch) return;
    if (clickedStart && !session) {
      pendingEnd = ch;
    } else {
      clickedStart = false;
      endClick(ch);
    }
  }
  function sawStatus(ch, a) {
    const previous = cloudRetry.active;
    if (ch === "status:dictationStatus") cloudRetry.status(String(a));
    if (previous !== null && cloudRetry.active === null) {
      clearCloudRelease();
      releaseWisprKeys();
    }
    if (ch === "status:dictationStatus" && /^(idle|dismissed|error)$/.test(String(a))) {
      clickedStart = false;
      pendingEnd = null;
    }
  }

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
      case "cloudStart":
        clearCloudRelease();
        cloudStartedAt = Date.now();
        cloudCancelled = false;
        cloudRetry.start(msg.id);
        break;
      case "cloudDelivery": {
        brokerPastes.delivered(msg);
        break;
      }
      case "copyTranscript":
        try { electron.clipboard.writeText(String(msg.text)); }
        catch (error) {
          trace({ ev: "clipboard.failed", error: String(error) });
          daemon?.send({ op: "recover", text: String(msg.text), local: true });
        }
        break;
      case "insertTranscript":
        // The broker already declined this session. Reuse Wispr's editable
        // input check before ordinary typing so a transcript cannot disappear.
        Promise.resolve().then(() => globalThis.wisprHeadlessTextBoxInfo?.()).then((info) => {
          daemon?.send({ op: info?.isEditable && !info.couldNotGetTextBoxInfo ? "typeTranscript" : "recover", text: String(msg.text), local: true });
        }, () => daemon?.send({ op: "recover", text: String(msg.text), local: true }));
        break;
      case "ready":
        trace({ ev: "daemon.ready" });
        break;
      case "started":
        cloudRetry.recording();
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
      case "click":
        if (msg.action === "cancel") {
          cloudRetry.recording(); // A cancel before audioStart is terminal too.
          cloudCancelled = true;
          clearCloudRelease();
        }
        onClick(msg.action);
        break;
      case "wisprKey":
        if (msg.down) { clearCloudRelease(); clickedStart = true; }
        if (msg.action === "ptt" && !msg.down && !cloudCancelled && cloudRetry.active !== null && Date.now() - cloudStartedAt < 650) {
          if (cloudReleaseTimer) clearTimeout(cloudReleaseTimer);
          // Wispr cancels PTT released inside its double-tap debounce. A
          // buffered recording has real audio even when replay takes no time.
          const id = cloudRetry.active;
          cloudReleaseTimer = setTimeout(() => {
            cloudReleaseTimer = null;
            if (cloudRetry.active === id) pressWisprKey("ptt", false);
          }, Math.max(0, 650 - (Date.now() - cloudStartedAt)));
        } else pressWisprKey(msg.action, Boolean(msg.down));
        break;
      case "hideKeys":
        hiddenKeys = new Set((Array.isArray(msg.keys) ? msg.keys : []).map(Number).filter(Number.isInteger));
        trace({ ev: "hideKeys", count: hiddenKeys.size });
        break;
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
      // A dictation the daemon transcribed itself; Wispr never sees it.
      case "transcript":
        trace({ ev: "local.transcript", chars: String(msg.text).length });
        break;
      case "transcriptFailed":
        trace({ ev: "local.failed", error: String(msg.error).slice(0, 200) });
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
        updateShortcuts(a?.state);
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
          cloudStartedAt = Date.now();
          session = a?.transcriptionEntityUuid ?? null;
          brokerPastes.start(session);
          trace({ ev: "rec.audioStart" });
          daemon.send({ op: "start", uuid: session });
          if (pendingEnd) {
            const ch = pendingEnd;
            pendingEnd = null;
            clickedStart = false;
            setImmediate(() => endClick(ch));
          }
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
        if (ch === "status:dictationStatus" && a === "initializing") trace({ ev: "rec.initializing", sinceKey: lastKeyAt ? Date.now() - lastKeyAt : null });
        sawStatus(ch, a);
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
          if (cloudRetry.notice(a)) return true;
          if (copyNotice(a)) return true;
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
    if (ch === "zubridge:state-update") {
      updateMics(args[0]?.state);
      updateShortcuts(args[0]?.state);
    }
    if (lazy.role === "status" && ch === "zubridge:state-update" && args[0]?.state) {
      lazy.prevState = lazy.lastState;
      lazy.lastState = args[0].state;
      updateBar(args[0].state);
    }
    if (lazy.role === "status") {
      if (ch === "notification:show" && cloudRetry.notice(args[0])) return true;
      if (ch === "notification:show" && copyNotice(args[0])) return true;
      sawStatus(ch, args[0]);
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
    if (ch === "keyboard:isRecording") recordingShortcut = Boolean(args[0]);
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

  // Tests only: the stand-in app plays daemon messages the test daemon does
  // not send.
  if (process.env.WISPR_HEADLESS_TEST_HOOKS) {
    globalThis.wisprHeadlessDaemonEvent = onDaemonEvent;
    globalThis.wisprHeadlessDaemonCommand = (message) => daemon?.send(message);
  }

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
