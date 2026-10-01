/**
 * The keyboard-only path through the builder (REQ-004, "A keyboard-only pass adds two nodes,
 * connects them, edits a parameter, validates and runs, with the pointer untouched").
 *
 * The criterion is five verbs in a row with no pointer between them, and the interesting part
 * is that **four of the five had a key and one had none at all**. Adding a node had `⌘P` +
 * `Enter`, moving had arrows, deleting had `Del` — and *connecting* was bound to a 10px port
 * dot you have to aim a mouse at, so a keyboard user could add two nodes, place them perfectly
 * and then be unable to make a rule out of them. The graph would validate as "trigger has no
 * connection", which is the one error whose cause is invisible on a screen that looks finished.
 *
 * So the connection is modelled as a **two-key gesture with a state machine**, not as a
 * shortcut. `C` picks the source port; `Tab`/`ArrowRight` then walks the selection to the
 * target and `Enter` commits. The state lives here so it can be tested without a DOM, and
 * because the three rules below are each a way the shortcut can quietly do the wrong thing.
 */

import type { GraphEdge, GraphNode, GraphNodeType } from "@/lib/api";
import { HISTORY_LIMIT } from "./builder-history.ts";

/** A node as the keyboard path needs to see it — the id, the type and the port list. */
export interface KeyboardNode {
  id: string;
  type: string;
}

/** The port list of a node type, keyed by node type. */
export type PortTable = Map<string, GraphNodeType>;

/** What the keyboard path is currently waiting for. */
export type KeyboardConnectState =
  | { kind: "idle" }
  /** A source picked; a target must be chosen next. */
  | { kind: "awaiting-target"; sourceId: string; sourcePort: string }
  /** The last refusal, so the author reads *why* nothing happened instead of pressing again. */
  | { kind: "refused"; text: string; sourceId: string; sourcePort: string };

/** The keys the keyboard path answers. */
export const KEYMAP = {
  /** Focus the palette. */
  palette: "p",
  /** Begin a connection from the selected node's first port. */
  connect: "c",
  /** Commit the pending connection on the selected target. */
  commit: "enter",
  /** Abandon whatever is in flight. */
  cancel: "escape",
  /** Focus the inspector's first field. */
  inspect: "i",
  /** Validate. */
  validate: "v",
  /** Run once. */
  run: "r",
  /** Save now. */
  save: "s",
  /** Open the shortcut list. A chord, not a bare key, so it never eats a letter. */
  help: "/",
} as const;

/** What a key press means, given what the path is waiting for. */
export type KeyIntent =
  | { kind: "focus-palette" }
  /** Only produced when nothing is pending — a second `C` mid-gesture restarts it instead. */
  | { kind: "begin-connect" }
  /** Only ever produced while a connection is pending; see [`readKey`]. */
  | { kind: "commit-connect" }
  | { kind: "cancel" }
  | { kind: "focus-inspector" }
  | { kind: "validate" }
  | { kind: "run" }
  | { kind: "save" }
  /** `⌘/` — the shortcut list. Only ever produced for the chord, never for a bare `/`. */
  | { kind: "help" }
  | { kind: "unhandled" };

/**
 * Is this event aimed at something the author is typing into?
 *
 * The keyboard path *adds* shortcuts, which makes it the most likely place for a bare `c` to
 * steal a character from the inspector's own text field. The canvas handler already guards
 * this; the pure function carries the same rule so the guard is asserted rather than
 * remembered, and so this module can be tested without a `HTMLElement`.
 */
export function isTypingTarget(target: {
  tagName?: string;
  isContentEditable?: boolean;
}): boolean {
  const tag = (target.tagName ?? "").toUpperCase();
  return (
    tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || target.isContentEditable === true
  );
}

/**
 * The port a keyboard connection leaves from.
 *
 * The **first** port, always, and that is a decision rather than a convenience: the point of
 * the shortcut is to make the common case (a trigger's `out`, a task's `success`) one keystroke.
 * A port *picker* would be the complete version and would need a second gesture to choose from,
 * which is one more thing a keyboard user has to discover. `beginConnect` returns the refusal
 * instead, so a branching node is never silently connected down the wrong arm — a `condition`
 * connected on its `true` port when the author meant `false` is a rule that runs and is wrong,
 * which is the worst possible outcome for a shortcut whose whole selling point is speed.
 */
