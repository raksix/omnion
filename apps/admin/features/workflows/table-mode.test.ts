/**
 * Table mode's rules, tested where they can be wrong.
 *
 * Each test names the failure it prevents, and the hostile case is the interesting one — the
 * obvious implementation passes the happy path and fails exactly one of these.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  buildTable,
  diffTableEdits,
  paramEntries,
  setLabel,
  setParam,
  stringValue,
  toGraph,
  typeLabel,
} from "./table-mode.ts";
import type { GraphEdge, GraphNode } from "@/lib/api";

const node = (id: string, type: string, label: string, params: Record<string, unknown> = {}): GraphNode => ({
  id,
  type,
  label,
  params,
  position: { x: 0, y: 0 },
});

const edge = (id: string, source: string, source_port: string, target: string): GraphEdge => ({
  id,
  source,
  source_port,
  target,
});

/** A valid spine: event → action → end. */
function spine(): { nodes: GraphNode[]; edges: GraphEdge[] } {
  return {
    nodes: [
      node("n1", "trigger.event", "On order", { event: "order.created" }),
      node("n2", "action", "Send mail", { template: "welcome" }),
      node("n3", "end", "Done"),
    ],
    edges: [edge("e1", "n1", "out", "n2"), edge("e2", "n2", "success", "n3")],
  };
}

test("a table over a valid spine has one row per node, in graph order", () => {
  const { nodes, edges } = spine();
  const table = buildTable(nodes, edges);
  assert.equal(table.rows.length, 3);
  assert.deepEqual(
    table.rows.map((r) => r.id),
    ["n1", "n2", "n3"],
  );
  assert.equal(table.dirty, false, "an unedited table must not be committable");
});

test("each row names where its node goes and what led here, by LABEL not by id", () => {
  const { nodes, edges } = spine();
  const { rows } = buildTable(nodes, edges);
  const action = rows[1];
  assert.deepEqual(action.outgoing, ["Done · success"]);
  assert.deepEqual(action.incoming, ["On order · out"]);
  // The ids would make the table useless in a review: the author labels nodes.
  assert.ok(!action.outgoing.join(" ").includes("n3"), "a row must not leak node ids");
});

test("a branching node lists BOTH outgoing ports — a find() would show one", () => {
  const nodes = [
    node("n1", "trigger.event", "On order", { event: "order.created" }),
    node("n2", "condition", "Is admin?"),
    node("n3", "action", "Mail admin"),
    node("n4", "action", "Mail customer"),
  ];
  const edges = [
    edge("e1", "n1", "out", "n2"),
    edge("e2", "n2", "true", "n3"),
    edge("e3", "n2", "false", "n4"),
  ];
  const { rows } = buildTable(nodes, edges);
  const condition = rows[1];
  assert.equal(condition.outgoing.length, 2, "both branches belong in the row");
  assert.ok(condition.outgoing.some((l) => l.startsWith("Mail admin · true")));
  assert.ok(condition.outgoing.some((l) => l.startsWith("Mail customer · false")));
});

test("a DANGLING edge is named rather than dropped — the canvas draws it, so the table must", () => {
  const { nodes } = spine();
  const edges = [edge("e1", "n1", "out", "n2"), edge("bad", "n2", "success", "ghost")];
  const { rows } = buildTable(nodes, edges);
  assert.deepEqual(rows[1].outgoing, ["(missing node) · success"]);
});

test("an orphan node has neither column filled, and says so with an empty list", () => {
  const { nodes } = spine();
  const { rows } = buildTable(nodes, [],);  // no edges at all
  const orphan = rows[1];
  assert.deepEqual(orphan.outgoing, []);
  assert.deepEqual(orphan.incoming, []);
});

test("a node with NO parameters reports zero, so an empty cell is not a broken read", () => {
  const { nodes, edges } = spine();
  const { rows } = buildTable(nodes, edges);
  assert.equal(rows[2].paramCount, 0);
  assert.deepEqual(paramEntries(rows[2]), []);
});

test("typing a value makes the draft dirty and writes it back by KEY", () => {
  const { nodes, edges } = spine();
  let table = buildTable(nodes, edges);
  table = setParam(table, "n1", "event", "order.paid");
  assert.equal(table.dirty, true);
  assert.equal(table.rows[0].params.event, "order.paid");

  const written = toGraph(table, nodes);
  assert.equal(written[0].params.event, "order.paid");
  assert.equal(written[1].params.template, "welcome", "untouched rows survive the commit");
});

test("typing the value a field already holds is NOT an edit — a commit would bump the version for nothing", () => {
  const { nodes, edges } = spine();
  let table = buildTable(nodes, edges);
  table = setParam(table, "n1", "event", "order.created");
  assert.equal(table.dirty, false, "same value typed again is not a change");
  assert.equal(diffTableEdits(table), false);
});

