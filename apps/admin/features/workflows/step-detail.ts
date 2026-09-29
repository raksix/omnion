/**
 * Which step a clicked node opens, and how that step reads.
 *
 * Criterion (REQ-004 slice 3, criterion 2, second half): *"After a run each node shows its
 * status pill, and clicking the node opens that step's inputs and output."* The pill is
 * `node-status.ts`; this is the click.
 *
 * ## Why this is a pure function and not a `find` in the inspector
 *
 * The click has three answers and only one of them is obvious, and the two non-obvious ones
 * are exactly the ones a `steps.find(s => s.node_id === id)` throws away:
 *
 * * **one node can be two steps.** A node with a `success` and an `error` output projects
 *   to two rows, and after a run one is `succeeded` while the other is `skipped`. Opening
 *   whichever the array happened to return first shows the branch that ran and hides the
 *   branch that did not — which is the whole reason the node is painted *diverged* in the
 *   first place. So the answer is a list, and the inspector shows every step behind the
 *   node, in run order.
 * * **a node can have no step at all.** A trigger, a note, and any node the run never
 *   reached. `find` returns `undefined` and the inspector has to guess what an `undefined`
 *   means; here it is a named state (`run: "absent"`) that the panel can say out loud,
 *   because "this node took no part in the last run" and "the run has not been read yet"
 *   are different sentences and a blank panel speaks neither.
 * * **the run may not be loaded.** A rule that has never run has no status layer at all,
 *   and the panel must say so instead of rendering an empty JSON viewer that looks like a
 *   step with no data.
 *
 * ## Why the payload is summarised rather than dumped
 *
 * A step's `params` and `output` are arbitrary JSON written by whatever the node does. The
 * first thing an operator wants from a payload is its *shape* — was it an object, an array,
 * a bare value, did it fail to parse — and `JSON.stringify` on a circular or `undefined`
 * value throws inside a render, taking the panel with it. So every payload is classified
 * first and formatted second, and the two non-object shapes are described in words rather
 * than rendered as a lone `42` under a heading.
 */

import type { RunStep } from "./node-status.ts";

/** A JSON value as the inspector can show it, and can describe. */
export type PayloadShape =
  /** The key was absent, or explicitly `null`. */
  | "absent"
  /** An object — the common case, rendered as a key list. */
  | "object"
  /** An array — rendered as a numbered list. */
  | "array"
  /** A bare JSON scalar, described in words. */
  | "scalar"
  /** Present but not a value this build can describe. */
  | "unrenderable";

/** One step's inputs or output, described rather than dumped. */
export interface DescribedPayload {
  shape: PayloadShape;
  /** Object entries, in insertion order. Empty for every other shape. */
  entries: Array<{ key: string; value: string }>;
  /** Array items, in order. Empty for every other shape. */
  items: string[];
  /** A one-line description for the shapes that are not lists. */
  headline: string;
  /** Whether there is anything at all to show. */
  hasContent: boolean;
}

/** Nothing, and the panel says so rather than showing an empty viewer. */
const NOTHING: DescribedPayload = {
  shape: "absent",
  entries: [],
  items: [],
  headline: "",
  hasContent: false,
};

/**
 * One JSON value at a depth cap, so a step that returns 10 000 records cannot lock the tab.
 *
 * The cap is on the *string length*, not on the structure: a deeply nested object renders
 * through the same `depth` counter, so neither shape can run away.
 */
const MAX_STRING = 2000;
const MAX_DEPTH = 3;

/**
 * Render a JSON value as the short line the inspector shows beside its key.
 *
 * A value that is longer than `MAX_STRING` is truncated with a visible marker rather than
 * silently cut: a reader who sees `…` knows there is more, and a reader who does not sees
 * a complete-looking value that is a lie.
 */
function oneLine(value: unknown, depth = 0): string {
  if (value === null) return "null";
  if (value === undefined) return "undefined";
  const type = typeof value;
  if (type === "string") {
    const text = value as string;
    if (text.length <= MAX_STRING) return JSON.stringify(text);
    return `${JSON.stringify(text.slice(0, MAX_STRING))}… (${text.length} characters)`;
  }
  if (type === "number" || type === "boolean" || type === "bigint") return String(value);
  if (depth >= MAX_DEPTH) {
    const open = Array.isArray(value) ? "[" : "{";
    return `${open}…`;
  }
  if (Array.isArray(value)) {
    return `[${value.length} item${value.length === 1 ? "" : "s"}]`;
  }
  const entries = Object.entries(value as Record<string, unknown>);
  return `{${entries.length} key${entries.length === 1 ? "" : "s"}}`;
}

/**
 * Describe a payload, or say it is not there.
 *
 * The distinction between `absent` and `empty` is load-bearing: a step whose output is `{}`
 * ran and produced nothing, and a step whose output is missing never produced anything.
 * They read the same in a viewer that only checks for emptiness.
 */
