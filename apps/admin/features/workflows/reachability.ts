/**
 * Which nodes the engine's walk from the trigger reaches — the client's half of
 * `plan_from_node`'s first question (REQ-004, slice 4).
 *
 * ## Why the browser needs a walk at all
 *
 * *Run from here* offers a live button wherever a run could start, and the server decides
 * that by walking the graph from the trigger (`plan_from_node` → `project_walk`). The
 * canvas has no such walk, so it substituted the question a card can answer about itself
 * — "does any connection leave this node" — and the two answers disagreed.
 *
 * The disagreement was not cosmetic. **An unconnected node got a live button**, because
 * "no connection leaves it" is the answer that makes it look like the end of the path
 * rather than a node the engine cannot see, and `startability` refused that shape only
 * for *inert* nodes. But an unconnected node is not an edge case in this builder: every
 * node is dropped from the palette un-wired and connected afterwards, so that is the
 * state the canvas spends most of its life in. Every press on such a button was refused
 * by the server with `unknown_node`, and the refusal read like a statement about the run
 * rather than about the wiring.
 *
 * ## The port rule, and why it is read rather than typed
 *
 * The walk follows a connection only if the engine would follow it, and the engine's rule
 * is `!terminal` — a port that ends the run has nothing after it to walk onto. That fact
 * lives in `crates/workflows/src/graph.rs`, next to every port, and it used to be a
 * five-string `matches!` list beside it. A third hand-written copy in TypeScript would
 * make the drift certain rather than likely, so **the port table is read**: the palette
 * ships `terminal` to the browser already, and this walks what the server ships.
 *
 * `run_from.rs` asserts over the whole registry that `followed_port == !terminal`, which
 * is what licenses this to read the flag rather than repeat the list.
 *
 * ## What this deliberately does not do
 *
 * It does **not** reproduce the validator. A graph can fail validation for a dozen
 * reasons, none of which this module guesses at, and a projection that invented a
 * "not runnable" verdict would be a second opinion on a screen the server already has an
 * opinion about. The walk answers one question — *is this node on the path* — and the
 * press still goes to the server for the answer to every other one.
 */

import type { GraphEdge, GraphNode, GraphNodeType } from "@/lib/api";

/** One edge as the walk needs it, with the port's terminal flag resolved. */
export interface WalkEdge {
  /** The node the connection leaves. */
  source: string;
  /** The node it arrives at. */
  target: string;
  /** The port key the connection leaves by, e.g. `out`, `success`, `false`. */
  source_port: string;
}

/** What the walk can be built from. */
export interface WalkWorld {
  nodes: GraphNode[];
  edges: GraphEdge[];
  /** The palette, keyed by node type. A missing entry means an unknown type. */
  types: Map<string, GraphNodeType>;
}

/**
 * `true` when the engine's walk would follow a connection leaving this port.
 *
 * **An unknown port answers `true`**, and the reason is the validator rather than
 * optimism: `validate` refuses an edge on a port its node does not export, so this is
 * only ever asked about a port that exists. A type the palette does not carry (a plugin
 * node, a card drawn before the plugin was disabled) is treated as walkable, because the
 * alternative is greying out a button on a graph the server may well accept — and a
 * wrongly-*disabled* control is a bug the author cannot work around, while a wrongly-
 * enabled one is answered by the server with a sentence.
 */
export function portWalksThrough(
  port: string,
  nodeType: GraphNodeType | undefined,
): boolean {
  if (!nodeType) return true;
  const spec = nodeType.outputs.find((output) => output.key === port);
  if (!spec) return true;
  return !spec.terminal;
}

/**
 * The nodes the engine's walk from the trigger reaches.
 *
 * Two departures from the engine's walk, and both of them are about *which graph this
 * is* rather than about the algorithm:
 *
 * * **the order is by first arrival, not "first edge in the saved array".** The engine
 *   refuses an ambiguous branch at save time (`ambiguous_branch`), so on a stored graph
 *   there is never a choice to make. On an *unsaved* canvas there is — and picking by
 *   array order would put a whole run behind an arbitrary one. First arrival is a
 *   heuristic, and it is labelled as one: it cannot make a refusal disappear, because a
 *   run is never started from a graph the server has not seen.
 * * **no cycle bound is needed beyond the visited set.** The engine carries one because
 *   its walk is also the projection; this one answers a yes/no per node and a visited set
 *   is the whole of it.
 *
 * A graph with no trigger — mid-edit, or a save that failed — yields an **empty** set,
 * which the inspector reads as "not reached" and therefore refuses the control. That is
 * the safe direction: an author who has not built a path yet is not about to start a run
 * at any node of it.
 */
export function reachedNodes(world: WalkWorld): Set<string> {
  const trigger = world.nodes.find((node) => node.type.startsWith("trigger."));
  if (!trigger) return new Set();

  const typeOf = (id: string): GraphNodeType | undefined =>
    world.types.get(world.nodes.find((node) => node.id === id)?.type ?? "");

  const reached = new Set<string>([trigger.id]);
  const queue = [trigger.id];
  while (queue.length > 0) {
    const current = queue.shift() as string;
    const from = world.nodes.find((node) => node.id === current);
    if (!from) continue;
    for (const edge of world.edges) {
      if (edge.source !== current) continue;
      if (!portWalksThrough(edge.source_port, typeOf(current))) continue;
      if (reached.has(edge.target)) continue;
      reached.add(edge.target);
      queue.push(edge.target);
    }
  }
  return reached;
}

/**
 * Whether the walk reaches this node, or `null` when the question cannot be answered.
 *
 * `null` is a real answer, not a fallback: a graph with no trigger has no walk, and a
 * function that answered "no" for it would grey out *Run from here* on every card of a
 * rule the author is halfway through writing. `startability` reads `null` as "offer it"
 * — the press goes to the server either way, and the server is the authority.
 */
export function reachedByTrigger(
  world: WalkWorld,
  nodeId: string,
): boolean | null {
  if (!world.nodes.some((node) => node.type.startsWith("trigger."))) return null;
  return reachedNodes(world).has(nodeId);
}

/** True when no connection leaves this node — it is the end of its own path. */
export function isLastOnPath(world: WalkWorld, nodeId: string): boolean {
  return !world.edges.some((edge) => edge.source === nodeId);
}