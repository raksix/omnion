/**
 * Whether a node can be *Run from here*, and what the press would say.
 *
 * The button is on every node card, so the question "can this one be started?" has to be
 * answered for the whole palette without a round trip — and answered *honestly*, because
 * the one node that cannot be started is the one an operator is most likely to try: the
 * end of the graph. "Run from here" on the end node is a run that settles `completed`
 * having done nothing, which is the most misleading answer this screen can give.
 *
 * The rules mirror the server's plan (`crates/workflows/src/run_from.rs`) and are written
 * as a pure function for the same reason `conflict.ts` and `save-arbitration.ts` are: a
 * rule about whether a control may be pressed cannot be tested inside a React callback,
 * and the two ways it goes wrong — a live button that always fails, and a dead button on
 * a node that would have worked — are both invisible until somebody clicks them.
 *
 * The server is still the authority. This decides what to *offer*; the response decides
 * what happened. A disagreement between the two shows up as an error message naming the
 * node, never as a silent wrong run.
 */

/** What a node contributes to a run, as the canvas needs to know it. */
export interface StartabilityNode {
  /** The node's id. */
  id: string;
  /** The node type's key, e.g. `action`, `end`, `trigger.manual`, `note`. */
  type: string;
  /** Whether the node type contributes a step of its own. */
  inert: boolean;
}

/** The answer for one node. */
export interface Startability {
  /** Whether the button is offered at all. */
  canStart: boolean;
  /** Why not, when it is not. Shown as the button's tooltip and as its disabled reason. */
  reason: string | null;
}

/** Node types that are the start of a run rather than a step within one. */
function isTrigger(type: string): boolean {
  return type.startsWith("trigger.");
}

/** The node that ends a run. A `stop` step has nothing after it. */
function isEnd(type: string): boolean {
  return type === "end";
}

/**
 * Whether *Run from here* is offered on this node, and why not when it is not.
 *
 * Three refusals, and they are refusals for three different reasons, which is why this
 * is not `type === "end"`:
 *
 * * **a trigger** *can* be started from. An operator re-running a rule from the top is a
 *   real thing to want, and a canvas that greys out its own trigger cannot express it.
 * * **an inert node** (a note) contributes no step, so "start here" means "start after
 *   what came before". Offering it is honest; the server resolves it to the next step.
 * * **the end node** cannot. It is a `stop` step, and a run started at a stop has nothing
 *   after it to do.
 *
 * A node with no runnable step *after* it is the end case regardless of which node that
 * is, so the caller passes whether the node is the last one on the path. Without that, a
 * future node type with no ports would offer a button the server refuses, and the
 * operator would learn it by pressing it.
 */
export function startability(
  node: StartabilityNode,
  isLastOnPath: boolean,
): Startability {
  if (isEnd(node.type)) {
    return {
      canStart: false,
      reason: "The end of the graph has nothing after it to run — start one node before.",
    };
  }
  if (isLastOnPath && !isTrigger(node.type) && node.inert) {
    return {
      canStart: false,
      reason:
        "Nothing follows this node, so a run started here would have no step to run. " +
        "Start one node before.",
    };
  }
  return { canStart: true, reason: null };
}

/**
 * The sentence a started run reports, for the toast under the button.
 *
 * It names the node on purpose. "Run started" is true of every run the button can produce
 * and says nothing about which one this was; a run whose prefix was skipped is a
 * different fact from a whole run, and the operator needs to be able to tell them apart
 * without opening the trace.
 */
export function startMessage(
  nodeLabel: string,
  skipped: Array<{ name: string }>,
): string {
  if (skipped.length === 0) {
    return `Run started at ${nodeLabel}.`;
  }
  const names = skipped.map((entry) => entry.name);
  const last = names[names.length - 1];
  const others = names.slice(0, -1);
  const list =
    others.length > 0 ? `${others.join(", ")} and ${last}` : (last ?? "");
  return `Run started at ${nodeLabel}. ${list} ${skipped.length === 1 ? "was" : "were"} skipped.`;
}
