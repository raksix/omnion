/**
 * The builder's narrow-viewport lock (REQ-004, "Below 1024px the builder is read-only with the
 * banner, Table mode stays editable, and no control is unreachable").
 *
 * Three things live here and nothing else: the breakpoint itself, the reason a control is
 * disabled, and the list of what a locked builder must still offer. They are pure because
 * each of them is a decision a screen can get wrong *quietly* — a lock that is half applied
 * is worse than none, and a control that merely greys out is a dead button, which the
 * definition of done forbids outright.
 *
 * **Why 1024 and not a phone breakpoint.** The three panes are 260 + 320 = 580px of chrome;
 * below 1024 the canvas gets under 440px, which is narrower than a node card (220px) plus
 * the two margins a drag needs to aim at. The layout was not the reason, though — the reason
 * is that every gesture in this builder is a *precision* gesture (port dots are 10px, snap
 * is 8px, a marquee has to start on empty canvas), and a finger cannot aim any of them. So
 * the lock is not "small screens get a different layout", it is "this screen needs a pointer
 * and says so".
 */

/** The width the builder is designed for. At or above it, everything is editable. */
export const EDITOR_MIN_WIDTH = 1024;

/** A control that mutates the graph, and what the locked builder must do with it. */
export type EditingControl =
  /** Palette entry — adds a node to the graph. */
  | "add-node"
  /** Dragging or arrow-nudging a card. */
  | "move-node"
  /** Drawing a connection between ports. */
  | "connect"
  /** `Del` on a node or a selected edge. */
  | "delete"
  /** The inspector's parameter fields. */
  | "edit-parameter"
  /** Undo / redo / duplicate / copy / paste. */
  | "history"
  /** The auto-layout button. */
  | "auto-layout";

/** Every control the lock governs. Written out so a new control cannot be added and forgotten. */
export const EDITING_CONTROLS: readonly EditingControl[] = [
  "add-node",
  "move-node",
  "connect",
  "delete",
  "edit-parameter",
  "history",
  "auto-layout",
];

/** What a control does while the builder is locked. */
export interface LockedControl {
  control: EditingControl;
  /**
   * `true` when the control is *hidden* rather than merely disabled.
   *
   * The distinction is the whole answer to "no control is unreachable": a hidden control
   * cannot be tabbed to, so it cannot be a dead end, and a disabled one is still in the tab
   * order unless it is also `aria-disabled` and skipped. A control that is drawn, disabled,
   * and focusable is the definition of unreachable.
   */
  hidden: boolean;
  /** Why it is not available, in the author's words. Shown as a tooltip, never invented in JSX. */
  reason: string;
}

/** The banner text, so the message is asserted rather than eyeballed. */
export const LOCK_BANNER = {
  title: "Editing needs a larger screen",
  body:
    "This builder is read-only on a narrow screen — dragging cards, drawing connections and " +
    "editing parameters need a pointer and room to aim. Table mode stays editable.",
  /** The route the banner sends the author to, since read-only must still be *useful*. */
  tableModeLabel: "Open Table mode",
} as const;

/**
 * Whether the editor is locked at a given viewport width.
 *
 * Exactly `>= EDITOR_MIN_WIDTH` is editable, which matches the CSS media query's own edge
 * (`min-width: 1024px`): one pixel of disagreement between the two and the builder unlocks
 * a pixel before the layout has room for it, which is the kind of off-by-one that only
 * shows up in a screenshot taken at exactly the breakpoint.
 */
export function isEditorLocked(viewportWidth: number): boolean {
  return viewportWidth < EDITOR_MIN_WIDTH;
}

/**
 * How one control behaves at a width.
 *
 * Three shapes, and each answers a different half of the criterion:
 * - **wide** — enabled, not mentioned.
 * - **locked, hidden** — mutation that cannot be done accurately without a pointer
 *   (add / connect / move / delete / layout). Not in the tab order at all.
 * - **locked, visible-but-disabled** — the *affordances* an author needs to understand the
 *   graph, and the escape hatches that are honest about being read-only. Auto layout is
 *   visible because a reader asks "is this laid out sensibly?" and a greyed button that
 *   says why is a better answer than a button that is simply gone.
 */
export function controlAt(width: number, control: EditingControl): LockedControl {
  if (!isEditorLocked(width)) {
    return { control, hidden: false, reason: "" };
  }
  switch (control) {
    case "auto-layout":
      return {
        control,
        hidden: false,
        reason: "Layout changes the definition, and editing needs a larger screen.",
      };
    case "history":
      // Undo/redo is *history of writes*. On a read-only builder there is nothing to undo in
      // this tab, and a button that is enabled and does nothing is worse than one that is
      // disabled — so it is hidden, not greyed.
      return { control, hidden: true, reason: "Nothing to undo — this view only reads." };
    default:
      return {
        control,
        hidden: true,
        reason: "Editing needs a larger screen — Table mode stays editable.",
      };
  }
}

/** Every control's locked behaviour, for the toolbar/inspector to read in one pass. */
export function lockPlan(width: number): Record<EditingControl, LockedControl> {
  const plan = {} as Record<EditingControl, LockedControl>;
  for (const control of EDITING_CONTROLS) {
    plan[control] = controlAt(width, control);
  }
  return plan;
}

/** `true` when the plan hides nothing, i.e. the builder is fully editable. */
export function isFullyEditable(width: number): boolean {
  return !isEditorLocked(width);
}

/**
 * The class the builder root takes at a width.
 *
 * Exported as a function rather than a constant because the criterion is about the *pair* of
 * states: a lock that only sets `pointer-events: none` on the canvas still lets the
 * inspector take focus and scroll, and the author is then typing into a field that silently
 * discards every keystroke. The rule is `inert` on the editing regions, which removes them
 * from the tab order and from hit testing in one attribute.
 */
export function builderLayoutClass(width: number): string {
  return isEditorLocked(width) ? "builder-locked" : "builder-wide";
}

/**
 * Does this key *read* the graph rather than change it?
 *
 * The lock has to be a whitelist and not a list of mutations, and the reason is specific:
 * a lock implemented in the pointer handlers alone is a lock with a hole in it exactly the
 * size of the hardware keyboard. A phone with a Bluetooth keyboard, or a tablet with a
 * case, gets a full set of shortcuts on a screen the banner calls read-only — and `Del`
 * deleting a node is the demonstration. So the keyboard path asks this function, and the
 * whitelist is: navigation and inspection, never mutation.
 *
 * `Del`, `Backspace`, `Enter` and every `⌘` chord are excluded by construction, because
 * they are absent from this list rather than from a denylist — a new mutating shortcut is
 * refused by default, and adding it to the read set is a deliberate act.
 */
export function isReadingKey(event: { key: string; metaKey?: boolean; ctrlKey?: boolean }): boolean {
  if (event.metaKey || event.ctrlKey) {
    // Every `⌘` chord in the builder is a write: undo, redo, copy, paste, duplicate, save,
    // select-all, palette. None of them is safe to allow read-only — `⌘Z` on a screen where
    // nothing was written is at best a no-op and at worst a stale undo from before the lock.
    return false;
  }
  if (event.key === "Delete" || event.key === "Backspace" || event.key === "Enter") {
    return false;
  }
  return [
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
    "Tab",
    "Escape",
    "Home",
    "End",
    " ",
  ].includes(event.key);
}
