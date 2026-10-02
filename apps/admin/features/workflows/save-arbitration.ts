/**
 * What a save request is allowed to start (REQ-004, slice 2).
 *
 * The criterion is "⌘S during a pending autosave does not write twice", and the reason it
 * needs its own module is that the rule is not about ⌘S. ⌘S is only the *second* caller:
 * the debounce armed by the last edit is the first, and the danger is that both fire. A
 * builder that wrote the graph twice for one keystroke would advance `graph_version` twice,
 * and a second tab watching that workflow would be handed a conflict that no author caused
 * — the guard would report two people editing when there was one.
 *
 * So the decision is a pure function of the two facts that matter, and it is here rather
 * than in the key handler because a rule that decides whether a write goes out cannot be
 * tested inside a React callback. `builder-view.tsx` owns the timers and the ref; this owns
 * the answer.
 *
 * The test that matters is the hostile one: after a write is already on the wire, nothing
 * may start a second. The two refusals look alike from the keyboard and are not alike at
 * all — one cancels a timer that has not fired yet, the other joins a request that has
 * already left.
 */

/** What the save machinery currently looks like. */
export interface SaveStateSnapshot {
  /** A debounce is armed; `graph` will be written when it fires unless cancelled. */
  debounceArmed: boolean;
  /** A write has left and has not been answered. */
  writeInFlight: boolean;
}

/** What pressing save should cause. */
export type SaveAction =
  /** Write now, and cancel the debounce that was going to write the same graph. */
  | "write-now"
  /** Nothing new leaves: a write is already on the wire and owns the version column. */
  | "join-in-flight";

/**
 * Decide what a save press does.
 *
 * The order of the two checks is the whole function. In-flight wins, because a request that
 * has already been sent is the only one that can still collide on `graph_version` — the
 * armed debounce has not written anything yet and can simply be replaced.
 */
export function arbitrateSave(state: SaveStateSnapshot): SaveAction {
  if (state.writeInFlight) return "join-in-flight";
  if (state.debounceArmed) return "write-now";
  return "write-now";
}

/**
 * Whether the save indicator should read "saving" after a press.
 *
 * Both actions do, including `join-in-flight`: a key that visibly does nothing is
 * indistinguishable from a broken one, and an author who cannot tell whether the press
 * landed will press it again — which is how a queue of identical writes starts.
 */
export function savePressIsAcknowledged(action: SaveAction): boolean {
  return action === "write-now" || action === "join-in-flight";
}
