// Stand-in for Wispr's main process, with the same window layout and IPC
// channels, driven through a scripted scenario. Prints PASS/FAIL lines and
// exits non-zero on the first failure.
"use strict";

const { app, BrowserWindow, ipcMain, clipboard } = require("electron");
const { execSync } = require("child_process");
const path = require("path");

app.commandLine.appendSwitch("no-sandbox");

let failures = 0;
const check = (ok, what) => {
  console.log(`${ok ? "PASS" : "FAIL"} ${what}`);
  if (!ok) failures++;
};
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (what, ok, ms = 10_000) => {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (ok()) return true;
    await wait(50);
  }
  return false;
};

// Renderer processes of this app, by process tree.
function renderers() {
  const rows = execSync("ps -eo pid,ppid,args --no-headers").toString().split("\n").map((l) => l.trim().split(/\s+/));
  const mine = new Set([String(process.pid)]);
  let grew = true;
  while (grew) {
    grew = false;
    for (const [pid, ppid] of rows) if (mine.has(ppid) && !mine.has(pid)) { mine.add(pid); grew = true; }
  }
  return rows.filter(([pid, , ...args]) => mine.has(pid) && args.join(" ").includes("--type=renderer")).length;
}

const got = [];
const loaded = [];
const acks = new Map(); // window id -> count
const chunks = [];
const daemonEvents = [];
globalThis.wisprHeadlessObserveDaemonEvent = (event) => daemonEvents.push(event);
let started = null;
let done = null;
let gone = 0;

ipcMain.on("test:got", (_e, a) => got.push(a));
ipcMain.on("test:loaded", (_e, a) => loaded.push(a.role));
ipcMain.on("zubridge:state-update-ack", (e, a) => acks.set(e.sender.id, (acks.get(e.sender.id) ?? 0) + (a?.updateId ? 1 : 0)));
ipcMain.on("dictation:recordingStarted", (_e, a) => { started = a; });
ipcMain.on("dictation:audioChunk", (_e, a) => chunks.push(a));
ipcMain.on("dictation:recordingDone", (_e, a) => { done = a; });
ipcMain.on("status:reportAlphaCatchRects", () => {});
let startClicks = 0;
ipcMain.on("status:startClicked", () => startClicks++);
let cancelled = null;
ipcMain.on("status:cancelClicked", (_e, a) => { cancelled = a; });

const page = (role) => path.join(__dirname, "renderer", role, "index.html");
const make = (role, title) =>
  new BrowserWindow({ title, show: false, transparent: true, webPreferences: { preload: path.join(__dirname, "renderer", role, "preload.js"), sandbox: false, backgroundThrottling: false } });

