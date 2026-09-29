/**
 * Whether a node offers *Retry this node*, and what the press would say.
 *
 * The companion to `run-from-here.ts`, and the client half of REQ-004 slice 3's criterion 3:
 * *"Retry this node" re-runs only that node without duplicating earlier side effects.*
 *
 * ## Why this is not the same question as "Run from here"
 *
 * The two buttons sit on the same card and read like the same question — "may I run from
 * here?" — and they have opposite answers on the nodes an operator clicks most. A node
 * that already **succeeded** is the best place to start a new run and the worst place to
 * retry one: there is no failure to try again. Answering "yes" because *Run from here* said
 * yes would put a live control on every green card, and the server's answer is a 400 with a
 * sentence — a button that only ever fails is worse than no button, because the operator
 * learns to distrust the whole canvas.
 *
 * ## Why the run's state comes first
 *
 * A run that is still going, or that somebody cancelled, is refused before the node is even
 * looked at. The order is the server's (`retry_node.rs`) and it is copied rather than
 * re-derived: a client that agreed with the server about *which* refusals exist but not
 * about their *order* would show a live button during a live run, and the click would
 * answer `409` while the screen said the node was fine.
 *
 * ## Why a node the run never reached is its own answer
 *
 * A trigger, a note, and any node past where a `stop` policy closed the run all have no
 * step. "Nothing to retry" would be true and useless for them — the node did not fail, it
 * was never part of the run — and the two sentences are the only thing that tells an
 * operator which of the two they are looking at.
 */

/** A run's status, as far as the canvas needs it. */
export type RunStatus =
  | "pending"
  | "running"
  | "completed"
  | "failed"
  | "cancelled";

/** What the canvas knows about one node's part in the last run. */
export interface RetryNodeFacts {
  /** The run's status, or `null` when no run has been read. */
  runStatus: RunStatus | null;
  /** The node's own status in that run, or `null` when it has no step. */
  nodeStatus: string | null;
}

/** The answer for one node. */
export interface RetryAnswer {
  /** Whether the button is offered at all. */
  canRetry: boolean;
  /** Why not, when it is not. Shown as the tooltip and the disabled reason. */
  reason: string | null;
  /**
   * What kind of refusal this is, so a caller can tell "the run is busy" (which will pass)
   * from "this node has nothing to retry" (which never will). Mirrors the server's codes.
   */
  code:
    | "run_still_running"
    | "run_cancelled"
    | "node_not_in_run"
    | "nothing_to_retry"
    | null;
}

/** Statuses a step can be retried *from*. Mirrors `RETRYABLE` in `retry_node.rs`. */
const RETRYABLE: ReadonlySet<string> = new Set(["failed", "cancelled", "ignored"]);

/**
 * Whether *Retry this node* is offered on this node, and why not when it is not.
 *
 * The refusals are checked in the server's order, and the order is the point:
 *
 * 1. **the run's state**, because a step read from a live run describes a run that is
 *    about to change underneath the click;
 * 2. **the node is in the run**, because "took no part in this run" and "did not fail" are
 *    different sentences about different objects;
 * 3. **the node's status**, because only a step that actually failed can be tried again.
 *
 * A **succeeded** node is refused at step 3 with `nothing_to_retry` — the honest answer for
 * the button's own semantics, distinct from `run_from_here`'s "yes, you may start here",
 * and the two disagreeing on the same card is correct rather than inconsistent.
 */
export function retryAnswer(nodeLabel: string, facts: RetryNodeFacts): RetryAnswer {
  if (facts.runStatus === "running") {
    return {
      canRetry: false,
      code: "run_still_running",
      reason:
        "This run is still going. Wait for it to finish, or cancel it first.",
    };
  }
  if (facts.runStatus === "cancelled") {
    return {
      canRetry: false,
      code: "run_cancelled",
      reason:
        "This run was cancelled on purpose, so it cannot be retried. Start a new run instead.",
    };
  }
  if (facts.nodeStatus === null) {
    return {
      canRetry: false,
      code: "node_not_in_run",
      reason: `${nodeLabel} took no part in the last run, so there is nothing to retry on it.`,
    };
  }
  if (!RETRYABLE.has(facts.nodeStatus)) {
    return {
      canRetry: false,
      code: "nothing_to_retry",
      reason: `${nodeLabel} did not fail in the last run, so there is nothing to try again.`,
    };
  }
  return { canRetry: true, code: null, reason: null };
}

/**
 * The sentence a finished retry reports.
 *
 * It names the node and says how much of the run moved, because "retried" is true of every
 * press this button can produce and says nothing about the *scope* — which is the entire
 * distinction this control exists to make against a tail re-run. An operator who cannot
 * see "only this node" from the toast has no way to tell the two apart after the fact.
 */
export function retryMessage(
  nodeLabel: string,
  stepNo: number,
  requeued: number,
): string {
  if (requeued !== 1) {
    // The server answers 1 for a node retry and the walk asserts it, so a count other than
    // one is a tail retry wearing this button's name — and it has already re-sent whatever
    // came before. Saying so loudly is better than reporting a success that hid it.
    return `WARNING: retrying ${nodeLabel} re-queued ${requeued} steps, not one. Open the run to see what repeated.`;
  }
  return `Retried ${nodeLabel} (step ${stepNo}) on its own. Nothing else in the run was re-run.`;
}
