/**
 * What the builder announces to a screen reader (REQ-004, slice 4 — the accessibility
 * assertions).
 *
 * ## The defect this file exists for
 *
 * The walk landed on `Tab` last tick and the shortcut list stopped lying about it. What the
 * walk could not do is *speak*, and a canvas is the one screen where a screen reader is
 * completely blind by default: the canvas is a `div` with `role="application"`, the cards are
 * `role="button"` with `tabIndex={-1}`, and every one of them renders its name as a bare
 * `<span>` and a `<p>`. A reader that arrives on a card announces `Send mail, button` — it
 * does not announce the node type, it does not announce the parameters, and it does not
 * announce that a second card is now selected. A selection with no announcement is a selection
 * the author cannot verify, and the whole point of the roving focus ring is that the author
 * can verify where they are.
 *
 * **Three of the builder's live regions were mounted conditionally, which is the quiet way to
 * have no live region at all.**
 *
 * * The save indicator is a *different element* per state. `dirty` renders one `<span>`, a
 *   `conflict` renders another that is the only one with `role="alert"`, an `error` a third.
 *   A live region is only announced when its *content changes* while the element is already in
 *   the accessibility tree — so a save going `dirty → saving → saved` swaps the node and
 *   announces nothing. The one branch that did carry a role was `conflict`, which is the
 *   branch a screen reader user *must not miss*, and it was the one that only exists after a
 *   second tab has already overwritten the author.
 * * The connection notice is the same shape: `{linkNotice ? <p role="status">…</p> : null}`.
 * * The narrow-screen lock banner is `role="status"` and disappears from the tree on resize.
 *
 * So the fix is structural, not a matter of adding three more `aria-live` attributes:
 * **the region is always in the tree, and only its text changes.**
 *
 * ## Why this is a pure module and not JSX
 *
 * The rules here are sentences about *what may be said*, and JSX can only be tested by
 * rendering it — which this repo does not do (there is no React test renderer in the tree, and
 * adding one to assert six strings would be the wrong trade). Splitting the decision from the
 * markup makes every claim below a plain function call, and leaves the component responsible
 * for exactly one thing: rendering the returned object.
 *
 * The alternative, a text test reading the source, is the shape that already produced one
 * false green in this file's neighbourhood: a guard that checked the *text* of a call reported
 * a handler that was wired inside a `NEVER_TRUE` branch as working. The assertions here
 * cannot have that defect, because they call the function.
 */

import { membersOf, type CanvasSelection } from "./selection.ts";

/**
 * Politeness of a live region.
 *
 * `assertive` interrupts whatever the reader is saying; `polite` queues it. The distinction is
 * the whole difference between a screen reader that is usable and one that talks over itself,
 * and the rule is about *consequence*, not about how urgent the thing feels:
 *
 * * The author is about to lose work, or work has already been lost → `assertive`.
 * * The author is being told what just happened to something they were already looking at →
 *   `polite`.
 */
export type Politeness = "assertive" | "polite";

/**
 * The name a card announces.
 *
 * **Three facts, and a card that announces one of them is a card the author cannot use.**
 * Which node it is (`Send mail`) comes from the label, so it is free. What kind of thing it
 * is (a trigger, an action, an end) is the *node type* — and that is the distinction the
 * canvas exists to draw, so a reader that cannot hear it is hearing "Send mail, button" and
 * not knowing whether it is the first or the last card in a rule. Its parameters are the
 * third: a card whose field reads `to: unset` and one reading `to: ops@…` are the same node
 * to a visual author and completely different nodes to an author editing by keyboard.
 *
 * The parameters are rendered as a stable, spoken prefix (`to: unset`) rather than a
 * serialised blob, because `JSON.stringify` on a parameter object is noise: a screen reader
 * says "left brace, quote, to, colon, quote, ops at example dot com, quote, right brace", and
 * a reader is exactly the audience that should not have to hear braces. An object with several
 * keys is read as its entries in a stable key order; an empty one says so, rather than saying
 * nothing — a card with no parameters announced is indistinguishable from a card that failed
 * to render.
 */
