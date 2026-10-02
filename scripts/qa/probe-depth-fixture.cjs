#!/usr/bin/env node
/*
 * The depth pass's fixture lifetime and its result, exercised without a browser.
 *
 * The AI provider pass starts a local HTTP endpoint so there is something real to connect, test
 * and discover against. Two things about that fixture have bitten this pass, both silently:
 *
 *   1. **It was closed before the assertions that need it ran.** `fake.close()` lived in a
 *      `finally` that closed the endpoint, and discovery ran *after* it. A dead port answers
 *      nothing, so no model was registered — and then the capability editor and the flag toggle,
 *      which both read the model list, reported empty. Three results that read like broken screens
 *      and were really the pass having taken its own fixture away. A depth pass that closes its
 *      fixture before the last assertion is not testing the product, it is testing its own
 *      teardown order.
 *   2. **Its result was assigned at the end.** A throw between the `finally` and the last line
 *      left `report.aiProviders` undefined; the log printed `ai providers: undefined` and the
 *      report carried no trace of the eleven assertions that had already been recorded to
 *      `clicks.jsonl`. A pass that fails halfway has proved *something*, and the artifact the
 *      report is built from had it all.
 *
 * Both are properties of the source's own ordering, so both are checked against the source. The
 * alternative — a comment saying "keep the endpoint open" — is exactly what was there before.
 */
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const WALK = path.join(__dirname, "walkthrough.cjs");
const source = fs.readFileSync(WALK, "utf8");

let pass = 0;
let fail = 0;
const test = (name, fn) => {
  try {
    fn();
    pass += 1;
    console.log(`  ok  ${name}`);
  } catch (err) {
    fail += 1;
    console.error(`  FAIL ${name}\n       ${err.message}`);
  }
};

/** The body of one function, braces balanced, from its `async function name(` line. */
const fnBody = (name) => {
  const start = source.indexOf(`async function ${name}(`);
  assert.notEqual(start, -1, `${name} is not in walkthrough.cjs`);
  const open = source.indexOf("{", start);
  let depth = 0;
  for (let i = open; i < source.length; i += 1) {
    if (source[i] === "{") depth += 1;
    else if (source[i] === "}") {
      depth -= 1;
      if (depth === 0) return source.slice(open, i + 1);
    }
  }
  throw new Error(`unbalanced braces in ${name}`);
};

const body = fnBody("runAiProviderDepth");

test("the fixture is closed only after the last assertion that needs it", () => {
  // Every position that has to be compared: an assertion that runs *after* the close is blind.
  const close = body.indexOf("fake.close()");
  assert.notEqual(close, -1, "the pass never closes its endpoint — it leaks a listener per run");
  for (const [name, call] of [
    ["discovery", "discoverTwice(page, fake)"],
    ["the capability editor", "readCapabilityEditor(page)"],
    ["the flag toggle", "toggleOneCapability(page)"],
    ["the health panels", "exerciseHealthPanels(page)"],
  ]) {
    const at = body.indexOf(call);
    assert.notEqual(at, -1, `${name} (${call}) is not called by the pass`);
    assert.ok(
      at < close,
      `${name} runs at offset ${at} but the endpoint closes at ${close} — it is talking to a dead port`,
    );
  }
  // And the close has to be in a `finally`, or a throw leaks the listener instead.
  const lastFinally = body.lastIndexOf("} finally {");
  assert.ok(lastFinally !== -1 && close > lastFinally, "the endpoint must be closed in a finally");
});

test("the result is published from the try, not only at the end of the function", () => {
  const assigns = [...body.matchAll(/report\.aiProviders = steps;/g)].map((m) => m.index);
  assert.ok(assigns.length >= 2, `expected the result to be published at least twice, found ${assigns.length}`);
  // The first publish has to sit inside the first `finally` — that is the one that runs when the
  // first half throws. A publish only after everything would be the bug again.
  const firstFinally = body.indexOf("} finally {");
  const firstEnd = body.indexOf("} catch");
  assert.ok(
    assigns.some((a) => a > firstFinally && a < firstEnd + 1),
    "nothing publishes the result inside the first finally, so a throw loses it",
  );
});

test("a failing second half becomes a finding, not a shrug", () => {
  assert.ok(body.includes("} catch (cause) {"), "the slice-2/3 half has no catch");
  // A catch that logs and swallows would leave the report reading as if the whole pass ran.
  assert.ok(
    body.includes("aiStateFindings.push"),
    "a half-run pass must queue a finding; a swallowed throw is indistinguishable from a pass",
  );
  assert.ok(
    body.includes('note({ step: "failed"'),
    "the failure is not recorded as a step, so it is not in the artifact either",
  );
});

test("the discovery assertion is what a dead endpoint would have broken", () => {
  // The regression's shape: discovery must precede the model-dependent checks, because discovery
  // is what registers the models the other two read. Asserted as an order, not as a comment.
  const discovery = body.indexOf("discoverTwice(page, fake)");
  const editor = body.indexOf("readCapabilityEditor(page)");
  const toggle = body.indexOf("toggleOneCapability(page)");
  assert.ok(discovery < editor && discovery < toggle, "discovery must run before the checks that read models");
});

test("the window opens before the click, because the click can outrun it", () => {
  // Playwright resolves `click()` as soon as the browser dispatches it, and this panel's refused
  // POST lands in ~50 ms — measured, not assumed. A window opened *after* the click has already
  // missed the failure it was opened for, so it claims nothing and the report files the pass's own
  // 400 as a defect. This is the one ordering that reads like a product bug and is not one.
  const i = source.indexOf("const submitsAForm = meta.tag === \"button\"");
  assert.notEqual(i, -1, "the submit branch is gone from the interactor");
  const reg = source.indexOf("expectRefusal(", i);
  const click = source.indexOf('await page.locator(`[data-qa-idx="${i}"]`).click', i);
  assert.notEqual(reg, -1 && reg > i, "the inline submit is never registered");
  assert.notEqual(click, -1, "the click is gone from the interactor");
  assert.ok(reg < click, `the window opens at ${reg} and the click is at ${click} — it misses the failure`);
  // …and it has to be closed after the response arrives, not straight after the click.
  const close = source.indexOf("endRefusalWindow(", i);
  assert.ok(close > click, "the window closes before the click, so nothing is ever covered");
});

test("a narrowed registration is a 4xx vocabulary, and the filler uses it", () => {
  // The filler submits sample values into a real form: a 4xx is the product refusing it, a 5xx is
  // the API crashing on input it should have rejected. Registering the whole vocabulary would let
  // the crash through.
  const i = source.indexOf("interact(${pageName}): a sample-filled form is submitted on purpose");
  assert.notEqual(i, -1, "the inline submit registration reason is gone");
  const window = source.slice(source.lastIndexOf("expectRefusal(", i), i + 200);
  assert.ok(
    /\[400, 401, 403, 422\]/.test(window),
    "the inline submit must register 4xx only, so a 500 in the window stays a finding",
  );
});

console.log(`\n${pass}/${pass + fail} PASS${fail ? "" : ""}`);
process.exitCode = fail ? 1 : 0;
