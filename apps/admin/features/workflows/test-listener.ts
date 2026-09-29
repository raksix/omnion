/**
 * *Listen for a real event* — the builder's panel rules (REQ-004 slice 3, criterion 5).
 *
 * Everything here is a pure function of the canvas selection and the server's answer, for
 * the same reason `conflict.ts` and `node-status.ts` are: a rule about whether a control may
 * be pressed cannot be tested inside a React callback, and a rule about whether it *should*
 * be pressed should not be a side effect of rendering.
 *
 * The two decisions the toolbar needs are:
 *
 *  1. **may this node be listened to?** — a node that is not a trigger has no event of its
 *     own, and the server refuses it. The panel has to agree *before* the press, because a
 *     control that is enabled and then answers 400 is a dead control wearing an enabled
 *     attribute, and the sentence the server sends is not a substitute for knowing which
 *     node to select.
 *
 *  2. **what does the panel say about the listeners right now?** — armed / captured /
 *     expired, and how long is left. The countdown is derived from the server's
 *     `expires_at` rather than from a local tick counter, because a local counter drifts the
 *     moment the tab is backgrounded and would then say "12 minutes left" about a listener
 *     that expired four minutes ago.
 */

/** A node as the canvas holds it, reduced to what this module needs. */
export interface ListenerNode {
  id: string;
  type: string;
  label: string;
}

/** What the server says about one listener. */
export interface ListenerRow {
  id: string;
  node_id: string;
  event_name: string;
  status: "armed" | "captured" | "expired";
  expires_at: string;
  expires_in_seconds: number;
  payload: Record<string, unknown> | null;
  payload_text?: string;
}

/** Why a node cannot be listened to, as a named answer rather than a boolean. */
export type ListenRefusal = "no_node" | "not_a_trigger" | "already_armed";

/** Whether a control may be pressed, and — when it may not — what to say instead. */
export interface Startability {
  /** `true` when the press is allowed. */
  canListen: boolean;
  /** The node the press would arm for, when it is allowed. */
  nodeId: string | null;
  /** The code a client should branch on, when it is not allowed. */
  refusal: ListenRefusal | null;
  /** The sentence shown in place of the button. */
  message: string | null;
}

/**
 * Whether *Listen* may be pressed for the current selection.
 *
 * The refusal order is load-bearing and is the one thing here a reader could reorder by
 * accident:
 *
 *  * **`no_node` first.** A panel that reported "select a trigger node" while nothing is
 *    selected tells the author to go and do something they have already done.
 *  * **`not_a_trigger` before `already_armed`.** A node that is not a trigger can never be
 *    listened to, so an armed listener on a *different* node says nothing about this one —
 *    and the canvas supports two armed nodes at once, which is the whole point of a
 *    node-scoped listener.
 */
export function startability(
  selected: readonly ListenerNode[] | null | undefined,
  listeners: readonly ListenerRow[] | null | undefined,
  now: number = Date.now(),
): Startability {
  const node = selected?.[0] ?? null;

  if (!node) {
    return {
      canListen: false,
      nodeId: null,
      refusal: "no_node",
      message: "Select the node whose event you want to see, then listen for it.",
    };
  }

  if (!isTriggerType(node.type)) {
    return {
      canListen: false,
      nodeId: node.id,
      refusal: "not_a_trigger",
      message: `“${node.label}” is a ${describeType(node.type)} node, and a listener waits for the event that starts a run. Select a trigger node.`,
    };
  }

  const armed = (listeners ?? []).some(
    (row) => row.node_id === node.id && row.status === "armed" && secondsLeft(row, now) > 0,
  );
  if (armed) {
    return {
      canListen: false,
      nodeId: node.id,
      refusal: "already_armed",
      message: "This node is already listening. Wait for its event, or select another node.",
    };
  }

  return { canListen: true, nodeId: node.id, refusal: null, message: null };
}

/** The registry's own rule, read off the type key rather than a second list. */
export function isTriggerType(type: string): boolean {
  return type.startsWith("trigger.");
}

/** The registry's label for a type key, with the `trigger.` prefix stripped. */
export function describeType(type: string): string {
  return type.includes(".") ? type.split(".").slice(1).join(".") : type;
}

/**
 * Seconds left on a listener, from its **expiry instant** rather than from the server's
 * countdown.
 *
 * The server sends `expires_in_seconds` because it is authoritative about the moment it
 * answered, and this function recomputes from `expires_at` because the panel ticks once a
 * second afterwards and a stored number would never move. Taking the smaller of the two is
 * what keeps a slow tab from showing a bar past the end: a server that answers
 * `expires_in_seconds: 900` for a row that expired ten minutes ago is a stale read, and the
 * honest answer is "nothing left", not "fifteen minutes".
 */
export function secondsLeft(row: ListenerRow, now: number = Date.now()): number {
  const expiry = Date.parse(row.expires_at);
  if (Number.isNaN(expiry)) return 0;
  const fromInstant = Math.floor((expiry - now) / 1000);
  return Math.max(0, Math.min(fromInstant, row.expires_in_seconds));
}

/** The panel's sentence for the whole listener area, in the voice of the state. */
export function summarySentence(
  listeners: readonly ListenerRow[] | null | undefined,
  now: number = Date.now(),
): string {
  const rows = listeners ?? [];
  const armed = rows.filter((row) => row.status === "armed" && secondsLeft(row, now) > 0);
  if (armed.length === 0) {
    const captured = rows.find((row) => row.status === "captured");
    if (captured) return `Captured ${captured.event_name} — the payload is below.`;
    const expired = rows.find((row) => row.status === "expired");
    if (expired) return `The listener for ${expired.event_name} expired with no event.`;
    return "Not listening. Pick a trigger node and press Listen for a real event.";
  }

  const first = armed[0];
  const left = secondsLeft(first, now);
  const when = formatDuration(left);
  const more = armed.length > 1 ? ` and ${armed.length - 1} more` : "";
  return armed.length > 1
    ? `Listening for ${first.event_name}${more} — ${when} left.`
    : `Listening for ${first.event_name} — ${when} left.`;
}

/**
 * A duration in the words a countdown wants.
 *
 * "14m 59s" is what a stopwatch says; "15 minutes" is what a person waiting for an event
 * reads. Under a minute the seconds are the interesting part and the minutes are noise, so
 * the two are not the same string.
 */
export function formatDuration(seconds: number): string {
  if (seconds <= 0) return "no time left";
  if (seconds < 60) return `${seconds}s left`;
  const minutes = Math.floor(seconds / 60);
  const rest = seconds % 60;
  if (minutes < 60) {
    return rest === 0 ? `${minutes} minutes left` : `${minutes}m ${rest}s left`;
  }
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${minutes % 60}m left`;
}

/**
 * The row the panel should show the payload from, and why not an older one.
 *
 * The read answers with the newest capture already, but the panel also holds rows it
 * fetched before, and `listeners[0]` is the wrong answer whenever a *newer expired* row is
 * on top — which is the normal state of a rule whose listener timed out. So this prefers a
 * capture and only falls back to an armed row, and never to an expired one.
 */
export function payloadSource(
  listeners: readonly ListenerRow[] | null | undefined,
  now: number = Date.now(),
): ListenerRow | null {
  const rows = listeners ?? [];
  return (
    rows.find((row) => row.status === "captured") ??
    rows.find((row) => row.status === "armed" && secondsLeft(row, now) > 0) ??
    null
  );
}