export function cardAnnouncement(input: {
  label: string;
  nodeType: string;
  kindLabel: string | null;
  params: Record<string, unknown> | null | undefined;
  selected: boolean;
  partOfGroup: boolean;
}): string {
  const parts: string[] = [`${input.label}, ${input.nodeType}`];
  if (input.kindLabel) {
    // The kind is the word the palette and the problems panel already use, so the three
    // surfaces of the builder cannot invent three vocabularies for the same node.
    parts.push(input.kindLabel);
  }
  if (input.selected) {
    parts.push("selected");
  } else if (input.partOfGroup) {
    // "Selected" for a group member and "selected" for the focus would be the same two
    // words for two different things, and the difference is the one `Del` acts on. The
    // inspector is the focus; the outline is the union — so the announcement has to name which.
    parts.push("in a selection of 1");
  }
  const parameters = spokenParams(input.params);
  if (parameters.length > 0) {
    parts.push(parameters.join(", "));
  }
  return parts.join(". ");
}

function spokenParams(params: Record<string, unknown> | null | undefined): string[] {
  if (!params) {
    return [];
  }
  const out: string[] = [];
  for (const key of Object.keys(params).sort()) {
    out.push(`${key}: ${spokenValue(params[key])}`);
  }
  return out;
}

/**
 * One parameter value, said out loud.
 *
 * A value that is absent has to be *said* as absent. An empty string, an `undefined` and a
 * `null` all render as nothing on the card, and a reader that skipped them would announce
 * "to:" and stop, which reads as a rendering bug rather than an unset field.
 */
export function spokenValue(value: unknown): string {
  if (value === null) {
    return "unset";
  }
  if (value === undefined) {
    return "unset";
  }
  if (typeof value === "string") {
    return value.trim() === "" ? "unset" : value;
  }
  if (typeof value === "number") {
    return Number.isFinite(value) ? String(value) : "unset";
  }
  if (typeof value === "boolean") {
    return value ? "yes" : "no";
  }
  if (Array.isArray(value)) {
    return value.length === 0 ? "none" : value.map((item) => spokenValue(item)).join(", ");
  }
  if (typeof value === "object") {
    const keys = Object.keys(value as Record<string, unknown>);
    return keys.length === 0 ? "none" : `${keys.length} field${keys.length === 1 ? "" : "s"}`;
  }
  return "unset";
}

/**
 * The save indicator as a screen reader should hear it.
 *
 * **The region must already exist.** `politeness` is the *only* thing that decides whether
 * this is a region at all: the two `dirty → saving → saved` transitions are `polite` and must
 * not interrupt, while a `conflict` is `assertive` and must — but it cannot become assertive
 * by *gaining* the attribute, because a role added to a node that did not exist in the
 * accessibility tree before the change is not announced. The caller therefore renders one
 * permanent `role="status"` container and puts this text inside it; `assertive` is expressed
 * with `aria-live` on that same container, which a live-region role *may* carry and which does
 * not remove the region.
 *
 * The spoken text is deliberately **not** the visible text. The visible pill says "Saved",
 * which is two words and means nothing to a reader who did not hear what was saved; the
 * announcement names the version, because the version is the thing a second tab is racing, and
 * a reader hearing "Saved, version 7" knows whether a conflict message that follows is about
 * their own write.
 *
 * `clean` announces nothing at all. It is the state the builder is in for most of its life,
 * and a region that says "no pending changes" the moment the page opens is a region that
 * trains a reader to stop listening to it.
 */
export function saveAnnouncement(state: {
  kind: "clean" | "dirty" | "saving" | "saved" | "conflict" | "error";
  version?: number | null;
  /** Only the two failing states have anything to add to the sentence. */
  message?: string;
}): { text: string; politeness: Politeness; empty: boolean } {
  const version = typeof state.version === "number" ? `, version ${state.version}` : "";
  const spoken = (state.message ?? "").trim();
  const withMessage = (sentence: string) => (spoken === "" ? sentence : `${sentence} ${spoken}`);
  switch (state.kind) {
    case "clean":
      return { text: "", politeness: "polite", empty: true };
    case "dirty":
      return { text: "Unsaved changes", politeness: "polite", empty: false };
    case "saving":
      return { text: `Saving${version}`, politeness: "polite", empty: false };
    case "saved":
      return { text: `Saved${version}`, politeness: "polite", empty: false };
    case "conflict":
      return {
        text: withMessage("Save refused. Another tab saved a newer version of this rule."),
        politeness: "assertive",
        empty: false,
      };
    case "error":
      return {
        text: withMessage("Save failed."),
        politeness: "assertive",
        empty: false,
      };
  }
}