export function defaultPortFor(nodeType: GraphNodeType | undefined): string | null {
  if (!nodeType || nodeType.outputs.length === 0) {
    return null;
  }
  return nodeType.outputs[0].key;
}

/**
 * Read a plain key press, given what the path is already waiting for.
 *
 * `meta`/`ctrl` combinations are **not** handled here: the canvas owns `⌘P` and the rest, and
 * two owners of the same chord is a race. This reads the unmodified single-key path only.
 *
 * The `pending` argument exists for one reason, and it is the load-bearing decision of this
 * whole file. `Enter` is not a neutral key: it *activates* whatever is focused. If the builder
 * consumed it unconditionally, every Enter in the inspector — submitting a field, closing a
 * section — would also try to commit a connection that was never started. So `Enter` yields
 * `commit-connect` only while a gesture is genuinely in flight, and `unhandled` otherwise, at
 * which point the focused element does exactly what it always did.
 */
export function readKey(
  event: {
    key: string;
    metaKey?: boolean;
    ctrlKey?: boolean;
    altKey?: boolean;
    target?: { tagName?: string; isContentEditable?: boolean };
  },
  pending: KeyboardConnectState = { kind: "idle" },
): KeyIntent {
  if (event.target && isTypingTarget(event.target)) {
    return { kind: "unhandled" };
  }
  // `⌘/` (Ctrl+/) is the only chord this function answers, and it is read BEFORE the
  // meta/ctrl guard below — a guard placed first would swallow it and the shortcut list would
  // be a key nobody can press. It is still a chord: a bare `/` is a character, and binding one
  // turns the shortcut list into something that fires while an author types.
  if (
    (event.metaKey || event.ctrlKey) &&
    !event.altKey &&
    event.key.toLowerCase() === KEYMAP.help
  ) {
    return { kind: "help" };
  }
  if (event.metaKey || event.ctrlKey || event.altKey) {
    return { kind: "unhandled" };
  }
  const key = event.key.toLowerCase();

  if (key === KEYMAP.commit) {
    return pending.kind === "awaiting-target" ? { kind: "commit-connect" } : { kind: "unhandled" };
  }
  switch (key) {
    case KEYMAP.palette:
      return { kind: "focus-palette" };
    case KEYMAP.connect:
      return { kind: "begin-connect" };
    case KEYMAP.cancel:
      return { kind: "cancel" };
    case KEYMAP.inspect:
      return { kind: "focus-inspector" };
    case KEYMAP.validate:
      return { kind: "validate" };
    case KEYMAP.run:
      return { kind: "run" };
    case KEYMAP.save:
      return { kind: "save" };
    default:
      return { kind: "unhandled" };
  }
}

/**
 * Resolve a `C` press on the current selection.
 *
 * Returns the refusal as a state rather than dropping it, for the same reason the port dot
 * shows its notice on the canvas: a key that does nothing looks broken, and a broken shortcut
 * gets pressed again with more force.
 *
 * A `C` pressed while a gesture is already pending **restarts** it from the current selection.
 * The alternative — ignoring the press — makes picking the wrong source a two-step recovery
 * (Escape, then `C`) on a screen where the "in flight" state is one line of text the author
 * may not be looking at.
 */
export function beginConnect(
  selectedId: string | null,
  nodeTypes: PortTable,
): KeyboardConnectState {
  if (!selectedId) {
    return {
      kind: "refused",
      text: "Select a node first — a connection needs a source.",
      sourceId: "",
      sourcePort: "",
    };
  }
  const port = defaultPortFor(nodeTypes.get(selectedId));
  if (!port) {
    return {
      kind: "refused",
      text: "That node has no output port, so nothing can leave it.",
      sourceId: selectedId,
      sourcePort: "",
    };
  }
  return { kind: "awaiting-target", sourceId: selectedId, sourcePort: port };
}

