"use client";

/**
 * The ⌘K palette (REQ-002, slice 2).
 *
 * One box over the whole panel: type two characters and the index answers with a section per
 * provider, five rows each, ordered by how much that provider matched. Everything the palette
 * shows is real — the rows come from `GET /api/v1/search`, the counts from the same answer, the
 * "recent searches" from the account's own history and the "recently viewed" from this browser's
 * trail. A provider whose screen does not exist yet contributes no section, so no row is a click
 * into nothing.
 *
 * Keyboard rules the component owns:
 *
 * * `↑`/`↓` walk the whole list across section boundaries, `Enter` opens the highlighted row,
 *   `⌘Enter`/`Ctrl+Enter` opens it in a new tab, `Tab`/`Shift+Tab` jump to the next or previous
 *   section, `Esc` closes, `?` toggles the shortcut list.
 * * Under two characters nothing is searched; the recent lists are shown instead.
 * * A request that is overtaken by a newer keystroke is dropped, so a slow answer can never
 *   replace the rows of a faster, newer one.
 */
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";

import { Clock, FileText, Globe, History, Images, Loader2, Search, X } from "lucide-react";
import { useRouter } from "next/navigation";

import {
  ApiError,
  clearRecentSearches,
  fetchRecentSearches,
  searchAll,
  suggestTitles,
  type SearchHit,
  type SearchResult,
  type SearchSuggestion,
} from "@/lib/api";
import {
  MIN_QUERY,
  buildSections,
  resultsUrl,
  sectionUrl,
  siteFromUrl,
  splitHighlight,
} from "@/lib/search-palette";
import {
  forgetHiddenRecents,
  hideRecentSearch,
  readHiddenRecents,
  readRecentViews,
  type RecentView,
} from "@/lib/search-memory";
import { useSites } from "@/lib/sites";

/** How long a keystroke waits before it becomes a request. */
const DEBOUNCE_MS = 120;
/** How many recent searches the palette lists (the store keeps twenty). */
const RECENT_LIMIT = 8;
/** Hits per search request: enough for five rows of every provider that answers. */
const PER_PAGE = 50;

const PROVIDER_ICONS: Record<string, typeof FileText> = {
  pages: FileText,
  media: Images,
  sites: Globe,
};

/** One row the keyboard can land on. */
type PaletteItem = {
  id: string;
  kind: "hit" | "suggestion" | "see-all" | "everywhere" | "recent" | "view";
  /** Section the row belongs to; `-1` for the rows that sit under every section. */
  section: number;
  label: string;
  /** Where activating the row goes. */
  url?: string;
  /** The query a recent row re-runs. */
  query?: string;
};

type SearchPaletteProps = {
  /** What the header box carried when the palette opened. */
  initialQuery: string;
  /** Close the palette. */
  onClose: () => void;
};

/** One option row: the icon gutter, the two-line body and an optional trailing control. */
function Option({
  id,
  active,
  onActivate,
  onHover,
  icon,
  children,
  trailing,
}: {
  id: string;
  active: boolean;
  onActivate: () => void;
  onHover: () => void;
  icon: ReactNode;
  children: ReactNode;
  trailing?: ReactNode;
}) {
  return (
    <div
      id={id}
      role="option"
      aria-selected={active}
      onClick={onActivate}
      onMouseMove={onHover}
      className={`flex min-h-11 cursor-pointer items-center gap-2.5 rounded-lg px-2 py-1.5 transition lg:min-h-10 ${
        active ? "bg-accent-soft" : "hover:bg-quiet-soft"
      }`}
    >
      <span className="flex size-4 shrink-0 items-center justify-center text-muted" aria-hidden>
        {icon}
      </span>
      <span className="flex min-w-0 flex-1 flex-col leading-tight">{children}</span>
      {trailing}
    </div>
  );
}

