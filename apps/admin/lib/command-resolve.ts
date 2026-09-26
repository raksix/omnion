/**
 * The AI resolution card's own logic (docs/requests/REQ-032, slice 4) — kept out of the component
 * so the rules the card follows can be read (and changed) without touching the dialog.
 *
 * The API owns the reading: what was understood, whether it may be run and where it lands. This
 * module owns *when* the box asks and how the card talks about the answer — a typed pause, a
 * confidence the operator can see, and an `Edit as search` destination that always exists.
 */
import type { Resolution } from "./api";

/** How long a keystroke waits before the phrase is sent to be read. */
export const RESOLVE_DEBOUNCE_MS = 250;
/** How short a phrase may be before it is not worth reading. */
export const RESOLVE_MIN_CHARS = 4;
/** The confidence at or above which the card stops asking and starts stating. */
export const CONFIDENT_AT = 0.75;

/** What the card is doing right now. */
export type ResolveState = "idle" | "resolving" | "ready" | "failed";

/**
 * `true` when the box should ask the platform what the words mean.
 *
 * Only the open mode asks: a prefixed box (`>` commands, `#` sites, `?` help) already says what
 * the reader wants, and a phrase shorter than a few characters is still being typed.
 */
export function shouldResolve(mode: string, term: string): boolean {
  if (mode !== "all") {
    return false;
  }
  return term.trim().length >= RESOLVE_MIN_CHARS;
}

/** Confidence as the card's own word for it. */
export function confidenceLevel(confidence: number): "high" | "medium" | "low" {
  if (confidence >= CONFIDENT_AT) {
    return "high";
  }
  return confidence >= 0.45 ? "medium" : "low";
}

/** Confidence as the percentage the card prints. */
export function confidenceLabel(confidence: number): string {
  return `${Math.round(confidence * 100)}%`;
}

/** `true` when the card should offer Run — the server's own answer, never a local guess. */
export function canRun(resolution: Resolution | null): boolean {
  if (!resolution?.runnable) {
    return false;
  }
  // A search (or a screen command) lands somewhere; an action command is run through the run
  // endpoint instead, so it carries a command id rather than a route.
  return Boolean(resolution.route || resolution.intent.command_id);
}

/**
 * Where "Edit as search" lands.
 *
 * A search reading goes exactly where it points; a command reading or an unreadable phrase hands
 * the words to the results screen, which is the one destination that can never be wrong — the
 * operator can fix the filters there by hand.
 */
export function editAsSearchUrl(resolution: Resolution, typed: string): string {
  if (resolution.search_route) {
    return resolution.search_route;
  }
  const words = (resolution.intent.query || resolution.query || typed).trim();
  return `/search?q=${encodeURIComponent(words)}`;
}

/** The line the card prints under its title, naming where the reading came from. */
export function sourceLabel(resolution: Resolution): string {
  if (resolution.source === "model" && resolution.model) {
    return `Read by ${resolution.model}`;
  }
  if (resolution.degraded) {
    return "Read locally — the model did not answer";
  }
  return "Read on the server";
}

/** The title the card gives the reading. */
export function cardTitle(resolution: Resolution): string {
  switch (resolution.intent.kind) {
    case "command":
      return "This looks like a command";
    case "search":
      return resolution.runnable ? "This looks like a search" : "Here is what I read";
    default:
      return "I am not sure what this means";
  }
}