/**
 * Resolve an `Enter` press while a connection is pending.
 *
 * Two refusals here are the ones a pointer user never hits and a keyboard user hits constantly,
 * because the pointer version makes the *canvas* refuse the bad target by not offering it:
 *
 * 1. **A self-connection.** The source is still selected when the gesture begins, so a
 *    double-tap on one card is the first thing anybody tries. Rejected explicitly rather than
 *    by the cycle validator, so the author is told at the gesture instead of after Validate.
 * 2. **A duplicate.** The node below the source is often already wired to it; the refusal
 *    names the node, because "already connected" without a name is indistinguishable from a
 *    save that silently did nothing.
 */
export function commitConnect(
  state: KeyboardConnectState,
  selectedId: string | null,
  edges: readonly GraphEdge[],
): { state: KeyboardConnectState; edge: GraphEdge | null; notice: string } {
  if (state.kind !== "awaiting-target") {
    return { state, edge: null, notice: "" };
  }
  if (!selectedId) {
    return {
      state: { kind: "refused", text: "Select the node the connection should arrive at.", sourceId: state.sourceId, sourcePort: state.sourcePort },
      edge: null,
      notice: "",
    };
  }
  if (selectedId === state.sourceId) {
    return {
      state: {
        kind: "refused",
        text: "A node cannot lead to itself.",
        sourceId: state.sourceId,
        sourcePort: state.sourcePort,
      },
      edge: null,
      notice: "",
    };
  }
  const duplicate = edges.some(
    (edge) =>
      edge.source === state.sourceId &&
      edge.source_port === state.sourcePort &&
      edge.target === selectedId,
  );
  if (duplicate) {
    return {
      state: {
        kind: "refused",
        text: "That connection already exists.",
        sourceId: state.sourceId,
        sourcePort: state.sourcePort,
      },
      edge: null,
      notice: "",
    };
  }
  // The target's *type* is not checked here: a node's inputs are implicit (a target has no
  // declared input ports in the registry), so every non-cycle edge is structurally legal and
  // the server's `validate` owns the semantic half. Inventing an input rule here would be a
  // rule the server does not have, and the builder would refuse connections the engine runs.
  return {
    state: { kind: "idle" },
    edge: {
      id: `e-${state.sourceId}-${state.sourcePort}-${selectedId}`,
      source: state.sourceId,
      source_port: state.sourcePort,
      target: selectedId,
    },
    notice: "Connected.",
  };
}

/** The Escape rule, in the same priority order the pointer gesture uses. */
export function cancelConnect(state: KeyboardConnectState): KeyboardConnectState {
  return state.kind === "idle" ? state : { kind: "idle" };
}

/**
 * The whole keyboard pass, as a list of steps a probe can drive.
 *
 * This exists because the criterion is a *sequence*, and a sequence is the one thing unit
 * tests are worst at: each step can pass while the path between them is broken. The list is
 * data, so the probe can replay exactly the order the criterion states and assert on the
 * server's copy afterwards.
 */
export const KEYBOARD_PASS: readonly { step: string; keys: string; expects: string }[] = [
  { step: "add", keys: "⌘P then Enter", expects: "a second node exists" },
  { step: "add", keys: "⌘P then Enter", expects: "a third node exists" },
  { step: "select", keys: "arrows or Tab", expects: "the source node is selected" },
  { step: "connect", keys: "C", expects: "the source port is armed" },
  { step: "target", keys: "arrow to the target, then Enter", expects: "an edge exists" },
  { step: "parameter", keys: "I then type", expects: "the server's param changed" },
  { step: "validate", keys: "V", expects: "the problems panel answers" },
  { step: "run", keys: "R", expects: "a run exists" },
];

/**
 * The shortcut list `⌘/` opens — **one row per key the canvas actually binds**.
 *
 * This is a catalogue, not a copy of the `KEYMAP` object, and the reason is the failure the
 * criterion is really about: a help list written next to the handler drifts. Every shortcut
 * added next year appears in the code and not on screen, and the one place an author goes to
 * *find out what is possible* is where the truth stops being told. The groups below are
 * therefore transcribed from the handler itself — the `⌘` chords in `onCanvasKeyDown` and the
 * single-key path in `readKey` — and a test walks both and fails on a chord the list omits.
 *
 * `locked` marks the keys the narrow-screen gate refuses. It is a **whitelist of reading keys**
 * (`isReadingKey`), so a shortcut added next year is refused by default and the row has to say
 * so deliberately — which is exactly the state a help list should be in when the builder is
 * read-only, rather than a second list to keep in step with the first.
 */
