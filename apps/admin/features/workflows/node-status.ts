/**
 * What a node card shows about the run it took part in.
 *
 * Criterion: *"After a run each node shows its status pill, and clicking the node opens that
 * step's inputs and output."* The first half is this file; the second half is the
 * inspector, which is a different question (which step, not what status).
 *
 * ## Why the mapping is a pure function and not a `switch` in the card
 *
 * Every way of getting this wrong looks the same on screen: a canvas where half the nodes
 * are painted and half are not, and no way to tell which half is lying. So the interesting
 * decisions are all here, where they can be asserted:
 *
 * * **a node with no step paints nothing.** Not "pending", not a grey dot. A `skipped`
 *   prefix is visible on the nodes it covers, and a note is decoration; a pill on either
 *   would be a claim about work that was never scheduled. This is the single most
 *   important rule here — inventing a pill is how a canvas ends up asserting a status the
 *   engine never reported.
 * * **two steps from one node.** A node with a `success` and an `error` output projects to
 *   two steps, and after a run one is `succeeded` while the other is `skipped`. Painting
 *   one arbitrarily is a lie; the node is *diverged* — which is its own pill, its own
 *   colour, and its own reason. The alternative (last write wins, the way a `Map` would
 *   do it) makes the outcome depend on the order the server happened to return rows in.
 * * **a run that is still going** is a status too: `running` is the state an operator
 *   watches, and a canvas that only paints settled runs looks dead mid-run.
 *
 * ## Why the server is the authority
 *
 * This decides what to *paint*; `status` on the row is what happened. They can only
 * disagree if the run being read is not the run of the graph on screen, which is why a
 * run carries `started_from_node` and the pill for a skipped step carries its reason
 * verbatim rather than a summary written here.
 */

/** One step of a run, as the canvas needs it. */
export interface RunStep {
  /** 1-based position in the run. */
  step_no: number;
  /** The graph node it came from, when the rule was started from a graph. */
  node_id?: string | null;
  /** `pending`, `running`, `waiting`, `succeeded`, `failed`, `cancelled` or `skipped`. */
  status: string;
  /** Why a skipped step did not run, in the run's own words. */
  skip_reason?: string | null;
  /** Attempts made, shown on the pill of a step that took more than one. */
  attempts?: number;
}

/** The one thing a card needs in order to paint a pill. */
export interface NodeRunStatus {
  /** The status to paint, or `null` when the node took no part in the run. */
  status: string | null;
  /** `diverged` when one node produced both a settled and an unrun step. */
  shape: "none" | "single" | "diverged";
  /** The steps behind this node, for the inspector to open. */
  stepNos: number[];
  /** The reason a skipped step carries, verbatim from the run. */
  skipReason: string | null;
}

/** A node that took no part in the run: no pill, and the reason is in the caller. */
const ABSENT: NodeRunStatus = {
  status: null,
  shape: "none",
  stepNos: [],
  skipReason: null,
};

/**
 * Index a run's steps by the node each came from.
 *
 * The key is the node id, and the value is a list rather than a step because a node with
 * two outgoing branches projects to two steps. Keying by `step_no` instead and looking the
 * node up per card is the same computation done once per card, and it is the version that
 * silently drops the second branch.
 *
 * Steps with no `node_id` are dropped, not bucketed under an empty key: a rule whose
 * definition predates the builder has no node to attribute them to, and attributing them
 * to whichever node sits at the same index is a guess. The canvas then paints nothing for
 * them, which is the honest reading.
 */
export function indexStepsByNode(steps: RunStep[]): Map<string, RunStep[]> {
  const byNode = new Map<string, RunStep[]>();
  for (const step of steps) {
    const nodeId = step.node_id;
    if (!nodeId) continue;
    const bucket = byNode.get(nodeId);
    if (bucket) bucket.push(step);
    else byNode.set(nodeId, [step]);
  }
  return byNode;
}

/**
 * What to paint on one node's card, given the run it took part in.
 *
 * `steps` is what [`indexStepsByNode`] found for this node — empty when the node was not
 * in the run, which is the case that must paint nothing.
 */
export function nodeRunStatus(steps: RunStep[]): NodeRunStatus {
  if (steps.length === 0) return ABSENT;

  const statuses = steps.map((step) => step.status);
  const settled = new Set(statuses);
  // One node, two steps, one of them unrun: the node is where the run branched and only
  // one side was taken. Painting the settled one is true but incomplete, and painting
  // "failed" when a `skipped` sibling is the real story is false. It gets its own shape.
  const diverged = settled.size > 1 && statuses.includes("skipped");
  if (diverged) {
    return {
      status: "diverged",
      shape: "diverged",
      stepNos: steps.map((step) => step.step_no),
      // The reason the *unrun* side carries is the one an operator needs: the settled side
      // explains itself.
      skipReason: steps.find((step) => step.status === "skipped")?.skip_reason ?? null,
    };
  }

  // Two steps of the same node that agree (`pending` and `running` cannot happen, but a
  // retried step and its successor could) collapse to the least settled of the two: a node
  // whose steps are all `succeeded` is done, and one that is still `running` is running.
  const order = ["running", "waiting", "pending", "failed", "cancelled", "succeeded", "skipped"];
  const status =
    order.find((candidate) => statuses.includes(candidate)) ?? statuses[0] ?? null;

  return {
    status,
    shape: "single",
    stepNos: steps.map((step) => step.step_no),
    skipReason: steps.find((step) => step.status === "skipped")?.skip_reason ?? null,
  };
}

/**
 * The sentence under a pill.
 *
 * A skipped step's reason is the run's own text and is never rewritten here: the criterion
 * asks the trace to say why, and a summary written in the client is a second version of
 * that sentence that can disagree with the one the server wrote. Everything else is a
 * plain noun phrase, because a pill that needs a tooltip to be understood is a pill that
 * was not readable.
 */
export function pillText(status: NodeRunStatus): string | null {
  if (status.status === null) return null;
  if (status.status === "skipped") {
    return status.skipReason ?? "Skipped — the run did not reach it.";
  }
  if (status.status === "diverged") {
    return status.skipReason ?? "One branch ran, the other did not.";
  }
  if (status.status === "succeeded") return "Succeeded";
  if (status.status === "failed") return "Failed";
  if (status.status === "running") return "Running…";
  if (status.status === "waiting") return "Waiting";
  if (status.status === "cancelled") return "Cancelled";
  if (status.status === "pending") return "Queued";
  // An unrecognised status still gets a word rather than nothing: a pill that renders
  // empty is a dead control, and a new engine status reaching an older client is exactly
  // when a reader most needs to be told something.
  return status.status;
}

/**
 * The one-word label on the pill itself.
 *
 * Deliberately not the full sentence: the pill is ~90px wide on a node card, and a
 * truncated sentence is an ellipsis, not information. The sentence lives in the title
 * attribute, which is where a long explanation belongs.
 */
export function pillLabel(status: NodeRunStatus): string | null {
  switch (status.status) {
    case "succeeded":
      return "Done";
    case "failed":
      return "Failed";
    case "running":
      return "Running";
    case "waiting":
      return "Waiting";
    case "cancelled":
      return "Cancelled";
    case "pending":
      return "Queued";
    case "skipped":
      return "Skipped";
    case "diverged":
      return "Diverged";
    default:
      return null;
  }
}
