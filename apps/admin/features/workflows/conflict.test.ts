/**
 * The conflict rules' own tests (REQ-004 slice 2).
 *
 * The bug this module was written for is not visible in a screenshot: the conflict banner
 * rendered, said "keep editing to overwrite it", and every further save was refused — because
 * the tab kept quoting the version it had loaded, which the second editor had already moved.
 * The author was left a working Reload and a dead end, and the only way to save was to throw
 * their work away. A UI that *looks* like it handled a conflict is exactly how that survives.
 *
 * So the two questions ("what does the server say it holds", "what may the next save quote")
 * are pulled out of the click handler and answered here, where they can be asked whether they
 * are right.
 */
import assert from "node:assert/strict";
import test from "node:test";

import { readVersionFrom, resolveConflict } from "./conflict.ts";

const SERVER_MESSAGE =
  "this rule was saved by somebody else (it is now at version 7); reload to see their change, or keep editing to overwrite it";

test("readVersionFrom reads the version the server named", () => {
  assert.equal(readVersionFrom(SERVER_MESSAGE), 7);
});

test("readVersionFrom returns null rather than guessing a version it was not given", () => {
  // The dangerous case is a *plausible* default — 0, or one more than we hold — because it
  // converts an unknown into a confident overwrite of a colleague's work.
  assert.equal(readVersionFrom("this rule was saved by somebody else"), null);
  assert.equal(readVersionFrom(""), null);
});

test("readVersionFrom refuses a version that is not a plain count", () => {
  assert.equal(readVersionFrom("at version 7.5"), null);
  assert.equal(readVersionFrom("at version seven"), null);
});

test("a conflict offers a confirmed overwrite that quotes the server's version", () => {
  // Fails if the overwrite ever re-derives the base locally: the resolution is built from
  // `conflict.version` alone, so a change to quote `localVersion + 1` is a change to this
  // function's signature, which the test below pins shut.
  const resolution = resolveConflict({ message: SERVER_MESSAGE, version: 7 });
  assert.equal(resolution.choice, "overwrite");
  assert.equal(resolution.nextVersion, 7);
  assert.equal(resolution.requiresConfirmation, true);
  // The cost is named on the button itself, not in a toast nobody reads afterwards.
  assert.ok(resolution.confirmLabel.includes("7"));
});

test("a conflict with no named version falls back to reload and offers no dead button", () => {
  const resolution = resolveConflict({
    message: "this rule was saved by somebody else",
    version: null,
  });
  assert.equal(resolution.choice, "reload");
  assert.equal(resolution.nextVersion, null);
  assert.equal(resolution.requiresConfirmation, false);
});

test("nothing may be written before a reload, because writing destroys what the author asked to see", () => {
  // The regression: taking the reload exit and then firing a queued autosave would overwrite
  // the very version the author asked to look at. There is no path here that returns a
  // quotable version alongside the reload choice.
  const reload = resolveConflict({ message: SERVER_MESSAGE, version: null });
  assert.equal(reload.nextVersion, null);
  const overwrite = resolveConflict({ message: SERVER_MESSAGE, version: 7 });
  assert.equal(overwrite.requiresConfirmation, true);
});

test("a real server message round-trips into a working resolution", () => {
  // The two halves are used together in `persist`, so the parse and the decision are checked
  // as one path: a version that parses but resolves to null would be a banner that renders
  // and then refuses to save.
  const message = SERVER_MESSAGE;
  const resolution = resolveConflict({ message, version: readVersionFrom(message) });
  assert.equal(resolution.nextVersion, 7);
  assert.equal(resolution.requiresConfirmation, true);
});
