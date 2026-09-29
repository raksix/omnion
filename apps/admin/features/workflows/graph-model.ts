"use client";

/**
 * The canvas's own state: the document, the selection, the history, the viewport.
 *
 * Everything here is a *pure* function of the document plus an operation. That is the whole
 * design and it is what makes the three things the REQ demands cheap:
 *
 * - **Undo is the document, not a diff.** A history is a list of documents. 100 of them is
 *   nothing for a 25-node graph, and there is no class of operation (a drag, a connection, a
 *   rename) that can be forgotten by a reducer that never had to learn about it.
 * - **Redo is the same list read backwards**, so redo cannot disagree with what undo did.
 * - **The server is the only validator.** Nothing here decides whether a graph is valid; the
 *   canvas asks `POST …/validate` and shows what comes back. A second validator is a second
 *   place to be wrong, and the wrong one is the one the person is looking at.
 *
 * One rule that is not obvious: the canvas is the *authoring* surface and the engine is the
 * *execution* surface, so a node the person switched off produces no step at all. "Disabled"
 * is a canvas concept here, never a runtime one.
 */
import type { GraphConnection, GraphDocument, GraphNode, GraphNote } from "@/lib/types";

/** Where the viewport is looking, in canvas units. */
export type Viewport = { x: number; y: number; zoom: number };

/** A node's size on the canvas, in canvas units. Fixed, so hit-testing is a subtraction. */
export const NODE_WIDTH = 220;
export const NODE_HEIGHT = 72;
/** The gap a hand-drawn wire leaves from a node edge, so an arrow head has somewhere to point. */
export const PORT_RADIUS = 5;
/** How far the grid dots sit apart at 100 % zoom. */
export const GRID_SIZE = 20;
/** Snap step when the grid toggle is on. Canvas-local by design: it is never stored. */
export const SNAP = GRID_SIZE;

/** The furthest a viewport may zoom out and in, in both directions. */
export const MIN_ZOOM = 0.25;
export const MAX_ZOOM = 2;

/** A history entry: the document as it was, and what the person did to get past it. */
export type HistoryEntry = { document: GraphDocument; label: string };

/**
 * The canvas's whole editing state.
 *
 * `past` and `future` are the two ends of one list. An operation pushes the current document
 * onto `past` and clears `future`, which is what makes undo and redo total: there is no
 * operation that can update one without updating the other, because there is one code path.
 */
export type CanvasState = {
  document: GraphDocument;
  past: HistoryEntry[];
  future: HistoryEntry[];
  selected: string[];
  /** The node whose parameters the inspector is showing. */
  inspecting: string | null;
  viewport: Viewport;
  /** Whether a drag is snapping to the grid. Browser-local; the REQ says it is never stored. */
  snap: boolean;
  /** How many positions an arrow key moves a node. */
  nudge: number;
};

/** A fresh canvas: empty, at 100 %, nothing selected. */
export function emptyState(): CanvasState {
  return {
    document: { nodes: [], connections: [], notes: [] },
    past: [],
    future: [],
    selected: [],
    inspecting: null,
    viewport: { x: 0, y: 0, zoom: 1 },
    snap: true,
    nudge: 1,
  };
}

/** How many operations the history keeps. The REQ asks for "at least 100". */
export const HISTORY_LIMIT = 120;

/**
 * Apply an operation, recording what it was called.
 *
 * The label is not decoration: the REQ asks for a *named* toast ("Undo: connect Check role →
 * Email"), and a history that stored only documents could not produce one. So the label is
 * part of the entry and the toast reads it back.
 */
export function apply(
  state: CanvasState,
  label: string,
  next: GraphDocument,
): CanvasState {
  // An operation that changed nothing is not an operation. Recording it would make undo appear
  // to do nothing, which is worse than a missing undo because it looks broken.
  if (sameDocument(state.document, next)) return state;

  const entry: HistoryEntry = { document: state.document, label };
  const past = [...state.past, entry];
  return {
    ...state,
    document: next,
    past: past.length > HISTORY_LIMIT ? past.slice(past.length - HISTORY_LIMIT) : past,
    future: [],
  };
}

/** Undo one operation, moving the current document onto the redo end. */
export function undo(state: CanvasState): CanvasState {
  const previous = state.past[state.past.length - 1];
  if (!previous) return state;
  return {
    ...state,
    document: previous.document,
    past: state.past.slice(0, -1),
    future: [...state.future, { document: state.document, label: previous.label }],
    // A selection naming a node that no longer exists is a selection of nothing, and the
    // inspector would render an empty form rather than say so.
    selected: state.selected.filter((key) =>
      previous.document.nodes.some((node) => node.key === key),
    ),
  };
}