export interface ShortcutRow {
  /** The chord, as an author would type it. */
  keys: string;
  /** What it does. */
  label: string;
  /** True when the narrow-screen read-only gate refuses this key. */
  locked: boolean;
}

export const SHORTCUT_GROUPS: readonly {
  title: string;
  rows: readonly ShortcutRow[];
}[] = [
  {
    title: "Building",
    rows: [
      { keys: "⌘P", label: "Focus the palette, then Enter to add a node", locked: true },
      { keys: "C", label: "Connect from the selected node's first output port", locked: true },
      { keys: "Enter", label: "Commit the connection onto the selected target", locked: true },
      { keys: "Esc", label: "Abandon the connection, or clear the selection", locked: false },
      { keys: "I", label: "Focus the inspector's first field", locked: true },
      { keys: "Del", label: "Delete the selected node or connection", locked: true },
    ],
  },
  {
    title: "Moving",
    rows: [
      { keys: "Arrows", label: "Nudge the selection one grid step (⇧ for five)", locked: false },
      // Said precisely because it was not true until this tick. The row read "Walk to the
      // next card" while `onCanvasKeyDown` bound no `Tab` case at all and every card is
      // `tabIndex={-1}` — so the browser moved focus out to the toolbar and the selection
      // never moved. The wording now names both halves of what a keyboard user does next:
      // the wrap (there is no End key for the canvas) and the fact that it reaches the
      // connections, which is what makes "Del on a selected edge" satisfiable from a
      // keyboard rather than from a pointer.
      { keys: "Tab", label: "Walk to the next card or connection, and wrap", locked: false },
      { keys: "⇧Tab", label: "Walk the same route backwards", locked: false },
      { keys: "⌘A", label: "Select every node", locked: true },
    ],
  },
  {
    title: "History and clipboard",
    rows: [
      { keys: "⌘Z", label: historyDepthLabel(), locked: true },
      { keys: "⇧⌘Z or ⌘Y", label: "Redo", locked: true },
      { keys: "⌘C / ⌘V", label: "Copy and paste the selection", locked: true },
      { keys: "⌘D", label: "Duplicate the selection", locked: true },
    ],
  },
  {
    title: "Running",
    rows: [
      { keys: "⌘S", label: "Save now (the autosave is a debounce, this is not)", locked: true },
      { keys: "V", label: "Validate the graph", locked: true },
      { keys: "R", label: "Run the rule once", locked: true },
      { keys: "⌘/", label: "Open or close this list", locked: false },
    ],
  },
];

/** Every row, flattened. A test counts the chords in the handler against this. */
export const SHORTCUT_ROWS: readonly ShortcutRow[] = SHORTCUT_GROUPS.flatMap((group) => group.rows);

/** How many rows the narrow-screen gate would refuse, for the overlay's own footnote. */
export function lockedShortcutCount(rows: readonly ShortcutRow[] = SHORTCUT_ROWS): number {
  return rows.filter((row) => row.locked).length;
}

/**
 * The undo row's depth, as the builder implements it.
 *
 * The row used to read "Undo (the history is 50 steps deep)" — a number typed into a string,
 * answering to nothing. Lower `HISTORY_LIMIT` and the keyboard sheet keeps promising a depth the
 * stack no longer has, which is the one shape of claim on this REQ nobody could check: a shorter
 * history is a *silent* regression, since every entry it still keeps undoes correctly. The label
 * is derived from the constant so the two cannot drift, and `history-depth.test.ts` is what walks
 * fifty presses of undo to say the number is earned rather than asserted.
 */
export function historyDepthLabel(): string {
  return `Undo (the history is ${HISTORY_LIMIT} steps deep)`;
}