/** The overlay itself. Mounted only while it is open. */
export function SearchPalette({ initialQuery, onClose }: SearchPaletteProps) {
  const router = useRouter();
  const { selectSite } = useSites();
  const [mounted, setMounted] = useState(false);
  const [query, setQuery] = useState(initialQuery);
  const [result, setResult] = useState<SearchResult | null>(null);
  const [suggestions, setSuggestions] = useState<SearchSuggestion[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [recents, setRecents] = useState<string[] | null>(null);
  const [hidden, setHidden] = useState<string[]>([]);
  const [views, setViews] = useState<RecentView[]>([]);
  const [activeIndex, setActiveIndex] = useState(0);
  const [shortcutsOpen, setShortcutsOpen] = useState(false);
  const [attempt, setAttempt] = useState(0);

  const inputRef = useRef<HTMLInputElement | null>(null);
  // Every request carries the number of the keystroke it belongs to; an answer whose number is
  // no longer the latest is dropped instead of painted.
  const tokenRef = useRef(0);

  useEffect(() => {
    setMounted(true);
  }, []);

  // Opening: the recent lists are read once; the input takes focus as soon as the portal exists
  // (the first render is `null`, so the focus has to wait for the mount to land).
  useEffect(() => {
    setHidden(readHiddenRecents());
    setViews(readRecentViews());
    let cancelled = false;
    fetchRecentSearches()
      .then((queries) => {
        if (!cancelled) {
          setRecents(queries);
        }
      })
      .catch(() => {
        // The history is a convenience: when it cannot be read the palette still searches.
        if (!cancelled) {
          setRecents([]);
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (mounted) {
      inputRef.current?.focus();
    }
  }, [mounted]);

  const trimmed = query.trim();

  // The search itself: a short debounce, then the index and the suggestions together.
  useEffect(() => {
    if (trimmed.length < MIN_QUERY) {
      tokenRef.current += 1;
      setResult(null);
      setSuggestions(null);
      setError(null);
      setLoading(false);
      return;
    }

    const token = tokenRef.current + 1;
    tokenRef.current = token;
    setLoading(true);

    const timer = setTimeout(() => {
      Promise.allSettled([
        searchAll({ q: trimmed, per_page: PER_PAGE }),
        suggestTitles(trimmed),
      ]).then(([searchOutcome, suggestOutcome]) => {
        if (tokenRef.current !== token) {
          return;
        }
        if (searchOutcome.status === "fulfilled") {
          setResult(searchOutcome.value);
          setError(null);
        } else {
          const cause = searchOutcome.reason;
          setResult(null);
          setError(
            cause instanceof ApiError ? cause.message : "The search could not be completed.",
          );
        }
        setSuggestions(suggestOutcome.status === "fulfilled" ? suggestOutcome.value : []);
        setLoading(false);
      });
    }, DEBOUNCE_MS);

    return () => clearTimeout(timer);
  }, [trimmed, attempt]);

  const sections = useMemo(() => buildSections(result), [result]);

  // The store keeps every run of a query; the palette shows each one once.
  const visibleRecents = useMemo(() => {
    const seen = new Set<string>();
    const rows: string[] = [];
    for (const value of recents ?? []) {
      if (hidden.includes(value) || seen.has(value)) {
        continue;
      }
      seen.add(value);
      rows.push(value);
      if (rows.length >= RECENT_LIMIT) {
        break;
      }
    }
    return rows;
  }, [recents, hidden]);

  const searching = trimmed.length >= MIN_QUERY;
  const showSuggestions = searching && result === null && (suggestions?.length ?? 0) > 0;

  const items = useMemo<PaletteItem[]>(() => {
    const list: PaletteItem[] = [];
    if (searching) {
      sections.forEach((section, index) => {
        section.rows.forEach((hit, row) => {
          list.push({
            id: `hit-${index}-${row}`,
            kind: "hit",
            section: index,
            label: hit.title || hit.url,
            url: hit.url,
          });
        });
        if (section.truncated) {
          list.push({
            id: `see-all-${index}`,
            kind: "see-all",
            section: index,
            label: `See all ${section.total} ${section.label}`,
            url: sectionUrl(trimmed, section.provider),
          });
        }
      });
      if (showSuggestions) {
        (suggestions ?? []).slice(0, 5).forEach((suggestion, index) => {
          list.push({
            id: `suggestion-${index}`,
            kind: "suggestion",
            section: 0,
            label: suggestion.title || suggestion.url,
            url: suggestion.url,
          });
        });
      }
      list.push({
        id: "everywhere",
        kind: "everywhere",
        section: -1,
        label: `See all results for “${trimmed}”`,
        url: resultsUrl(trimmed),
      });
    } else {
      visibleRecents.forEach((value, index) => {
        list.push({ id: `recent-${index}`, kind: "recent", section: 0, label: value, query: value });
      });
      views.forEach((view, index) => {
        list.push({
          id: `view-${index}`,
          kind: "view",
          section: 1,
          label: view.label,
          url: view.path,
        });
      });
    }
    return list;
  }, [searching, sections, trimmed, showSuggestions, suggestions, visibleRecents, views]);

  // A new list starts at its first row; a shrinking list can never point past its end.
  useEffect(() => {
    setActiveIndex(0);
  }, [items]);

  const go = useCallback(
    (url: string, newTab: boolean) => {
      // A hit knows which site it belongs to; the panel follows it there before it navigates, so
      // the screen a click lands on already points at the right site.
      const site = siteFromUrl(url);
      if (site && !newTab) {
        selectSite(site);
      }
      if (newTab) {
        window.open(url, "_blank", "noopener");
      } else {
        router.push(url);
      }
      onClose();
    },
    [onClose, router, selectSite],
  );

  const activate = useCallback(
    (item: PaletteItem, newTab: boolean) => {
      if (item.kind === "recent" && item.query) {
        setQuery(item.query);
        inputRef.current?.focus();
        return;
      }
      if (item.url) {
        go(item.url, newTab && (item.kind === "hit" || item.kind === "suggestion" || item.kind === "view"));
      }
    },
    [go],
  );

  const jumpSection = useCallback(
    (direction: 1 | -1) => {
      const current = items[activeIndex];
      if (!current) {
        setActiveIndex(0);
        return;
      }
      const sectionsSeen = [...new Set(items.map((item) => item.section))];
      const at = sectionsSeen.indexOf(current.section);
      const next = sectionsSeen[at + direction];
      if (next === undefined) {
        setActiveIndex(direction === 1 ? 0 : Math.max(items.length - 1, 0));
        return;
      }
      const landing = items.findIndex((item) => item.section === next);
      if (landing >= 0) {
        setActiveIndex(landing);
      }
    },
    [activeIndex, items],
  );

  const onKeyDown = (event: React.KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setActiveIndex((index) => Math.min(index + 1, Math.max(items.length - 1, 0)));
      return;
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      setActiveIndex((index) => Math.max(index - 1, 0));
      return;
    }
    if (event.key === "Enter") {
      const item = items[activeIndex];
      if (item) {
        event.preventDefault();
        activate(item, event.metaKey || event.ctrlKey);
      }
      return;
    }
    if (event.key === "Tab") {
      event.preventDefault();
      jumpSection(event.shiftKey ? -1 : 1);
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      onClose();
      return;
    }
    if (event.key === "?") {
      event.preventDefault();
      setShortcutsOpen((open) => !open);
    }
  };

  const removeRecent = (value: string) => {
    setHidden(hideRecentSearch(value));
  };

  const clearRecents = () => {
    forgetHiddenRecents();
    setHidden([]);
    setRecents([]);
    // Clearing is the account's own list: the API forgets it, the browser forgets what it hid.
    clearRecentSearches().catch(() => undefined);
  };

  const highlight = (title: string, terms: string[]) =>
    splitHighlight(title, terms).map((part, index) =>
      part.match ? (
        <mark key={index} className="rounded bg-accent-soft px-0.5 text-accent-strong">
          {part.text}
        </mark>
      ) : (
        <span key={index}>{part.text}</span>
      ),
    );

  const hitRow = (hit: SearchHit, id: string, index: number) => {
    const Icon = PROVIDER_ICONS[hit.provider] ?? Search;
    const label = sections.find((candidate) => candidate.provider === hit.provider)?.label;
    return (
      <Option
        key={id}
        id={id}
        active={index === activeIndex}
        onActivate={() => activate(items[index], false)}
        onHover={() => setActiveIndex(index)}
        icon={<Icon className="size-3.5" />}
        trailing={
          label ? (
            <span className="shrink-0 rounded-md border border-line bg-canvas px-1.5 py-0.5 text-[10.5px] text-muted">
              {label}
            </span>
          ) : null
        }
      >
        <span className="truncate text-[13px] text-ink">{highlight(hit.title, result?.terms ?? [])}</span>
        {hit.subtitle ? (
          <span className="truncate text-[11.5px] text-muted">{hit.subtitle}</span>
        ) : null}
      </Option>
    );
  };

  const body = () => {
    if (searching && error) {
      return (
        <div className="flex flex-col items-center gap-2 px-4 py-8 text-center">
          <p className="text-[13px] font-medium">Search is unavailable</p>
          <p className="max-w-sm text-[12px] text-muted">{error}</p>
          <button
            type="button"
            data-palette-retry
            onClick={() => setAttempt((value) => value + 1)}
            className="mt-1 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
          >
            Try again
          </button>
        </div>
      );
    }

    if (searching && loading && result === null && !showSuggestions) {
      return (
        <div aria-live="polite" className="px-1 py-2">
          <span className="sr-only">Searching…</span>
          {[0, 1, 2].map((row) => (
            <div key={row} className="mb-1.5 h-9 animate-pulse rounded-lg bg-quiet-soft" />
          ))}
        </div>
      );
    }

    if (searching && result && result.total === 0) {
      return (
        <div className="flex flex-col items-center gap-2 px-4 py-8 text-center">
          <p className="text-[13px] font-medium">Nothing matched “{result.query}”</p>
          <p className="max-w-sm text-[12px] text-muted">
            Try fewer words, check the spelling, or narrow the search with type:page, type:media or
            type:sites.
          </p>
          {(result.hints ?? []).map((hint) => (
            <p key={hint} className="max-w-sm text-[11.5px] text-muted">
              {hint}
            </p>
          ))}
        </div>
      );
    }

    if (searching) {
      return (
        <>
          {(result?.hints ?? []).map((hint) => (
            <p key={hint} className="px-2 py-1 text-[11.5px] text-muted">
              {hint}
            </p>
          ))}
          {showSuggestions ? (
            <div className="mb-2">
              <SectionHeader label="Suggestions" count={(suggestions ?? []).length} />
              {(suggestions ?? []).slice(0, 5).map((suggestion, index) => {
                const item = items[index];
                if (!item) {
                  return null;
                }
                const Icon = PROVIDER_ICONS[suggestion.provider] ?? Search;
                return (
                  <Option
                    key={item.id}
                    id={item.id}
                    active={index === activeIndex}
                    onActivate={() => activate(item, false)}
                    onHover={() => setActiveIndex(index)}
                    icon={<Icon className="size-3.5" />}
                  >
                    <span className="truncate text-[13px] text-ink">
                      {suggestion.title}
                    </span>
                  </Option>
                );
              })}
            </div>
          ) : null}
          {sections.map((section, sectionIndex) => (
            <div key={section.provider} className="mb-2">
              <SectionHeader label={section.label} count={section.total} />
              {section.rows.map((hit, row) => {
                const id = `hit-${sectionIndex}-${row}`;
                const index = items.findIndex((item) => item.id === id);
                return hitRow(hit, id, index);
              })}
              {section.truncated ? (
                <SeeAllRow
                  id={`see-all-${sectionIndex}`}
                  label={`See all ${section.total} ${section.label}`}
                  active={items.findIndex((item) => item.id === `see-all-${sectionIndex}`) === activeIndex}
                  onActivate={() => {
                    const item = items.find((candidate) => candidate.id === `see-all-${sectionIndex}`);
                    if (item) {
                      activate(item, false);
                    }
                  }}
                  onHover={() =>
                    setActiveIndex(
                      items.findIndex((item) => item.id === `see-all-${sectionIndex}`),
                    )
                  }
                />
              ) : null}
            </div>
          ))}
        </>
      );
    }

    // Nothing typed yet (or fewer than two characters): what this account did recently.
    return (
      <>
        {visibleRecents.length > 0 ? (
          <div className="mb-2">
            <SectionHeader
              label="Recent searches"
              count={visibleRecents.length}
              action={
                <button
                  type="button"
                  onClick={clearRecents}
                  className="rounded-md px-1.5 py-0.5 text-[11px] text-muted transition hover:bg-quiet-soft hover:text-ink"
                >
                  Clear
                </button>
              }
            />
            {visibleRecents.map((value, index) => {
              const id = `recent-${index}`;
              return (
                <div key={id} className="flex items-center gap-1">
                  <div className="min-w-0 flex-1">
                    <Option
                      id={id}
                      active={index === activeIndex}
                      onActivate={() => {
                        setQuery(value);
                        inputRef.current?.focus();
                      }}
                      onHover={() => setActiveIndex(index)}
                      icon={<Clock className="size-3.5" />}
                    >
                      <span className="truncate text-[13px] text-ink">{value}</span>
                    </Option>
                  </div>
                  <button
                    type="button"
                    aria-label={`Remove “${value}” from recent searches`}
                    onClick={() => removeRecent(value)}
                    className="rounded-md p-1.5 text-muted transition hover:bg-quiet-soft hover:text-ink"
                  >
                    <X className="size-3.5" aria-hidden />
                  </button>
                </div>
              );
            })}
          </div>
        ) : null}

        {views.length > 0 ? (
          <div className="mb-2">
            <SectionHeader label="Recently viewed" count={views.length} />
            {views.map((view, index) => {
              const id = `view-${index}`;
              const itemIndex = items.findIndex((item) => item.id === id);
              return (
                <Option
                  key={id}
                  id={id}
                  active={itemIndex === activeIndex}
                  onActivate={() => {
                    const item = items.find((candidate) => candidate.id === id);
                    if (item) {
                      activate(item, false);
                    }
                  }}
                  onHover={() => setActiveIndex(itemIndex)}
                  icon={<History className="size-3.5" />}
                >
                  <span className="truncate text-[13px] text-ink">{view.label}</span>
                  <span className="truncate text-[11.5px] text-muted">{view.path}</span>
                </Option>
              );
            })}
          </div>
        ) : null}

        {visibleRecents.length === 0 && views.length === 0 ? (
          <div className="flex flex-col items-center gap-2 px-4 py-8 text-center">
            <p className="text-[13px] font-medium">Search the whole platform</p>
            <p className="max-w-sm text-[12px] text-muted">
              Two characters are enough. Pages, media and sites answer; narrow the search with
              type:page, type:media or type:sites.
            </p>
          </div>
        ) : null}
      </>
    );
  };

  if (!mounted) {
    return null;
  }

  const activeId = items[activeIndex]?.id;

  return createPortal(
    <div className="fixed inset-0 z-50" data-search-palette>
      <button
        type="button"
        aria-label="Close search"
        onClick={onClose}
        className="absolute inset-0 h-full w-full cursor-default bg-ink/40"
      />
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Search Omnion"
        // Escape closes from anywhere inside the palette — the input is not the only thing that
        // can hold focus (a row, the "Clear" button, the shortcut list).
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.preventDefault();
            onClose();
          }
        }}
        className="absolute inset-0 flex flex-col bg-surface lg:inset-x-auto lg:top-[8vh] lg:bottom-auto lg:left-1/2 lg:h-auto lg:max-h-[80vh] lg:w-[640px] lg:-translate-x-1/2 lg:rounded-xl lg:border lg:border-line lg:shadow-2xl"
      >
        <div className="flex items-center gap-2 border-b border-line px-3 py-2.5">
          <Search className="size-4 shrink-0 text-muted" aria-hidden />
          <input
            ref={inputRef}
            data-palette-input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={onKeyDown}
            placeholder="Search Omnion…"
            aria-label="Search Omnion"
            role="combobox"
            aria-expanded="true"
            aria-controls="omnion-palette-list"
            aria-activedescendant={activeId}
            aria-autocomplete="list"
            autoComplete="off"
            spellCheck={false}
            className="min-w-0 flex-1 bg-transparent text-[14px] text-ink outline-none placeholder:text-muted"
          />
          {loading ? (
            <Loader2 className="size-4 shrink-0 animate-spin text-muted" aria-hidden />
          ) : null}
          <button
            type="button"
            aria-label="Close search"
            onClick={onClose}
            className="rounded-md p-1.5 text-muted transition hover:bg-quiet-soft hover:text-ink lg:hidden"
          >
            <X className="size-4" aria-hidden />
          </button>
        </div>

        <div
          id="omnion-palette-list"
          role="listbox"
          aria-label="Search results"
          className="min-h-0 flex-1 overflow-y-auto px-1.5 py-2"
        >
          {body()}
        </div>

        {shortcutsOpen ? (
          <div className="border-t border-line bg-canvas/60 px-3 py-2">
            <p className="mb-1 text-[11px] font-medium tracking-wide text-muted uppercase">
              Keyboard
            </p>
            <ul className="grid grid-cols-1 gap-x-4 gap-y-0.5 text-[12px] text-muted sm:grid-cols-2">
              {[
                ["⌘K / Ctrl+K", "open or close this palette"],
                ["/", "focus the search box"],
                ["↑ ↓", "move through the rows"],
                ["Enter", "open the highlighted row"],
                ["⌘Enter", "open it in a new tab"],
                ["Tab / Shift+Tab", "jump to the next section"],
                ["Esc", "close the palette"],
              ].map(([keys, what]) => (
                <li key={keys} className="flex items-center justify-between gap-3">
                  <span>{what}</span>
                  <kbd className="rounded border border-line bg-surface px-1.5 py-0.5 text-[10.5px] text-ink">
                    {keys}
                  </kbd>
                </li>
              ))}
            </ul>
          </div>
        ) : null}

        <div className="flex items-center justify-between gap-2 border-t border-line px-3 py-2">
          <p className="flex items-center gap-2 truncate text-[11px] text-muted">
            <span>↑↓ navigate</span>
            <span className="hidden sm:inline">↵ open</span>
            <span className="hidden sm:inline">⌘↵ new tab</span>
            <span>esc close</span>
          </p>
          <button
            type="button"
            aria-expanded={shortcutsOpen}
            onClick={() => setShortcutsOpen((open) => !open)}
            className="shrink-0 rounded-md px-1.5 py-1 text-[11px] text-muted transition hover:bg-quiet-soft hover:text-ink"
            data-palette-shortcuts
          >
            Shortcuts ?
          </button>
        </div>
      </div>
    </div>,
    document.body,
  );
}

