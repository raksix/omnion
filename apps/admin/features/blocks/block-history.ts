"use client";

/**
 * The editor's undo/redo history (REQ-063, slice 4 — acceptance 9).
 *
 * "Undo/redo covers at least 50 steps including nesting changes, and `⌘Z` after a save restores
 * the pre-save state in the draft." Two claims hide in that sentence and each one shapes this
 * file:
 *
 *  - **At least 50 steps** is a depth, not a size. A history that holds the whole tree per step
 *    at 400 blocks is 200 trees, so the cap is on *how many snapshots* are kept, and the oldest
 *    are dropped first. Dropping the oldest is the only defensible end: an author fifty edits
 *    into a session who reaches for undo expects the last one, not the first.
 *  - **After a save** is the part that is easy to get wrong. A save is a *server* fact — the
 *    tree on disk moved forward and a new draft revision exists. So a save is itself an undoable
 *    step, and undoing past it must return the editor to the tree it had *before* the save, not
 *    to an empty stack. An implementation that clears the history on save satisfies "undo after a
 *    save" for exactly one step and then silently loses the author's work.
 *
 * The snapshots are the trees themselves, and every operation in `block-tree.ts` is pure and
 * returns a new array, so a snapshot is a value that can never be mutated out from under the
 * stack. Nothing here copies a tree; identity *is* the copy.
 */
import type { ContentBlock } from "@omnion/types";

/** Most snapshots the history keeps. The REQ asks for at least 50; 100 leaves room. */
export const HISTORY_LIMIT = 100;

/**
 * One undoable step.
 *
 * The label is what the author reads next to the undo control, so it says what *they* did
 * ("Add a column") rather than which helper ran (`addColumn`).
 */
export interface HistoryEntry {
  /** The tree as it was BEFORE this step ran. */
  blocks: ContentBlock[];
  /** The selection before the step, so undo returns the caret where the work was. */
  selected: number[] | null;
  /** What the step did, in the author's words. */
  label: string;
}

/** The history plus where the cursor sits in it. */
export interface History {
  /** Snapshots, oldest first. The last one is the state before the newest change. */
  past: HistoryEntry[];
  /** Snapshots undone but still redoable, newest first. */
  future: HistoryEntry[];
}

/** A history with nothing in it. */
export function emptyHistory(): History {
  return { past: [], future: [] };
}

/** `true` when there is a step to undo. */
export function canUndo(history: History): boolean {
  return history.past.length > 0;
}

/** `true` when there is a step to redo. */
export function canRedo(history: History): boolean {
  return history.future.length > 0;
}

/**
 * Record a change.
 *
 * A step is only recorded when it actually changed something: every tree helper returns its
 * input when the operation is impossible (moving the first block up, editing a path that no
 * longer exists), and a snapshot taken from an unchanged tree would make undo a no-op button
 * that still consumes a press.
 */
export function record(
  history: History,
  entry: HistoryEntry,
  changed: boolean,
): History {
  if (!changed) {
    return history;
  }
  // A new step invalidates the redo branch: the future belongs to a timeline the author just
  // left, and keeping it would let them redo into a state that no longer follows from here.
  const past = [...history.past, entry].slice(-HISTORY_LIMIT);
  return { past, future: [] };
}

/** The last undoable step, without removing it. */
export function peekUndo(history: History): HistoryEntry | null {
  return history.past.length > 0 ? history.past[history.past.length - 1] : null;
}

/** The last redoable step, without removing it. */
export function peekRedo(history: History): HistoryEntry | null {
  return history.future.length > 0 ? history.future[0] : null;
}

/**
 * Step back one change.
 *
 * The *current* tree is pushed onto the redo branch as the "after" of the step being undone, so
 * redo has something to return to. Its label is the undone step's own label with the tense
 * flipped: reading "Redo add a column" is a sentence an author can act on, and "Redo" on its
 * own tells them nothing about what they are about to get back.
 */
export function undo(
  history: History,
  current: ContentBlock[],
  currentSelected: number[] | null,
): { history: History; entry: HistoryEntry } | null {
  const entry = peekUndo(history);
  if (!entry) {
    return null;
  }
  return {
    history: {
      past: history.past.slice(0, -1),
      future: [{ blocks: current, selected: currentSelected, label: entry.label }, ...history.future],
    },
    entry,
  };
}

/** Step forward again, returning the snapshot the undo took back. */
export function redo(
  history: History,
  current: ContentBlock[],
  currentSelected: number[] | null,
): { history: History; entry: HistoryEntry } | null {
  const entry = peekRedo(history);
  if (!entry) {
    return null;
  }
  return {
    history: {
      past: [...history.past, { blocks: current, selected: currentSelected, label: entry.label }].slice(
        -HISTORY_LIMIT,
      ),
      future: history.future.slice(1),
    },
    entry,
  };
}

/**
 * Whether a change is a *typing* change rather than a structural one.
 *
 * A prop edit fires on every keystroke, so recording each one would fill a 100-step history with
 * 100 single letters and evict every real edit. Structural steps (insert, delete, move, nest,
 * column changes, pattern insert) are recorded individually; consecutive prop edits collapse
 * into one step that a later undo takes back whole.
 *
 * Identity is a `blockId`, not the label, because the label is derived text: "Edit Heading" and
 * "Edit Text" would merge into one step if the label were the key, and taking back a heading's
 * words would silently take back a paragraph's too.
 */
export interface StepIdentity {
  /** The block the step touched, or `null` for a structural step. */
  blockId: string | null;
  /** The step's own kind, so "Edit" and "Show only on desktop" never merge. */
  kind: string;
}

/** `true` when this step is a continuation of the previous one. */
export function isTypingStep(previous: StepIdentity | null, step: StepIdentity): boolean {
  if (!previous || step.blockId === null) {
    return false;
  }
  return previous.blockId === step.blockId && previous.kind === step.kind;
}