export function describePayload(value: unknown): DescribedPayload {
  if (value === undefined || value === null) return NOTHING;

  if (Array.isArray(value)) {
    if (value.length === 0) {
      return {
        shape: "array",
        entries: [],
        items: [],
        headline: "An empty list.",
        hasContent: true,
      };
    }
    return {
      shape: "array",
      entries: [],
      items: value.map((item) => oneLine(item)),
      headline: `${value.length} item${value.length === 1 ? "" : "s"}`,
      hasContent: true,
    };
  }

  if (typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>).map(([key, entry]) => ({
      key,
      value: oneLine(entry),
    }));
    if (entries.length === 0) {
      return {
        shape: "object",
        entries: [],
        items: [],
        headline: "An empty object — the step ran and returned nothing.",
        hasContent: true,
      };
    }
    return {
      shape: "object",
      entries,
      items: [],
      headline: `${entries.length} key${entries.length === 1 ? "" : "s"}`,
      hasContent: true,
    };
  }

  // A bare scalar: `true`, `42`, `"done"`. Described, not rendered under a heading.
  const rendered = oneLine(value);
  const type = value === null ? "null" : typeof value;
  return {
    shape: "scalar",
    entries: [],
    items: [],
    headline: `${rendered} — a single ${type} value.`,
    hasContent: true,
  };
}

/** A step as the inspector needs it: its status, its inputs, its output, its failure. */
export interface RunStepDetail {
  step: RunStep;
  inputs: DescribedPayload;
  output: DescribedPayload;
}

/** What a click on a node found, and what it could not find. */
export type RunDetail =
  /** No run has been read for this rule yet. */
  | { kind: "no-run" }
  /** The node took no part in the last run — a trigger, a note, a node the run never reached. */
  | { kind: "node-absent"; nodeId: string }
  /** The node was in the run. `steps` has at least one entry. */
  | { kind: "node"; nodeId: string; steps: RunStepDetail[]; diverged: boolean };

/** The steps of a run that came from one node, in run order. */
function stepsForNode(steps: RunStep[], nodeId: string): RunStep[] {
  return steps
    .filter((step) => step.node_id === nodeId)
    .sort((left, right) => left.step_no - right.step_no);
}

/**
 * What clicking a node opens.
 *
 * `runSteps` is the *whole* run's step list, not the index: a map keyed by node is the shape
 * that loses the second branch, and this function's only job is to be the one place that
 * answers "which steps is this node", so the loss cannot happen twice.
 */
export function runDetailForNode(
  nodeId: string,
  runSteps: RunStep[] | null,
): RunDetail {
  // `null` is "no run has been read" and `[]` is "the run had no steps at all". A rule
  // whose first run is still `pending` has steps, so the two are genuinely different and
  // the empty-array case is not folded into the missing one.
  if (runSteps === null) return { kind: "no-run" };

  const steps = stepsForNode(runSteps, nodeId);
  if (steps.length === 0) return { kind: "node-absent", nodeId };

  const statuses = new Set(steps.map((step) => step.status));
  return {
    kind: "node",
    nodeId,
    diverged: statuses.size > 1,
    steps: steps.map((step) => ({
      step,
      inputs: describePayload(step.params),
      output: describePayload(step.output),
    })),
  };
}

/**
 * The heading for the trace panel, naming what it is showing.
 *
 * A panel whose title does not change with its content teaches the reader to ignore the
 * title, and "Step 2 of 4" is the difference between a trace and a mystery.
 */
export function traceHeading(detail: RunDetail): string {
  if (detail.kind === "no-run") return "Last run";
  if (detail.kind === "node-absent") return "This node took no part in the last run";
  const first = detail.steps[0].step;
  const last = detail.steps[detail.steps.length - 1].step;
  if (detail.steps.length === 1) return `Step ${first.step_no}`;
  return `Steps ${first.step_no} and ${last.step_no} — this node branches`;
}

/**
 * The sentence under the heading.
 *
 * The `diverged` case is the one that needs a sentence: two steps behind one node looks
 * like a bug until you know one of them was the branch not taken, and the node's own
 * `skip_reason` is the run's word for which.
 */
export function traceSubheading(detail: RunDetail): string {
  if (detail.kind === "no-run") {
    return "Run this rule and the steps it took will open here.";
  }
  if (detail.kind === "node-absent") {
    return "A trigger, a note, or a node the run never reached. Its inputs are below.";
  }
  const unrun = detail.steps.find((entry) => entry.step.status === "skipped");
  if (!unrun) {
    return `${detail.steps.length} step${detail.steps.length === 1 ? "" : "s"} behind this node.`;
  }
  return unrun.step.skip_reason ?? "One branch ran, the other did not.";
}
