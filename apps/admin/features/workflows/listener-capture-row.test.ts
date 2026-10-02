/**
 * The listener panel's capture block has to be able to go RED, and this file is what proves
 * the RENDER can — the pure rule in `test-listener.test.ts` only ever proved the rule.
 *
 * ## Why a unit test reads a component
 *
 * `captureView()` returns `waiting | captured | expired`, and six cases in `test-listener.test.ts`
 * pin that function down exactly. They were all green against the panel that drew a green
 * "Captured page.published" over an armed row with no payload under it, because the defect was
 * never in the function — it was in the two lines of JSX that ignored it:
 *
 * ```tsx
 * <CheckCircle2 ... /> Captured {captured.event_name}
 * ```
 *
 * The heading read the row's `status` and the component held the correct function two dozen
 * lines above. **A rule that is tested and a render that ignores it are the same ship as a rule
 * that is missing**, and the unit suite said the first was handled. So every assertion here is
 * about the *wiring* — that the heading asks `captureView`, that the green tick is unreachable
 * without a payload, and that the capture id the browser probe reads is written where the probe
 * reads it.
 *
 * ## What is asserted, and what is deliberately NOT
 *
 * * Every assertion is **structural**: that the component imports and calls the rule, and that
 *   the word "Captured" cannot reach the screen except through the rule's own sentence.
 * * None of it claims the panel *renders* correctly. A regex cannot run React. The honest claim
 *   is the inverse one — the row is wired to the code under test, so reverting the render
 *   reaches a red suite instead of passing silently. The browser pass proves the value.
 * * Comments are stripped before every search, because this file's own prose names the exact
 *   string it forbids. A guard that trips on the sentence describing the defect cannot police
 *   the defect — `reload-rebase-row.test.ts` hit exactly that and says so.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const PANEL = readFileSync(new URL("./listener-panel.tsx", import.meta.url), "utf8");

const stripComments = (source: string): string =>
  source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^[ \t]*\/\/.*$/gm, "");

const panel = stripComments(PANEL);

/** The JSX return block, so a match cannot be satisfied by a helper outside the render. */
function renderTail(): string {
  const at = panel.lastIndexOf("return (");
  assert.notEqual(at, -1, "the panel has no render to check — the file's shape changed");
  return panel.slice(at);
}

test("the render asks captureView, and the answer is what the heading prints", () => {
  // The wiring itself. A panel that computed a view and printed `row.status` anyway would
  // pass every other case in this file.
  assert.match(panel, /import\s*\{[^}]*\bcaptureView\b[^}]*\}\s*from\s*["']@\/features\/workflows\/test-listener["']/);
  assert.match(panel, /const\s+view\s*=\s*captured\s*\?\s*captureView\(captured,\s*now\)\s*:\s*null/);
  assert.match(
    renderTail(),
    /\{view\?\.sentence\s*\?\?\s*captured\.event_name\}/,
    "the heading must print the view's sentence, not a sentence built from the status",
  );
});

test("the green tick is reachable only for a captured view", () => {
  // `CheckCircle2` is the checkmark. It is the whole defect in one glyph: a listener that has
  // heard nothing was drawn with the mark that means it did.
  const tick = renderTail().indexOf("CheckCircle2");
  assert.notEqual(tick, -1, "the captured mark disappeared — someone removed the capture state");
  // The mark sits inside a ternary, so the branch that can select it must be the view's.
  const branch = renderTail().slice(Math.max(0, tick - 220), tick);
  assert.match(
    branch,
    /view\?\.kind\s*===\s*["']captured["']/,
    "the green check must be guarded by a captured view, not by the row's status",
  );
  // And it must NOT be keyed on the status the old code used.
  assert.equal(
    /view\?\.kind\s*===\s*["']captured["']\s*\?\s*\(\s*<CheckCircle2/.test(renderTail()),
    true,
    "the check is rendered on a different branch than the captured view",
  );
});

test("the countdown is shown while waiting, not for every non-expired row", () => {
  // The countdown used to hang off `status === "armed"`, which is the same status-driven read:
  // a row the server calls `captured` with no payload was silently given no countdown, and a
  // late capture was given one it did not need.
  const at = renderTail().indexOf("data-listener-countdown");
  assert.notEqual(at, -1, "the countdown left the panel while something is still listening");
  assert.match(
    renderTail().slice(Math.max(0, at - 160), at),
    /view\?\.kind\s*===\s*["']waiting["']/,
    "the countdown must follow the view's kind, not the row's status",
  );
});

test("the capture kind is on the DOM for the browser probe to read", () => {
  // `data-listener-capture` is what the walkthrough waits for, and the *kind* is what makes the
  // wait meaningful: a probe that waits for the element and reads its text can no longer tell a
  // captured row from a waiting one, because the two no longer say the same thing.
  assert.match(
    renderTail(),
    /data-listener-capture=\{captured\.id\}\s+data-listener-capture-kind=\{view\?\.kind\}/,
    "the capture block must carry the kind the probe distinguishes states with",
  );
  // The probe's own selector, so a rename cannot leave the row reading a marker nobody writes.
  const walkthrough = stripComments(
    readFileSync(new URL("../../../../scripts/qa/walkthrough.cjs", import.meta.url), "utf8"),
  );
  // A row that waits for the capture block and never reads the kind cannot tell a captured row
  // from a waiting one — the two said the same thing before this fix, so the probe passed on
  // either. It has to read the attribute, and this assertion is what makes the row's reading
  // mean "captured" rather than "something is drawn here".
  assert.match(
    walkthrough,
    /data-listener-capture-kind/,
    "the probe waits for the capture block but never reads its kind, so a captured row and a " +
      "waiting one are indistinguishable to it",
  );
});

test("an expired row is described as expired, and the panel still shows its history", () => {
  // The three ways a listener can stop being useful, and the panel has to say which one
  // happened: a captured payload, a window that closed, or a listener that never heard. The
  // three kinds are a rule, so they are asserted against the RULE file — reading them out of the
  // panel would be the fifth wrong anchor of this shape, and it is the exact mistake this file
  // exists to catch.
  const rule = stripComments(readFileSync(new URL("./test-listener.ts", import.meta.url), "utf8"));
  for (const kind of ["captured", "waiting", "expired"]) {
    assert.match(
      rule,
      new RegExp(`kind:\\s*["']${kind}["']`),
      `the rule lost its ${kind} answer`,
    );
  }
  // The spent rows stay visible — a listener that quietly disappears is indistinguishable from
  // one that was never armed, which is the reason that paragraph is in the panel at all.
  assert.match(renderTail(), /data-listener-expired-count/);
});
