/**
 * Table mode: the same graph as rows, and the one rule that keeps the two views honest.
 *
 * Criterion (REQ-004, criterion 8): *"Table mode renders the same definition, edits parameters,
 * and stays consistent with the canvas after a save in either mode."*
 *
 * ## Why a table at all, when the canvas already draws the graph
 *
 * Because the canvas cannot be reviewed in a screenshot, cannot be diffed, and cannot be read
 * by somebody who has never seen a node editor. The list is the same `graph` jsonb — not a
 * second projection, not a re-derivation — so a value edited here is the value the canvas
 * draws, and the criterion is only about *which* object each side mutates.
 *
 * ## The one rule: a row is an EDIT BUFFER, never a source
 *
 * The obvious implementation writes each row back on blur, per field. That produces a graph
 * where a node's `label` and its `params.title` are two writes that can be interleaved with
 * another save, and a canvas that redraws mid-edit and moves the caret. So the table holds a
 * **draft** of the whole definition and commits it with the builder's own autosave: one graph,
 * one write, one version. `diffTableEdits` is the whole contract, and it answers one question —
 * *did the author change anything* — because a commit of an unchanged draft advances
 * `graph_version` and hands the next tab a conflict nobody caused.
 *
 * ## Why a parameter edit is by KEY and not by index
 *
 * `params` is a registry-shaped object whose keys are declared by the node type
 * (`node_types.rs`), and two node types can declare the same key with different meanings. A
 * row that edits `params[0]` is editing "whatever happens to be first", which is a value the
 * author never chose. `setParam` therefore takes the key the field is labelled with, and
 * `pristineParams` is what makes an untouched field distinguishable from one set to the same
 * value — a node whose `event` is `order.created` and whose author typed `order.created` again
 * has made no edit, and committing that would bump the version for nothing.
 */

import type { GraphNode, GraphEdge } from "@/lib/api";

/** A single node, as a table row. */
export type TableRow = {
  id: string;
  type: string;
  label: string;
  /** The label the canvas draws for the node type, or the raw key when it is not in the registry. */
  typeLabel: string;
  /** Where the node sits on the canvas — the table shows it so a row is locatable. */
  position: { x: number; y: number };
  /** `targetNodeLabel · sourcePort` per outgoing edge, in the order the edges are stored. */
  outgoing: string[];
  /** Node labels this node's incoming edges arrive from, in edge order. */
  incoming: string[];
  /** The node's parameters, as authored. */
  params: Record<string, unknown>;
  /** The parameters as they were when the table was opened — the pristine copy. */
  originalParams: Record<string, unknown>;
  /** The label as it was when the table was opened. */
  originalLabel: string;
  /** Whether THIS row carries an author edit. The draft's `dirty` is the OR over these. */
  dirty: boolean;
  /** The number of parameters, so a row with none can say "no parameters" rather than look empty. */
  paramCount: number;
};

/** The table's draft of a definition: the rows plus the edges between them. */
export type TableDraft = {
  rows: TableRow[];
  edges: GraphEdge[];
  /**
   * True when at least one row differs from what the table was opened with. A draft with no
   * edits must not be committed: the write would advance the version and hand the next tab a
   * conflict that no author created.
   */
  dirty: boolean;
};

/** Type labels for the built-in node families, so a row is readable without the canvas. */
const TYPE_LABELS: Record<string, string> = {
  "trigger.event": "Event trigger",
  "trigger.schedule": "Schedule",
  "trigger.manual": "Manual",
  "trigger.hook": "Inbound hook",
  condition: "Condition",
  switch: "Switch",
  action: "Action",
  wait: "Wait",
  approval: "Approval",
  http_request: "HTTP request",
  transform: "Transform",
  sub_workflow: "Sub-workflow",
  end: "End",
  note: "Note",
};

/** The name a table row shows for a node type. Unknown types keep their key — a plugin node's
 * key is the only thing that identifies it, and blanking it would make two plugin rows
 * indistinguishable. */
export function typeLabel(nodeType: string): string {
  return TYPE_LABELS[nodeType] ?? nodeType;
}

/** `params` is arbitrary JSON from the registry's schema; the table edits it as a string map. */
function asParamRecord(params: unknown): Record<string, unknown> {
  if (params && typeof params === "object" && !Array.isArray(params)) {
    return params as Record<string, unknown>;
  }
  return {};
}

/** Deep-enough equality for parameter values: JSON-shaped, so a stringify compare is exact. */
function paramsEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (a === null || b === null || typeof a !== typeof b) return false;
  if (typeof a !== "object") return false;
  return JSON.stringify(a) === JSON.stringify(b);
}

/**
 * Build the table's rows from a graph.
 *
 * The label lookup is a `Map` built once: a `nodes.find` per edge is quadratic in a graph
 * with a wide switch, and a *missing* node resolves to the empty string — which is what makes
 * a dangling edge invisible in the table while the canvas draws it heading nowhere. A dangling
 * edge is named as `"(missing node) · out"`, because "table mode renders the same definition"
 * means it renders a broken one too.
 */
