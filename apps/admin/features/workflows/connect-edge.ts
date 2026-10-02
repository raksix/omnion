/**
 * What drawing a connection is allowed to do (REQ-004 slice 2).
 *
 * The gesture lives in the canvas, but the rules do not: "which ports may leave this card"
 * is a question about the *node type*, while the gesture hands over a *node id*. Keeping the
 * answer in one exported function is what stops the two from being confused again — the bug
 * this module was written for was a lookup of a node id in the type map, which never matches,
 * so every connection refused with "has no output ports" and the refusal looked correct.
 *
 * Two properties are deliberate:
 *
 * * **A refusal names itself.** Each failure returns a sentence built from the author's own
 *   vocabulary — the node's label, the ports it actually exports — because a refusal the user
 *   cannot read is indistinguishable from a broken canvas.
 * * **The rule runs before any state changes.** A refused gesture must leave the graph exactly
 *   as it was, so the caller never has to undo a half-applied connection.
 */

/** One port a node type exposes. */
export interface PortSpec {
  key: string;
  label: string;
}

/** The part of a node type the connection rules need. */
export interface ConnectableType {
  label: string;
  outputs: PortSpec[];
}

/** A node on the canvas: an id and the type whose ports it exposes. */
export interface ConnectableNode {
  id: string;
  type: string;
}

/** An edge already on the canvas. */
export interface ConnectableEdge {
  id: string;
  source: string;
  source_port: string;
  target: string;
}

/** The edge a successful connection produces. */
export type NewEdge = ConnectableEdge;

/** Why a gesture was refused, and what to say about it. */
export type ConnectRefusal =
  | { ok: false; reason: "unknown_source" | "unknown_port" | "no_outputs" | "self" | "taken"; text: string }
  | { ok: true; edge: NewEdge; text: string };

/**
 * The id of the source node's type, or `undefined` when the id is not on the canvas.
 *
 * This is the step that was missing: ports belong to a *type*, the gesture carries an *id*.
 */
export function typeKeyOf(
  nodeId: string,
  nodes: readonly ConnectableNode[],
): string | undefined {
  return nodes.find((candidate) => candidate.id === nodeId)?.type;
}

/**
 * Decide whether a connection may be drawn, and build its edge.
 *
 * `types` is keyed by node *type*; `nodes` maps a canvas id to the type it was placed as.
 */
export function decideConnection(
  source: string,
  sourcePort: string,
  target: string,
  nodes: readonly ConnectableNode[],
  types: ReadonlyMap<string, ConnectableType>,
  edges: readonly ConnectableEdge[],
  nextEdgeId: string,
): ConnectRefusal {
  const typeKey = typeKeyOf(source, nodes);
  const sourceType = typeKey ? types.get(typeKey) : undefined;
  const sourceLabel = sourceType?.label ?? typeKey ?? source;

  if (!sourceType) {
    return { ok: false, reason: "unknown_source", text: `${sourceLabel} is not on the canvas.` };
  }

  const outputs = sourceType.outputs ?? [];
  const port = outputs.find((candidate) => candidate.key === sourcePort);
  if (!port) {
    // The two messages are different bugs to the reader: "there is nothing to leave" is a
    // node whose type ends the run, and "that port does not exist" is a stale gesture.
    return outputs.length > 0
      ? {
          ok: false,
          reason: "unknown_port",
          text: `${sourceLabel} has no “${sourcePort}” port. It exports ${outputs
            .map((candidate) => candidate.key)
            .join(", ")}.`,
        }
      : { ok: false, reason: "no_outputs", text: `${sourceLabel} has no output ports, so nothing can leave it.` };
  }

  // A self-connection is a cycle of length one. The server's validator would catch it, but the
  // user is watching the canvas, and a line drawn back into its own node looks like it worked.
  if (source === target) {
    return { ok: false, reason: "self", text: `${sourceLabel} cannot connect to itself.` };
  }

  if (
    edges.some(
      (edge) => edge.source === source && edge.source_port === sourcePort && edge.target === target,
    )
  ) {
    return {
      ok: false,
      reason: "taken",
      text: `${sourceLabel} · ${port.label} already leads to that node.`,
    };
  }

  const targetKey = typeKeyOf(target, nodes);
  const targetLabel = (targetKey ? types.get(targetKey)?.label : undefined) ?? target;
  return {
    ok: true,
    edge: { id: nextEdgeId, source, source_port: sourcePort, target },
    text: `${sourceLabel} · ${port.label} → ${targetLabel}`,
  };
}
