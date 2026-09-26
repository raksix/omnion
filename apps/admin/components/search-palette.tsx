"use client";

/**
 * The ⌘K palette — the command centre's front door (REQ-002 slice 2, extended by REQ-032).
 *
 * One box over the whole panel. What it holds decides what it offers:
 *
 * * nothing yet — the commands this account may run, the ones worth suggesting on the screen it
 *   is on, and its own recent searches and commands;
 * * a few characters — commands that match, then the index's answer grouped by provider;
 * * a leading `>` — commands only; `@` people, `#` sites, `:` settings narrow the search to that
 *   one kind; `?` alone opens the shortcut sheet.
 *
 * Everything the palette shows is real: the commands come from `GET /api/v1/commands` (already
 * filtered to the caller's permissions), the rows from `GET /api/v1/search`, and the history from
 * the account's own record. A provider whose screen does not exist contributes no section, so no
 * row is a click into nothing.
 *
 * Keyboard rules the component owns:
 *
 * * `↑`/`↓` walk the whole list across section boundaries, `Enter` opens the highlighted row,
 *   `⌘Enter`/`Ctrl+Enter` opens it in a new tab, `Tab`/`Shift+Tab` jump to the next or previous
 *   section, `Esc` closes and hands focus back to whatever had it before the palette opened.
 * * Under two characters (in the unfiltered mode) nothing is searched; the command list and the
 *   recent lists are shown instead.
 * * A request that is overtaken by a newer keystroke is dropped, so a slow answer can never
 *   replace the rows of a faster, newer one.
 */
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";

import {
  Clock,
  Command as CommandIcon,
  FilePlus,
  FileText,
  Globe,
  History,
  Images,
  LayoutDashboard,
  Loader2,
  Search,
  SlidersHorizontal,
  Sparkles,
  X,
  type LucideIcon,
} from "lucide-react";
import { usePathname, useRouter } from "next/navigation";

import {
  ApiError,
  clearCommandRecents,
  fetchCommandContext,
  fetchCommandRecents,
  fetchCommands,
  recordCommandRecent,
  searchAll,
  suggestTitles,
  type CommandInfo,
  type CommandRecent,
  type SearchHit,
  type SearchResult,
  type SearchSuggestion,
} from "@/lib/api";
import {
  matchCommands,
  modeLabel,
  modePlaceholder,
  modeSearches,
  modeTypes,
  readMode,
  recentRows,
  type PaletteMode,
} from "@/lib/command-center";
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
/** How many recent rows the palette lists. */
const RECENT_LIMIT = 8;
/** Hits per search request: enough for five rows of every provider that answers. */
const PER_PAGE = 50;
/** Commands listed while the box is empty. */
const COMMAND_LIMIT = 8;
/** Commands listed above the results while searching. */
const COMMAND_INLINE = 5;

const PROVIDER_ICONS: Record<string, LucideIcon> = {
  pages: FileText,
  media: Images,
  sites: Globe,
};

/** The panel's own icon names, as the registry spells them. */
const COMMAND_ICONS: Record<string, LucideIcon> = {
  "layout-dashboard": LayoutDashboard,
  search: Search,
  "sliders-horizontal": SlidersHorizontal,
  "file-text": FileText,
  "file-plus": FilePlus,
  images: Images,
  globe: Globe,
  sparkles: Sparkles,
};

