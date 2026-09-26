"use client";

/**
 * What the palette remembers in the browser (REQ-002, slice 2).
 *
 * Two small lists live here, both scoped to this browser rather than to the account: the screens
 * it visited (the palette's "Recently viewed" — the panel's own trail, which the API does not
 * keep) and the recent searches the account removed from the palette. Removal is local on
 * purpose: the API keeps the newest twenty queries per account and offers one "forget
 * everything", so hiding a single row is a browser preference, not a platform fact.
 *
 * Every reader is total — blocked or corrupted storage answers an empty list, never a throw.
 */

/** How many screens "Recently viewed" keeps. */
export const RECENT_VIEWS_LIMIT = 5;

/** How many hidden queries are remembered; one that ages out of this list can reappear. */
const HIDDEN_LIMIT = 50;

const VIEWS_KEY = "omnion.palette.recent-views";
const HIDDEN_KEY = "omnion.palette.hidden-recents";

/** One screen the account visited, as the palette lists it. */
export type RecentView = {
  /** Panel path (`/pages`). */
  path: string;
  /** The screen's own title. */
  label: string;
  /** When it was last visited, RFC 3339. */
  at: string;
};

function readJson<T>(key: string, fallback: T): T {
  if (typeof window === "undefined") {
    return fallback;
  }
  try {
    const raw = window.localStorage.getItem(key);
    if (!raw) {
      return fallback;
    }
    return JSON.parse(raw) as T;
  } catch {
    return fallback;
  }
}

function writeJson(key: string, value: unknown): void {
  if (typeof window === "undefined") {
    return;
  }
  try {
    window.localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // A blocked storage costs the panel its memory of the list and nothing else.
  }
}

/** The screens this browser visited, newest first. */
export function readRecentViews(): RecentView[] {
  const rows = readJson<RecentView[]>(VIEWS_KEY, []);
  if (!Array.isArray(rows)) {
    return [];
  }
  return rows
    .filter(
      (row) =>
        row !== null &&
        typeof row === "object" &&
        typeof row.path === "string" &&
        typeof row.label === "string",
    )
    .slice(0, RECENT_VIEWS_LIMIT);
}

/** Remember a screen visit: the newest wins, the list stays at [`RECENT_VIEWS_LIMIT`]. */
export function pushRecentView(view: { path: string; label: string }): void {
  const path = view.path.trim();
  if (!path) {
    return;
  }
  const label = view.label.trim() || path;
  const rows = readRecentViews().filter((row) => row.path !== path);
  rows.unshift({ path, label, at: new Date().toISOString() });
  writeJson(VIEWS_KEY, rows.slice(0, RECENT_VIEWS_LIMIT));
}

/** The recent searches this browser hides from the palette. */
export function readHiddenRecents(): string[] {
  const rows = readJson<string[]>(HIDDEN_KEY, []);
  return Array.isArray(rows) ? rows.filter((row) => typeof row === "string" && row.trim()) : [];
}

/** Hide one query from the recent list; answers the set as it stands afterwards. */
export function hideRecentSearch(query: string): string[] {
  const value = query.trim();
  if (!value) {
    return readHiddenRecents();
  }
  const rows = [value, ...readHiddenRecents().filter((row) => row !== value)].slice(0, HIDDEN_LIMIT);
  writeJson(HIDDEN_KEY, rows);
  return rows;
}

/** Forget every hidden query — what "Clear" does to the local half of the memory. */
export function forgetHiddenRecents(): void {
  writeJson(HIDDEN_KEY, []);
}
