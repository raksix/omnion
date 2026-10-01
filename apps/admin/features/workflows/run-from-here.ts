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
 *
 * ## The one question this module may not answer for itself
 *
 * `canStart` is a promise the **server** keeps or breaks. The server decides by walking the
 * graph from the trigger (`plan_from_node` → `project_walk`), and this function has no
 * walk — it is handed the answer to the *easy* question, "does any connection leave this
 * node", because that is all a card can see.
 *
 * Those two disagree, and the disagreement used to be invisible because the server's
 * refusals read like explanations of something else:
 *
 * * **an unconnected node.** The canvas cannot know whether the trigger can reach it, so
 *   it answered "yes" — and every node in this builder is dropped *un-wired first* and
 *   connected second, so this is not an edge case, it is the editing state itself. The
 *   button was live and `POST /run-from-node` refused it every time.
 * * **a node whose connections all leave on a port that ends the run.** `hasOutgoing`
 *   counts *any* connection; the engine walks only the ports that do not stop it. A
 *   condition wired solely on `false` therefore looked connected and was not reachable.
 *
 * Both are now refused here, from facts the client actually has (`reachedByTrigger` is
 * the walk's own verdict, shipped with the palette), so the control says why instead of
 * failing on press. **The client decides what to offer; the response decides what
 * happened.** A button that is offered and then refused is still this feature's worst
 * outcome, and it now needs a positive reason on both sides to exist.
 */

/** What a node contributes to a run, as the canvas needs to know it. */
export interface StartabilityNode {
  /** The node's id. */
  id: string;
  /** The node type's key, e.g. `action`, `end`, `trigger.manual`, `note`. */
  type: string;
  /** Whether the node type contributes a step of its own. */
  inert: boolean;
  /**
   * Whether the engine's walk from the trigger reaches this node.
   *
   * `null` means *not known yet* — the canvas has no projection to hand the inspector
   * (an unsaved graph, or a load that failed) — and `null` answers **yes**. Refusing to
   * offer the control on "we could not tell" would grey out the button on every freshly
   * loaded rule, which is the same teaching-a-user-to-click-anyway failure as the
   * opposite. The press still goes to the server, which is the authority either way.
   */
  reachedByTrigger?: boolean | null;
}

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
  // **The refusal the canvas can make and used to be unable to.** A node the trigger
  // cannot reach is refused by `plan_from_node` with `unknown_node`, on every press,
  // forever. The sentence has to name the fix rather than restate the symptom, so it
  // points at the connection instead of at the run.
  if (node.reachedByTrigger === false) {
    return {
      canStart: false,
      reason:
        "Nothing reaches this node from the trigger, so the engine has no path through it " +
        "here — connect it into the graph first.",
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
