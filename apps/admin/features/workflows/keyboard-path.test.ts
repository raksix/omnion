import { strict as assert } from "node:assert";
import test from "node:test";

// The `@/` alias, not a relative path: `keyboard-path.ts` imports it that way and a test that
// reaches the same file by two different specifiers proves nothing about the module graph.
import type { GraphEdge, GraphNodeType } from "@/lib/api";
import {
  KEYBOARD_PASS,
  KEYMAP,
  beginConnect,
  cancelConnect,
  commitConnect,
  defaultPortFor,
  isTypingTarget,
  readKey,
  type KeyboardConnectState,
  type PortTable,
} from "./keyboard-path.ts";

function nodeType(
  key: string,
  outputs: { key: string; label: string }[],
): GraphNodeType {
  return {
    key,
    label: key,
    category: "Actions",
    summary: "",
    outputs: outputs.map((port) => ({ ...port })),
    params: [],
    inert: false,
    defaults: {},
  } as unknown as GraphNodeType;
}

const TRIGGER = nodeType("trigger.event", [{ key: "out", label: "Next" }]);
const CONDITION = nodeType("condition", [
  { key: "true", label: "Yes" },
  { key: "false", label: "No" },
]);
const END = nodeType("end", []);

const TYPES: PortTable = new Map<string, GraphNodeType>([
  ["trigger.event", TRIGGER],
  ["condition", CONDITION],
  ["end", END],
]);

const IDLE: KeyboardConnectState = { kind: "idle" };

// ---- the guard that protects the inspector -----------------------------------------------------

test("a bare letter typed into a field is never a shortcut", () => {
  // 'c' is the connect key and a letter people type. Without this guard the inspector's own
  // fields eat it, so a keyboard user cannot write a value that contains those letters.
  for (const tagName of ["INPUT", "TEXTAREA", "SELECT", "input", "textarea"]) {
    assert.equal(
      isTypingTarget({ tagName }),
      true,
      `${tagName} must be recognised as a typing target`,
    );
    assert.equal(readKey({ key: "c", target: { tagName } }).kind, "unhandled");
  }
  assert.equal(isTypingTarget({ tagName: "DIV", isContentEditable: true }), true);
  assert.equal(isTypingTarget({ tagName: "DIV", isContentEditable: false }), false);
  assert.equal(isTypingTarget({ tagName: "BUTTON" }), false);
});

test("a modified key belongs to the canvas, not to this path", () => {
  // ⌘P is already the canvas's; two owners of one chord is a race and the loser's shortcut
  // fires at random.
  assert.equal(readKey({ key: "p", metaKey: true }).kind, "unhandled");
  assert.equal(readKey({ key: "c", ctrlKey: true }).kind, "unhandled");
  assert.equal(readKey({ key: "v", altKey: true }).kind, "unhandled");
});

test("every verb the criterion names has a key", () => {
  assert.equal(readKey({ key: KEYMAP.palette }).kind, "focus-palette");
  assert.equal(readKey({ key: KEYMAP.connect }).kind, "begin-connect");
  assert.equal(readKey({ key: KEYMAP.inspect }).kind, "focus-inspector");
  assert.equal(readKey({ key: KEYMAP.validate }).kind, "validate");
  assert.equal(readKey({ key: KEYMAP.run }).kind, "run");
  assert.equal(readKey({ key: KEYMAP.save }).kind, "save");
  assert.equal(readKey({ key: KEYMAP.cancel }).kind, "cancel");
  // Case must not matter, or the shortcut works on one keyboard layout and not another.
  assert.equal(readKey({ key: "C" }).kind, "begin-connect");
  assert.equal(readKey({ key: "R" }).kind, "run");
  assert.equal(readKey({ key: "q" }).kind, "unhandled");
});

// ---- Enter is the load-bearing guard ---------------------------------------------------------

test("Enter commits a connection ONLY while one is pending", () => {
  // This is the bug the design exists to prevent: Enter also activates the focused element,
  // so consuming it unconditionally would fire a connect attempt out of every inspector
  // field and every focused toolbar button.
  assert.equal(readKey({ key: "Enter" }).kind, "unhandled");
  assert.equal(readKey({ key: "Enter" }, IDLE).kind, "unhandled");
  assert.equal(
    readKey({ key: "Enter" }, { kind: "awaiting-target", sourceId: "a", sourcePort: "out" }).kind,
    "commit-connect",
  );
  // A *refused* gesture is finished: Enter must not retry it behind the author's back.
  assert.equal(
    readKey({ key: "Enter" }, { kind: "refused", text: "no", sourceId: "a", sourcePort: "" }).kind,
    "unhandled",
  );
});

// ---- the connection gesture ------------------------------------------------------------------

