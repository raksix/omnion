"use client";

/**
 * The event console: what the platform recorded, and what it is able to record.
 *
 * Two tabs, because they answer two different questions and mixing them would make both worse:
 *
 * * **Feed** — *what happened*. Newest first, narrowed by name, site, actor and window. A row
 *   expands into the payload as a JSON tree, because the payload is the only part of an event
 *   anybody actually consumes; a feed that shows a name and a timestamp is a log file.
 * * **Catalogue** — *what could happen*. The compiled registry: every name, its area, what it
 *   means, the fields it carries and how many deliveries it produced in the last day. A name
 *   the platform has reserved is shown as such, with the reason, rather than hidden — a
 *   subscriber who cannot see that `plugin.installed` is named but unbuilt has no way to know
 *   whether to wait for it or stop looking for it.
 *
 * Three claims this screen makes, each one a way a feed lies to the person reading it:
 *
 * 1. **The filters live in the query string.** A filtered view that cannot be pasted to a
 *    colleague is a view that has to be rebuilt by hand, and the rebuild is where mistakes
 *    happen. The same URL also survives a reload, which is how an operator confirms to a second
 *    person what they are looking at.
 * 2. **Pagination is keyset, and "Load older" only exists when there is something older.** The
 *    page answers `has_more` from the row past the page rather than from a second count, so
 *    the button cannot offer a page that is empty.
 * 3. **A `live` name with zero deliveries is a fact, not a blank cell.** It means the platform
 *    records the event and nobody has subscribed to it — which is the single most useful thing
 *    to learn from the catalogue, and the reason the count is next to the name rather than
 *    buried in a tooltip.
 */
import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Activity,
  ChevronDown,
  ChevronRight,
  CircleDot,
  Copy,
  Filter,
  ListTree,
  RefreshCw,
  Radio,
  Search,
  TriangleAlert,
  X,
} from "lucide-react";
import { useRouter, useSearchParams } from "next/navigation";

import { fetchEventCatalogue, fetchEvents, type ApiError } from "@/lib/api";
import type {
  CatalogueEntry,
  EventCatalogue,
  EventFilters,
  EventRow,
} from "@/lib/types";

/** How many rows one page carries. Also the minimum: the API clamps to 1..200. */
const PAGE = 25;

/** The windows the filter offers, as an offset from now in hours. */
const WINDOWS: { value: string; label: string; hours: number | null }[] = [
  { value: "", label: "All time", hours: null },
  { value: "1h", label: "Last hour", hours: 1 },
  { value: "24h", label: "Last 24 hours", hours: 24 },
  { value: "7d", label: "Last 7 days", hours: 24 * 7 },
  { value: "30d", label: "Last 30 days", hours: 24 * 30 },
];

/** The earliest instant a window reaches back to, as RFC 3339. `null` means "all time". */
function windowStart(hours: number | null): string | undefined {
  if (hours === null) return undefined;
  return new Date(Date.now() - hours * 3_600_000).toISOString();
}

/** Read the filters out of the query string. */
function filtersFrom(params: URLSearchParams): EventFilters {
  const filters: EventFilters = {};
  // `getAll` and not `get`: several names mean "any of these", and reading one of them would
  // make a second selection look like it did nothing.
  const names = params.getAll("name").filter(Boolean);
  if (names.length > 0) filters.name = names;
  const site = params.get("site_id");
  if (site) filters.site_id = site;
  const actor = params.get("actor_user_id");
  if (actor) filters.actor_user_id = actor;
  const from = params.get("from");
  if (from) filters.from = from;
  const to = params.get("to");
  if (to) filters.to = to;
  return filters;
}

type Tab = "feed" | "catalogue";

/** Read the tab and the window out of the query string. */
function viewFrom(params: URLSearchParams): { tab: Tab; window: string } {
  const tab = params.get("tab") === "catalogue" ? "catalogue" : "feed";
  return { tab, window: params.get("window") ?? "" };
}

