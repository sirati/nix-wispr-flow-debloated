"use strict";
const { test } = require("node:test");
const assert = require("node:assert/strict");
const { EventEmitter } = require("node:events");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const vm = require("node:vm");

// Exercise the same helper framing and routing used inside Electron.
const cache = fs.mkdtempSync(path.join(os.tmpdir(), "wispr-routing-"));
process.on("exit", () => fs.rmSync(cache, { recursive: true, force: true }));
const context = vm.createContext({ require, Buffer, setTimeout, clearTimeout, setImmediate,
  process: { env: { XDG_CACHE_HOME: cache }, nextTick: process.nextTick, pid: process.pid } });
const source = fs.readFileSync(path.join(__dirname, "../shim.js"), "utf8");
vm.runInContext(source.slice(0, source.indexOf("// Nothing here may stop Wispr")) + `
  globalThis.testing = { createPasteRouter, gateHelper, escapeHelper, unescapeHelper,
    setGate(gate) { pasteGate = gate; } };
`, context);
const { testing } = context;
const tick = () => new Promise((resolve) => setImmediate(resolve));

test("only correlated dictations go through the broker and retain submit intent", () => {
  const sent = [], results = [];
  const router = testing.createPasteRouter((message) => sent.push(message), () => true);
  router.start("dict-1");
  for (let i = 0; i < 300; i++) router.start(`newer-${i}`);
  assert.equal(router.route({ text: "unrelated paste" }, () => assert.fail()), false);
  assert.equal(router.route({ transcriptEntityUUID: "other", text: "old paste" }, () => assert.fail()), false);
  assert.equal(router.route({ logAttributes: { transcriptEntityUuid: "dict-1" }, text: "a+b|c", submit: true }, (result) => results.push(result)), true);
  assert.equal(sent[0].uuid, "dict-1");
  assert.equal(sent[0].text, "a+b|c");
  assert.equal(sent[0].submit, true);
  router.delivered({ request: sent[0].request, fallback: true, applied: false });
  router.delivered({ request: sent[0].request, applied: true });
  assert.equal(results.length, 1);
  assert.equal(results[0].fallback, true);
});

test("daemon loss and timeouts never authorize a global paste", async () => {
  const results = [];
  const router = testing.createPasteRouter(() => {}, () => true, 5);
  router.start("dict-1");
  router.route({ transcriptEntityUUID: "dict-1", text: "lost" }, (result) => results.push(result));
  await new Promise((resolve) => setTimeout(resolve, 15));
  assert.equal(results.length, 1);
  assert.equal(results[0].applied, false);
  assert.equal(results[0].fallback, undefined);
  router.route({ transcriptEntityUUID: "dict-1", text: "lost again" }, (result) => results.push(result));
  router.disconnected();
  assert.equal(results.length, 2);
  assert.equal(results[1].fallback, undefined);
});

test("helper pastes preserve framing on fallback and emit acknowledged outcomes for direct delivery", async () => {
  const writes = [], messages = [];
  const helper = new EventEmitter();
  const fd = new EventEmitter();
  const stdin = { write(chunk, done) { writes.push(String(chunk)); done?.(); return true; } };
  helper.stdin = stdin;
  helper.stdio = [stdin, null, null, fd];
  testing.gateHelper(helper);
  const frame = (message) => testing.escapeHelper(JSON.stringify(message)) + "|";
  fd.on("data", (chunk) => {
    const message = JSON.parse(testing.unescapeHelper(String(chunk).slice(0, -1)));
    messages.push(message);
    if (message.HelperAPIRequest?.PasteOutcome) {
      stdin.write(frame({ HelperAPIResponse: { uuid: message.HelperAPIRequest.uuid, ACK: true } }));
      // Wispr's follow-up key would otherwise act on whichever app has focus.
      stdin.write(frame({ HelperAPIRequest: { uuid: "enter", SimulateKeyPress: { payload: { keycode: 36, flags: [] } } } }));
    }
  });
  let complete;
  testing.setGate((_payload, done) => { complete = done; return true; });
  const paste = frame({ HelperAPIRequest: { uuid: "request-1", PasteText: { payload: { text: "a+b|c", pasteId: "paste-1" } } } });
  stdin.write(paste);
  assert.equal(writes.length, 0);
  complete({ fallback: true });
  assert.equal(writes[0], paste);
  writes.length = 0;
  stdin.write(paste);
  complete({ applied: true });
  await tick();
  assert.equal(writes.length, 0, "no paste, outcome ACK, or follow-up key reaches the native helper");
  assert.equal(messages[0].HelperAPIResponse.uuid, "request-1");
  const outcome = messages.find((m) => m.HelperAPIRequest?.PasteOutcome)?.HelperAPIRequest.PasteOutcome.payload;
  assert.equal(outcome.success, true);
  assert.equal(outcome.pasteId, "paste-1");
  testing.setGate(() => false);
  stdin.write(paste);
  assert.equal(writes[0], paste, "ordinary native pastes remain intact");
});