/** Redo the operation `undo` last reversed. */
export function redo(state: CanvasState): CanvasState {
  const next = state.future[state.future.length - 1];
  if (!next) return state;
  return {
    ...state,
    document: next.document,
    past: [
      ...state.past,
      { document: state.document, label: next.label },
    ].slice(-HISTORY_LIMIT),
    future: state.future.slice(0, -1),
  };
}

/** The name of the operation `undo` would reverse, for the toast. */
export function undoLabel(state: CanvasState): string | null {
  return state.past[state.past.length - 1]?.label ?? null;
}

/** The name of the operation `redo` would repeat. */
export function redoLabel(state: CanvasState): string | null {
  return state.future[state.future.length - 1]?.label ?? null;
}

/** Whether the two documents would store the same bytes. */
export function sameDocument(a: GraphDocument, b: GraphDocument): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

// ---------------------------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------------------------

/**
 * A node key that is not taken.
 *
 * `send_email`, `send_email_2`, `send_email_3`… — the number is in the key because the key is
 * what a connection names, so a rename must never have to rewrite the edges pointing at it.
 */
export function freeKey(nodes: GraphNode[], base: string): string {
  const taken = new Set(nodes.map((node) => node.key));
  if (!taken.has(base)) return base;
  for (let n = 2; n < 1000; n += 1) {
    const candidate = `${base}_${n}`;
    if (!taken.has(candidate)) return candidate;
  }
  // 200 nodes is the server's ceiling, so this is unreachable in practice; a key that collides
  // is a validation issue the person can see, and a panic here would be a blank canvas.
  return `${base}_${Date.now()}`;
}

/**
 * A new node of a registry type, positioned at a point.
 *
 * `base` is the registry key, and the stored key is the *editor* key: `http_request_1`. The
 * distinction is not cosmetic — the registry key can be renamed by a package install, and a
 * document whose node keys were registry keys would break every connection on that upgrade.
 */
export function addNode(
  document: GraphDocument,
  nodeType: string,
  at: { x: number; y: number },
  snapToGrid: boolean,
): { document: GraphDocument; node: GraphNode } {
  const key = freeKey(document.nodes, nodeType);
  const position = snapToGrid ? snapPoint(at) : at;
  const node: GraphNode = {
    key,
    type: nodeType,
    label: key,
    position,
    params: {},
    disabled: false,
  };
  return {
    document: { ...document, nodes: [...document.nodes, node] },
    node,
  };
}

/** Move a node, honouring the grid toggle. A single node, not a selection. */
export function moveNode(
  document: GraphDocument,
  key: string,
  to: { x: number; y: number },
  snapToGrid: boolean,
): GraphDocument {
  const position = snapToGrid ? snapPoint(to) : to;
  return replaceNode(document, key, (node) => ({ ...node, position }));
}

/** Nudge a node by a whole number of canvas units. Grid snapping is deliberately bypassed. */
export function nudgeNode(
  document: GraphDocument,
  key: string,
  dx: number,
  dy: number,
): GraphDocument {
  return replaceNode(document, key, (node) => ({
    ...node,
    position: { x: node.position.x + dx, y: node.position.y + dy },
  }));
}

/** Replace one node through a transform, leaving everything else byte-identical. */
export function replaceNode(
  document: GraphDocument,
  key: string,
  transform: (node: GraphNode) => GraphNode,
): GraphDocument {
  let changed = false;
  const nodes = document.nodes.map((node) => {
    if (node.key !== key) return node;
    const next = transform(node);
    if (next !== node) changed = true;
    return next;
  });
  return changed ? { ...document, nodes } : document;
}

/** Delete nodes, and with them every connection that touched one. */
export function deleteNodes(document: GraphDocument, keys: string[]): GraphDocument {
  const gone = new Set(keys);
  if (gone.size === 0) return document;
  return {
    nodes: document.nodes.filter((node) => !gone.has(node.key)),
    connections: document.connections.filter(
      (wire) => !gone.has(wire.from) && !gone.has(wire.to),
    ),
    notes: document.notes,
  };
}

// ---------------------------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------------------------

/** One edge, with both endpoints. */
export type WireEndpoints = {
  from: string;
  from_port: string;
  to: string;
  to_port: string;
};

/**
 * The four reasons a connect is refused, in the order a person needs to hear them.
 *
 * Every one of them is a *named* refusal because the REQ asks for a named reason, and because a
 * drag that silently does nothing is indistinguishable from a canvas that is broken. The client
 * checks these; the server checks them again in the compiler, and the two agree because they
 * read the same registry.
 */