test("C picks the first output port, and says so when there is none", () => {
  const armed = beginConnect("trigger.event", TYPES);
  assert.deepEqual(armed, { kind: "awaiting-target", sourceId: "trigger.event", sourcePort: "out" });
  assert.equal(defaultPortFor(CONDITION), "true");
  assert.equal(defaultPortFor(END), null);

  const nothing = beginConnect("end", TYPES);
  assert.equal(nothing.kind, "refused");
  assert.match(nothing.kind === "refused" ? nothing.text : "", /no output port/i);

  const noSelection = beginConnect(null, TYPES);
  assert.equal(noSelection.kind, "refused");
  assert.match(noSelection.kind === "refused" ? noSelection.text : "", /select a node/i);
});

test("an unknown node type refuses rather than guessing a port name", () => {
  // A node whose type left the registry (a plugin removed, a rename) must not be wired
  // through a fabricated port — the edge would be structurally legal and semantically absurd.
  const answer = beginConnect("plugin.gone", TYPES);
  assert.equal(answer.kind, "refused");
});

test("Enter on a second node makes the edge", () => {
  const armed = beginConnect("trigger.event", TYPES);
  const done = commitConnect(armed, "condition", []);
  assert.equal(done.state.kind, "idle");
  assert.equal(done.notice, "Connected.");
  assert.deepEqual(done.edge, {
    id: "e-trigger.event-out-condition",
    source: "trigger.event",
    source_port: "out",
    target: "condition",
  });
});

test("a node cannot lead to itself — the first thing a double-tap tries", () => {
  const armed = beginConnect("condition", TYPES);
  const refused = commitConnect(armed, "condition", []);
  assert.equal(refused.edge, null, "the first gesture a keyboard user tries must not create a cycle");
  assert.equal(refused.state.kind, "refused");
  assert.match(refused.state.kind === "refused" ? refused.state.text : "", /cannot lead to itself/i);
});

test("a duplicate is refused by name, not silently skipped", () => {
  const armed = beginConnect("trigger.event", TYPES);
  const existing: GraphEdge[] = [
    { id: "e1", source: "trigger.event", source_port: "out", target: "condition" },
  ];
  const refused = commitConnect(armed, "condition", existing);
  assert.equal(refused.edge, null);
  assert.match(refused.state.kind === "refused" ? refused.state.text : "", /already exists/i);
});

test("the same target on a DIFFERENT port is a different connection, not a duplicate", () => {
  // `condition` exports true and false. A rule with both arms reaching the same node is
  // legitimate, and a duplicate check that ignored the port would refuse it and leave the
  // author with no way to say what they meant.
  const armed = beginConnect("condition", TYPES);
  const existing: GraphEdge[] = [
    { id: "e1", source: "condition", source_port: "false", target: "end" },
  ];
  const done = commitConnect(armed, "end", existing);
  assert.equal(done.state.kind, "idle", done.state.kind === "refused" ? done.state.text : "");
  assert.equal(done.edge?.source_port, "true");
});

test("a refusal is a dead end until the author picks a new source", () => {
  // Nothing may be committed out of a refused state: the guard on Enter already says so, and
  // this asserts it from the other side.
  const refused = commitConnect(beginConnect("condition", TYPES), "condition", []);
  const again = commitConnect(refused.state, "trigger.event", []);
  assert.equal(again.edge, null, "a refusal must not become an edge on the next Enter");
});

test("Enter with nothing selected names what to do", () => {
  const armed = beginConnect("trigger.event", TYPES);
  const refused = commitConnect(armed, null, []);
  assert.equal(refused.edge, null);
  assert.match(refused.state.kind === "refused" ? refused.state.text : "", /select the node/i);
});

test("Escape ends a gesture and is a no-op when there is none", () => {
  const armed = beginConnect("trigger.event", TYPES);
  assert.equal(cancelConnect(armed).kind, "idle");
  assert.equal(cancelConnect(IDLE).kind, "idle");
  assert.equal(cancelConnect({ kind: "refused", text: "x", sourceId: "", sourcePort: "" }).kind, "idle");
});

test("C while a gesture is pending re-arms from the new selection", () => {
  // Escape-then-C is two steps to recover from a mis-picked source; re-arming makes it one.
  const armed = beginConnect("trigger.event", TYPES);
  const rearmed = beginConnect("condition", TYPES);
  assert.deepEqual(rearmed, { kind: "awaiting-target", sourceId: "condition", sourcePort: "true" });
  assert.notDeepEqual(armed, rearmed);
});

// ---- the sequence the criterion states ---------------------------------------------------------

test("the documented pass is the five verbs in the criterion's order", () => {
  // Data, not prose: the probe replays it, and a criterion that lists its own instrument is
  // the one that cannot be argued about later.
  assert.deepEqual(
    KEYBOARD_PASS.map((step) => step.step),
    ["add", "add", "select", "connect", "target", "parameter", "validate", "run"],
  );
  for (const step of KEYBOARD_PASS) {
    assert.notEqual(step.keys, "", `${step.step} must name its keys`);
    assert.notEqual(step.expects, "", `${step.step} must name what it proves`);
  }
});
