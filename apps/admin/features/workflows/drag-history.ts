/**
 * A drag is ONE undo step, and it is the one edit whose two ends live in different handlers.
 *
 * ## What was broken
 *
 * Every other edit routes through `commit(key, before, next)` — it hands the history the graph
 * as it was *before* the change, in the same call that applies the change. A drag could not do
 * that, because it does not know it is a drag: `pointerdown` on a card starts a gesture that
 * emits one `pointermove` per frame and finishes at some later `pointerup`. So `commitMove` did
 * the only thing it could do — queue a save — and the position change went into the graph with
 * nothing in `historyRef` to describe it. The Undo button stayed grey, `⌘Z` was a no-op, and
 * the redo stack was never touched.
 *
 * The wrong answer *looks* right, which is why it survived: the arrow-key nudge **is** routed
 * through `commit("nudge", …)`, so a keyboard author's arrow keys undo and a mouse author's drag
 * does not. Both are "move a node". One is a released key and one is a released mouse, and
 * nothing in the product distinguished them.
 *
 * ## Why the "before" has to be captured on the way DOWN
 *
 * The tempting fix is to record on `pointerup` with `before: <something>`. There is no
 * something: by the time the pointer is up, every frame has already written its position, and
 * the only snapshot that describes where the node *was* is one nobody took. Reconstructing it
 * from `position - delta` is the reconstruction that is wrong the moment a drag crosses the
 * `clampCoord` boundary or a `snap()` grid line, and it is wrong silently. So `beginDrag` is
 * called where the pre-drag graph still exists, and it hands back the token that `endDrag`
 * spends.
 *
 * ## Why the gesture must SEAL the previous group
 *
 * `sealGroup` was written for exactly this caller — its own doc comment names "a drag that
 * ended" — and nothing called it, because there was no drag in the history to seal. Once a drag
 * *is* recorded, the window bites: drag a card, let go, drag the same card again three hundred
 * milliseconds later, and `record` sees two entries with the same key inside `COALESCE_MS` and
 * merges them. The merged entry keeps the FIRST `before` and takes the SECOND `after`, so the
 * position between the two drags becomes unreachable by either key. That is a time machine
 * with a hole in it. `endDrag` seals first, so every release ends its own group.
 */
import { COALESCE_MS, record, sealGroup, type History, type HistorySnapshot } from "./builder-history.ts";

/** What `pointerdown` takes and `pointerup` spends. */
export interface DragOrigin {
  /** The graph as it was when the gesture started — what one press of undo restores. */
  before: HistorySnapshot;
  /** The nodes this gesture is allowed to move, sorted. */
  ids: string[];
}

/** The coalescing key for a gesture moving exactly this set of nodes. */
export function dragKey(ids: string[]): string {
  return `move:${[...ids].sort().join(",")}`;
}

/**
 * Open a gesture.
 *
 * `ids` is the set the caller is *permitted* to move, not the set it moved: that is what makes
 * the key stable across a gesture that turned out to be a click. Two gestures on the same node
 * therefore share a key — which is exactly why `endDrag` seals.
 */
export function beginDrag(ids: string[], before: HistorySnapshot): DragOrigin {
  return { before, ids: [...ids] };
}

/**
 * Close a gesture into one history entry.
 *
 * `now` is injected for the same reason `record` takes it: a test that cannot choose the clock
 * cannot test a window.
 *
 * A click that never moved is not an entry — `record` already compares the two snapshots and
 * returns the same history, so the Undo button does not grow a step for a gesture the author
 * would not recognise as a change.
 */
export function endDrag(
  history: History,
  origin: DragOrigin,
  after: HistorySnapshot,
  now: number,
): History {
  // Seal BEFORE recording. The gesture is over, so the next one — on any node — must be its own
  // step rather than an extension of this one. Sealing after the record would age the entry it
  // had just written, which is the wrong entry.
  return record(sealGroup(history, now), {
    key: dragKey(origin.ids),
    before: origin.before,
    after,
    now,
  });
}

/**
 * Did this gesture actually move anything?
 *
 * Exposed as a decision rather than left to `record`'s snapshot comparison so a caller can say
 * it out loud: "no change" and "changed" differ by more than a boolean here, because a click
 * that ends a drag is also the gesture a *marquee* may have begun.
 */
export function dragMoved(origin: DragOrigin, after: HistorySnapshot): boolean {
  return JSON.stringify(origin.before) !== JSON.stringify(after);
}

/** The window a caller must respect to keep two drags apart. Re-exported so it cannot drift. */
export { COALESCE_MS };