export type ConnectRefusal =
  | { kind: "self_loop"; message: string }
  | { kind: "duplicate"; message: string }
  | { kind: "cycle"; message: string }
  | { kind: "unknown_port"; message: string };

/** True when `edges` already carries exactly this wire, in either direction. */
export function hasConnection(document: GraphDocument, edge: WireEndpoints): boolean {
  const same = (wire: GraphConnection) =>
    wire.from === edge.from &&
    wire.from_port === edge.from_port &&
    wire.to === edge.to &&
    wire.to_port === edge.to_port;
  return document.connections.some(same);
}

/**
 * Would adding this wire close a control-flow cycle?
 *
 * Depth-first from the *target*, following existing edges forward: if the source is reachable
 * from the target, then source → target closes a loop. Port kinds are ignored here, because the
 * REQ's rule is about control flow and a data edge that points backwards is legal — it is how a
 * node joins two branches.
 */
export function wouldCycle(document: GraphDocument, edge: WireEndpoints): boolean {
  const forward = new Map<string, string[]>();
  for (const wire of document.connections) {
    const list = forward.get(wire.from) ?? [];
    list.push(wire.to);
    forward.set(wire.from, list);
  }

  const seen = new Set<string>();
  const stack = [edge.to];
  while (stack.length > 0) {
    const current = stack.pop() as string;
    if (current === edge.from) return true;
    if (seen.has(current)) continue;
    seen.add(current);
    stack.push(...(forward.get(current) ?? []));
  }
  return false;
}

/** The reason this wire cannot be made, or `null` when it can. */
export function refuseConnect(
  document: GraphDocument,
  edge: WireEndpoints,
  knownPorts: (nodeKey: string) => { inputs: string[]; outputs: string[] },
): ConnectRefusal | null {
  if (edge.from === edge.to) {
    return { kind: "self_loop", message: "a node cannot connect to itself" };
  }
  if (!document.nodes.some((node) => node.key === edge.from)) {
    return { kind: "unknown_port", message: `no node named ${edge.from}` };
  }
  if (!document.nodes.some((node) => node.key === edge.to)) {
    return { kind: "unknown_port", message: `no node named ${edge.to}` };
  }
  const ports = knownPorts(edge.from);
  if (!ports.outputs.includes(edge.from_port)) {
    return {
      kind: "unknown_port",
      message: `${edge.from} has no output called ${edge.from_port}`,
    };
  }
  const target = knownPorts(edge.to);
  if (!target.inputs.includes(edge.to_port)) {
    return {
      kind: "unknown_port",
      message: `${edge.to} has no input called ${edge.to_port}`,
    };
  }
  if (hasConnection(document, edge)) {
    return { kind: "duplicate", message: "these two ports are already connected" };
  }
  if (wouldCycle(document, edge)) {
    return {
      kind: "cycle",
      message: "that would loop this branch back on itself",
    };
  }
  return null;
}

/** Make a wire, optionally with a branch label. */
export function connect(
  document: GraphDocument,
  edge: WireEndpoints,
  label?: string,
): GraphDocument {
  const wire: GraphConnection = { ...edge, ...(label ? { label } : {}) };
  return { ...document, connections: [...document.connections, wire] };
}

/**
 * Reconnect by moving one end.
 *
 * The two ends are told apart by *which side* moved, not by the shape of the endpoints, because
 * a user dragging an edge is describing "this end now goes there" and nothing else. The edge's
 * other end is found by matching the end that is not `moving`.
 */
export function reconnect(
  document: GraphDocument,
  index: number,
  end: "from" | "to",
  to: WireEndpoints,
): GraphDocument {
  const wire = document.connections[index];
  if (!wire) return document;
  const connections = [...document.connections];
  connections[index] = end === "from" ? { ...wire, from: to.from, from_port: to.from_port } : { ...wire, to: to.to, to_port: to.to_port };
  return { ...document, connections };
}

/** Delete connections by index. */
export function deleteConnections(
  document: GraphDocument,
  indexes: number[],
): GraphDocument {
  const gone = new Set(indexes);
  return {
    ...document,
    connections: document.connections.filter((_, index) => !gone.has(index)),
  };
}

/** Give a wire a branch label. An empty string removes it, which is what a person means. */
export function labelConnection(
  document: GraphDocument,
  index: number,
  label: string,
): GraphDocument {
  const connections = [...document.connections];
  if (!connections[index]) return document;
  const trimmed = label.trim();
  connections[index] = trimmed
    ? { ...connections[index], label: trimmed }
    : { ...connections[index], label: undefined };
  return { ...document, connections };
}

