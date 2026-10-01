/**
 * A wholesale graph replacement has to rebase the undo history and the selection.
 *
 * ## What was broken
 *
 * `load()` is not a refresh. It is the **Reload** exit of the two-tab conflict — the button the
 * server's own sentence offers ("reload to see their change, or keep editing to overwrite it")
 * — and it is also what the error state's "Try again" calls. On success it replaces the graph
 * with the SERVER's, which on the conflict path is the *other editor's* definition, and it
 * replaced `nodes`/`edges` and nothing else.
 *
 * The history survived that write, and the two halves of what survives are both wrong in the
 * direction that destroys work:
 *
 * * Every entry still describes the graph that was on screen a moment ago. A press of undo
 *   therefore restores a definition the author chose to discard, and `doUndo` ends in
 *   `queueSave()` — so the discarded graph is PUT back over the other tab's work. The write
 *   quotes `versionRef`, which `load()` has just advanced to the server's current version, so
 *   the server **accepts** it: no conflict, no second refusal, a silent total overwrite. The
 *   guard this whole feature exists for is undone by the undo button — by the one control the
 *   author reaches for when something looks wrong.
 * * The selection is not pruned either, so an inspector can keep showing a node the reload
 *   removed, and a keyboard action aimed at it acts on a node that is not in the graph.
 *
 * ## Why the history is emptied rather than extended
 *
 * Keeping an entry for the adoption would be prettier ("undo the reload") and is wrong: undo
 * would restore the pre-adoption graph and `queueSave` would write it, so the one press an
 * author is most likely to try after a conflict is the one that destroys the work they just
 * decided to keep. The history describes edits to a graph that is no longer on the screen, so
 * the honest state is no history — which is also exactly what a fresh tab has.
 *
 * ## Why this is a function and not three lines in `load`
 *
 * The same class as `conflict.ts` and `node-status.ts`: the decision is a rule, it is worth
 * stating in one place, and a rule that lives only inside a `useCallback` can only be tested by
 * reading the component's source.
 */
import { emptyHistory, type History } from "./builder-history.ts";
import { pruneSelection, type CanvasSelection } from "./selection.ts";

export interface Rebase {
  /** The history to keep. Empty: the graph it described is no longer on the canvas. */
  history: History;
  /** The selection, with every id the loaded graph does not contain removed. */
  selection: CanvasSelection;
}

/**
 * Rebase after the graph was replaced wholesale from the server.
 *
 * `current` is the selection as it stands and `alive` the id set of the graph just adopted.
 * Both are arguments rather than closure reads so the rule is a function of its inputs, and so
 * a test can build the one case that matters: a selection pointing at a node the OTHER editor
 * deleted — which is the ordinary outcome of the reload this exists to serve.
 *
 * The selection is **pruned, not cleared**. Clearing it would be safe but it throws away a
 * perfectly valid focus: the other editor routinely leaves your selected card alone, and a
 * reload that dumps the inspector for no reason is a reload that discards the author's place.
 *
 * `pruneSelection` decides the rest, and one of its rules is worth stating here because it
 * looks like a gap and is not: when the focused card is gone, focus falls to the first
 * SURVIVING MEMBER OF THE SELECTION, and to nothing at all when the selection was a single
 * click — a click is a focus with no group (`selectNode` returns `nodes: []`), so there is no
 * member to fall back to. It never falls back to some other card on the canvas, because
 * silently focusing a node the author did not choose reads as the reload having picked
 * something for them, which is the one behaviour that would make a reload feel like a
 * different tab taking the wheel.
 */
export function rebaseAfterReload(current: CanvasSelection, alive: readonly string[]): Rebase {
  return { history: emptyHistory(), selection: pruneSelection(current, alive) };
}