export function buildTable(
  nodes: GraphNode[],
  edges: GraphEdge[],
  typeLabels: Record<string, string> = TYPE_LABELS,
): TableDraft {
  const labels = new Map<string, string>();
  for (const node of nodes) labels.set(node.id, node.label);

  // A dangling edge is named, not hidden: the canvas draws it heading nowhere, and "table mode
  // renders the same definition" means it renders a broken one too.
  const nameOf = (id: string): string => labels.get(id) ?? "(missing node)";

  const outgoing = new Map<string, string[]>();
  const incoming = new Map<string, string[]>();
  for (const edge of edges) {
    const from = nameOf(edge.source);
    const to = nameOf(edge.target);
    const port = edge.source_port;

    const push = (map: Map<string, string[]>, key: string, line: string) => {
      const list = map.get(key);
      if (list) list.push(line);
      else map.set(key, [line]);
    };
    // What the author needs in each column: where this node goes next, and what led here.
    push(outgoing, edge.source, `${to} · ${port}`);
    push(incoming, edge.target, `${from} · ${port}`);
  }

  const rows = nodes.map((node): TableRow => {
    const params = asParamRecord(node.params);
    return {
      id: node.id,
      type: node.type,
      label: node.label,
      typeLabel: typeLabels[node.type] ?? node.type,
      position: { x: node.position?.x ?? 0, y: node.position?.y ?? 0 },
      outgoing: outgoing.get(node.id) ?? [],
      incoming: incoming.get(node.id) ?? [],
      params,
      originalParams: { ...params },
      originalLabel: node.label,
      dirty: false,
      paramCount: Object.keys(params).length,
    };
  });

  return { rows, edges, dirty: false };
}

/**
 * Set one parameter on one row, by key.
 *
 * Returns a new draft rather than mutating: the table renders from the draft and the canvas
 * from the graph, and a shared mutable object is how the two drift. An empty string **removes**
 * the key rather than storing `""` — a field cleared by the author is a parameter the author
 * does not want, and a registry that validates `event` as a non-empty string would reject
 * `""` on save while the table showed a filled field.
 */
export function setParam(draft: TableDraft, nodeId: string, key: string, raw: string): TableDraft {
  const rows = draft.rows.map((row) => {
    if (row.id !== nodeId) return row;

    const next = { ...row.params };
    if (raw.trim() === "") {
      delete next[key];
    } else {
      next[key] = raw;
    }

    return {
      ...row,
      params: next,
      paramCount: Object.keys(next).length,
      dirty: !paramsEqual(next, row.originalParams),
    };
  });

  return { ...draft, rows, dirty: rows.some((row) => row.dirty) };
}

/** Rename a node's label.
 *
 * An empty label restores the node's **own original** label rather than falling back to the id.
 * The id is the wrong fallback because it is not reversible: the author clears a field, and a
 * node called "Send mail" is now permanently called "n2" — a change nobody asked for, on a
 * canvas that now shows an id where a name was. Restoring what the row opened with makes the
 * clear an *undo*, which is what clearing a field means in every other editor.
 */
export function setLabel(draft: TableDraft, nodeId: string, label: string): TableDraft {
  const rows = draft.rows.map((row) => {
    if (row.id !== nodeId) return row;
    const next = label.trim() === "" ? row.originalLabel : label.trim();
    return { ...row, label: next, dirty: next !== row.originalLabel };
  });
  return { ...draft, rows, dirty: rows.some((row) => row.dirty) };
}

/** Whether this draft carries an author edit. The commit button is disabled when false, and a
 * save of an unedited draft is refused for the same reason: it would advance `graph_version`
 * and manufacture a conflict for the next tab. */
export function diffTableEdits(draft: TableDraft): boolean {
  return draft.rows.some(
    (row) =>
      !paramsEqual(row.params, row.originalParams) || row.label !== row.originalLabel,
  );
}

/**
 * The graph a commit writes: every row's parameters, back into the node objects.
 *
 * Labels are **not** applied here. The canvas owns a node's label while it is being dragged and
 * renamed there, and a table that wrote labels on commit would clobber a rename made in the
 * other view in the same save window — the two exits of criterion 8 fighting each other. The
 * table's label column is therefore display-only until a table-side rename is wired with the
 * same autosave path; the parameter columns are the criterion's edit surface and they are real.
 */
export function toGraph(draft: TableDraft, nodes: GraphNode[]): GraphNode[] {
  return nodes.map((node) => {
    const row = draft.rows.find((r) => r.id === node.id);
    if (!row) return node;
    return { ...node, params: { ...row.params } as Record<string, unknown> };
  });
}

/** A row's parameter entries in a stable order, so the table does not reshuffle on each render. */
export function paramEntries(row: TableRow): Array<[string, string]> {
  return Object.keys(row.params)
    .sort()
    .map((key) => [key, stringValue(row.params[key])]);
}

/** A parameter value as the single-line text an `<input>` holds. Objects and arrays are shown
 * as their JSON rather than `[object Object]`, which is what a template literal would produce
 * and which reads as a bug on screen. */
export function stringValue(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  try {
    return JSON.stringify(value);
  } catch {
    return "";
  }
}