app.whenReady().then(async () => {
  const status = make("status", "Flow Status Indicator");
  const hub = make("hub", "Flow Hub");
  const menu = make("contextMenu", "Flow Context Menu");
  for (const w of [status, hub, menu]) w.webContents.on("render-process-gone", () => gone++);
  status.loadFile(page("status"));
  menu.loadFile(page("contextMenu"));
  hub.loadFile(page("hub"));
  status.showInactive();

  await wait(2_000);
  check(renderers() === 0, `no renderers at startup (have ${renderers()})`);
  check(status.isVisible(), "status window counts as shown");

  // Store sync: virtual windows acknowledge.
  for (let i = 0; i < 3; i++) for (const w of [status, hub, menu]) w.webContents.send("zubridge:state-update", { updateId: `u${i}-${w.id}`, state: { prefs: { n: i } } });
  await wait(300);
  check([status, hub, menu].every((w) => acks.get(w.webContents.id) === 3), `every virtual window acknowledged 3 updates (${[...acks.values()]})`);

  // First-time store data does not load the status page, and the resting
  // bar is on by Wispr's rule.
  const withSubscription = (id, daysLeft) => ({
    updateId: id,
    state: { prefs: { user: { hideFlowBarPermanently: false } }, subscription: { daysLeft } },
  });
  status.webContents.send("zubridge:state-update", withSubscription("s1", 5));
  await wait(800);
  check(renderers() === 0, "first-time store data does not load the status page");

  // Nix pins the version: update checks always find nothing.
  const { autoUpdater } = require("electron");
  let updateAnswer = null;
  autoUpdater.once("update-not-available", () => { updateAnswer = "none"; });
  autoUpdater.once("update-available", () => { updateAnswer = "available"; });
  autoUpdater.setFeedURL({ url: "https://example.invalid/feed" });
  autoUpdater.checkForUpdates();
  await wait(100);
  check(updateAnswer === "none", `update checks find no update (${updateAnswer})`);

  // The resting bar begins capture immediately, even before Wispr starts.
  let xdotool = null;
  try { xdotool = execSync("command -v xdotool").toString().trim(); } catch {}
  if (xdotool) {
    execSync(`${xdotool} mousemove 640 786 click 1`); // the resting bar: 40x8 at 10 px above the bottom of the 1280x800 screen
    await until("click", () => daemonEvents.some((event) => event.op === "cloudStart"), 3_000);
    check(daemonEvents.some((event) => event.op === "cloudStart"), "a resting-bar click starts its own retained cloud capture");
  } else {
    console.log("SKIP bar click (no xdotool)");
  }

  // Dictation through the recorder.
  hub.webContents.send("dictation:audioStart", { useOpus: false, transcriptionEntityUuid: "dict-1" });
  // Wispr's sequence for a hands-free dictation (double tap).
  status.webContents.send("status:dictationStatus", "initializing");
  status.webContents.send("status:popo");
  status.webContents.send("status:dictationStatus", "listening");
  status.webContents.send("status:micActive", true);
  status.webContents.send("status:micSilence", false);
  status.webContents.send("status:commandModeStatus", false);
  status.webContents.send("status:audioLevel", 0.5);
  await until("started", () => started);
  check(started?.transcriptionEntityUuid === "dict-1", "recordingStarted came back for the session");
  await until("chunks", () => chunks.length >= 10);
  check(renderers() === 0, "hands-free dictation draws in Rust, no status page");
  if (xdotool) {
    // The hands-free pill: 103x30 at the bottom centre; cancel is its left button.
    execSync(`${xdotool} mousemove 603 775 click 1`);
    await until("cancel", () => cancelled !== null, 3_000);
    check(cancelled === "user_action_cancel_button", `the cancel button sends status:cancelClicked (${cancelled})`);
  }
  hub.webContents.send("dictation:audioStop");
  status.webContents.send("status:dictationStatus", "processing");
  await until("done", () => done);
  check(done?.transcriptionEntityUuid === "dict-1" && done?.micAcquisitionSucceeded === true, "recordingDone came back with timing");
  check(chunks.length >= 10 && chunks.every((c, i) => c.index === i), `chunks in order from 0 (${chunks.length})`);
  check(chunks.every((c) => c.packet instanceof ArrayBuffer && c.encoding === "wav" && c.transcriptionEntityUuid === "dict-1"), "chunks are WAV ArrayBuffers for the session");
  check(chunks.slice(0, -1).every((c) => c.packet.byteLength === 1280 && c.duration === 40), "whole chunks are 640 samples, 40 ms");
  status.webContents.send("status:dictationStatus", "idle");
  check(renderers() === 0, "still no renderers after a dictation");

  // Wispr's "Transcript cancelled" pill is drawn in Rust, with its Undo.
  let callback = null;
  let cleared = null;
  ipcMain.on("notification:callback", (_e, a) => { callback = a; });
  ipcMain.on("notification:clear", (_e, a) => { cleared = a; });
  status.webContents.send("notification:show", {
    type: "TranscriptCancelled", status: "info", title: { key: "transcript_cancelled_title" },
    actions: [{ text: { key: "transcript_cancelled_detailed_action_undo" }, callback: "retry_last_text" }],
    variant: "condensed", window: "status", timeout: 3000, playSound: false,
  });
  await wait(300);
  check(renderers() === 0, "the cancel notification draws in Rust, no status page");
  if (xdotool) {
    // Without fonts the bar estimates 0.55 x 15 px per character: the pill is
    // 266x48 at the bottom centre, Undo at x 197..250 of it.
    execSync(`${xdotool} mousemove 730 766 click 1`);
    await until("undo", () => callback !== null, 3_000);
    check(callback?.callback === "retry_last_text" && callback?.type === "TranscriptCancelled", `Undo sends notification:callback retry_last_text (${callback?.callback})`);
    await until("closed", () => cleared !== null, 3_000);
    check(cleared?.reason === "action", `the pill closes after Undo with reason action (${cleared?.reason})`);
  }
  cleared = null;
  // (Off the pill: the pointer on it pauses the timer.)
  if (xdotool) execSync(`${xdotool} mousemove 10 10`);
  status.webContents.send("notification:show", {
    type: "TranscriptCancelled", title: { key: "transcript_cancelled_title" }, actions: [], variant: "condensed", timeout: 500,
  });
  await until("timeout", () => cleared !== null, 3_000);
  check(cleared?.reason === "timeout", `an untouched notification times out (${cleared?.reason})`);

  // Wispr's custom copy prompt and local insertion without an editable
  // target both retain text in the Rust bubble, including after idle IPC.
  status.webContents.send("notification:show", {
    type: "VoidDictation", component: "voidDictation", variant: "custom",
    body: "cloud recovery text", callbackOperationId: "unrelated-copy", timeout: 3000,
  });
  await wait(400);
  check(renderers() === 0, "Wispr's custom copy prompt stays in Rust");
  if (xdotool) {
    execSync(`${xdotool} mousemove 710 766 click 1`);
    await until("cloud copied", () => clipboard.readText() === "cloud recovery text");
    check(clipboard.readText() === "cloud recovery text", "Copy recovers the Wispr transcript");
    await wait(800);
    globalThis.wisprHeadlessDaemonEvent({ op: "insertTranscript", text: "local recovery text" });
    await wait(300);
    status.webContents.send("status:dictationStatus", "idle");
    execSync(`${xdotool} mousemove 710 766 click 1`);
    await until("local copied", () => clipboard.readText() === "local recovery text");
    check(clipboard.readText() === "local recovery text", "Copy recovers a local transcript without an editable input");
    await wait(800);
    check(renderers() === 0, "both Copy actions leave the status renderer unloaded");
  }

  // Two unfinished jobs stay in native processing rows. A green icon starts
  // local recognition on the retained audio without cancelling cloud work.
  const command = globalThis.wisprHeadlessDaemonCommand;
  command({ op: "dictate", backend: "wispr" });
  await until("queued cloud started", () => daemonEvents.filter((event) => event.op === "cloudStart").length >= 2);
  hub.webContents.send("dictation:audioStart", { transcriptionEntityUuid: "stack-1" });
  await until("stack recording", () => started?.transcriptionEntityUuid === "stack-1");
  await wait(100);
  command({ op: "dictateEnd" });
  hub.webContents.send("dictation:audioStop");
  status.webContents.send("status:dictationStatus", "processing");
  await until("stack done", () => done?.transcriptionEntityUuid === "stack-1");
  command({ op: "dictate", backend: "wispr" });
  await wait(100);
  command({ op: "dictateEnd" });
  await wait(300);
  if (xdotool) {
    const before = daemonEvents.length;
    execSync(`${xdotool} mousemove 688 721 click 1`);
    await until("local alternative", () => daemonEvents.slice(before).some((event) => event.op === "log" && event.msg.includes("local alternative failed")));
    const attempted = daemonEvents.slice(before).some((event) => event.op === "log" && event.msg.includes("local alternative failed"));
    check(attempted, "the upper processing bubble's green icon starts local recognition");
    if (!attempted) console.log("green button diagnostics", JSON.stringify(daemonEvents.slice(before)));
    check(cancelled === "user_action_cancel_button", "the green icon does not cancel Wispr");
  }
  callback = null;
  status.webContents.send("status:dictationStatus", "error");
  status.webContents.send("notification:show", { type: "TranscriptionError", actions: [{ callback: "retry_last_text" }] });
  await until("automatic retry", () => callback?.type === "TranscriptionError", 3000);
  check(callback?.callback === "retry_last_text", "Wispr's error automatically invokes its real Retry action");
  check(renderers() === 0, "queued processing and Retry keep Chromium unloaded");
  status.webContents.send("status:dictationStatus", "processing");
  status.webContents.send("status:dictationStatus", "idle");
  await until("second queued job", () => daemonEvents.filter((event) => event.op === "cloudStart").length >= 3);
  hub.webContents.send("dictation:audioStart", { transcriptionEntityUuid: "stack-2" });
  await until("second replay started", () => started?.transcriptionEntityUuid === "stack-2");
  hub.webContents.send("dictation:audioStop");
  await until("second replay finished", () => done?.transcriptionEntityUuid === "stack-2");
  status.webContents.send("status:dictationStatus", "idle");
  await wait(300);

  if (xdotool) {
    const before = daemonEvents.length;
    execSync(`${xdotool} mousemove 688 721 click 1`);
    const retained = await until("local alternative after idle", () => daemonEvents.slice(before).some((event) => event.op === "log" && event.msg.includes("local alternative failed")));
    check(retained, "an undelivered dictation keeps its processing bubble and local action after Wispr goes idle");
  }

  // A notification needs the real status page.
  status.webContents.send("notification:show", { type: "tip" });
  await until("status loaded", () => loaded.includes("status"));
  await until("notification delivered", () => got.some((g) => g.role === "status" && g.ch === "notification:show"));
  check(got.some((g) => g.role === "status" && g.ch === "notification:show"), "notification reached the status page after it loaded");
  check(renderers() === 1, `one renderer while the notification shows (${renderers()})`);
  // The page reports something drawn, so it stays while the notification is up.
  await wait(Number(process.env.WISPR_HEADLESS_TEST_IDLE_MS) + 500);
  check(renderers() === 1, "status page stays while the notification is active");
  status.webContents.send("notification:clear", { reason: "timeout" });
  ipcMain.emit("status:reportAlphaCatchRects", { sender: status.webContents }, { rects: [], shapeRects: [], viewport: {} });
  await until("status stopped", () => renderers() === 0, Number(process.env.WISPR_HEADLESS_TEST_IDLE_MS) + 4_000);
  check(renderers() === 0, "status renderer stopped once nothing is shown");
  check(gone === 0, `the app saw no render-process-gone (${gone})`);
  check(!status.isDestroyed(), "status window object survives");

  // Showing the Hub (settings) loads it; hiding stops it again.
  hub.show();
  await until("hub loaded", () => loaded.includes("hub"));
  check(loaded.includes("hub") && renderers() === 1, "hub page loads when shown");
  hub.hide();
  await until("hub stopped", () => renderers() === 0, Number(process.env.WISPR_HEADLESS_TEST_IDLE_MS) + 4_000);
  check(renderers() === 0, "hub renderer stopped after it was hidden");

  // A message the shim does not handle wakes the Hub and is delivered.
  hub.webContents.send("media:startMeetingMicCapture", { sessionId: "m", phase: "start" });
  await until("meeting delivered", () => got.some((g) => g.role === "hub" && g.ch === "media:startMeetingMicCapture"));
  check(got.some((g) => g.role === "hub" && g.ch === "media:startMeetingMicCapture"), "unhandled message woke the hub and arrived");

  // The status page can load a second time.
  loaded.length = 0;
  status.webContents.send("notification:show", { type: "tip" });
  await until("status reloaded", () => loaded.includes("status"));
  check(loaded.includes("status"), "status page loads again after being stopped");

  check(gone === 0, `still no render-process-gone seen by the app (${gone})`);

  // A changed value on the wake list loads the status page: stop it first.
  status.webContents.send("notification:clear", { reason: "test" });
  ipcMain.emit("status:reportAlphaCatchRects", { sender: status.webContents }, { rects: [{ x: 0, y: 0, width: 78, height: 18 }], shapeRects: [], viewport: {} });
  // (The hub is still up from the meeting step, so look at the status window only.)
  await until("status stopped again", () => status.webContents.isCrashed(), Number(process.env.WISPR_HEADLESS_TEST_IDLE_MS) + 4_000);
  check(status.webContents.isCrashed(), "status stops when only the resting bar is drawn");
  loaded.length = 0;
  status.webContents.send("zubridge:state-update", withSubscription("s2", 4));
  await until("woken by store", () => loaded.includes("status"), 5_000);
  check(loaded.includes("status"), "a changed subscription value loads the status page");

  // Wispr's push to talk and hands-free are bound to the keys wispr-headless
  // presses for them (here 131 and 130, F20 and F19 on Linux), keeping its
  // other shortcuts: one SET_PREFS dispatch, then helper:updateShortcuts.
  const dispatches = [];
  ipcMain.on("zubridge:dispatch", (_e, a) => dispatches.push(a));
  let shortcutUpdates = 0;
  ipcMain.on("helper:updateShortcuts", () => shortcutUpdates++);
  const fnShortcuts = { 63: "ptt", "49+63": "popo", "59+63": "lens", 53: "dismiss" };
  // Wispr's dictation shortcuts are kept from Wispr: the helper's key events
  // that would complete one never arrive, everything else does, numbered
  // without gaps, and the dropped ones are acknowledged to the helper.
  hub.webContents.send("zubridge:state-update", { updateId: "keys", state: { prefs: { user: { shortcuts: fnShortcuts } } } });
  await until("rebind", () => dispatches.length > 0 && shortcutUpdates > 0, 3_000);
  const action = dispatches[0]?.action;
  const sorted = (o) => JSON.stringify(Object.fromEntries(Object.entries(o ?? {}).sort()));
  check(
    dispatches.length === 1 && action?.type === "SET_PREFS" && typeof action.__id === "string" &&
      sorted(action.payload?.user?.shortcuts) === sorted({ 131: "ptt", 130: "popo", "59+63": "lens", 53: "dismiss" }) &&
      Object.keys(action.payload).join() === "user" && Object.keys(action.payload.user).join() === "shortcuts",
    `Fn-based shortcuts are rebound with SET_PREFS (${JSON.stringify(dispatches)})`,
  );
  check(shortcutUpdates === 1, `then helper:updateShortcuts (${shortcutUpdates})`);
  status.webContents.send("zubridge:state-update", { updateId: "keys2", state: { prefs: { user: { shortcuts: fnShortcuts } } } });
  await wait(300);
  check(dispatches.length === 1, `the same shortcuts are rebound once (${dispatches.length})`);

  const fs = require("fs");
  const ackFile = path.join(app.getPath("temp"), `wispr-test-acks-${process.pid}`);
  const goFile = `${ackFile}.go`;
  const helper = require("child_process").spawn(path.join(__dirname, "wispr-flow-linux-helper"), [ackFile, goFile], { stdio: ["pipe", "pipe", "inherit", "pipe"] });
  const fromHelper = [];
  let pendingData = "";
  const unescape = (m) => m.replaceAll("+2", "|").replaceAll("+1", "+");
  const escape = (m) => m.replaceAll("+", "+1").replaceAll("|", "+2");
  // Wispr's reader (HelperAPIClient.pushData); it acknowledges each key event.
  helper.stdio[3].on("data", (e) => {
    pendingData += e.toString("utf8");
    while (pendingData.includes("|")) {
      const m = pendingData.split("|")[0];
      pendingData = pendingData.slice(m.length + 1);
      const request = JSON.parse(unescape(m)).HelperAPIRequest;
      fromHelper.push(request);
      if (request.KeypressEvent) helper.stdin.write(escape(JSON.stringify({ HelperAPIResponse: { uuid: request.uuid, ACK: true } })) + "|");
    }
  });
  const keyEvents = () => fromHelper.filter((r) => r.KeypressEvent);
  const keys = () => keyEvents().map((r) => `${r.KeypressEvent.payload.eventType.slice(10)} ${r.KeypressEvent.payload.key}`);
  await until("helper", () => keys().length >= 6, 5_000);
  await wait(300);
  check(keys().join() === "press 49,release 49,press 53,release 53,press 59,release 59", `only keys that start no Wispr dictation reach Wispr (${keys().join()})`);
  check(fromHelper.some((r) => r.AppInfoUpdate?.payload?.appName === "a|b+c"), "other helper messages arrive unchanged");
  // While Wispr's settings record a shortcut, every key arrives.
  ipcMain.emit("keyboard:isRecording", { sender: hub.webContents }, true);
  fs.writeFileSync(goFile, "");
  await until("recording keys", () => keys().length >= 8, 5_000);
  check(keys().slice(6).join() === "press 63,release 63", `keys arrive while a shortcut is recorded (${keys().slice(6).join()})`);
  ipcMain.emit("keyboard:isRecording", { sender: hub.webContents }, false);

  // The daemon's own shortcut keys (hideKeys) never reach Wispr.
  const daemonSays = globalThis.wisprHeadlessDaemonEvent;
  daemonSays({ op: "hideKeys", keys: [165] });
  fs.writeFileSync(`${goFile}2`, "");
  await until("hidden keys", () => keys().length >= 10, 5_000);
  await wait(300);
  check(keys().slice(8).join() === "press 53,release 53", `the daemon's hotkeys are kept from Wispr (${keys().slice(8).join()})`);

  // The daemon presses Wispr's push to talk and hands-free keys.
  for (const [a, down] of [["ptt", true], ["ptt", false], ["popo", true], ["popo", false]]) daemonSays({ op: "wisprKey", action: a, down });
  await until("injected", () => keys().length >= 14, 3_000);
  check(keys().slice(10).join() === "press 131,release 131,press 130,release 130", `wisprKey presses and releases the configured keys (${keys().slice(10).join()})`);
  const injected = keyEvents().slice(10);
  check(
    injected.every((r) => Object.keys(r).sort().join() === "KeypressEvent,uuid" && Object.keys(r.KeypressEvent).join() === "payload" &&
      Object.keys(r.KeypressEvent.payload).every((k) => ["eventType", "key", "index", "inputType"].includes(k))),
    `injected key events carry only Wispr's fields (${JSON.stringify(injected[0])})`,
  );
  check(keyEvents().every((r, i) => r.KeypressEvent.payload.index === i + 1), `key events reach Wispr numbered 1, 2, ... (${keyEvents().map((r) => r.KeypressEvent.payload.index).join()})`);

  await wait(300);
  helper.stdin.end();
  await new Promise((r) => helper.once("exit", r));
  const helperAcks = fs.readFileSync(ackFile, "utf8").split("|").filter(Boolean).map((m) => JSON.parse(unescape(m)).HelperAPIResponse);
  const ackedUuids = helperAcks.map((a) => a.uuid);
  const realUuids = Array.from({ length: 16 }, (_, i) => `k${i + 1}`);
  check(helperAcks.every((a) => a.ACK === true) && ["k1", "k4", "k8", "k9", "k13", "k16"].every((u) => ackedUuids.includes(u)), `the helper gets an ACK for each dropped key event (${ackedUuids.join()})`);
  check(ackedUuids.length === 16 && realUuids.every((u) => ackedUuids.includes(u)), `the helper gets one ACK per key event it sent, none for injected ones (${ackedUuids.join()})`);

  // Without a helper, a wisprKey does nothing.
  const before = fromHelper.length;
  daemonSays({ op: "wisprKey", action: "ptt", down: true });
  await wait(200);
  check(fromHelper.length === before, "wisprKey without a helper is ignored");
  for (const f of [ackFile, goFile, `${goFile}2`]) fs.rmSync(f, { force: true });

  // Shortcuts already on those keys are left alone.
  hub.webContents.send("zubridge:state-update", { updateId: "keys3", state: { prefs: { user: { shortcuts: { 131: "ptt", 130: "popo", "59+63": "lens", 53: "dismiss" } } } } });
  await wait(300);
  check(dispatches.length === 1, `rebound shortcuts are not dispatched again (${dispatches.length})`);

  console.log(failures === 0 ? "ALL PASSED" : `${failures} FAILED`);
  app.exit(failures === 0 ? 0 : 1);
});

setTimeout(() => { console.log("FAIL timeout"); app.exit(2); }, 120_000);