/**
 * The connection outcome, and whether the notice has to be *cleared* before it can change.
 *
 * A live region re-announces when its text changes, so a second refusal with the *same*
 * sentence is silent. That is not a cosmetic problem: "Event · Next already leads to that
 * node" is the one message an author needs twice in a row (they press the same port twice
 * because the first one did not look like it took), and a silent repeat is indistinguishable
 * from a dead gesture. A monotonic counter rides along so the caller can force a change.
 */
export function linkAnnouncement(notice: { tone: "ok" | "error"; text: string } | null): {
  text: string;
  politeness: Politeness;
  /** A caller that rendered a different message must bump this to make the reader speak. */
  nonce: number;
} {
  if (!notice) {
    return { text: "", politeness: "polite", nonce: 0 };
  }
  return {
    text: notice.text,
    // A refusal is an error the author just made; a success is confirmation of something they
    // were already doing. Same shape, different consequence, so different politeness.
    politeness: notice.tone === "error" ? "assertive" : "polite",
    nonce: hashText(notice.text),
  };
}

/** A small stable hash, so the nonce changes exactly when the sentence changes. */
function hashText(text: string): number {
  let hash = 0;
  for (let index = 0; index < text.length; index += 1) {
    hash = (hash * 31 + text.charCodeAt(index)) | 0;
  }
  return hash;
}

/**
 * The canvas's own announcement, for a change the author caused and the reader must hear.
 *
 * This is the third region and it is the one the previous two ticks of work could not have
 * found by reading the shortcut list: a selection that moved is not a text change anywhere
 * on the page — the card's outline is a CSS class — so without this the *only* signal a reader
 * gets for the entire Tab walk is the card's own accessible name changing, which is only a
 * live announcement if the card itself is a live region, and making every card one would make
 * a 200-node graph announce itself all at once on load.
 */
export function selectionAnnouncement(
  current: CanvasSelection,
  selectedCount = membersOf(current).length,
): { text: string; politeness: Politeness; empty: boolean } {
  if (current.edge) {
    return {
      text: "Connection selected. Delete removes it.",
      politeness: "polite",
      empty: false,
    };
  }
  if (selectedCount > 1) {
    return {
      text: `${selectedCount} cards selected. Delete removes all of them.`,
      politeness: "polite",
      empty: false,
    };
  }
  if (current.focus) {
    return { text: "1 card selected.", politeness: "polite", empty: false };
  }
  return { text: "", politeness: "polite", empty: true };
}

/**
 * The narrow-screen lock, as an announcement.
 *
 * The banner is `role="status"` today and is mounted only when `locked` is true, so a reader
 * hears it if the resize happens *while* the reader is on the page and not at all otherwise —
 * and the second case is the one that matters, because a narrow screen is usually narrow
 * *before* the page loads. The text names the way out, because a lock with no announced exit
 * is the same dead end the criterion forbids with a mouse.
 */
export function lockAnnouncement(locked: boolean, tableModeLabel: string): {
  text: string;
  politeness: Politeness;
} {
  if (!locked) {
    return { text: "", politeness: "polite" };
  }
  return {
    text: `This builder is read-only on a narrow screen. Editing is off; ${tableModeLabel} still works.`,
    politeness: "assertive",
  };
}

/**
 * The help dialog's own a11y contract, as data.
 *
 * `aria-modal` and a role are not enough for a dialog a keyboard can open: focus has to
 * *move* into it and come back, and a dialog that takes focus and never gives it back strands
 * the author behind the page they came from. This returns the two values the component has to
 * honour, and the test asserts the *round trip* rather than either half — a focus trap that
 * forgets to restore is a different bug from one that never trapped, and both render as
 * "the dialog works" in a screenshot.
 */
export const HELP_DIALOG = {
  role: "dialog",
  modal: true,
  label: "Keyboard shortcuts",
  /** The element inside the dialog that receives focus when it opens. */
  initialFocus: "close" as const,
  /** Escape closes it, and the canvas's own ladder is asked afterwards. */
  closesOnEscape: true,
};
