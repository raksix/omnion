/**
 * Resolving an optimistic-concurrency conflict (REQ-004, slice 2).
 *
 * Autosave plus a `graph_version` guard means the guarantee is the `409`, not the timer: a
 * second editor's save moves the stored version, and this tab's next write is refused. What
 * the client then owes the author is a *choice*, and the server's own message already names
 * the two sides of it — "reload to see their change, or keep editing to overwrite it".
 *
 * The second half of that sentence was, until now, a promise the client could not keep. After
 * a conflict `versionRef` stayed at the value the tab loaded, so *every* subsequent PUT
 * quoted a version that was one behind and was refused again: the banner said "keep editing"
 * and editing accomplished nothing, forever. The author was left with a working Reload and a
 * dead end, and the only way to save was to throw their work away.
 *
 * So the conflict becomes a decision with two named exits, and the rule for taking the second
 * one lives here as a pure function — it decides *which* version to quote, not how to talk to
 * the server. A decision about data that lived inside a click handler cannot be tested; this
 * one can, and the test that matters most is the hostile one: a save must not be able to
 * quote a version the server never named, because that is a silent overwrite wearing the
 * costume of a resolution.
 */

/** The conflict as the client received it. */
export interface Conflict {
  /** The sentence the server sent. Shown to the author verbatim. */
  message: string;
  /**
   * The version the server says it holds, read out of `message`.
   *
   * `null` when the message does not name one — in which case the safe exit is the only
   * honest one, because "overwrite" with no known base is not an overwrite, it is a guess.
   */
  version: number | null;
}

/**
 * Read the version out of the server's conflict message.
 *
 * `graph_store::replace_graph` writes "it is now at version {current}". Matching the number
 * in that one sentence keeps the client from having to parse a second source of truth, and
 * returning `null` for anything that does not fit is deliberate: a version invented from a
 * sentence that did not contain one is precisely how an editor loses a colleague's work.
 */
export function readVersionFrom(message: string): number | null {
  // The trailing boundary is load-bearing. Without it `at version 7.5` parses as 7 — the
  // regex matches the integer prefix and stops — and the client would then quote a version
  // the server never held. That is the overwrite guard failing open on malformed input,
  // which is the one direction it must never fail in.
  const match = /at version (\d+)(?![.\d])/.exec(message);
  if (!match) return null;
  const version = Number.parseInt(match[1], 10);
  return Number.isSafeInteger(version) && version >= 0 ? version : null;
}

/** What the author chose, and what each choice means. */
export type ConflictChoice = "reload" | "overwrite";

/** What the client should do next, and what it must not do. */
export interface ConflictResolution {
  choice: ConflictChoice;
  /**
   * The version the next save must quote, or `null` when nothing may be written yet.
   *
   * `reload` yields `null`: the author asked to see the other editor's work, and the only
   * honest next step is to fetch it — writing first would destroy exactly what they asked
   * to look at.
   */
  nextVersion: number | null;
  /** True when the author must confirm before the other editor's change is destroyed. */
  requiresConfirmation: boolean;
  /** One sentence for the confirm button, so the cost of the click is named. */
  confirmLabel: string;
}

/**
 * Decide what a conflict means for the next save.
 *
 * `overwrite` quotes the version the *server* named, never `localVersion + 1` and never the
 * version this tab happens to hold: the guard exists so that two editors cannot both believe
 * they won, and re-deriving the base locally is how that guarantee quietly becomes a no-op.
 */
export function resolveConflict(conflict: Conflict): ConflictResolution {
  if (conflict.version === null) {
    // Nothing to quote. Saying "keep mine" here would offer a button that cannot work, and a
    // dead button in a conflict banner is worse than no banner: the author is already behind
    // and now has to work out which control is lying.
    return {
      choice: "reload",
      nextVersion: null,
      requiresConfirmation: false,
      confirmLabel: "Reload their version",
    };
  }
  return {
    choice: "overwrite",
    nextVersion: conflict.version,
    // Overwriting is the destructive half, so it is confirmed. The cost has to be named at
    // the moment of the click, not explained afterwards in a toast nobody reads.
    requiresConfirmation: true,
    confirmLabel: `Overwrite their changes (version ${conflict.version})`,
  };
}
