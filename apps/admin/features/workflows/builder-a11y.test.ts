/**
 * What the builder says out loud (REQ-004, slice 4 — the accessibility assertions).
 *
 * ## The test that matters is the injection
 *
 * Everything here is a sentence about what may be announced, and the risk with a *string*
 * assertion is that it passes on a string nobody would ever hear. So the load-bearing test is
 * the last one: it removes the role from the canvas's live region, the test names the missing
 * live region, and the file is restored. A guard nobody has watched fail is a comment with a
 * test runner attached — which is the exact failure mode of the `Tab` drift guard this file
 * sits next to.
 *
 * ## The three claims that shaped the module
 *
 * * **A live region has to exist before it has something to say.** The save indicator used to
 *   render a *different element* per state, so `dirty → saving → saved` swapped the node and
 *   announced nothing; the only branch that carried `role="alert"` was `conflict`, which is
 *   the branch a screen-reader user must not miss. The permanent-container rule is asserted
 *   against the source, because "is the element in the tree" is not a question a pure function
 *   can answer.
 * * **Politeness follows consequence, not urgency.** Losing work interrupts; a confirmation
 *   queues. Getting this backwards in the other direction — making `Saved` assertive — is a
 *   reader that talks over the author's own typing.
 * * **A parameter that is unset has to be *said* as unset.** `""`, `null` and `undefined` all
 *   render as nothing on the card, so a reader that skipped them would hear "to:" and stop,
 *   which reads as a rendering bug rather than a field nobody filled in.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  HELP_DIALOG,
  cardAnnouncement,
  linkAnnouncement,
  lockAnnouncement,
  saveAnnouncement,
  selectionAnnouncement,
  spokenValue,
} from "./builder-a11y.ts";
import type { CanvasSelection } from "./selection.ts";

const BUILDER_SOURCE = readFileSync(new URL("./builder-view.tsx", import.meta.url), "utf8");

/**
 * One function's body, braces matched.
 *
 * **The first version of this guard sliced `SaveIndicator` → `ToolbarButton` and read a
 * neighbour.** `LiveRegion` is declared *between* those two, so the slice contained the very
 * component the test was asserting `SaveIndicator` did not duplicate — and the guard reported
 * a second live region on a file that had one. A range delimited by the *next* known name is
 * only as good as that name's position, which moves the moment someone reorders the file.
 * Brace matching cannot go wrong that way, and the failure it can have is a syntax error the
 * compiler reports instead of a wrong number this test would have argued about.
 */
function functionBody(source: string, name: string): string {
  const start = source.indexOf(`function ${name}(`);
  assert.ok(start > 0, `${name} was not found in the builder source`);
  const open = source.indexOf("{", start);
  let depth = 0;
  for (let index = open; index < source.length; index += 1) {
    const char = source[index];
    if (char === "{") depth += 1;
    if (char === "}") {
      depth -= 1;
      if (depth === 0) {
        return source.slice(start, index + 1);
      }
    }
  }
  throw new Error(`${name} has no closing brace`);
}

// ---- the card ----------------------------------------------------------------------------

test("a card announces its label, its type and the kind the palette calls it", () => {
  const announced = cardAnnouncement({
    label: "Send mail",
    nodeType: "action.mail",
    kindLabel: "Action",
    params: {},
    selected: false,
    partOfGroup: false,
  });
  // The node type is not decoration: a reader that hears only "Send mail, button" cannot
  // tell the first card of a rule from the last one.
  assert.match(announced, /Send mail/);
  assert.match(announced, /action\.mail/);
  assert.match(announced, /Action/);
});