/** One section's title line: the label on the left, the count on the right. */
function SectionHeader({
  label,
  count,
  action,
}: {
  label: string;
  count: number;
  action?: ReactNode;
}) {
  return (
    <div className="flex items-center justify-between gap-2 px-2 py-1.5">
      <span className="text-[11px] font-medium tracking-wide text-muted uppercase">{label}</span>
      <span className="flex items-center gap-2">
        <span className="text-[11px] text-muted">{count}</span>
        {action}
      </span>
    </div>
  );
}

/** A section's "see all" row, styled like an option. */
function SeeAllRow({
  id,
  label,
  active,
  onActivate,
  onHover,
}: {
  id: string;
  label: string;
  active: boolean;
  onActivate: () => void;
  onHover: () => void;
}) {
  return (
    <div
      id={id}
      role="option"
      aria-selected={active}
      onClick={onActivate}
      onMouseMove={onHover}
      className={`flex min-h-11 cursor-pointer items-center justify-between gap-2 rounded-lg px-2 py-1.5 transition lg:min-h-10 ${
        active ? "bg-accent-soft" : "hover:bg-quiet-soft"
      }`}
    >
      <span className="truncate text-[12.5px] font-medium text-accent-strong">{label}</span>
      <span className="shrink-0 text-[11px] text-muted">opens the results screen</span>
    </div>
  );
}