/** One row the keyboard can land on. */
type PaletteItem = {
  id: string;
  kind:
    | "command"
    | "hit"
    | "suggestion"
    | "see-all"
    | "everywhere"
    | "recent-query"
    | "recent-command"
    | "view";
  /** Section the row belongs to; `-1` for the rows that sit under every section. */
  section: number;
  label: string;
  /** Where activating the row goes. */
  url?: string;
  /** The query a recent row re-runs. */
  query?: string;
  /** The registry id a command row runs. */
  commandId?: string;
  /** How many results the search answered with, for the history entry. */
  resultCount?: number | null;
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
  const pathname = usePathname();
  const { selectSite } = useSites();
  const [mounted, setMounted] = useState(false);
  const [query, setQuery] = useState(initialQuery);
  const [result, setResult] = useState<SearchResult | null>(null);
  const [suggestions, setSuggestions] = useState<SearchSuggestion[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [commands, setCommands] = useState<CommandInfo[] | null>(null);
  const [contextCommands, setContextCommands] = useState<CommandInfo[]>([]);
  const [commandsError, setCommandsError] = useState(false);
  const [recents, setRecents] = useState<CommandRecent[] | null>(null);
  const [hidden, setHidden] = useState<string[]>([]);
  const [views, setViews] = useState<RecentView[]>([]);
  const [activeIndex, setActiveIndex] = useState(0);
  const [shortcutsOpen, setShortcutsOpen] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const [registryAttempt, setRegistryAttempt] = useState(0);

  const inputRef = useRef<HTMLInputElement | null>(null);
  // Every request carries the number of the keystroke it belongs to; an answer whose number is
  // no longer the latest is dropped instead of painted.
  const tokenRef = useRef(0);

  const reading = readMode(query);
  const mode = reading.mode;
  const term = reading.term;

  useEffect(() => {
    setMounted(true);
  }, []);

  // Escape hands focus back to whatever had it before the palette opened; the effect's cleanup
  // runs when the palette unmounts, which is exactly when that should happen.
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    return () => {
      previous?.focus?.();
    };
  }, []);

  // Opening: the browser's own trail, then the commands this account may run, the suggestions for
  // this screen and its own history. Each answer is independent — a failing command list never
  // blanks the history and the other way round.
  useEffect(() => {
    setHidden(readHiddenRecents());
    setViews(readRecentViews());
    let cancelled = false;

    const route = pathname || "/";
    Promise.allSettled([
      fetchCommands(),
      fetchCommandContext(route),
      fetchCommandRecents(),
    ]).then(([commandsOutcome, contextOutcome, recentsOutcome]) => {
      if (cancelled) {
        return;
      }
      if (commandsOutcome.status === "fulfilled") {
        setCommands(commandsOutcome.value);
        setCommandsError(false);
      } else {
        setCommands([]);
        setCommandsError(true);
      }
      setContextCommands(contextOutcome.status === "fulfilled" ? contextOutcome.value : []);
      setRecents(recentsOutcome.status === "fulfilled" ? recentsOutcome.value : []);
    });

    return () => {
      cancelled = true;
    };
  }, [pathname, registryAttempt]);

  useEffect(() => {
    if (mounted) {
      inputRef.current?.focus();
    }
  }, [mounted]);

  // The search itself: a short debounce, then the index and the suggestions together. A narrowed
  // mode (`#sites`) searches from the first character; the open mode waits for two.
  useEffect(() => {
    const minimum = mode === "all" ? MIN_QUERY : 1;
    if (!modeSearches(mode) || term.length < minimum) {
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

    const types = modeTypes(mode);
    const timer = setTimeout(() => {
      Promise.allSettled([
        searchAll({ q: term, per_page: PER_PAGE, filters: types ? { type: types } : undefined }),
        suggestTitles(term),
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
          setError(cause instanceof ApiError ? cause.message : "The search could not be completed.");
        }
        setSuggestions(suggestOutcome.status === "fulfilled" ? suggestOutcome.value : []);
        setLoading(false);
      });
    }, DEBOUNCE_MS);

    return () => clearTimeout(timer);
  }, [term, mode, attempt]);

  const sections = useMemo(() => buildSections(result), [result]);

  const browsing = term.length === 0;
  const minimum = mode === "all" ? MIN_QUERY : 1;
  const searching = modeSearches(mode) && term.length >= minimum;

  // The commands the box offers right now: the suggestions for this screen first while it is
  // empty, the matches once something is typed.
  const commandSuggestions = useMemo(() => {
    if (!commands || !modeSearches(mode)) {
      return [];
    }
    if (mode === "people" || mode === "sites" || mode === "settings") {
      return [];
    }
    if (browsing) {
      const seen = new Set<string>();
      const merged: CommandInfo[] = [];
      for (const command of [...(contextCommands ?? []), ...commands]) {
        if (seen.has(command.id)) {
          continue;
        }
        seen.add(command.id);
        merged.push(command);
        if (merged.length >= COMMAND_LIMIT) {
          break;
        }
      }
      return merged;
    }
    return matchCommands(commands, term, mode === "commands" ? COMMAND_LIMIT + 4 : COMMAND_INLINE);
  }, [commands, contextCommands, browsing, term, mode]);

  // The store keeps every run of a query; the palette shows each one once, minus the ones this
  // browser asked to forget.
  const visibleRecents = useMemo(() => {
    const rows = recentRows(recents ?? [], RECENT_LIMIT + 2);
    return rows
      .filter((row) => row.kind !== "query" || !hidden.includes(row.query))
      .slice(0, RECENT_LIMIT);
  }, [recents, hidden]);

  const showSuggestions = searching && result === null && (suggestions?.length ?? 0) > 0;

  const items = useMemo<PaletteItem[]>(() => {
    const list: PaletteItem[] = [];
    let section = 0;

    const commandItems = (commands: CommandInfo[]) => {
      commands.forEach((command, index) => {
        list.push({
          id: `command-${index}`,
          kind: "command",
          section,
          label: command.title,
          url: command.route,
          commandId: command.id,
        });
      });
      if (commands.length > 0) {
        section += 1;
      }
    };

    if (mode === "help") {
      return list;
    }

    if (searching) {
      commandItems(commandSuggestions);
      sections.forEach((group, index) => {
        group.rows.forEach((hit, row) => {
          list.push({
            id: `hit-${index}-${row}`,
            kind: "hit",
            section,
            label: hit.title || hit.url,
            url: hit.url,
            resultCount: result?.total ?? null,
          });
        });
        if (group.truncated) {
          list.push({
            id: `see-all-${index}`,
            kind: "see-all",
            section,
            label: `See all ${group.total} ${group.label}`,
            url: sectionUrl(term, group.provider),
            resultCount: group.total,
          });
        }
        section += 1;
      });
      if (showSuggestions) {
        (suggestions ?? []).slice(0, 5).forEach((suggestion, index) => {
          list.push({
            id: `suggestion-${index}`,
            kind: "suggestion",
            section,
            label: suggestion.title || suggestion.url,
            url: suggestion.url,
            // Suggestions answer before the index does, so there is no result count to carry.
            resultCount: null,
          });
        });
        section += 1;
      }
      list.push({
        id: "everywhere",
        kind: "everywhere",
        section: -1,
        label: `See all results for “${term}”`,
        url: resultsUrl(term),
        resultCount: result?.total ?? null,
      });
      return list;
    }

    // Nothing typed yet (or a mode that searches nothing yet): the commands this account may run
    // on this screen, then its own history.
    commandItems(commandSuggestions);
    visibleRecents.forEach((row, index) => {
      if (row.kind === "query") {
        list.push({
          id: `recent-${index}`,
          kind: "recent-query",
          section,
          label: row.label,
          query: row.query,
          resultCount: row.resultCount,
        });
      } else {
        list.push({
          id: `recent-command-${index}`,
          kind: "recent-command",
          section,
          label: row.label,
          url: row.route,
          commandId: row.commandId,
        });
      }
    });
    if (visibleRecents.length > 0) {
      section += 1;
    }
    views.forEach((view, index) => {
      list.push({
        id: `view-${index}`,
        kind: "view",
        section,
        label: view.label,
        url: view.path,
      });
    });
    return list;
  }, [
    mode,
    searching,
    commandSuggestions,
    sections,
    showSuggestions,
    suggestions,
    term,
    result,
    visibleRecents,
    views,
  ]);

  const itemIndex = useMemo(
    () => new Map(items.map((item, index) => [item.id, index])),
    [items],
  );

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

  const rememberQuery = useCallback((text: string, count: number | null | undefined) => {
    const trimmed = text.trim();
    if (!trimmed) {
      return;
    }
    recordCommandRecent({
      kind: "query",
      query: trimmed,
      result_count: typeof count === "number" ? count : undefined,
    }).catch(() => undefined);
  }, []);

  const rememberCommand = useCallback((commandId: string) => {
    recordCommandRecent({ kind: "command", command_id: commandId }).catch(() => undefined);
  }, []);

  const activate = useCallback(
    (item: PaletteItem, newTab: boolean) => {
      if (item.kind === "recent-query" && item.query) {
        setQuery(item.query);
        inputRef.current?.focus();
        return;
      }
      if (item.kind === "command" && item.commandId) {
        // An action command runs; a navigation command only opens a screen. Either way the run is
        // remembered, and the owning screen does the writing.
        rememberCommand(item.commandId);
      }
      if (
        (item.kind === "hit" || item.kind === "see-all" || item.kind === "everywhere") &&
        term
      ) {
        // Committing to a result set is what makes a search worth keeping in the history.
        rememberQuery(term, item.resultCount);
      }
      if (item.url) {
        go(
          item.url,
          newTab &&
            (item.kind === "hit" ||
              item.kind === "suggestion" ||
              item.kind === "view" ||
              item.kind === "command" ||
              item.kind === "recent-command"),
        );
      }
    },
    [go, rememberCommand, rememberQuery, term],
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
    // Clearing is the account's own record: the API forgets it, the browser forgets what it hid.
    clearCommandRecents().catch(() => undefined);
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

  const rowIndex = (id: string) => itemIndex.get(id) ?? -1;

  const commandRow = (command: CommandInfo, index: number) => {
    const id = `command-${index}`;
    const Icon = COMMAND_ICONS[command.icon] ?? CommandIcon;
    return (
      <Option
        key={id}
        id={id}
        active={rowIndex(id) === activeIndex}
        onActivate={() => {
          const item = items[rowIndex(id)];
          if (item) {
            activate(item, false);
          }
        }}
        onHover={() => setActiveIndex(rowIndex(id))}
        icon={<Icon className="size-3.5" />}
        trailing={
          <span className="shrink-0 rounded-md border border-line bg-canvas px-1.5 py-0.5 text-[10.5px] text-muted">
            Command
          </span>
        }
      >
        <span className="truncate text-[13px] text-ink">{command.title}</span>
        <span className="truncate text-[11.5px] text-muted">{command.hint}</span>
      </Option>
    );
  };

  const hitRow = (hit: SearchHit, id: string, label: string | null) => (
    <Option
      key={id}
      id={id}
      active={rowIndex(id) === activeIndex}
      onActivate={() => {
        const item = items[rowIndex(id)];
        if (item) {
          activate(item, false);
        }
      }}
      onHover={() => setActiveIndex(rowIndex(id))}
      icon={(() => {
        const Icon = PROVIDER_ICONS[hit.provider] ?? Search;
        return <Icon className="size-3.5" />;
      })()}
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

  const helpPanel = (
    <div data-palette-help className="px-3 py-3">
      <p className="mb-2 text-[11px] font-medium tracking-wide text-muted uppercase">Keyboard</p>
      <ul className="grid grid-cols-1 gap-x-6 gap-y-1 text-[12.5px] text-muted sm:grid-cols-2">
        {[
          ["⌘K / Ctrl+K", "open or close this palette"],
          ["/", "focus the search box"],
          ["↑ ↓", "move through the rows"],
          ["Enter", "open the highlighted row"],
          ["⌘Enter", "open it in a new tab"],
          ["Tab / Shift+Tab", "jump to the next section"],
          ["Esc", "close the palette"],
          [">", "commands only"],
          ["@", "people only"],
          ["#", "sites only"],
          [":", "settings only"],
          ["?", "this list"],
        ].map(([keys, what]) => (
          <li key={keys} className="flex items-center justify-between gap-3">
            <span>{what}</span>
            <kbd className="rounded border border-line bg-surface px-1.5 py-0.5 text-[10.5px] text-ink">
              {keys}
            </kbd>
          </li>
        ))}
      </ul>
      <p className="mt-3 text-[12px] text-muted">
        Commands run the screens of the features your account may open; a command you may not run
        is not listed at all.
      </p>
    </div>
  );

  const body = () => {
    if (mode === "help") {
      return helpPanel;
    }

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

    if (mode === "commands" && commandSuggestions.length === 0) {
      return (
        <div className="flex flex-col items-center gap-2 px-4 py-8 text-center">
          <p className="text-[13px] font-medium">No command matches “{term}”</p>
          <p className="max-w-sm text-[12px] text-muted">
            Commands come from the features your account may open. Try fewer words, or clear the
            prefix to search the content instead.
          </p>
        </div>
      );
    }

    if (searching && !modeTypes(mode) && result && result.total === 0 && commandSuggestions.length === 0) {
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

          {commandSuggestions.length > 0 ? (
            <div className="mb-2">
              <SectionHeader label="Commands" count={commandSuggestions.length} />
              {commandSuggestions.map((command, index) => commandRow(command, index))}
            </div>
          ) : null}

          {commandsError ? (
            <div className="mb-2 rounded-lg border border-line bg-canvas px-2 py-2 text-[12px] text-muted">
              <span>The command list could not be loaded. </span>
              <button
                type="button"
                data-palette-commands-retry
                onClick={() => setRegistryAttempt((value) => value + 1)}
                className="font-medium text-accent-strong underline"
              >
                Try again
              </button>
            </div>
          ) : null}

          {showSuggestions ? (
            <div className="mb-2">
              <SectionHeader label="Suggestions" count={(suggestions ?? []).length} />
              {(suggestions ?? []).slice(0, 5).map((suggestion, index) => {
                const id = `suggestion-${index}`;
                const Icon = PROVIDER_ICONS[suggestion.provider] ?? Search;
                return (
                  <Option
                    key={id}
                    id={id}
                    active={rowIndex(id) === activeIndex}
                    onActivate={() => {
                      const item = items[rowIndex(id)];
                      if (item) {
                        activate(item, false);
                      }
                    }}
                    onHover={() => setActiveIndex(rowIndex(id))}
                    icon={<Icon className="size-3.5" />}
                  >
                    <span className="truncate text-[13px] text-ink">{suggestion.title}</span>
                  </Option>
                );
              })}
            </div>
          ) : null}

          {sections.map((group, index) => (
            <div key={group.provider} className="mb-2">
              <SectionHeader label={group.label} count={group.total} />
              {group.rows.map((hit, row) => hitRow(hit, `hit-${index}-${row}`, group.label))}
              {group.truncated ? (
                <SeeAllRow
                  id={`see-all-${index}`}
                  label={`See all ${group.total} ${group.label}`}
                  active={rowIndex(`see-all-${index}`) === activeIndex}
                  onActivate={() => {
                    const item = items[rowIndex(`see-all-${index}`)];
                    if (item) {
                      activate(item, false);
                    }
                  }}
                  onHover={() => setActiveIndex(rowIndex(`see-all-${index}`))}
                />
              ) : null}
            </div>
          ))}

          {mode && !modeTypes(mode) ? null : (
            <p className="px-2 py-1 text-[11.5px] text-muted">
              Narrowed to {modeLabel(mode).toLowerCase()}.
            </p>
          )}
        </>
      );
    }

    // Nothing typed yet (or the mode holds no term): the commands this account may run on this
    // screen, then its own history, then where it has been.
    return (
      <>
        {commandSuggestions.length > 0 ? (
          <div className="mb-2">
            <SectionHeader
              label={browsing && mode === "all" ? "Suggested for this screen" : "Commands"}
              count={commandSuggestions.length}
            />
            {commandSuggestions.map((command, index) => commandRow(command, index))}
          </div>
        ) : null}

        {commandsError ? (
          <div className="mb-2 rounded-lg border border-line bg-canvas px-2 py-2 text-[12px] text-muted">
            <span>The command list could not be loaded. </span>
            <button
              type="button"
              data-palette-commands-retry
              onClick={() => setRegistryAttempt((value) => value + 1)}
              className="font-medium text-accent-strong underline"
            >
              Try again
            </button>
          </div>
        ) : null}

        {visibleRecents.length > 0 ? (
          <div className="mb-2">
            <SectionHeader
              label="Recent"
              count={visibleRecents.length}
              action={
                <button
                  type="button"
                  data-palette-clear-recents
                  onClick={clearRecents}
                  className="rounded-md px-1.5 py-0.5 text-[11px] text-muted transition hover:bg-quiet-soft hover:text-ink"
                >
                  Clear
                </button>
              }
            />
            {visibleRecents.map((row, index) => {
              const id = row.kind === "query" ? `recent-${index}` : `recent-command-${index}`;
              const icon =
                row.kind === "query" ? (
                  <Clock className="size-3.5" />
                ) : (
                  <CommandIcon className="size-3.5" />
                );
              const trailing =
                row.kind === "query" ? (
                  row.resultCount !== null ? (
                    <span className="shrink-0 text-[11px] text-muted">{row.resultCount} results</span>
                  ) : null
                ) : (
                  <span className="shrink-0 rounded-md border border-line bg-canvas px-1.5 py-0.5 text-[10.5px] text-muted">
                    Command
                  </span>
                );
              return (
                <div key={id} className="flex items-center gap-1">
                  <div className="min-w-0 flex-1">
                    <Option
                      id={id}
                      active={rowIndex(id) === activeIndex}
                      onActivate={() => {
                        const item = items[rowIndex(id)];
                        if (item) {
                          activate(item, false);
                        }
                      }}
                      onHover={() => setActiveIndex(rowIndex(id))}
                      icon={icon}
                      trailing={trailing}
                    >
                      <span className="truncate text-[13px] text-ink">{row.label}</span>
                      {row.kind === "command" ? (
                        <span className="truncate text-[11.5px] text-muted">{row.route}</span>
                      ) : null}
                    </Option>
                  </div>
                  {row.kind === "query" ? (
                    <button
                      type="button"
                      aria-label={`Remove “${row.query}” from recent searches`}
                      onClick={() => removeRecent(row.query)}
                      className="rounded-md p-1.5 text-muted transition hover:bg-quiet-soft hover:text-ink"
                    >
                      <X className="size-3.5" aria-hidden />
                    </button>
                  ) : null}
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
              return (
                <Option
                  key={id}
                  id={id}
                  active={rowIndex(id) === activeIndex}
                  onActivate={() => {
                    const item = items[rowIndex(id)];
                    if (item) {
                      activate(item, false);
                    }
                  }}
                  onHover={() => setActiveIndex(rowIndex(id))}
                  icon={<History className="size-3.5" />}
                >
                  <span className="truncate text-[13px] text-ink">{view.label}</span>
                  <span className="truncate text-[11.5px] text-muted">{view.path}</span>
                </Option>
              );
            })}
          </div>
        ) : null}

        {commandSuggestions.length === 0 && visibleRecents.length === 0 && views.length === 0 ? (
          <div className="flex flex-col items-center gap-2 px-4 py-8 text-center">
            <p className="text-[13px] font-medium">Search the whole platform</p>
            <p className="max-w-sm text-[12px] text-muted">
              Two characters are enough. Pages, media and sites answer; narrow the search with
              type:page, type:media or type:sites — or type &gt; for the commands you may run.
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
  const chipLabel = mode === "all" ? null : modeLabel(mode);

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
          {chipLabel ? (
            <span
              data-palette-mode={mode}
              className="flex shrink-0 items-center gap-1 rounded-md border border-line bg-accent-soft px-1.5 py-0.5 text-[11px] font-medium text-accent-strong"
            >
              {chipLabel}
              <button
                type="button"
                aria-label={`Leave ${chipLabel} mode`}
                data-palette-mode-clear
                onClick={() => setQuery(term)}
                className="rounded p-0.5 transition hover:bg-surface"
              >
                <X className="size-3" aria-hidden />
              </button>
            </span>
          ) : null}
          <input
            ref={inputRef}
            data-palette-input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={onKeyDown}
            placeholder={modePlaceholder(mode)}
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

        {shortcutsOpen && mode !== "help" ? (
          <div className="max-h-56 overflow-y-auto border-t border-line bg-canvas/60 px-3 py-2">
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
                [">", "commands only"],
                ["@", "people only"],
                ["#", "sites only"],
                [":", "settings only"],
                ["?", "this list"],
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
            <span className="hidden sm:inline">&gt; commands</span>
            <span>esc close</span>
          </p>
          {mode === "help" ? (
            <button
              type="button"
              data-palette-mode-clear
              onClick={() => setQuery("")}
              className="shrink-0 rounded-md px-1.5 py-1 text-[11px] text-muted transition hover:bg-quiet-soft hover:text-ink"
            >
              Close help
            </button>
          ) : (
            <button
              type="button"
              aria-expanded={shortcutsOpen}
              onClick={() => setShortcutsOpen((open) => !open)}
              className="shrink-0 rounded-md px-1.5 py-1 text-[11px] text-muted transition hover:bg-quiet-soft hover:text-ink"
              data-palette-shortcuts
            >
              Shortcuts ?
            </button>
          )}
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
