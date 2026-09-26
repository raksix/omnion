/**
 * The palette's own logic, kept out of the component: which providers the panel can open, how a
 * result set becomes sections, and how a title is split around the words that matched.
 *
 * The API owns the hits and their order; this module only decides how many of them a section
 * shows and whether a section may be rendered at all. A provider the panel has no screen for gets
 * no section — a row that cannot go anywhere is worse than a section that is not there.
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

/** How many rows a section shows before it offers "see all". */
export const SECTION_ROWS = 5;

/** The shortest query the palette searches with; anything shorter keeps the recent lists. */
export const MIN_QUERY = 2;

/** One section of the palette: the rows it shows plus where the rest of them live. */
export type PaletteSection = {
  /** Provider key (`pages`). */
  provider: PaletteProviderKey;
  /** Section title as the panel writes it. */
  label: string;
  /** At most [`SECTION_ROWS`] hits, in the API's rank order. */
  rows: SearchHit[];
  /** Everything the provider matched for this query, from the API's own counts. */
  total: number;
  /** `true` when the provider matched more than the rows shown. */
  truncated: boolean;
};

/** The panel screen behind a provider key, or `null` when there is none yet. */
export function paletteProvider(
  provider: string,
): { label: string; route: string } | null {
  const known = (PALETTE_PROVIDERS as Record<string, { label: string; route: string }>)[provider];
  return known ?? null;
}

/**
 * Turn one answer of `GET /api/v1/search` into the palette's sections: grouped by provider,
 * ordered by how much each provider matched, each capped at [`SECTION_ROWS`] rows.
 */
export function buildSections(result: SearchResult | null): PaletteSection[] {
  if (!result) {
    return [];
  }

  const totals = new Map<string, number>();
  for (const count of result.counts ?? []) {
    totals.set(count.provider, count.count);
  }

  const grouped = new Map<string, SearchHit[]>();
  for (const hit of result.hits ?? []) {
    const rows = grouped.get(hit.provider);
    if (rows) {
      rows.push(hit);
    } else {
      grouped.set(hit.provider, [hit]);
    }
  }

  const sections: PaletteSection[] = [];
  for (const [provider, rows] of grouped) {
    const known = paletteProvider(provider);
    if (!known) {
      continue;
    }
    const total = Math.max(totals.get(provider) ?? rows.length, rows.length);
    sections.push({
      provider: provider as PaletteProviderKey,
      label: known.label,
      rows: rows.slice(0, SECTION_ROWS),
      total,
      truncated: total > rows.length || rows.length > SECTION_ROWS,
    });
  }

  sections.sort((a, b) => b.total - a.total || a.label.localeCompare(b.label));
  return sections;
}

/** Where a section's "see all" goes: the results screen narrowed to that one provider. */
export function sectionUrl(query: string, provider: string): string {
  const raw = `${query.trim()} type:${provider}`.trim();
  return `/search?q=${encodeURIComponent(raw)}`;
}

/** Where the palette's own "see all results" goes. */
export function resultsUrl(query: string): string {
  return `/search?q=${encodeURIComponent(query.trim())}`;
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
