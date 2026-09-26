"use client";

/**
 * The ⌘K palette — the command centre's front door (REQ-002 slice 2, extended by REQ-032).
 *
 * One box over the whole panel. What it holds decides what it offers:
 *
 * * nothing yet — the commands this account may run, the ones worth suggesting on the screen it
 *   is on, and its own recent searches and commands;
 * * a few characters — commands that match, then the index's answer grouped by provider;
 * * a leading `>` — commands only, no index call at all; `@` people, `#` sites, `:` settings
 *   narrow the search to that one kind; `?` alone opens the shortcut sheet.
 *
 * Everything the palette shows is real: the commands come from `GET /api/v1/commands` (already
 * filtered to the caller's permissions), the rows from `GET /api/v1/search`, and the history from
 * the account's own record. A provider whose screen does not exist contributes no section, so no
 * row is a click into nothing.
 *
 * **Action commands (slice 3).** A command whose `kind` is `action` does not open a screen: it
 * runs through `POST /api/v1/commands/{id}/run`, which re-checks the command's own permission and
 * executes it through the owning service. A command that carries `confirm` shows its question
 * first — the card above the list, `↵` to run and `esc` to take it back — and the answer the
 * owning service gives is what the result card prints: the palette never invents an outcome.
 *
 * **Federated answers (slice 2).** The palette asks each provider its own question, in parallel,
 * with `history=false` (typing is not a search worth remembering — committing to a result is,
 * and that is recorded separately). Each group therefore has its own life: it shows a skeleton
 * while its request is in flight, its rows when it answers, and its own retryable error — with
 * the failure's code in a tooltip — when its request fails. One slow or failing provider never
 * blanks the others, which is what "result groups stream independently" means. A whole-index
 * count rides along so the "see all results" row and the "outside your permissions" line are the
 * API's own numbers rather than a sum of whatever happened to arrive.
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
  CheckCircle,
  Clock,
  Command as CommandIcon,
  Eraser,
  FilePlus,
  FileText,
  Globe,
  History,
  Images,
  Loader2,
  LayoutDashboard,
  RefreshCw,
  Search,
  SlidersHorizontal,
  Sparkles,
  TriangleAlert,
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
  runCommand,
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
  PALETTE_PROVIDER_ORDER,
  askAiUrl,
  groupFromAnswer,
  groupFromError,
  groupsAllFailed,
  groupsTotal,
  paletteProvider,
  pendingGroups,
  resultsUrl,
  sectionUrl,
  siteFromUrl,
  splitHighlight,
  withGroup,
  type PaletteGroup,
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
const DEBOUNCE_MS = 100;
/** How many recent rows the palette lists. */
const RECENT_LIMIT = 8;
/** Hits asked of one provider: enough for five rows plus the "see all" verdict. */
const PER_GROUP = 6;
/** How many providers are asked at once; the rest follow in waves. */
const GROUP_CONCURRENCY = 3;
/** The whole-index count asks for one row; it is the totals that matter. */
const AGGREGATE_PER_PAGE = 1;
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
  "refresh-cw": RefreshCw,
  eraser: Eraser,
};

/** What the palette knows about an action command that is running, or has just run. */
type RunState =
  | { status: "idle" }
  | { status: "running"; command: CommandInfo }
  | { status: "done"; command: CommandInfo; message: string }
  | { status: "failed"; command: CommandInfo; message: string; code: string };

/** The failure of one request, in the pieces a group row shows. */
type GroupFailure = { code: string; status: number; message: string };

/** Read one caught failure as a group failure. */
function asGroupFailure(cause: unknown): GroupFailure {
  if (cause instanceof ApiError) {
    return { code: cause.code, status: cause.status, message: cause.message };
  }
  return {
    code: "network_error",
    status: 0,
    message: "The Omnion API could not be reached.",
  };
}