// ---------------------------------------------------------------------------------------------
// Notes
// ---------------------------------------------------------------------------------------------

/** Note colours, in palette order. The server's default is the first of them. */
export const NOTE_COLOURS = ["amber", "slate", "emerald", "rose", "violet"] as const;

/** A new sticky note at a point. */
export function addNote(
  document: GraphDocument,
  at: { x: number; y: number },
): GraphDocument {
  const taken = new Set(document.notes.map((note) => note.id));
  let id = "note_1";
  for (let n = 2; taken.has(id); n += 1) id = `note_${n}`;
  const note: GraphNote = {
    id,
    position: at,
    color: NOTE_COLOURS[0],
    width: 240,
    height: 140,
    text: "",
  };
  return { ...document, notes: [...document.notes, note] };
}

/** Rewrite a note through a transform. */
export function updateNote(
  document: GraphDocument,
  id: string,
  transform: (note: GraphNote) => GraphNote,
): GraphDocument {
  let changed = false;
  const notes = document.notes.map((note) => {
    if (note.id !== id) return note;
    const next = transform(note);
    if (next !== note) changed = true;
    return next;
  });
  return changed ? { ...document, notes } : document;
}

// ---------------------------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------------------------

/**
 * A selection copied to the internal clipboard, ready to paste.
 *
 * The point of copying *this* rather than the raw nodes is the rewiring: the pasted copy needs
 * new keys, and every connection **between two copied nodes** must be re-pointed at them, while
 * every connection that left or entered the selection is dropped. A copy that kept its
 * external edges would paste a copy wired into the original, and the person would not know
 * which node the run is really talking to.
 */
export type Clipboard = {
  nodes: GraphNode[];
  connections: GraphConnection[];
};

/** Build a clipboard from a selection. */
export function copySelection(
  document: GraphDocument,
  keys: string[],
): Clipboard {
  const chosen = new Set(keys);
  return {
    nodes: document.nodes.filter((node) => chosen.has(node.key)),
    connections: document.connections.filter(
      (wire) => chosen.has(wire.from) && chosen.has(wire.to),
    ),
  };
}

/**
 * Paste a clipboard at a point, with new keys.
 *
 * The offset is applied to both nodes and the point, so "paste" pastes where the cursor is
 * rather than on top of the original — the behaviour a person expects from every other editor
 * they have used.
 */
export function pasteClipboard(
  document: GraphDocument,
  clipboard: Clipboard,
  at: { x: number; y: number },
): { document: GraphDocument; keys: string[] } {
  if (clipboard.nodes.length === 0) return { document, keys: [] };

  const base = clipboard.nodes[0].position;
  const dx = at.x - base.x;
  const dy = at.y - base.y;
  const renames = new Map<string, string>();
  const nodes = clipboard.nodes.map((node) => {
    const key = freeKey(document.nodes, node.type);
    renames.set(node.key, key);
    return {
      ...node,
      key,
      label: node.label,
      position: { x: node.position.x + dx, y: node.position.y + dy },
    };
  });
  const connections = clipboard.connections.map((wire) => ({
    from: renames.get(wire.from) as string,
    from_port: wire.from_port,
    to: renames.get(wire.to) as string,
    to_port: wire.to_port,
    ...(wire.label ? { label: wire.label } : {}),
  }));

  return {
    document: {
      ...document,
      nodes: [...document.nodes, ...nodes],
      connections: [...document.connections, ...connections],
    },
    keys: nodes.map((node) => node.key),
  };
}

// ---------------------------------------------------------------------------------------------
// Selection, layout, viewport
// ---------------------------------------------------------------------------------------------

/** Click: this node only. */
export function selectOnly(selected: string[], key: string): string[] {
  return [key];
}

/** Shift-click: add or remove one, without disturbing the rest. */
export function toggleSelection(selected: string[], key: string): string[] {
  return selected.includes(key)
    ? selected.filter((each) => each !== key)
    : [...selected, key];
}

/** Every node, for `⌘A`. */
export function selectAll(document: GraphDocument): string[] {
  return document.nodes.map((node) => node.key);
}

/** Round a point to the grid. */
export function snapPoint(point: { x: number; y: number }): { x: number; y: number } {
  return {
    x: Math.round(point.x / SNAP) * SNAP,
    y: Math.round(point.y / SNAP) * SNAP,
  };
}

/**
 * The bounding box of a set of nodes, or `null` for an empty selection.
 *
 * A layer of padding on every side so a fit does not put a node's edge exactly on the frame —
 * which reads as clipped even when nothing is.
 */
