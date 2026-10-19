// Stand-in for Wispr's main process, with the same window layout and IPC
// channels, driven through a scripted scenario. Prints PASS/FAIL lines and
// exits non-zero on the first failure.
"use strict";

const { app, BrowserWindow, ipcMain } = require("electron");
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

  // A click on the Rust bar reaches the app as Wispr's own start click.
  let xdotool = null;
  try { xdotool = execSync("command -v xdotool").toString().trim(); } catch {}
  if (xdotool) {
    execSync(`${xdotool} mousemove 640 786 click 1`); // the resting bar: 40x8 at 10 px above the bottom of the 1280x800 screen
    await until("click", () => startClicks > 0, 3_000);
    check(startClicks === 1, `a click on the resting bar arrives as status:startClicked (${startClicks})`);
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
  console.log(failures === 0 ? "ALL PASSED" : `${failures} FAILED`);
  app.exit(failures === 0 ? 0 : 1);
});

setTimeout(() => { console.log("FAIL timeout"); app.exit(2); }, 120_000);
