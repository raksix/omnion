/**
 * The palette's own logic, kept out of the component: which providers the panel can open, what
 * one provider's answer looks like while it is still in flight, and how a title is split around
 * the words that matched.
 *
 * The API owns the hits and their order; this module only decides how a provider's own answer
 * becomes a section and whether a section may be rendered at all. A provider the panel has no
 * screen for gets no section — a row that cannot go anywhere is worse than a section that is not
 * there.
 *
 * Federated answers (REQ-032, slice 2): the palette asks each provider its own question, so each
 * group carries its own state — pending, ready or failed — and one slow or failing provider never
 * blanks the rest of the palette. That is what [`PaletteGroup`] describes.
 */
import type { SearchHit, SearchResult } from "./api";

/** Providers whose hits the panel can open today, with the screen a click lands on. */
export const PALETTE_PROVIDERS = {
  pages: { label: "Pages", route: "/pages" },
  media: { label: "Media", route: "/media" },
  sites: { label: "Sites", route: "/sites" },
  // Activity, translations and settings arrived with their providers (slice 3). A row of theirs
  // carries its own destination — an audit entry opens the page it touched, a translation opens
  // its page's editor, the settings row opens the screen that owns it — so the section opens
  // real screens even though none of the three has a list screen of its own.
  logs: { label: "Activity", route: "/search" },
  translations: { label: "Translations", route: "/pages" },
  settings: { label: "Settings", route: "/settings/search" },
} as const;

/** One of the provider keys the palette knows. */
export type PaletteProviderKey = keyof typeof PALETTE_PROVIDERS;

/**
 * The providers the palette asks on every keystroke, in the order their sections appear.
 *
 * The registry's own order, and fixed: a streaming list that reorders itself as answers land
 * would move rows under the reader's eyes. Counts decide nothing about where a section sits.
 */
export const PALETTE_PROVIDER_ORDER: readonly PaletteProviderKey[] = [
  "pages",
  "media",
  "sites",
  "logs",
  "translations",
  "settings",
];

/** How many rows a section shows before it offers "see all". */
export const SECTION_ROWS = 5;

/** The shortest query the palette searches with; anything shorter keeps the recent lists. */
export const MIN_QUERY = 2;

/** One provider's own state in the palette. */
export type PaletteGroup = {
  /** Provider key (`pages`); a provider the panel has no screen for still gets a group. */
  provider: string;
  /** Section title as the panel writes it. */
  label: string;
  /** `pending` until this provider's own request has answered. */
  status: "pending" | "ready" | "error";
  /** At most [`SECTION_ROWS`] hits, in the API's rank order. */
  rows: SearchHit[];
  /** What the provider matched, from its own answer; `0` while it is still in flight. */
  total: number;
  /** `true` when the provider matched more than the rows shown. */
  truncated: boolean;
  /** The failure of this provider's request, when it failed. */
  error?: { code: string; status: number; message: string };
};

/** The panel screen behind a provider key, or `null` when there is none yet. */
export function paletteProvider(
  provider: string,
): { label: string; route: string } | null {
  const known = (PALETTE_PROVIDERS as Record<string, { label: string; route: string }>)[provider];
  return known ?? null;
}

/** A provider's title: the panel's own name for it, or the provider key itself. */
export function providerLabel(provider: string): string {
  return paletteProvider(provider)?.label ?? provider;
}

/** The groups every search of the whole index starts from: one pending row per provider. */
export function pendingGroups(providers: readonly string[]): PaletteGroup[] {
  return providers.map((provider) => ({
    provider,
    label: providerLabel(provider),
    status: "pending" as const,
    rows: [],
    total: 0,
    truncated: false,
  }));
}

/** One provider's answer as its group. */
export function groupFromAnswer(provider: string, answer: SearchResult): PaletteGroup {
  const rows = (answer.hits ?? []).slice(0, SECTION_ROWS);
  const total = Math.max(answer.total ?? 0, rows.length);
  return {
    provider,
    label: providerLabel(provider),
    status: "ready",
    rows,
    total,
    truncated: total > rows.length,
  };
}