test("clearing a field REMOVES the key rather than storing an empty string", () => {
  const { nodes, edges } = spine();
  let table = buildTable(nodes, edges);
  table = setParam(table, "n1", "event", "");
  assert.ok(!("event" in table.rows[0].params), "an empty string is not a parameter value");
  assert.equal(table.rows[0].paramCount, 0);

  const written = toGraph(table, nodes);
  assert.ok(!("event" in written[0].params), "the graph must not carry the cleared key either");
});

test("an edit then an undo returns the draft to clean, and the commit is refused again", () => {
  const { nodes, edges } = spine();
  let table = buildTable(nodes, edges);
  table = setParam(table, "n1", "event", "order.paid");
  assert.equal(diffTableEdits(table), true);
  table = setParam(table, "n1", "event", "order.created");
  assert.equal(diffTableEdits(table), false, "typing the original value back is an undo");
  assert.equal(table.dirty, false);
});

test("an edit on one row does not mark another row dirty", () => {
  const { nodes, edges } = spine();
  const table = setParam(buildTable(nodes, edges), "n2", "template", "receipt");
  assert.equal(table.rows[0].dirty, false);
  assert.equal(table.rows[1].dirty, true);
  assert.equal(table.dirty, true, "the DRAFT is dirty even though one row is not");
});

test("a node with a nested parameter shows its JSON, not [object Object]", () => {
  const nodes = [node("n1", "transform", "Shape", { fields: { a: 1, b: ["x"] } })];
  const { rows } = buildTable(nodes, []);
  const [key, value] = paramEntries(rows[0])[0];
  assert.equal(key, "fields");
  assert.equal(value, '{"a":1,"b":["x"]}');
});

test("params are listed in a stable order, so the table does not reshuffle per render", () => {
  const nodes = [node("n1", "action", "A", { z: "1", a: "2", m: "3" })];
  const { rows } = buildTable(nodes, []);
  assert.deepEqual(
    paramEntries(rows[0]).map(([k]) => k),
    ["a", "m", "z"],
  );
});

test("a cyclic parameter renders as empty rather than throwing inside a render", () => {
  const cyclic: Record<string, unknown> = {};
  cyclic.self = cyclic;
  const nodes = [node("n1", "action", "A", { loop: cyclic })];
  const { rows } = buildTable(nodes, []);
  assert.equal(stringValue(cyclic), "", "a cycle is described, not thrown");
});

test("a non-object params (a bare array, or a number) is read as no parameters", () => {
  const nodes = [node("n1", "action", "A", ["not", "a", "map"] as unknown as Record<string, unknown>)];
  const { rows } = buildTable(nodes, []);
  assert.equal(rows[0].paramCount, 0, "an array has no parameter keys to edit");
});

test("an unknown node type keeps its key — blanking it makes two plugin rows identical", () => {
  const nodes = [node("n1", "acme.custom_thing", "Custom")];
  const { rows } = buildTable(nodes, []);
  assert.equal(rows[0].typeLabel, "acme.custom_thing");
  assert.equal(typeLabel("acme.custom_thing"), "acme.custom_thing");
  assert.equal(typeLabel("condition"), "Condition");
});

test("a node with no position reads as the origin, not NaN", () => {
  const nodes = [{ id: "n1", type: "note", label: "N", params: {} } as unknown as GraphNode];
  const { rows } = buildTable(nodes, []);
  assert.deepEqual(rows[0].position, { x: 0, y: 0 });
});

test("a rename is an edit, and clearing a label restores the one it had rather than naming the row", () => {
  const { nodes, edges } = spine();
  let table = setLabel(buildTable(nodes, edges), "n2", "Send receipt");
  assert.equal(table.rows[1].label, "Send receipt");
  assert.equal(diffTableEdits(table), true);

  // Clearing a label is a real edit the author made, and it has to land somewhere. Restoring the
  // node's OWN original label is the only fallback that is reversible: falling back to the id
  // renames "Send mail" to "n2" for good, which is a change the author never asked for and a
  // canvas that now shows an id where a name was.
  table = setLabel(table, "n2", "  ");
  assert.equal(table.rows[1].label, "Send mail", "clearing restores the authored label");
  assert.equal(diffTableEdits(table), false, "and it is an undo, not a rename to an id");
});

test("buildTable does not alias the graph's params — an edit must not mutate the canvas's copy", () => {
  const { nodes, edges } = spine();
  const table = setParam(buildTable(nodes, edges), "n1", "event", "order.paid");
  assert.equal(nodes[0].params.event, "order.created", "the source graph is untouched");
  assert.notEqual(table.rows[0].params, nodes[0].params, "a shared object is how the views drift");
});