export function EventConsole() {
  const router = useRouter();
  const params = useSearchParams();
  const search = params?.toString() ?? "";
  const filters = useMemo(() => filtersFrom(new URLSearchParams(search)), [search]);
  const { tab, window: windowKey } = useMemo(
    () => viewFrom(new URLSearchParams(search)),
    [search],
  );

  const [rows, setRows] = useState<EventRow[]>([]);
  const [cursor, setCursor] = useState<number | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<number | null>(null);
  const [cursorIndex, setCursorIndex] = useState(-1);
  const [catalogue, setCatalogue] = useState<EventCatalogue | null>(null);
  const [catalogueError, setCatalogueError] = useState<string | null>(null);
  const [catalogueArea, setCatalogueArea] = useState<string>("");
  const [nameQuery, setNameQuery] = useState("");
  const searchInput = useRef<HTMLInputElement>(null);

  const activeWindow = WINDOWS.find((entry) => entry.value === windowKey) ?? WINDOWS[0];
  // The window is a *relative* filter and so cannot be stored as an absolute instant in the
  // URL: `from=2026-09-29T01:00:00Z` in a link is already stale when the colleague opens it,
  // and it silently means "the last two hours" for them and "nothing at all" for the sender
  // depending on which end reads it. The URL therefore says `window=24h` and the instant is
  // computed at request time.
  const pageFilters = useMemo<EventFilters>(
    () => ({ ...filters, from: filters.from ?? windowStart(activeWindow.hours) }),
    [filters, activeWindow.hours],
  );

  const load = useCallback(
    async (append = false) => {
      setLoading(true);
      setError(null);
      try {
        const page = await fetchEvents({
          ...pageFilters,
          limit: PAGE,
          cursor: append ? (cursor ?? undefined) : undefined,
        });
        setRows((previous) => (append ? [...previous, ...page.events] : page.events));
        setCursor(page.next_cursor);
        setHasMore(page.has_more);
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setLoading(false);
      }
    },
    [pageFilters, cursor],
  );

  useEffect(() => {
    void load(false);
    setCursorIndex(-1);
    setExpanded(null);
  }, [load]);

  // The catalogue is a fact about the platform, not about the current filter, so it is read
  // once when the tab is opened rather than on every filter keystroke.
  useEffect(() => {
    if (tab !== "catalogue" || catalogue) return;
    let cancelled = false;
    void (async () => {
      try {
        const answer = await fetchEventCatalogue();
        if (!cancelled) {
          setCatalogue(answer);
          setCatalogueError(null);
        }
      } catch (caught) {
        if (!cancelled) setCatalogueError((caught as ApiError).message);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [tab, catalogue]);

  /** Rewrite the query string — the URL is the state, so the back button walks the filters. */
  const write = useCallback(
    (mutate: (next: URLSearchParams) => void, path = "/events") => {
      const next = new URLSearchParams(search);
      mutate(next);
      const query = next.toString();
      router.replace(query ? `${path}?${query}` : path);
    },
    [router, search],
  );

  const setFilter = (key: string, value: string | undefined) => {
    write((next) => {
      if (value) next.set(key, value);
      else next.delete(key);
    });
  };

  const toggleName = (name: string) => {
    write((next) => {
      const names = next.getAll("name").filter(Boolean);
      const at = names.indexOf(name);
      if (at >= 0) names.splice(at, 1);
      else names.push(name);
      next.delete("name");
      for (const entry of names) next.append("name", entry);
    });
  };

  const reset = () => router.replace("/events");

  const activeFilters =
    (filters.name?.length ?? 0) +
    (filters.site_id ? 1 : 0) +
    (filters.actor_user_id ? 1 : 0) +
    (filters.from ? 1 : 0) +
    (filters.to ? 1 : 0) +
    (windowKey ? 1 : 0);

  const knownNames = useMemo(() => {
    const seen = new Set<string>();
    for (const entry of rows) seen.add(entry.name);
    return [...seen].sort();
  }, [rows]);

  /** Names offered in the filter menu: the ones on this page, plus the whole live catalogue
   *  when it has been read. A menu limited to the current page would make an event that
   *  exists but is older than the window impossible to filter for, which is exactly when an
   *  operator is looking for it. */
  const filterNames = useMemo(() => {
    const names = new Set(knownNames);
    for (const entry of catalogue?.events ?? []) {
      if (entry.status === "live") names.add(entry.name);
    }
    return [...names].sort();
  }, [knownNames, catalogue]);

  const shownCatalogue = useMemo(() => {
    if (!catalogue) return [];
    const needle = nameQuery.trim().toLowerCase();
    return catalogue.events.filter((entry) => {
      if (catalogueArea && entry.area !== catalogueArea) return false;
      if (!needle) return true;
      return (
        entry.name.toLowerCase().includes(needle) ||
        entry.description.toLowerCase().includes(needle) ||
        entry.group.toLowerCase().includes(needle)
      );
    });
  }, [catalogue, catalogueArea, nameQuery]);

  const copy = async (text: string, what: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setNotice(`Copied ${what}.`);
    } catch {
      // Clipboard access is refused in a browser that was not asked politely (or over plain
      // http). Saying "copied" when nothing was copied is worse than saying nothing, so the
      // failure is reported with the value the reader can still select by hand.
      setNotice("The browser refused clipboard access — select the value and copy it by hand.");
    }
  };

  const payloadText = (row: EventRow) => JSON.stringify(row.payload, null, 2);

  const onKeyDown = (event: React.KeyboardEvent<HTMLTableSectionElement>) => {
    if (event.key === "/" && searchInput.current) {
      event.preventDefault();
      searchInput.current.focus();
      return;
    }
    if (event.key === "Escape") {
      if (expanded !== null) {
        event.preventDefault();
        setExpanded(null);
        return;
      }
      if (activeFilters > 0) {
        event.preventDefault();
        reset();
      }
      return;
    }
    if (event.key === "j" || event.key === "ArrowDown") {
      event.preventDefault();
      setCursorIndex((index) => Math.min(index + 1, rows.length - 1));
      return;
    }
    if (event.key === "k" || event.key === "ArrowUp") {
      event.preventDefault();
      setCursorIndex((index) => Math.max(index - 1, 0));
      return;
    }
    const row = rows[cursorIndex];
    if (!row) return;
    if (event.key === "Enter") {
      event.preventDefault();
      setExpanded((current) => (current === row.id ? null : row.id));
    }
  };

  return (
    <div className="flex flex-col gap-4">
      {/* The tabs. `tab` is in the URL because the two tabs read completely different data and
          a reader who has linked the Catalogue tab to a colleague should land on it. */}
      <div role="tablist" aria-label="Event views" className="flex gap-1 border-b border-line">
        {(
          [
            { id: "feed", label: "Feed", icon: Activity },
            { id: "catalogue", label: "Catalogue", icon: ListTree },
          ] as const
        ).map((entry) => {
          const Icon = entry.icon;
          const active = tab === entry.id;
          return (
            <button
              key={entry.id}
              type="button"
              role="tab"
              id={`event-tab-${entry.id}`}
              aria-selected={active}
              aria-controls={`event-panel-${entry.id}`}
              data-event-tab={entry.id}
              onClick={() => setFilter("tab", entry.id === "feed" ? undefined : entry.id)}
              className={`-mb-px flex items-center gap-1.5 border-b-2 px-3 py-2 text-[13px] transition ${
                active
                  ? "border-accent font-medium text-ink"
                  : "border-transparent text-muted hover:text-ink"
              }`}
            >
              <Icon className="size-3.5" aria-hidden />
              {entry.label}
            </button>
          );
        })}
      </div>

      {tab === "feed" ? (
        <>
          <section
            aria-label="Feed filters"
            data-event-filters
            className="flex flex-wrap items-end gap-3 rounded-xl border border-line bg-surface p-3"
          >
            <label className="flex flex-col gap-1 text-[11.5px] text-muted">
              <span className="font-medium">Find an event name</span>
              <input
                ref={searchInput}
                id="event-name-search"
                type="search"
                value={nameQuery}
                placeholder="Type to narrow the name menu"
                onChange={(event) => setNameQuery(event.target.value)}
                className="w-56 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
              />
            </label>

            <label className="flex flex-col gap-1 text-[11.5px] text-muted">
              <span className="font-medium">Window</span>
              <select
                id="event-window"
                value={windowKey}
                onChange={(event) => setFilter("window", event.target.value || undefined)}
                className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
              >
                {WINDOWS.map((entry) => (
                  <option key={entry.value} value={entry.value}>
                    {entry.label}
                  </option>
                ))}
              </select>
            </label>

            <button
              type="button"
              onClick={() => void load(false)}
              data-event-refresh
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted transition hover:text-ink"
            >
              <RefreshCw className="size-3.5" aria-hidden />
              Refresh
            </button>

            {activeFilters > 0 ? (
              <button
                type="button"
                onClick={reset}
                data-event-reset
                className="rounded-lg px-2.5 py-1.5 text-[12.5px] text-accent-strong hover:underline"
              >
                Reset filters
              </button>
            ) : null}
          </section>

          {/* The chosen names, each removable. A multi-select that only shows inside a closed
              menu leaves the reader unable to tell what the table below is filtered to. */}
          {filters.name && filters.name.length > 0 ? (
            <div
              data-event-name-chips
              className="flex flex-wrap items-center gap-1.5 rounded-xl border border-line bg-surface px-3 py-2"
            >
              <Filter className="size-3.5 text-muted" aria-hidden />
              <span className="text-[12px] text-muted">Showing</span>
              {filters.name.map((name) => (
                <button
                  key={name}
                  type="button"
                  data-event-name-chip={name}
                  onClick={() => toggleName(name)}
                  className="inline-flex items-center gap-1 rounded-full bg-accent-soft px-2 py-0.5 text-[11.5px] text-accent-strong"
                >
                  {name}
                  <X className="size-3" aria-hidden />
                </button>
              ))}
            </div>
          ) : null}

          {notice ? (
            <p
              data-event-notice
              role="status"
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] text-muted"
            >
              {notice}
            </p>
          ) : null}

          {error ? (
            <div
              data-event-error
              role="alert"
              className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
            >
              <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
              <span className="flex-1">{error}</span>
              <button
                type="button"
                onClick={() => void load(false)}
                className="text-accent-strong hover:underline"
              >
                Retry
              </button>
            </div>
          ) : null}

          <div className="overflow-hidden rounded-xl border border-line bg-surface">
            {loading && rows.length === 0 ? (
              <div aria-busy="true" data-event-skeleton className="flex flex-col gap-2 p-4">
                {Array.from({ length: 6 }, (_, index) => (
                  <div key={index} className="h-9 animate-pulse rounded bg-quiet-soft" />
                ))}
              </div>
            ) : rows.length === 0 ? (
              <div
                data-event-empty
                className="flex flex-col items-center gap-3 px-4 py-16 text-center"
              >
                <Radio className="size-6 text-muted" aria-hidden />
                <div>
                  <p className="text-[13.5px] font-medium">Nothing recorded yet.</p>
                  <p className="text-[12.5px] text-muted">
                    {activeFilters > 0
                      ? "No event matches these filters."
                      : "Publish a page to see the first event."}
                  </p>
                </div>
                {activeFilters > 0 ? (
                  <button
                    type="button"
                    onClick={reset}
                    className="text-[12.5px] text-accent-strong hover:underline"
                  >
                    Reset filters
                  </button>
                ) : null}
              </div>
            ) : (
              <div className="overflow-x-auto">
                <table data-event-table className="w-full border-collapse text-left text-[13px]">
                  <thead>
                    <tr className="border-b border-line text-[11.5px] text-muted">
                      <th scope="col" className="w-9 px-3 py-2.5">
                        <span className="sr-only">Expand</span>
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        Id
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        Name
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        Site
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        Actor
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        Payload
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        Recorded
                      </th>
                    </tr>
                  </thead>
                  <tbody onKeyDown={onKeyDown} tabIndex={0} aria-label="Events">
                    {rows.map((row, index) => {
                      const isCursor = index === cursorIndex;
                      const isOpen = expanded === row.id;
                      const summary = previewPayload(row.payload);
                      return (
                        <Fragment key={row.id}>
                          <tr
                            data-event-row={row.id}
                            data-cursor={isCursor ? "true" : undefined}
                            data-open={isOpen ? "true" : undefined}
                            onClick={() => setExpanded(isOpen ? null : row.id)}
                            className={`cursor-pointer border-b border-line/60 transition hover:bg-quiet-soft ${
                              isCursor ? "bg-accent-soft" : ""
                            }`}
                          >
                            <td className="px-3 py-2.5">
                              <button
                                type="button"
                                aria-label={isOpen ? `Collapse ${row.name}` : `Expand ${row.name}`}
                                aria-expanded={isOpen}
                                data-event-expand={row.id}
                                onClick={(event) => {
                                  event.stopPropagation();
                                  setExpanded(isOpen ? null : row.id);
                                }}
                                className="rounded p-0.5 text-muted hover:text-ink"
                              >
                                {isOpen ? (
                                  <ChevronDown className="size-4" aria-hidden />
                                ) : (
                                  <ChevronRight className="size-4" aria-hidden />
                                )}
                              </button>
                            </td>
                            <td className="px-3 py-2.5 text-muted tabular-nums">
                              {row.id}
                            </td>
                            <td className="px-3 py-2.5">
                              <span className="font-medium">{row.name}</span>
                            </td>
                            <td className="px-3 py-2.5 text-muted">
                              {row.site_id ? shortId(row.site_id) : "—"}
                            </td>
                            <td className="px-3 py-2.5 text-muted">
                              {row.actor_user_id ? shortId(row.actor_user_id) : "—"}
                            </td>
                            <td className="max-w-xs px-3 py-2.5">
                              <span className="block truncate text-muted" title={summary}>
                                {summary}
                              </span>
                            </td>
                            <td className="px-3 py-2.5 whitespace-nowrap text-muted tabular-nums">
                              {new Date(row.created_at).toLocaleString()}
                            </td>
                          </tr>
                          {isOpen ? (
                            <tr data-event-detail={row.id}>
                              <td colSpan={7} className="bg-canvas px-4 py-3">
                                <div className="flex flex-col gap-2">
                                  <div className="flex flex-wrap items-center gap-2">
                                    <span className="text-[12px] font-medium">Payload</span>
                                    <button
                                      type="button"
                                      data-event-copy={row.id}
                                      onClick={() => void copy(payloadText(row), "the payload")}
                                      className="inline-flex items-center gap-1 rounded-lg border border-line bg-surface px-2 py-1 text-[11.5px] text-muted transition hover:text-ink"
                                    >
                                      <Copy className="size-3" aria-hidden />
                                      Copy payload
                                    </button>
                                    <button
                                      type="button"
                                      data-event-copy-name={row.id}
                                      onClick={() => void copy(row.name, "the event name")}
                                      className="inline-flex items-center gap-1 rounded-lg border border-line bg-surface px-2 py-1 text-[11.5px] text-muted transition hover:text-ink"
                                    >
                                      <Copy className="size-3" aria-hidden />
                                      Copy name
                                    </button>
                                    <span className="text-[11.5px] text-muted">
                                      Recorded {new Date(row.created_at).toISOString()}
                                    </span>
                                  </div>
                                  <pre
                                    data-event-payload={row.id}
                                    className="max-h-72 overflow-auto rounded-lg border border-line bg-surface p-3 text-[12px] leading-relaxed"
                                  >
                                    {payloadText(row)}
                                  </pre>
                                </div>
                              </td>
                            </tr>
                          ) : null}
                        </Fragment>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
          </div>

          {hasMore ? (
            <div className="flex justify-center">
              <button
                type="button"
                data-event-more
                disabled={loading}
                onClick={() => void load(true)}
                className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] text-muted transition hover:text-ink disabled:opacity-50"
              >
                <Search className="size-3.5" aria-hidden />
                {loading ? "Loading…" : "Load older"}
              </button>
            </div>
          ) : null}
        </>
      ) : (
        <section
          id="event-panel-catalogue"
          role="tabpanel"
          aria-labelledby="event-tab-catalogue"
          className="flex flex-col gap-3"
        >
          {catalogueError ? (
            <div
              data-event-catalogue-error
              role="alert"
              className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
            >
              <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
              <span className="flex-1">{catalogueError}</span>
              <button
                type="button"
                onClick={() => {
                  setCatalogue(null);
                  setCatalogueError(null);
                }}
                className="text-accent-strong hover:underline"
              >
                Retry
              </button>
            </div>
          ) : null}

          {catalogue ? (
            <>
              <div
                data-event-catalogue-summary
                className="flex flex-wrap items-center gap-2 rounded-xl border border-line bg-surface px-3 py-2 text-[12.5px] text-muted"
              >
                <span className="font-medium text-ink">
                  {catalogue.events.length} event names
                </span>
                <span aria-hidden>·</span>
                <span data-catalogue-live>{catalogue.live_count} live</span>
                <span aria-hidden>·</span>
                <span data-catalogue-reserved>{catalogue.reserved_count} reserved</span>
                <span aria-hidden>·</span>
                <span>
                  up to {catalogue.max_subscriptions} subscriptions per endpoint
                </span>
              </div>

              <section
                aria-label="Catalogue filters"
                data-event-catalogue-filters
                className="flex flex-wrap items-end gap-3 rounded-xl border border-line bg-surface p-3"
              >
                <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                  <span className="font-medium">Area</span>
                  <select
                    id="event-area"
                    value={catalogueArea}
                    onChange={(event) => setCatalogueArea(event.target.value)}
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
                  >
                    <option value="">All areas</option>
                    {catalogue.areas.map((area) => (
                      <option key={area} value={area}>
                        {area}
                      </option>
                    ))}
                  </select>
                </label>
                <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                  <span className="font-medium">Search names and descriptions</span>
                  <input
                    id="event-catalogue-search"
                    type="search"
                    value={nameQuery}
                    placeholder="e.g. page, delivery, install"
                    onChange={(event) => setNameQuery(event.target.value)}
                    className="w-64 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
                  />
                </label>
                {catalogueArea || nameQuery ? (
                  <button
                    type="button"
                    data-event-catalogue-reset
                    onClick={() => {
                      setCatalogueArea("");
                      setNameQuery("");
                    }}
                    className="rounded-lg px-2.5 py-1.5 text-[12.5px] text-accent-strong hover:underline"
                  >
                    Reset
                  </button>
                ) : null}
              </section>

              <div className="overflow-hidden rounded-xl border border-line bg-surface">
                {shownCatalogue.length === 0 ? (
                  <p
                    data-event-catalogue-empty
                    className="px-4 py-10 text-center text-[12.5px] text-muted"
                  >
                    No event name matches this search.
                  </p>
                ) : (
                  <div className="overflow-x-auto">
                    <table
                      data-event-catalogue-table
                      className="w-full border-collapse text-left text-[13px]"
                    >
                      <thead>
                        <tr className="border-b border-line text-[11.5px] text-muted">
                          <th scope="col" className="px-3 py-2.5 font-medium">
                            Name
                          </th>
                          <th scope="col" className="px-3 py-2.5 font-medium">
                            Area
                          </th>
                          <th scope="col" className="px-3 py-2.5 font-medium">
                            Status
                          </th>
                          <th scope="col" className="px-3 py-2.5 font-medium">
                            Payload fields
                          </th>
                          <th scope="col" className="px-3 py-2.5 text-right font-medium">
                            Deliveries · 24 h
                          </th>
                          <th scope="col" className="px-3 py-2.5 font-medium">
                            <span className="sr-only">Actions</span>
                          </th>
                        </tr>
                      </thead>
                      <tbody>
                        {shownCatalogue.map((entry) => (
                          <CatalogueRow
                            key={entry.name}
                            entry={entry}
                            selected={filters.name?.includes(entry.name) ?? false}
                            onFilter={() => {
                              write((next) => next.set("tab", "feed"));
                              toggleName(entry.name);
                            }}
                          />
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}
              </div>
            </>
          ) : null}
        </section>
      )}

      {/* The name menu is a `details` element rather than a custom popover: it opens with the
          keyboard, closes with Escape, and works without JavaScript focus management — three
          things a hand-rolled dropdown has to re-implement and usually gets one of wrong. */}
      <NameMenu
        names={filterNames}
        selected={filters.name ?? []}
        query={nameQuery}
        onQuery={setNameQuery}
        onToggle={toggleName}
      />
    </div>
  );
}

/** One catalogue row, with its payload fields expanded on demand. */
function CatalogueRow({
  entry,
  selected,
  onFilter,
}: {
  entry: CatalogueEntry;
  selected: boolean;
  onFilter: () => void;
}) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <tr
        data-catalogue-row={entry.name}
        data-status={entry.status}
        className="border-b border-line/60 last:border-0"
      >
        <td className="px-3 py-2.5">
          <button
            type="button"
            aria-expanded={open}
            aria-label={open ? `Hide payload fields of ${entry.name}` : `Show payload fields of ${entry.name}`}
            data-catalogue-expand={entry.name}
            onClick={() => setOpen((value) => !value)}
            className="inline-flex items-center gap-1 text-left font-medium hover:underline"
          >
            {open ? (
              <ChevronDown className="size-3.5 shrink-0" aria-hidden />
            ) : (
              <ChevronRight className="size-3.5 shrink-0" aria-hidden />
            )}
            {entry.name}
          </button>
          <p className="mt-0.5 max-w-md text-[11.5px] text-muted">{entry.description}</p>
        </td>
        <td className="px-3 py-2.5 text-muted">{entry.area}</td>
        <td className="px-3 py-2.5">
          {entry.status === "live" ? (
            <span className="inline-flex items-center gap-1 rounded-full bg-positive-soft px-2 py-0.5 text-[11px] font-medium text-positive">
              <CircleDot className="size-2.5" aria-hidden />
              Live
            </span>
          ) : (
            <span
              title="Named and subscribable; the module that emits it is not shipped yet"
              className="inline-flex items-center gap-1 rounded-full bg-caution-soft px-2 py-0.5 text-[11px] font-medium text-caution"
            >
              Reserved
            </span>
          )}
        </td>
        <td className="px-3 py-2.5 text-muted tabular-nums">
          {entry.payload_fields.length}
        </td>
        <td className="px-3 py-2.5 text-right tabular-nums">
          {entry.deliveries_24h === 0 ? (
            <span className="text-muted">0</span>
          ) : (
            <span className="font-medium">{entry.deliveries_24h}</span>
          )}
        </td>
        <td className="px-3 py-2.5 text-right">
          <button
            type="button"
            data-catalogue-filter={entry.name}
            onClick={onFilter}
            className="rounded-md border border-line px-2 py-1 text-[11.5px] text-muted transition hover:text-ink"
          >
            {selected ? "Filtered" : "Filter feed"}
          </button>
        </td>
      </tr>
      {open ? (
        <tr data-catalogue-fields={entry.name}>
          <td colSpan={6} className="bg-canvas px-4 py-3">
            <ul className="flex flex-col gap-1">
              {entry.payload_fields.map((field) => (
                <li
                  key={field.name}
                  data-catalogue-field={entry.name}
                  className="flex flex-wrap items-baseline gap-2 text-[12px]"
                >
                  <code className="rounded bg-quiet-soft px-1.5 py-0.5">{field.name}</code>
                  <span className="text-muted">{field.kind}</span>
                  {field.required ? (
                    <span className="text-muted">always present</span>
                  ) : (
                    <span className="text-muted">may be absent</span>
                  )}
                </li>
              ))}
            </ul>
          </td>
        </tr>
      ) : null}
    </>
  );
}

/** The event-name picker, rendered once at the end of the screen and opened by the search box. */
function NameMenu({
  names,
  selected,
  query,
  onQuery,
  onToggle,
}: {
  names: string[];
  selected: string[];
  query: string;
  onQuery: (value: string) => void;
  onToggle: (name: string) => void;
}) {
  const needle = query.trim().toLowerCase();
  const shown = names.filter((name) => !needle || name.includes(needle));
  return (
    <details data-event-name-menu className="relative">
      <summary className="inline-flex cursor-pointer list-none items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] text-muted hover:text-ink">
        <ListTree className="size-3.5" aria-hidden />
        Event names
        {selected.length > 0 ? (
          <span className="rounded-full bg-accent-soft px-1.5 text-[11px] text-accent-strong">
            {selected.length}
          </span>
        ) : null}
      </summary>
      <div className="absolute bottom-full left-0 z-40 mb-2 max-h-72 w-80 overflow-auto rounded-xl border border-line bg-surface p-2 shadow-xl">
        <label className="flex flex-col gap-1 pb-2 text-[11.5px] text-muted">
          <span className="font-medium">Narrow the list</span>
          <input
            type="search"
            data-event-name-filter
            value={query}
            placeholder="Type a name or a group"
            onChange={(event) => onQuery(event.target.value)}
            className="w-full rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
          />
        </label>
        {shown.length === 0 ? (
          <p className="px-2 py-3 text-center text-[12px] text-muted">
            No name matches that text.
          </p>
        ) : (
          <ul className="flex flex-col">
            {shown.map((name) => (
              <li key={name}>
                <label className="flex items-center gap-2 rounded-lg px-2 py-1.5 text-[12.5px] hover:bg-quiet-soft">
                  <input
                    type="checkbox"
                    data-event-name-option={name}
                    checked={selected.includes(name)}
                    onChange={() => onToggle(name)}
                    className="size-3.5 accent-[var(--accent)]"
                  />
                  <span className="truncate">{name}</span>
                </label>
              </li>
            ))}
          </ul>
        )}
      </div>
    </details>
  );
}

/** A one-line preview of a payload, honest about being a preview. */
function previewPayload(payload: unknown): string {
  if (payload === null || payload === undefined) return "—";
  if (typeof payload !== "object") return String(payload);
  if (Array.isArray(payload)) return `[${payload.length} items]`;
  const entries = Object.entries(payload as Record<string, unknown>);
  if (entries.length === 0) return "{}";
  return entries
    .slice(0, 3)
    .map(([key, value]) => `${key}: ${formatValue(value)}`)
    .join(" · ");
}

function formatValue(value: unknown): string {
  if (value === null || value === undefined) return "—";
  if (typeof value === "object") return Array.isArray(value) ? `[${value.length}]` : "{…}";
  const text = String(value);
  return text.length > 24 ? `${text.slice(0, 24)}…` : text;
}

/** The last four hex digits of an id — enough to recognise, short enough to fit a column. */
function shortId(id: string): string {
  return id.length <= 8 ? id : `…${id.slice(-6)}`;
}