/** One provider's failure as its group — the other groups are untouched by it. */
export function groupFromError(
  provider: string,
  error: { code: string; status: number; message: string },
): PaletteGroup {
  return {
    provider,
    label: providerLabel(provider),
    status: "error",
    rows: [],
    total: 0,
    truncated: false,
    error,
  };
}

/** Replace one group in a list without disturbing the rest. */
export function withGroup(groups: PaletteGroup[], next: PaletteGroup): PaletteGroup[] {
  return groups.map((group) => (group.provider === next.provider ? next : group));
}

/** How many rows every group holds together — the "see all" row's fallback count. */
export function groupsTotal(groups: PaletteGroup[]): number {
  return groups.reduce((sum, group) => sum + (group.status === "ready" ? group.total : 0), 0);
}

/** `true` when every group has answered (or failed) — the search is no longer streaming. */
export function groupsSettled(groups: PaletteGroup[]): boolean {
  return groups.length > 0 && groups.every((group) => group.status !== "pending");
}

/** `true` when every group failed — the one state that blanks the palette's answer. */
export function groupsAllFailed(groups: PaletteGroup[]): boolean {
  return groups.length > 0 && groups.every((group) => group.status === "error");
}

/**
 * Where a section's "see all" goes: the results screen narrowed to that one provider.
 *
 * The narrowing travels as the screen's own `type` parameter, not as `type:` inside the query
 * text: the screen reads its filters from the URL, so the link lands with its chip already
 * applied, its facet rail knowing which type is on, and its search box still showing the words
 * the reader typed.
 */
export function sectionUrl(query: string, provider: string): string {
  const params = new URLSearchParams({ q: query.trim(), type: provider });
  return `/search?${params.toString()}`;
}

/** Where the palette's own "see all results" goes. */
export function resultsUrl(query: string): string {
  return `/search?q=${encodeURIComponent(query.trim())}`;
}

/**
 * Where "Ask AI" goes: the AI Hub's chat with the query already in its prompt.
 *
 * The palette resolves nothing by itself (natural-language resolution is its own slice); it hands
 * the words to the screen that can hold a conversation, prefilled, and the reader decides.
 */
export function askAiUrl(query: string): string {
  return `/ai?q=${encodeURIComponent(query.trim())}`;
}

/**
 * The site a hit's panel URL points at (`/pages?site=<id>`), when the URL carries one.
 *
 * The palette switches the panel's site to it before it navigates, so a hit always lands on the
 * site it came from — and a fresh load of the same URL reads the parameter itself.
 */
export function siteFromUrl(url: string): string | null {
  const index = url.indexOf("?");
  if (index < 0) {
    return null;
  }
  try {
    return new URLSearchParams(url.slice(index + 1)).get("site");
  } catch {
    return null;
  }
}

/** One piece of a title: the text, and whether it is one of the words that matched. */
export type Highlight = {
  text: string;
  match: boolean;
};

/**
 * Split a title around the query terms that appear in it, so the row can emphasise them.
 *
 * The match is literal and case-insensitive (the terms the API answered with are already
 * lower-cased); regular-expression characters inside a term are escaped, so a query can never
 * change how the split behaves.
 */
export function splitHighlight(title: string, terms: string[]): Highlight[] {
  const needles = terms.map((term) => term.trim()).filter(Boolean);
  if (needles.length === 0) {
    return [{ text: title, match: false }];
  }

  const escaped = needles.map((term) => term.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  const pattern = new RegExp(`(${escaped.join("|")})`, "gi");

  const parts: Highlight[] = [];
  let last = 0;
  for (const match of title.matchAll(pattern)) {
    const index = match.index ?? 0;
    if (index > last) {
      parts.push({ text: title.slice(last, index), match: false });
    }
    parts.push({ text: match[0], match: true });
    last = index + match[0].length;
  }
  if (last < title.length) {
    parts.push({ text: title.slice(last), match: false });
  }

  return parts.length > 0 ? parts : [{ text: title, match: false }];
}
