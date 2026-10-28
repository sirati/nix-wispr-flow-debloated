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
  globalThis.testing = { createCloudRetry, createPasteRouter, gateHelper, escapeHelper, unescapeHelper, patchWisprBundle, nativeNotice, pasteContext,
    setGate(gate) { pasteGate = gate; } };
`, context);
const { testing } = context;
const tick = () => new Promise((resolve) => setImmediate(resolve));

test("Wispr's retry notice is handled automatically while queued recording stays independent", async () => {
  const sent = [], retried = [];
  const retry = testing.createCloudRetry((message) => sent.push(message), (notice) => retried.push(notice), 20);
  assert.equal(retry.notice({ type: "TranscriptionError" }), false);
  retry.start(1);
  retry.recording();
  const notice = { type: "TranscriptionError", actions: [{ callback: "retry_last_text" }] };
  assert.equal(retry.notice(notice), true);
  retry.status("error");
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.equal(retried.length, 1, "status and notification schedule one retry");
  assert.equal(retried[0], notice);
  assert.equal(sent[0].value, "retrying");
  retry.status("retrying");
  retry.status("idle");
  assert.equal(sent.at(-1).value, "idle");
  retry.start(2); retry.notice(notice); retry.close();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(retried.length, 1, "closing cancels automatic retries");
});

test("a silent Retry failure tries again, and processing or a new job cancels the old timer", async () => {
  const retried = [];
  const retry = testing.createCloudRetry(() => {}, () => {
    retried.push(retry.active);
    if (retried.length === 2) retry.status("processing");
  }, 2);
  retry.start(1);
  retry.status("error");
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.deepEqual(retried, [1, 1]);
  retry.status("error");
  retry.start(2);
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.deepEqual(retried, [1, 1]);
  retry.close();
});

test("an older idle status cannot release a queued invocation before its recorder starts", () => {
  const sent = [];
  const retry = testing.createCloudRetry((event) => sent.push(event), () => {});
  retry.start(1); retry.recording(); retry.status("idle");
  retry.start(2); retry.status("idle");
  assert.equal(retry.active, 2);
  assert.equal(sent.length, 1);
  retry.recording(); retry.status("idle");
  assert.equal(sent.length, 2);
  assert.equal(sent[1].id, 2);
  retry.close();
});

for (const bundle of (process.env.WISPR_TEST_BUNDLES ?? "").split(":").filter(Boolean)) {
  test(`the shipped bundle retains correlation and spoken Enter on both insertion paths: ${bundle}`, () => {
    const patched = testing.patchWisprBundle(fs.readFileSync(bundle, "utf8"));
    new vm.Script(patched);
    const payload = /\{\.\.\.(\w+)\.L\[(\w+)\.Eg\.VoidDictation\],body:(\w+),callbackOperationId:__wisprDictationUUID\?\?globalThis\.wisprHeadlessPasteUUID\?\.\(\),dictationSubmit:!!__wisprDictationSubmit\}/.exec(patched);
    assert.ok(payload, "the copy message carries the exact originating UUID");
    const env = { [payload[1]]: { L: { VoidDictation: { component: "voidDictation" } } }, [payload[2]]: { Eg: { VoidDictation: "VoidDictation" } }, [payload[3]]: "completed text", __wisprDictationUUID: "original-session", __wisprDictationSubmit: true };
    const notice = vm.runInNewContext(`(${payload[0]})`, env);
    assert.equal(notice.callbackOperationId, "original-session");
    assert.equal(notice.body, "completed text");
    assert.equal(notice.dictationSubmit, true);
    env.__wisprDictationUUID = undefined;
    assert.equal(vm.runInNewContext(`(${payload[0]})`, env).callbackOperationId, undefined, "unrelated extension pastes cannot reuse a dictation session");
    const deferred = /__wisprDeferredCopy&&\(0,(\w+)\.sendVoidDictationNotification\)\((\w+),void 0,(\w+)\.ZZ\.uuid,(\w+)\)/.exec(patched);
    assert.ok(deferred, "pre-helper copy waits for Wispr's final formatted text and Enter decision");
    let copied;
    vm.runInNewContext(deferred[0], { __wisprDeferredCopy: true, [deferred[1]]: { sendVoidDictationNotification(...args) { copied = args; } }, [deferred[2]]: "completed text", [deferred[3]]: { ZZ: { uuid: "original-session", shouldPressEnterAfterPaste: false } }, [deferred[4]]: true });
    assert.deepEqual(copied, ["completed text", undefined, "original-session", true], "copy uses the just-computed decision before Wispr updates its shared state");
    // Use Wispr's real generated serializer, which rejects non-string values
    // in logAttributes before anything can reach the helper or paste hook.
    const converter = /class (\w+)\{static toHelperAPI\(/.exec(patched);
    assert.ok(converter);
    const converterBody = patched.slice(converter.index);
    const converterEnd = /\},\d+\([^)]*\)\{/.exec(converterBody);
    assert.ok(converterEnd);
    const native = vm.runInNewContext(converterBody.slice(0, converterEnd.index) + `;${converter[1]};`);
    const wirePaste = (attributes) => ({ HelperAPIRequest: { uuid: "paste-request", PasteText: { payload: { text: "completed text", pasteId: "paste-id", logAttributes: attributes } } } });
    assert.throws(() => native.helperAPIToJson(wirePaste({ transcriptEntityUuid: "original-session", dictationSubmit: true })), /Invalid value/);
    const correlation = /(\w+)\?\{transcriptEntityUuid:\1,dictationSubmit:String\(!!(\w+)\.ZZ\.shouldPressEnterAfterPaste\)\}:"dictation"===(\w+)\.kind\?\{transcriptEntityUuid:\3\.transcriptEntityUUID,dictationSubmit:String\(!!\2\.ZZ\.shouldPressEnterAfterPaste\)\}:void 0/.exec(patched);
    assert.ok(correlation, "ordinary helper pastes carry their owner's UUID");
    for (const submit of [true, false]) for (const uuid of [undefined, "original-session"]) {
      const attributes = vm.runInNewContext(correlation[0], { [correlation[1]]: uuid, [correlation[2]]: { ZZ: { shouldPressEnterAfterPaste: submit } }, [correlation[3]]: { kind: "dictation", transcriptEntityUUID: "original-session" } });
      assert.equal(attributes.transcriptEntityUuid, "original-session");
      const serialized = native.helperAPIToJson(wirePaste(attributes));
      const payload = JSON.parse(serialized).HelperAPIRequest.PasteText.payload;
      const sent = [];
      const router = testing.createPasteRouter((message) => sent.push(message), () => true);
      router.start("original-session");
      router.route(payload, () => {});
      assert.equal(sent.length, 1);
      assert.equal(sent[0].submit, submit, "the actual patched bundle's Enter intent reaches the API request");
      router.delivered({ request: sent[0].request, applied: true });
    }
    assert.match(patched, /globalThis\.wisprHeadlessTextBoxInfo=\w+;/);
  });
}

test("Wispr's custom copy notification preserves its transcript for the Rust UI", () => {
  const notice = testing.nativeNotice({ type: "VoidDictation", variant: "custom", component: "voidDictation", body: "kept text" }, "en-US");
  assert.equal(notice.op, "recover");
  assert.equal(notice.text, "kept text");
});

test("a late native paste failure keeps its UUID across async recovery callbacks", async () => {
  const helper = new EventEmitter(), fd = new EventEmitter();
  helper.stdin = { write() { return true; } };
  helper.stdio = [helper.stdin, null, null, fd];
  testing.setGate(() => false);
  testing.gateHelper(helper);
  const frame = (message) => testing.escapeHelper(JSON.stringify(message)) + "|";
  helper.stdin.write(frame({ HelperAPIRequest: { uuid: "request", PasteText: { payload: { pasteId: "old-paste", logAttributes: { transcriptEntityUuid: "original-session" }, text: "old text" } } } }));
  let observed;
  fd.on("data", () => { observed = Promise.resolve().then(() => testing.pasteContext.getStore()); });
  fd.emit("data", frame({ HelperAPIRequest: { uuid: "outcome", PasteOutcome: { payload: { pasteId: "old-paste", success: false } } } }));
  assert.equal(await observed, "original-session");
});

test("a failed helper paste and its subsequent copy prompt deliver only once", () => {
  const sent = [], results = [];
  const router = testing.createPasteRouter((message) => sent.push(message), () => true);
  router.start("dict-1");
  router.route({ transcriptEntityUUID: "dict-1", text: "kept" }, (result) => results.push(result));
  router.route({ transcriptEntityUUID: "dict-1", text: "kept" }, (result) => results.push(result));
  assert.equal(sent.length, 1);
  router.delivered({ request: sent[0].request, applied: false, error: "Original input closed" });
  assert.equal(results.length, 2);
  router.route({ transcriptEntityUUID: "dict-1", text: "kept" }, (result) => results.push(result));
  assert.equal(sent.length, 1);
  assert.equal(results[2].error, "Original input closed");
});

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