/** One row the keyboard can land on. */
type PaletteItem = {
  id: string;
  kind:
    | "command"
    | "hit"
    | "suggestion"
    | "see-all"
    | "everywhere"
    | "ask-ai"
    | "recent-query"
    | "recent-command"
    | "view";
  /** Section the row belongs to; `-1` for the rows that sit under every section. */
  section: number;
  label: string;
  /** Supporting line the action rows carry. */
  hint?: string;
  /** Where activating the row goes. */
  url?: string;
  /** The query a recent row re-runs. */
  query?: string;
  /** The registry id a command row runs. */
  commandId?: string;
  /** What running the command does (`navigate` opens `url`; `action` runs through the API). */
  commandKind?: "navigate" | "action";
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
  action,
  children,
  trailing,
}: {
  id: string;
  active: boolean;
  onActivate: () => void;
  onHover: () => void;
  icon: ReactNode;
  /** `data-palette-action` value, when the row is one of the palette's own actions. */
  action?: string;
  children: ReactNode;
  trailing?: ReactNode;
}) {
  return (
    <div
      id={id}
      role="option"
      aria-selected={active}
      data-palette-action={action}
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
  /** One entry per provider this search asks; each one loads, answers and fails on its own. */
  const [groups, setGroups] = useState<PaletteGroup[]>([]);
  /** The whole-index answer: its totals, its hints and the count outside the caller's scope. */
  const [answer, setAnswer] = useState<SearchResult | null>(null);
  /** The words the API matched on, for the row highlighting (any group answers with them). */
  const [terms, setTerms] = useState<string[]>([]);
  const [suggestions, setSuggestions] = useState<SearchSuggestion[] | null>(null);
  const [commands, setCommands] = useState<CommandInfo[] | null>(null);
  const [contextCommands, setContextCommands] = useState<CommandInfo[]>([]);
  const [commandsError, setCommandsError] = useState(false);
  const [recents, setRecents] = useState<CommandRecent[] | null>(null);
  const [hidden, setHidden] = useState<string[]>([]);
  const [views, setViews] = useState<RecentView[]>([]);
  const [activeIndex, setActiveIndex] = useState(0);
  const [shortcutsOpen, setShortcutsOpen] = useState(false);
  /** The action command whose confirmation card is showing; nothing runs until the caller says yes. */
  const [pendingRun, setPendingRun] = useState<CommandInfo | null>(null);
  /** The action command that is running, or the outcome of the last one. */
  const [runState, setRunState] = useState<RunState>({ status: "idle" });
  const [attempt, setAttempt] = useState(0);
  const [registryAttempt, setRegistryAttempt] = useState(0);

  const inputRef = useRef<HTMLInputElement | null>(null);
  // Every request carries the number of the keystroke it belongs to; an answer whose number is
  // no longer the latest is dropped instead of painted.
  const tokenRef = useRef(0);

  const reading = readMode(query);
  const mode = reading.mode;
  const term = reading.term;

  const minimum = mode === "all" ? MIN_QUERY : 1;
  const hunting = modeSearches(mode) && mode !== "commands" && term.length >= minimum;

  // Which providers this search asks: the one a narrowed mode names, or the whole index. A mode
  // with nothing to search (`>` commands, `?` help) asks nobody.
  const targets = useMemo<string[]>(() => {
    if (!modeSearches(mode) || mode === "commands") {
      return [];
    }
    const narrowed = modeTypes(mode);
    return narrowed ? [narrowed] : [...PALETTE_PROVIDER_ORDER];
  }, [mode]);

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

  // One provider's question, answered into its own group. Both the first pass and a group's own
  // retry come through here, so a retry can never overwrite a newer keystroke's answers.
  const ask = useCallback(
    (provider: string, text: string, token: number) =>
      searchAll({
        q: text,
        per_page: PER_GROUP,
        history: false,
        filters: { type: provider },
      })
        .then((group) => {
          if (tokenRef.current !== token) {
            return;
          }
          setGroups((current) => withGroup(current, groupFromAnswer(provider, group)));
          setTerms((current) => (current.length > 0 ? current : group.terms));
        })
        .catch((cause: unknown) => {
          if (tokenRef.current !== token) {
            return;
          }
          setGroups((current) => withGroup(current, groupFromError(provider, asGroupFailure(cause))));
        }),
    [],
  );

  // The search itself: a short debounce, then every provider's own question in parallel. A
  // narrowed mode searches from the first character; the open mode waits for two.
  useEffect(() => {
    if (!hunting) {
      tokenRef.current += 1;
      setGroups([]);
      setAnswer(null);
      setTerms([]);
      setSuggestions(null);
      return;
    }

    const token = tokenRef.current + 1;
    tokenRef.current = token;
    setGroups(pendingGroups(targets));
    setAnswer(null);
    setTerms([]);

    const text = term;
    const timer = setTimeout(() => {
      // The groups are asked in small waves. A burst of nine parallel requests queues against the
      // browser's own connection budget and starves everything else the page is fetching (a
      // Next.js navigation's prefetches included), so three go out at a time and the rest follow
      // as they answer — the sections still stream; the pipe is not hogged.
      const waves: string[][] = [];
      for (let index = 0; index < targets.length; index += GROUP_CONCURRENCY) {
        waves.push(targets.slice(index, index + GROUP_CONCURRENCY));
      }
      void (async () => {
        for (const wave of waves) {
          await Promise.all(wave.map((provider) => ask(provider, text, token)));
          if (tokenRef.current !== token) {
            return;
          }
        }
        // The whole-index count rides behind the first wave: the "see all results" total and the
        // number of results outside this account's permissions. It is a nicety, so it is asked
        // last and its failure costs the palette nothing — the sections stand alone.
        if (targets.length > 1) {
          searchAll({ q: text, per_page: AGGREGATE_PER_PAGE, history: false })
            .then((result) => {
              if (tokenRef.current === token) {
                setAnswer(result);
                setTerms((current) => (current.length > 0 ? current : result.terms));
              }
            })
            .catch(() => undefined);
        }
      })();

      suggestTitles(text)
        .then((rows) => {
          if (tokenRef.current === token) {
            setSuggestions(rows);
          }
        })
        .catch(() => {
          if (tokenRef.current === token) {
            setSuggestions([]);
          }
        });
    }, DEBOUNCE_MS);

    return () => clearTimeout(timer);
  }, [term, hunting, targets, attempt, ask]);

  /** Retry one provider: only its own group goes back to loading. */
  const retryGroup = useCallback(
    (provider: string) => {
      const token = tokenRef.current;
      setGroups((current) => withGroup(current, pendingGroups([provider])[0]));
      ask(provider, term, token);
    },
    [ask, term],
  );

  const allFailed = hunting && groupsAllFailed(groups);
  const noAnswerYet = groups.every((group) => group.status !== "ready");
  const showSuggestions = hunting && noAnswerYet && (suggestions?.length ?? 0) > 0;
  const firstPaint = hunting && noAnswerYet && !showSuggestions;

  // The commands the box offers right now: everything this account may run (suggestions for this
  // screen first) while the box is empty, the matches once something is typed.
  const commandSuggestions = useMemo(() => {
    if (!commands || !modeSearches(mode)) {
      return [];
    }
    if (mode === "people" || mode === "sites" || mode === "settings") {
      return [];
    }
    if (term.length === 0) {
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
    // A `>` search is a search for a command: it gets the longer list.
    return matchCommands(commands, term, mode === "commands" ? COMMAND_LIMIT : COMMAND_INLINE);
  }, [commands, contextCommands, term, mode]);

  /** The registry as a lookup, so a row knows what its command does today. */
  const commandById = useMemo(
    () => new Map((commands ?? []).map((command) => [command.id, command])),
    [commands],
  );

  // The store keeps every run of a query; the palette shows each one once, minus the ones this
  // browser asked to forget.
  const visibleRecents = useMemo(() => {
    const rows = recentRows(recents ?? [], RECENT_LIMIT + 2);
    return rows
      .filter((row) => row.kind !== "query" || !hidden.includes(row.query))
      .slice(0, RECENT_LIMIT);
  }, [recents, hidden]);

  const hiddenTotal = answer?.hidden_total ?? 0;
  const resultTotal = answer?.total ?? groupsTotal(groups);
  const nothingMatched =
    hunting &&
    !allFailed &&
    groups.length > 0 &&
    groups.every((group) => group.status === "ready" && group.rows.length === 0) &&
    commandSuggestions.length === 0 &&
    !showSuggestions;

  const items = useMemo<PaletteItem[]>(() => {
    const list: PaletteItem[] = [];
    let section = 0;

    const commandItems = (rows: CommandInfo[]) => {
      rows.forEach((command, index) => {
        const action = command.kind === "action";
        list.push({
          id: `command-${index}`,
          kind: "command",
          section,
          label: command.title,
          // An action runs; only a navigation command has somewhere to go.
          url: action ? undefined : command.route,
          commandId: command.id,
          commandKind: command.kind,
        });
      });
      if (rows.length > 0) {
        section += 1;
      }
    };

    if (mode === "help") {
      return list;
    }

    if (mode === "commands") {
      commandItems(commandSuggestions);
      return list;
    }

    if (hunting) {
      commandItems(commandSuggestions);
      groups.forEach((group) => {
        // A provider the panel has no screen for contributes no rows: no dead ends.
        if (!paletteProvider(group.provider)) {
          return;
        }
        group.rows.forEach((hit, row) => {
          list.push({
            id: `hit-${group.provider}-${row}`,
            kind: "hit",
            section,
            label: hit.title || hit.url,
            url: hit.url,
            resultCount: resultTotal,
          });
        });
        if (group.status === "ready" && group.truncated) {
          list.push({
            id: `see-all-${group.provider}`,
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
        hint: "opens the results screen",
        url: resultsUrl(term),
        resultCount: resultTotal,
      });
      // The no-results state offers the way out as well: the results screen itself, and the AI
      // Hub with the words already in its prompt.
      if (nothingMatched) {
        list.push({
          id: "ask-ai",
          kind: "ask-ai",
          section: -1,
          label: `Ask AI about “${term}”`,
          hint: "opens the AI Hub with this prompt",
          url: askAiUrl(term),
        });
      }
      return list;
    }

    // Nothing typed yet (or a mode that searches nothing): the commands this account may run on
    // this screen, then its own history.
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
          commandKind: commandById.get(row.commandId)?.kind,
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
    hunting,
    commandById,
    commandSuggestions,
    groups,
    showSuggestions,
    suggestions,
    term,
    resultTotal,
    nothingMatched,
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

  /**
   * Run one action command through the API, and show what the owning service answered.
   *
   * The palette never invents the outcome: the line it prints is the run endpoint's own message,
   * and a failure keeps its code so the tooltip (and the reader) can tell what went wrong.
   */
  const runAction = useCallback(async (command: CommandInfo) => {
    setPendingRun(null);
    setRunState({ status: "running", command });
    try {
      const outcome = await runCommand(command.id, true);
      setRunState({ status: "done", command, message: outcome.message });
    } catch (cause) {
      const failure = asGroupFailure(cause);
      setRunState({
        status: "failed",
        command,
        message: failure.message,
        code: failure.code,
      });
    }
  }, []);

  const activate = useCallback(
    (item: PaletteItem, newTab: boolean) => {
      if (item.kind === "recent-query" && item.query) {
        setQuery(item.query);
        inputRef.current?.focus();
        return;
      }
      if (item.kind === "command" || item.kind === "recent-command") {
        // Either kind of command row is remembered (the owning screen does any writing); an
        // action then runs, while a navigation command only opens its screen.
        if (item.commandId) {
          rememberCommand(item.commandId);
        }
        const command = item.commandId ? commandById.get(item.commandId) : undefined;
        if (command?.kind === "action") {
          if (command.confirm) {
            setPendingRun(command);
          } else {
            void runAction(command);
          }
          return;
        }
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
    [commandById, go, rememberCommand, rememberQuery, runAction, term],
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
    // A confirmation is the top of the stack: Enter is the yes the card is asking for and Escape
    // takes the question back, so neither reaches the rows or closes the palette.
    if (pendingRun) {
      if (event.key === "Enter") {
        event.preventDefault();
        void runAction(pendingRun);
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        setPendingRun(null);
        return;
      }
    }
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

  /**
   * The question an action command asks before it runs: what would run, what it costs, and the
   * two answers. Kept at the top of the list so it is impossible to miss and impossible to
   * answer by accident with the arrow keys.
   */
  const confirmCard = pendingRun ? (
    <div
      data-palette-confirm={pendingRun.id}
      className="mx-0.5 mb-2 rounded-lg border border-caution/40 bg-caution-soft px-3 py-2.5"
    >
      <div className="flex items-start gap-2">
        <TriangleAlert className="mt-0.5 size-4 shrink-0 text-caution" aria-hidden />
        <div className="min-w-0 flex-1">
          <p className="text-[13px] font-medium text-ink">Run “{pendingRun.title}”?</p>
          <p className="mt-0.5 text-[12px] text-muted">
            {pendingRun.hint}. This cannot be undone.
          </p>
        </div>
      </div>
      <div className="mt-2.5 flex flex-wrap items-center gap-2">
        <button
          type="button"
          data-palette-confirm-run
          onClick={() => void runAction(pendingRun)}
          className="min-h-11 rounded-md border border-accent-strong/30 bg-accent-soft px-3 py-1.5 text-[12px] font-medium text-accent-strong transition hover:bg-surface lg:min-h-0"
        >
          Run
        </button>
        <button
          type="button"
          data-palette-confirm-cancel
          onClick={() => setPendingRun(null)}
          className="min-h-11 rounded-md border border-line bg-surface px-3 py-1.5 text-[12px] text-muted transition hover:bg-quiet-soft hover:text-ink lg:min-h-0"
        >
          Cancel
        </button>
        <span className="text-[11px] text-muted">↵ run · esc cancel</span>
      </div>
    </div>
  ) : null;

  /** What the owning service answered — the run's own line, or its failure with the code kept. */
  const runCard =
    runState.status === "idle" ? null : (
      <div
        data-palette-run-result={runState.status}
        className={`mx-0.5 mb-2 rounded-lg border px-3 py-2.5 ${
          runState.status === "failed"
            ? "border-caution/40 bg-caution-soft"
            : "border-line bg-canvas/70"
        }`}
      >
        <div className="flex items-start gap-2">
          {runState.status === "running" ? (
            <Loader2 className="mt-0.5 size-4 shrink-0 animate-spin text-muted" aria-hidden />
          ) : runState.status === "failed" ? (
            <TriangleAlert className="mt-0.5 size-4 shrink-0 text-caution" aria-hidden />
          ) : (
            <CheckCircle className="mt-0.5 size-4 shrink-0 text-positive" aria-hidden />
          )}
          <div className="min-w-0 flex-1">
            <p className="text-[13px] font-medium text-ink">
              {runState.status === "running"
                ? `Running “${runState.command.title}”…`
                : runState.status === "failed"
                  ? `“${runState.command.title}” did not run`
                  : `“${runState.command.title}” is done`}
            </p>
            <p
              className="mt-0.5 text-[12px] break-words text-muted"
              title={runState.status === "failed" ? runState.code : undefined}
            >
              {runState.status === "running" ? "Waiting for the platform's answer." : runState.message}
            </p>
            {runState.status === "done" && runState.command.route ? (
              <button
                type="button"
                data-palette-run-details
                onClick={() => go(runState.command.route, false)}
                className="mt-1 rounded-md text-[12px] font-medium text-accent-strong underline-offset-2 hover:underline"
              >
                View details
              </button>
            ) : null}
          </div>
          {runState.status !== "running" ? (
            <button
              type="button"
              aria-label="Dismiss the run result"
              data-palette-run-dismiss
              onClick={() => setRunState({ status: "idle" })}
              className="shrink-0 rounded-md p-1 text-muted transition hover:bg-quiet-soft hover:text-ink"
            >
              <X className="size-3.5" aria-hidden />
            </button>
          ) : null}
        </div>
      </div>
    );

  const rowIndex = (id: string) => itemIndex.get(id) ?? -1;

  const highlight = (title: string, needles: string[]) =>
    splitHighlight(title, needles).map((part, index) =>
      part.match ? (
        <mark key={index} className="rounded bg-accent-soft px-0.5 text-accent-strong">
          {part.text}
        </mark>
      ) : (
        <span key={index}>{part.text}</span>
      ),
    );

  const commandRow = (command: CommandInfo, index: number) => {
    const id = `command-${index}`;
    const Icon = COMMAND_ICONS[command.icon] ?? CommandIcon;
    const action = command.kind === "action";
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
          <span
            data-palette-command-kind={command.kind}
            className={`shrink-0 rounded-md border px-1.5 py-0.5 text-[10.5px] ${
              action
                ? "border-accent-strong/30 bg-accent-soft text-accent-strong"
                : "border-line bg-canvas text-muted"
            }`}
          >
            {action ? "Action" : "Command"}
          </span>
        }
      >
        <span className="truncate text-[13px] text-ink">{command.title}</span>
        <span className="truncate text-[11.5px] text-muted">
          {action ? `${command.hint}${command.confirm ? " · asks first" : ""}` : command.hint}
        </span>
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
      <span className="truncate text-[13px] text-ink">{highlight(hit.title, terms)}</span>
      {hit.subtitle ? (
        <span className="truncate text-[11.5px] text-muted">{hit.subtitle}</span>
      ) : null}
    </Option>
  );

  /** The palette's own action row (the "see all results" and "Ask AI" rows). */
  const actionRow = (id: string) => {
    const item = items[rowIndex(id)];
    if (!item) {
      return null;
    }
    const Icon = item.kind === "ask-ai" ? Sparkles : Search;
    return (
      <Option
        key={id}
        id={id}
        action={item.kind}
        active={rowIndex(id) === activeIndex}
        onActivate={() => activate(item, false)}
        onHover={() => setActiveIndex(rowIndex(id))}
        icon={<Icon className="size-3.5" />}
        trailing={
          item.hint ? (
            <span className="shrink-0 text-[11px] text-muted">{item.hint}</span>
          ) : null
        }
      >
        <span className="truncate text-[13px] text-ink">{item.label}</span>
      </Option>
    );
  };

  /** One provider's section: its own skeleton, its own rows, or its own retryable error. */
  const renderGroup = (group: PaletteGroup) => {
    // A provider the panel has no screen for renders nothing — no rows, no error, no dead end.
    if (!paletteProvider(group.provider)) {
      return null;
    }
    if (group.status === "ready" && group.rows.length === 0) {
      return null;
    }
    return (
      <div
        key={group.provider}
        data-palette-section={group.provider}
        data-palette-section-state={group.status}
        className="mb-2"
      >
        <SectionHeader
          label={group.label}
          count={group.status === "ready" ? group.total : null}
        />
        {group.status === "pending" ? (
          <div data-palette-skeleton={group.provider} className="px-1">
            {[0, 1].map((row) => (
              <div key={row} className="mb-1.5 h-9 animate-pulse rounded-lg bg-quiet-soft" />
            ))}
          </div>
        ) : null}
        {group.status === "error" && group.error ? (
          <div
            data-palette-group-error={group.provider}
            className="mx-1 rounded-lg border border-line bg-canvas px-2 py-2 text-[12px] text-muted"
          >
            <span>{group.label} could not be searched. </span>
            <button
              type="button"
              data-palette-group-retry={group.provider}
              title={`${group.error.code} · status ${group.error.status}`}
              onClick={() => retryGroup(group.provider)}
              className="font-medium text-accent-strong underline"
            >
              Try again
            </button>
          </div>
        ) : null}
        {group.status === "ready" ? (
          <>
            {group.rows.map((hit, row) => hitRow(hit, `hit-${group.provider}-${row}`, group.label))}
            {group.truncated ? (
              <SeeAllRow
                key={`see-all-${group.provider}`}
                id={`see-all-${group.provider}`}
                provider={group.provider}
                label={`See all ${group.total} ${group.label}`}
                active={rowIndex(`see-all-${group.provider}`) === activeIndex}
                onActivate={() => {
                  const item = items[rowIndex(`see-all-${group.provider}`)];
                  if (item) {
                    activate(item, false);
                  }
                }}
                onHover={() => setActiveIndex(rowIndex(`see-all-${group.provider}`))}
              />
            ) : null}
          </>
        ) : null}
      </div>
    );
  };

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

  const commandsErrorNote = (
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
  );

  const suggestionsBlock = showSuggestions ? (
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
  ) : null;

  const body = () => {
    if (mode === "help") {
      return helpPanel;
    }

    // `>` is the commands mode: the registry answers alone, the index is never asked.
    if (mode === "commands") {
      return (
        <>
          {commandsError ? commandsErrorNote : null}
          {commandSuggestions.length === 0 && !commandsError ? (
            <div className="flex flex-col items-center gap-2 px-4 py-8 text-center">
              <p className="text-[13px] font-medium">No command matches “{term}”</p>
              <p className="max-w-sm text-[12px] text-muted">
                Commands come from the features your account may open. Try fewer words, or clear
                the prefix to search the content instead.
              </p>
            </div>
          ) : (
            <div className="mb-2">
              <SectionHeader label="Commands" count={commandSuggestions.length} />
              {commandSuggestions.map(commandRow)}
            </div>
          )}
        </>
      );
    }

    // Every provider failed: the one state that blanks the answer, with one retry for all of it.
    if (hunting && allFailed) {
      return (
        <div className="flex flex-col items-center gap-2 px-4 py-8 text-center">
          <p className="text-[13px] font-medium">Search is unavailable</p>
          <p className="max-w-sm text-[12px] text-muted">
            {groups.find((group) => group.error)?.error?.message ??
              "The search could not be completed."}
          </p>
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

    // The first paint: nothing has answered yet and the suggestions are the only rows that could.
    if (firstPaint) {
      return (
        <div aria-live="polite" className="px-1 py-2">
          <span className="sr-only">Searching…</span>
          {[0, 1, 2].map((row) => (
            <div key={row} className="mb-1.5 h-9 animate-pulse rounded-lg bg-quiet-soft" />
          ))}
        </div>
      );
    }

    if (nothingMatched) {
      return (
        <div
          data-palette-empty
          className="flex flex-col items-center gap-2 px-4 py-6 text-center"
        >
          <p className="text-[13px] font-medium">Nothing matched “{term}”</p>
          {hiddenTotal > 0 ? (
            <p data-palette-hidden className="max-w-sm text-[12px] text-muted">
              {hiddenTotal === 1
                ? "1 result is outside your permissions."
                : `${hiddenTotal} results are outside your permissions.`}
            </p>
          ) : null}
          <p className="max-w-sm text-[12px] text-muted">
            Try fewer words, check the spelling, or narrow the search with type:page, type:media or
            type:sites.
          </p>
          {(answer?.hints ?? []).map((hint) => (
            <p key={hint} className="max-w-sm text-[11.5px] text-muted">
              {hint}
            </p>
          ))}
          <div className="mt-1 w-full">{actionRow("everywhere")}</div>
          <div className="w-full">{actionRow("ask-ai")}</div>
        </div>
      );
    }

    if (hunting) {
      return (
        <>
          {(answer?.hints ?? []).map((hint) => (
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

          {commandsError ? commandsErrorNote : null}

          {suggestionsBlock}

          {groups.map(renderGroup)}

          {actionRow("everywhere")}

          {mode !== "all" ? (
            <p className="px-2 py-1 text-[11.5px] text-muted">
              Narrowed to {modeLabel(mode).toLowerCase()}.
            </p>
          ) : null}
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
              label={mode === "all" ? "Suggested for this screen" : "Commands"}
              count={commandSuggestions.length}
            />
            {commandSuggestions.map((command, index) => commandRow(command, index))}
          </div>
        ) : null}

        {commandsError ? commandsErrorNote : null}

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
              const recentAction =
                row.kind === "command" && commandById.get(row.commandId)?.kind === "action";
              const trailing =
                row.kind === "query" ? (
                  row.resultCount !== null ? (
                    <span className="shrink-0 text-[11px] text-muted">{row.resultCount} results</span>
                  ) : null
                ) : (
                  <span
                    data-palette-command-kind={recentAction ? "action" : "navigate"}
                    className={`shrink-0 rounded-md border px-1.5 py-0.5 text-[10.5px] ${
                      recentAction
                        ? "border-accent-strong/30 bg-accent-soft text-accent-strong"
                        : "border-line bg-canvas text-muted"
                    }`}
                  >
                    {recentAction ? "Action" : "Command"}
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
                        <span className="truncate text-[11.5px] text-muted">
                          {recentAction ? "runs again · asks first" : row.route}
                        </span>
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
  const busy = hunting && !allFailed && groups.some((group) => group.status === "pending");

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
        // can hold focus (a row, the "Clear" button, the shortcut list). A confirmation on the
        // table takes the key first: its question is withdrawn before the palette itself closes.
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.preventDefault();
            if (pendingRun) {
              setPendingRun(null);
              return;
            }
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
          {busy ? (
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
          {confirmCard}
          {runCard}
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
  /** The count, or `null` while the section's own answer is still in flight. */
  count: number | null;
  action?: ReactNode;
}) {
  return (
    <div className="flex items-center justify-between gap-2 px-2 py-1.5">
      <span className="text-[11px] font-medium tracking-wide text-muted uppercase">{label}</span>
      <span className="flex items-center gap-2">
        {count === null ? (
          <span className="text-[11px] text-muted" aria-hidden>
            …
          </span>
        ) : (
          <span className="text-[11px] text-muted">{count}</span>
        )}
        {action}
      </span>
    </div>
  );
}

/** A section's "see all" row, styled like an option. */
function SeeAllRow({
  id,
  provider,
  label,
  active,
  onActivate,
  onHover,
}: {
  id: string;
  provider: string;
  label: string;
  active: boolean;
  onActivate: () => void;
  onHover: () => void;
}) {
  return (
    <div
      id={id}
      data-palette-see-all={provider}
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
