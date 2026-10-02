/**
 * The builder's undo/redo history (REQ-004 slice 2).
 *
 * The builder's contract is that the *graph* is what is edited — nodes and edges — and that
 * every semantic change is undoable while a layout change is not. That second half is the
 * reason this is a separate module rather than three more `useState` calls: pan, zoom and a
 * node drag that ends where it started must never enter the history, or the undo button
 * spends a press on something nobody perceives as a change.
 *
 * Three rules, each of which is a way an undo stack usually lies:
 *
 * * **A history entry is a whole graph, not a delta.** A delta stack has to replay every
 *   earlier change to reconstruct the state you are undoing *to*, and the first bug in a
 *   delta operation silently corrupts everything after it. Snapshots make undo O(1) and make
 *   "what does one press return" answerable by reading the entry.
 * * **Two edits inside the coalesce window are one entry.** Dragging a node fires a `move` per
 *   pointer event; recording each one would turn one gesture into forty presses of undo. The
 *   window is keyed on a *label* the caller supplies, so a drag coalesces and a re-type of the
 *   same field does not silently merge two unrelated intentions.
 * * **Undo past the beginning is a no-op, and redo is discarded by a fresh edit.** A redo
 *   stack that survives a new edit is a time machine into a future that no longer exists.
 */

/** How long two changes with the same coalesce key are treated as one gesture, in ms. */
export const COALESCE_MS = 600;

/** How many entries the history keeps. Deep enough for a session, shallow enough for a phone. */
export const HISTORY_LIMIT = 100;

/** One editable snapshot. */
export interface HistorySnapshot {
  nodes: unknown[];
  edges: unknown[];
}

/** A single undoable step. */
export interface HistoryEntry {
  /** What the change was called; two changes sharing a key may coalesce into one. */
  key: string;
  /** When the entry was created, in epoch milliseconds. */
  at: number;
  /** The graph *before* the step, which is what a press of undo restores. */
  before: HistorySnapshot;
  /** The graph *after* the step, which is what a press of redo re-applies. */
  after: HistorySnapshot;
}

/** The mutable history the builder keeps in a ref. */
export interface History {
  entries: HistoryEntry[];
  /** Index of the entry `entries[index]` describes, or -1 when there is nothing to undo. */
  cursor: number;
}

export function emptyHistory(): History {
  return { entries: [], cursor: -1 };
}

/**
 * A defensive copy: the caller keeps mutating its own objects, the history must not see that.
 *
 * The copy is two levels deep on purpose. A shallow `{...node}` shares `position` with the
 * live node, so a drag that continues after the snapshot was taken mutates the history's idea
 * of "before" as well — and undo then restores a position the user has already moved on from,
 * which reads as an undo that half-works. `params` is copied for the same reason: the
 * inspector writes into it in place.
 */
export function snapshotOf(nodes: unknown[], edges: unknown[]): HistorySnapshot {
  const copyNode = (node: unknown): unknown => {
    const record_ = node as Record<string, unknown>;
    const position = record_.position as { x?: number; y?: number } | undefined;
    const params = record_.params;
    return {
      ...record_,
      ...(position ? { position: { ...position } } : {}),
      ...(params && typeof params === "object" ? { params: { ...(params as Record<string, unknown>) } } : {}),
    };
  };
  return {
    nodes: nodes.map(copyNode),
    edges: edges.map((edge) => ({ ...(edge as Record<string, unknown>) })),
  };
}

function sameSnapshot(a: HistorySnapshot, b: HistorySnapshot): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/**
 * Record a change.
 *
 * `before` is the graph as it was before the caller applied the change and `after` the graph
 * as it is now — the caller supplies both because it is the only party that knows what the
 * intermediate pointer-move states looked like.
 *
 * Returns the history to store, or the *same object* when the change is not worth an entry
 * (a drag that returned to where it started, a write that changed nothing), so the caller can
 * assign it unconditionally.
 */
export function record(
  history: History,
  entry: { key: string; before: HistorySnapshot; after: HistorySnapshot; now?: number },
): History {
  const now = entry.now ?? Date.now();

  // A change that did not change the graph is not a change. Without this, clicking a node and
  // releasing it without moving adds an entry and one press of undo appears to do nothing.
  if (sameSnapshot(entry.before, entry.after)) {
    return history;
  }

  const head = history.entries[history.cursor];
  const coalescable =
    head !== undefined &&
    head.key === entry.key &&
    now - head.at <= COALESCE_MS;

  if (coalescable) {
    // The gesture is still the same one: extend the existing entry rather than adding another,
    // and keep the *original* `before`, because that is the state undo must return to.
    const merged: HistoryEntry = {
      key: head.key,
      at: now,
      before: head.before,
      after: entry.after,
    };
    const entries = history.entries.slice(0, history.cursor);
    entries.push(merged);
    return { entries: entries.slice(-HISTORY_LIMIT), cursor: Math.min(entries.length, HISTORY_LIMIT) - 1 };
  }

  // Anything after the cursor is a future that a new edit has just cancelled.
  const entries = history.entries.slice(0, history.cursor + 1);
  entries.push({ key: entry.key, at: now, before: entry.before, after: entry.after });

  if (entries.length > HISTORY_LIMIT) {
    const trimmed = entries.slice(entries.length - HISTORY_LIMIT);
    return { entries: trimmed, cursor: trimmed.length - 1 };
  }
  return { entries, cursor: entries.length - 1 };
}

/** The graph a press of undo should restore, or `null` when there is nothing to undo. */
export function undoTarget(history: History): HistorySnapshot | null {
  if (history.cursor < 0) {
    return null;
  }
  return history.entries[history.cursor].before;
}

/** The graph a press of redo should restore, or `null` when there is nothing to redo. */
export function redoTarget(history: History): HistorySnapshot | null {
  if (history.cursor >= history.entries.length - 1) {
    return null;
  }
  return history.entries[history.cursor + 1].after;
}

/** A press of undo: the cursor moves back, and the caller restores `undoTarget`. */
export function undo(history: History): History {
  if (history.cursor < 0) {
    return history;
  }
  return { entries: history.entries, cursor: history.cursor - 1 };
}

/** A press of redo: the cursor moves forward. */
export function redo(history: History): History {
  if (history.cursor >= history.entries.length - 1) {
    return history;
  }
  return { entries: history.entries, cursor: history.cursor + 1 };
}

/**
 * Re-key a coalescing group by time rather than by call, for the caller that needs to end a
 * gesture explicitly (a drag that ended). Without it, a node dragged for ten seconds and then
 * released, followed by a re-drag of the same node five seconds later, coalesces into one
 * entry — so one undo removes both moves and the second one cannot be redone on its own.
 */
export function sealGroup(history: History, now: number): History {
  const head = history.entries[history.cursor];
  if (head === undefined) {
    return history;
  }
  if (now - head.at < COALESCE_MS) {
    const entries = history.entries.slice();
    entries[history.cursor] = { ...head, at: now - COALESCE_MS - 1 };
    return { entries, cursor: history.cursor };
  }
  return history;
}

/**
 * What the toolbar's undo/redo buttons are allowed to claim.
 *
 * A disabled button is a claim too, and the commonest version of this bug is a toolbar that
 * enables undo whenever `entries.length > 0` — which stays true after the user has undone
 * everything, so the button is offered for an operation that cannot happen.
 */
export function capabilities(history: History): { canUndo: boolean; canRedo: boolean } {
  return {
    canUndo: history.cursor >= 0,
    canRedo: history.cursor < history.entries.length - 1,
  };
}