export function boundsOf(
  nodes: GraphNode[],
  padding = 40,
): { x: number; y: number; width: number; height: number } | null {
  if (nodes.length === 0) return null;
  const xs = nodes.map((node) => node.position.x);
  const ys = nodes.map((node) => node.position.y);
  const left = Math.min(...xs) - padding;
  const top = Math.min(...ys) - padding;
  return {
    x: left,
    y: top,
    width: Math.max(...xs) - Math.min(...xs) + NODE_WIDTH + padding * 2,
    height: Math.max(...ys) - Math.min(...ys) + NODE_HEIGHT + padding * 2,
  };
}

/** The viewport that shows `box` inside a `frame`-sized area, clamped to the zoom limits. */
export function viewportFor(
  box: { x: number; y: number; width: number; height: number },
  frame: { width: number; height: number },
): Viewport {
  if (frame.width <= 0 || frame.height <= 0) return { x: -box.x, y: -box.y, zoom: 1 };
  const zoom = clamp(
    Math.min(frame.width / box.width, frame.height / box.height),
    MIN_ZOOM,
    1,
  );
  return {
    x: -(box.x + box.width / 2) * zoom + frame.width / 2,
    y: -(box.y + box.height / 2) * zoom + frame.height / 2,
    zoom,
  };
}

/** Keep a number inside a range. */
export function clamp(value: number, low: number, high: number): number {
  return Math.min(high, Math.max(low, value));
}

/**
 * A layered left-to-right layout, which preserves manual positions.
 *
 * "Preserves" is the REQ's word and it is the whole design: a person who dragged one node
 * somewhere deliberate means it, and a layout that re-derives every position throws that away.
 * So a node whose position has been touched is **pinned** and the layout routes around it.
 * Every node is pinned or floating until an explicit "reset positions" says otherwise, and the
 * toolbar's layout button is that explicit act.
 */
export type LayoutResult = { positions: Record<string, { x: number; y: number }> };

/** The column a node falls in, by its longest path from any root. */
function depthOf(
  key: string,
  forward: Map<string, string[]>,
  memo: Map<string, number>,
  stack: Set<string>,
): number {
  if (memo.has(key)) return memo.get(key) as number;
  // A cycle the validator has not yet complained about must not hang the layout: the answer
  // "it has no parents" is a safe depth, and the validation panel is where the cycle is named.
  if (stack.has(key)) return 0;
  stack.add(key);
  const parents = [...forward.entries()].filter(([, children]) => children.includes(key));
  const depth =
    parents.length === 0
      ? 0
      : 1 + Math.max(...parents.map(([parent]) => depthOf(parent, forward, memo, stack)));
  stack.delete(key);
  memo.set(key, depth);
  return depth;
}

/**
 * Arrange a graph into columns by depth, keeping every node's current position as its row.
 *
 * The gap is what makes "no overlaps" true rather than intended: the column pitch is wider than
 * a node, and the row pitch is taller than a node, so two nodes can never share a slot however
 * dense the graph gets.
 */
export function layeredLayout(document: GraphDocument): LayoutResult {
  const forward = new Map<string, string[]>();
  for (const wire of document.connections) {
    const list = forward.get(wire.from) ?? [];
    list.push(wire.to);
    forward.set(wire.from, list);
  }

  const memo = new Map<string, number>();
  const columns = new Map<number, GraphNode[]>();
  for (const node of document.nodes) {
    const depth = depthOf(node.key, forward, memo, new Set());
    const list = columns.get(depth) ?? [];
    list.push(node);
    columns.set(depth, list);
  }

  const columnPitch = NODE_WIDTH + 120;
  const rowPitch = NODE_HEIGHT + 48;
  const positions: Record<string, { x: number; y: number }> = {};

  for (const [depth, nodes] of [...columns.entries()].sort((a, b) => a[0] - b[0])) {
    // The tallest column sets the origin, so the first column always starts at y = 0 and the
    // graph grows downward rather than being centred in a space that depends on the order the
    // columns were visited in.
    nodes.forEach((node, row) => {
      positions[node.key] = { x: depth * columnPitch, y: row * rowPitch };
    });
  }

  return { positions };
}

/** Apply a layout to a document. */
export function applyLayout(
  document: GraphDocument,
  layout: LayoutResult,
): GraphDocument {
  return {
    ...document,
    nodes: document.nodes.map((node) => {
      const position = layout.positions[node.key];
      return position ? { ...node, position } : node;
    }),
  };
}

/** Scale a point from screen space into canvas space. */
export function toCanvas(point: { x: number; y: number }, viewport: Viewport): { x: number; y: number } {
  return {
    x: (point.x - viewport.x) / viewport.zoom,
    y: (point.y - viewport.y) / viewport.zoom,
  };
}