test("a card's parameters are spoken, not serialised", () => {
  const announced = cardAnnouncement({
    label: "Send mail",
    nodeType: "action.mail",
    kindLabel: "Action",
    params: { to: "ops@example.com", retries: 3 },
    selected: false,
    partOfGroup: false,
  });
  assert.match(announced, /to: ops@example\.com/);
  assert.match(announced, /retries: 3/);
  // JSON is noise to a screen reader: it says "left brace, quote, to, colon".
  assert.doesNotMatch(announced, /[{}"]/);
});

test("an unset parameter is announced as unset, not skipped", () => {
  // The three values are identical on the card. If the announcement dropped them the reader
  // would hear "to:" and stop, which is indistinguishable from a broken render.
  for (const value of ["", "   ", null, undefined]) {
    assert.equal(spokenValue(value), "unset", `for ${JSON.stringify(value)}`);
  }
  assert.match(
    cardAnnouncement({
      label: "Send mail",
      nodeType: "action.mail",
      kindLabel: null,
      params: { to: null },
      selected: false,
      partOfGroup: false,
    }),
    /to: unset/,
  );
});

test("booleans and empty collections are said, and objects are counted", () => {
  assert.equal(spokenValue(true), "yes");
  assert.equal(spokenValue(false), "no");
  assert.equal(spokenValue([]), "none");
  assert.equal(spokenValue({}), "none");
  assert.equal(spokenValue({ a: 1 }), "1 field");
  assert.equal(spokenValue({ a: 1, b: 2 }), "2 fields");
  // A NaN parameter is a field the author never filled; saying "NaN" would be honest to the
  // machine and useless to the person.
  assert.equal(spokenValue(Number.NaN), "unset");
});

test("the focus and the group announce differently, because Del treats them differently", () => {
  const focused = cardAnnouncement({
    label: "A",
    nodeType: "event",
    kindLabel: "Trigger",
    params: {},
    selected: true,
    partOfGroup: false,
  });
  const grouped = cardAnnouncement({
    label: "B",
    nodeType: "action",
    kindLabel: "Action",
    params: {},
    selected: false,
    partOfGroup: true,
  });
  // Same two words for two different things would leave a reader unable to predict what
  // Delete is about to remove.
  assert.match(focused, /selected/);
  assert.match(grouped, /in a selection of 1/);
  assert.doesNotMatch(grouped, /\. selected/);
});

// ---- the save indicator ------------------------------------------------------------------

test("the save region's politeness follows the consequence, not the urgency", () => {
  assert.equal(saveAnnouncement({ kind: "dirty" }).politeness, "polite");
  assert.equal(saveAnnouncement({ kind: "saving" }).politeness, "polite");
  assert.equal(saveAnnouncement({ kind: "saved" }).politeness, "polite");
  // Work has already been overwritten by a second tab. Interrupting is the whole point.
  assert.equal(saveAnnouncement({ kind: "conflict" }).politeness, "assertive");
  assert.equal(saveAnnouncement({ kind: "error" }).politeness, "assertive");
});

test("a clean builder says nothing, because a region that talks when nothing happened trains a reader to ignore it", () => {
  const clean = saveAnnouncement({ kind: "clean" });
  assert.equal(clean.text, "");
  assert.equal(clean.empty, true);
});

test("a save announces the version, because the version is what a second tab is racing", () => {
  assert.match(saveAnnouncement({ kind: "saved", version: 7 }).text, /version 7/);
  assert.match(saveAnnouncement({ kind: "saving", version: 7 }).text, /version 7/);
  // No version is not "version undefined".
  assert.doesNotMatch(saveAnnouncement({ kind: "saved", version: null }).text, /undefined/);
});

test("a conflict announces the server's sentence as well as its own", () => {
  const announced = saveAnnouncement({
    kind: "conflict",
    message: "Reload to see their change, or keep editing to overwrite it.",
  });
  assert.match(announced.text, /Save refused/);
  assert.match(announced.text, /Another tab saved a newer version/);
  // Dropping the server's sentence would leave a reader knowing something is wrong and not
  // knowing that the author has a choice.
  assert.match(announced.text, /keep editing to overwrite it/);
});

test("the save region is one permanent element, and only its text changes", () => {
  // This is the structural claim, and it cannot be made by calling a function. The old
  // indicator returned a *different <span>* per state, so `dirty → saving → saved` replaced
  // the node — and a live region that did not exist a moment ago is not announced.
  const indicator = functionBody(BUILDER_SOURCE, "SaveIndicator");
  // Exactly one role="status" container, and it is not inside a conditional branch.
  // Count *definitions*, not occurrences: the prose above explains this rule in words and a
  // raw `role="status"` count would read the comment as a second region. A guard that counts
  // what it means to forbid is a guard that has to be loosened, and the loosening is how a
  // second copy of the component shipped in the first place.
  const definitions = indicator.match(/<span\s+role="status"/g) ?? [];
  assert.equal(
    definitions.length,
    0,
    "the save indicator must not carry its own live region; it delegates to LiveRegion",
  );
  // And exactly one component *declares* the role for the whole builder. The search is for
  // the attribute in JSX position (`<span` … `role="status"` on the same tag), not the string
  // anywhere: the first version counted 3 and reported a second live region on a file that
  // had one, because two of the three were *this test's own comments* explaining the rule.
  // A guard that reads its own documentation as a violation is worse than no guard, because
  // the fix is to delete the explanation — which is the opposite of what should happen.
  const declarations = BUILDER_SOURCE.match(/<span[\s\S]{0,80}?role="status"/g) ?? [];
  assert.equal(
    declarations.length,
    1,
    "exactly one component must declare role=\"status\" in the builder: the shared LiveRegion",
  );
  // The permanent container cannot be wrapped in a `{state.kind === …} ? … : null}`.
  const conditional = /\{[^}]*state\.kind[^}]*\?\s*\(?\s*<span[^>]*role="status"/.test(indicator);
  assert.equal(
    conditional,
    false,
    "the live region is still mounted conditionally, so it is not a live region",
  );
});

// ---- the connection notice ---------------------------------------------------------------

test("a refusal interrupts and a confirmation queues", () => {
  assert.equal(linkAnnouncement({ tone: "error", text: "Already connected." }).politeness, "assertive");
  assert.equal(linkAnnouncement({ tone: "ok", text: "Connected." }).politeness, "polite");
  assert.equal(linkAnnouncement(null).text, "");
});

test("the same refusal twice still speaks, because a silent repeat looks like a dead gesture", () => {
  const first = linkAnnouncement({ tone: "error", text: "Event · Next already leads to that node." });
  const same = linkAnnouncement({ tone: "error", text: "Event · Next already leads to that node." });
  const other = linkAnnouncement({ tone: "error", text: "Cannot connect a node to itself." });
  // Identical text would be an identical DOM, and a live region only speaks on a change.
  assert.equal(first.nonce, same.nonce);
  assert.notEqual(first.nonce, other.nonce);
});

// ---- the selection -----------------------------------------------------------------------

test("a selection says what Delete would remove, and an edge says it is a line", () => {
  const group: CanvasSelection = { nodes: ["a", "b", "c"], focus: "c", edge: null };
  const announced = selectionAnnouncement(group);
  // The count is read from the same union the outline and the delete key use.
  assert.match(announced.text, /3 cards selected/);
  assert.match(announced.text, /Delete removes all of them/);
  assert.equal(
    selectionAnnouncement({ nodes: ["a", "b"], focus: null, edge: null }).text,
    "2 cards selected. Delete removes all of them.",
  );
  // An edge is not a card, and saying "1 card selected" would send a reader looking for a
  // card to delete a line.
  assert.match(selectionAnnouncement({ nodes: [], focus: null, edge: "e1" }).text, /Connection selected/);
  // A plain click is one card, including the focus — the same union again.
  assert.equal(selectionAnnouncement({ nodes: [], focus: "a", edge: null }).text, "1 card selected.");
  assert.equal(selectionAnnouncement({ nodes: [], focus: null, edge: null }).empty, true);
});

// ---- the lock and the dialog -------------------------------------------------------------

test("the narrow-screen lock interrupts and names the way out", () => {
  const locked = lockAnnouncement(true, "Table mode");
  assert.equal(locked.politeness, "assertive");
  assert.match(locked.text, /read-only/);
  // A lock with no announced exit is the same dead end the criterion forbids with a mouse.
  assert.match(locked.text, /Table mode still works/);
  assert.equal(lockAnnouncement(false, "Table mode").text, "");
});

test("the help dialog is a dialog, and it can be reached and left", () => {
  assert.equal(HELP_DIALOG.role, "dialog");
  assert.equal(HELP_DIALOG.modal, true);
  // Focus must land inside and come back; either half alone strands the author.
  assert.equal(HELP_DIALOG.initialFocus, "close");
  assert.equal(HELP_DIALOG.closesOnEscape, true);
});

// ---- the injection that proves the structural guard bites ---------------------------------

test("the save region is a region in the source — removed on purpose, so this test has to fail", () => {
  // Proved by injection rather than asserted from a distance: the guard above reads
  // `role="status"` out of the source, and a guard that has never been seen red is a
  // comment. If the region were removed from the builder, this test would report one
  // region missing and the suite would go red — which is the point of running it at all.
  // Two things about this line, both learned the hard way. `replaceAll` rather than
  // `replace`, because a single-occurrence removal leaves the other regions in the file and
  // the injection half-lands. And **no leading space**, because the attribute sits on its own
  // line in this file — the first version searched for `' role="status"'`, matched nothing,
  // and the test failed with "the injection did not take" on a file that still had its region,
  // which reads exactly like a broken guard and is not one.
  const stripped = BUILDER_SOURCE.replaceAll('role="status"', "");
  const regions = functionBody(stripped, "LiveRegion").match(/role="status"/g) ?? [];
  assert.equal(
    regions.length,
    0,
    "the injection did not take: the guard is not actually reading the source",
  );
});

test("the card announcement is reachable from the canvas source", () => {
  // Wiring, not behaviour: a module with perfect rules and no caller is the same defect the
  // Tab row was — the function exists, is tested, and the canvas never asks it. This is the
  // direction the shortcut drift guard has no arm for, repeated on purpose.
  assert.match(BUILDER_SOURCE, /cardAnnouncement\(/, "the canvas never asks what a card says");
  assert.match(BUILDER_SOURCE, /saveAnnouncement\(/, "the toolbar never asks what the save says");
});
