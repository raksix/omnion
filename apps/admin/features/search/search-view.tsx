"use client";

/**
 * `/search?q=…` — the results screen behind the palette's "See all" (REQ-002).
 *
 * The query lives in the URL together with every filter the rail applies, so a result set is a
 * link: the palette, a bookmark and the back button all land on the same search. The screen
 * shows what the index answered — the hits, the facet counts, how long the search took and which
 * provider contributed how many — and it is honest about every other state: no query yet,
 * loading, an API refusal (with its own code) and nothing matched.
 *
 * The facet counts come from the API with the same request (`facets=true`) and each group is
 * counted without its own filter, so "Pages 12" is the number clicking it would leave.
 */
import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent } from "react";

import {
  Check,
  Copy,
  Download,
  FileText,
  Globe,
  Images,
  Keyboard,
  RefreshCw,
  ScrollText,
  Search as SearchIcon,
  Settings2,
  SlidersHorizontal,
  Users,
  X,
} from "lucide-react";
import { useRouter, useSearchParams } from "next/navigation";

import {
  ApiError,
  downloadSearchExport,
  searchAll,
  type SearchFacetGroup,
  type SearchFilterInput,
  type SearchHit,
  type SearchResult,
  type SearchSort,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { splitHighlight } from "@/lib/search-palette";

/** Hits per page on this screen. */
const PER_PAGE = 50;

const SORTS: { value: SearchSort; label: string }[] = [
  { value: "relevance", label: "Relevance" },
  { value: "newest", label: "Newest first" },
  { value: "title", label: "Title A–Z" },
];

const SORT_ORDER: SearchSort[] = ["relevance", "newest", "title"];

const PROVIDER_ICONS: Record<string, typeof FileText> = {
  pages: FileText,
  media: Images,
  sites: Globe,
  users: Users,
  logs: ScrollText,
  translations: Globe,
  settings: Settings2,
};

/** What the rail knows how to filter, and the URL parameter each group carries. */
const FACET_PARAM: Record<string, string> = {
  type: "type",
  site: "site",
  owner: "owner",
  language: "lang",
  status: "status",
  updated: "updated",
};

/** One applied filter, as a chip. */
type Chip = {
  /** The URL parameter it clears. */
  param: string;
  /** The value shown next to the label. */
  label: string;
};

/** The key of one row, as the API and the export speak it. */
function rowKey(hit: SearchHit): string {
  return `${hit.provider}:${hit.entity_type}:${hit.entity_id}`;
}

/** Write one filter into a copy of the URL's parameters, or drop it. */
function withFilter(
  params: URLSearchParams,
  param: string,
  value: string | null,
): URLSearchParams {
  const next = new URLSearchParams(params.toString());
  next.delete("page");
  if (value === null) {
    next.delete(param);
  } else {
    next.set(param, value);
  }
  return next;
}

/** One row of the result set. */
function HitRow({
  hit,
  terms,
  providerLabel,
  selected,
  onToggle,
  onFocus,
}: {
  hit: SearchHit;
  terms: string[];
  providerLabel: string;
  selected: boolean;
  onToggle: (hit: SearchHit, event: { shiftKey: boolean }) => void;
  onFocus: (hit: SearchHit) => void;
}) {
  const Icon = PROVIDER_ICONS[hit.provider] ?? SearchIcon;
  return (
    <div
      data-search-row
      data-row-key={rowKey(hit)}
      className={`flex items-start gap-3 border-t border-line px-3 py-3 transition first:border-t-0 sm:px-4 ${
        selected ? "bg-accent-soft/40" : "hover:bg-canvas/60"
      }`}
    >
      <input
        type="checkbox"
        checked={selected}
        onChange={(event) =>
          onToggle(hit, { shiftKey: (event.nativeEvent as MouseEvent).shiftKey === true })
        }
        aria-label={`Select ${hit.title}`}
        data-row-checkbox
        className="mt-1 size-3.5 shrink-0 accent-accent"
      />
      <span className="mt-0.5 flex size-6 shrink-0 items-center justify-center rounded-md border border-line bg-canvas text-muted">
        <Icon className="size-3.5" aria-hidden />
      </span>
      <span className="flex min-w-0 flex-1 flex-col gap-0.5">
        <a
          href={hit.url}
          onFocus={() => onFocus(hit)}
          className="truncate text-[13.5px] font-medium text-ink outline-none hover:text-accent-strong focus-visible:underline"
        >
          {splitHighlight(hit.title, terms).map((part, index) =>
            part.match ? (
              <mark key={index} className="rounded bg-accent-soft px-0.5 text-accent-strong">
                {part.text}
              </mark>
            ) : (
              <span key={index}>{part.text}</span>
            ),
          )}
        </a>
        {hit.subtitle ? (
          <span className="truncate text-[12px] text-muted">{hit.subtitle}</span>
        ) : null}
        <span className="flex flex-wrap items-center gap-2 text-[11.5px] text-muted">
          <span className="rounded-md border border-line bg-canvas px-1.5 py-0.5">
            {providerLabel}
          </span>
          {hit.owner ? <span>{hit.owner}</span> : null}
          {hit.updated_at ? <span>Updated {formatTimestamp(hit.updated_at)}</span> : null}
        </span>
      </span>
      <a
        href={hit.url}
        className="mt-0.5 hidden shrink-0 rounded-md border border-line px-2 py-1 text-[11.5px] text-muted transition hover:bg-quiet-soft hover:text-ink sm:block"
        aria-label={`Open ${hit.title}`}
      >
        Open
      </a>
    </div>
  );
}

/** One group of the facet rail. */
function FacetGroupView({
  group,
  active,
  onToggle,
}: {
  group: SearchFacetGroup;
  active: Set<string>;
  onToggle: (group: SearchFacetGroup, value: string) => void;
}) {
  return (
    <section data-facet={group.key} className="flex flex-col gap-1.5">
      <h3 className="text-[11.5px] font-semibold tracking-wide text-muted uppercase">
        {group.title}
      </h3>
      <ul className="flex flex-col gap-0.5">
        {group.values.map((value) => {
          const isActive = active.has(`${group.key}:${value.value}`);
          return (
            <li key={value.value}>
              <button
                type="button"
                onClick={() => onToggle(group, value.value)}
                data-facet-value={`${group.key}:${value.value}`}
                aria-pressed={isActive}
                className={`flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-[12.5px] transition ${
                  isActive
                    ? "bg-accent-soft text-accent-strong"
                    : "text-ink hover:bg-quiet-soft"
                }`}
              >
                <span className="min-w-0 flex-1 truncate">{value.label}</span>
                <span className="shrink-0 font-mono text-[11.5px] text-muted">{value.count}</span>
              </button>
            </li>
          );
        })}
      </ul>
      {group.more > 0 ? (
        <p className="px-2 text-[11.5px] text-muted">{group.more} more values</p>
      ) : null}
    </section>
  );
}

/** The results screen. */
export function SearchView() {
  const router = useRouter();
  const params = useSearchParams();
  const q = (params.get("q") ?? "").trim();

  const [draft, setDraft] = useState(q);
  const [result, setResult] = useState<SearchResult | null>(null);
  const [status, setStatus] = useState<"idle" | "loading" | "ready" | "error">("idle");
  const [error, setError] = useState<{ code: string; message: string } | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [selection, setSelection] = useState<Set<string>>(new Set());
  const [lastSelected, setLastSelected] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [showShortcuts, setShowShortcuts] = useState(false);
  const [showFilters, setShowFilters] = useState(false);
  const [exporting, setExporting] = useState(false);
  const searchBox = useRef<HTMLInputElement | null>(null);

  const sort = (params.get("sort") as SearchSort | null) ?? "relevance";
  const page = Math.max(Number(params.get("page") ?? "1") || 1, 1);
  const filters: SearchFilterInput = useMemo(
    () => ({
      type: params.get("type") ?? undefined,
      site: params.get("site") ?? undefined,
      owner: params.get("owner") ?? undefined,
      language: params.get("lang") ?? undefined,
      status: params.get("status") ?? undefined,
      updated: params.get("updated") ?? undefined,
      before: params.get("before") ?? undefined,
      after: params.get("after") ?? undefined,
    }),
    [params],
  );
  const filterKey = useMemo(() => JSON.stringify(filters), [filters]);

  // The box follows the URL — a palette jump into `/search?q=…` fills it too.
  useEffect(() => {
    setDraft(q);
  }, [q]);

  // A new result set means a new selection: keeping row keys across queries would carry a
  // selection of things that are no longer on screen into the bulk actions.
  useEffect(() => {
    setSelection(new Set());
    setLastSelected(null);
  }, [q, sort, page, filterKey]);

  useEffect(() => {
    if (!q) {
      setResult(null);
      setStatus("idle");
      setError(null);
      return;
    }

    let cancelled = false;
    setStatus("loading");
    setError(null);
    searchAll({ q, page, per_page: PER_PAGE, sort, facets: true, filters })
      .then((answer) => {
        if (!cancelled) {
          setResult(answer);
          setStatus("ready");
        }
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setResult(null);
        setStatus("error");
        setError(
          cause instanceof ApiError
            ? { code: cause.code, message: cause.message }
            : { code: "unknown_error", message: "The search could not be completed." },
        );
      });

    return () => {
      cancelled = true;
    };
  }, [q, page, sort, attempt, filters, filterKey]);

  const navigate = useCallback(
    (next: URLSearchParams) => {
      const query = next.toString();
      router.replace(query ? `/search?${query}` : "/search");
    },
    [router],
  );

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const next = new URLSearchParams(params.toString());
    const trimmed = draft.trim();
    if (trimmed) {
      next.set("q", trimmed);
    } else {
      next.delete("q");
    }
    next.delete("page");
    navigate(next);
  };

  /** Apply or clear one filter; the URL is the only place a filter lives. */
  const applyFilter = useCallback(
    (param: string, value: string | null) => {
      navigate(withFilter(params, param, value));
    },
    [navigate, params],
  );

  /** Toggle one facet value in its group (the group's values are a comma list for `type`). */
  const toggleFacet = useCallback(
    (group: SearchFacetGroup, value: string) => {
      const param = FACET_PARAM[group.key] ?? group.key;
      const current = params.get(param);
      if (param === "type") {
        const values = new Set((current ?? "").split(",").filter(Boolean));
        if (values.has(value)) {
          values.delete(value);
        } else {
          values.add(value);
        }
        applyFilter(param, values.size === 0 ? null : [...values].join(","));
        return;
      }
      applyFilter(param, current === value ? null : value);
    },
    [applyFilter, params],
  );

  const hits = result?.hits ?? [];
  const activeFacets = useMemo(() => {
    const active = new Set<string>();
    for (const [groupKey, param] of Object.entries(FACET_PARAM)) {
      const raw = params.get(param);
      if (!raw) continue;
      for (const value of raw.split(",").filter(Boolean)) {
        active.add(`${groupKey}:${value}`);
      }
    }
    return active;
  }, [params]);

  const chips: Chip[] = useMemo(() => {
    const rows: Chip[] = [];
    const label = (groupKey: string, value: string) => {
      const group = result?.facets?.find((entry) => entry.key === groupKey);
      return group?.values.find((entry) => entry.value === value)?.label ?? value;
    };
    for (const [groupKey, param] of Object.entries(FACET_PARAM)) {
      const raw = params.get(param);
      if (!raw) continue;
      const values = raw.split(",").filter(Boolean);
      if (param === "type") {
        for (const value of values) {
          rows.push({ param: `type:${value}`, label: label(groupKey, value) });
        }
      } else {
        rows.push({ param, label: label(groupKey, values[0]) });
      }
    }
    if (params.get("before")) rows.push({ param: "before", label: `Before ${params.get("before")}` });
    if (params.get("after")) rows.push({ param: "after", label: `After ${params.get("after")}` });
    return rows;
  }, [params, result]);

  const removeChip = (chip: Chip) => {
    if (chip.param.startsWith("type:")) {
      const value = chip.param.slice("type:".length);
      const values = new Set((params.get("type") ?? "").split(",").filter(Boolean));
      values.delete(value);
      applyFilter("type", values.size === 0 ? null : [...values].join(","));
      return;
    }
    applyFilter(chip.param, null);
  };

  const toggleRow = useCallback(
    (hit: SearchHit, event: { shiftKey: boolean }) => {
      const key = rowKey(hit);
      const next = new Set(selection);
      if (event.shiftKey && lastSelected) {
        const from = hits.findIndex((row) => rowKey(row) === lastSelected);
        const to = hits.findIndex((row) => rowKey(row) === key);
        if (from >= 0 && to >= 0) {
          for (let index = Math.min(from, to); index <= Math.max(from, to); index += 1) {
            next.add(rowKey(hits[index]));
          }
        }
      } else if (next.has(key)) {
        next.delete(key);
      } else {
        next.add(key);
      }
      setSelection(next);
      setLastSelected(key);
    },
    [hits, lastSelected, selection],
  );

  const copyLinks = useCallback(async () => {
    const origin = typeof window === "undefined" ? "" : window.location.origin;
    const links = hits
      .filter((hit) => selection.has(rowKey(hit)))
      .map((hit) => `${origin}${hit.url}`);
    if (links.length === 0) {
      return;
    }
    try {
      await navigator.clipboard.writeText(links.join("\n"));
      setNotice(`${links.length} link${links.length === 1 ? "" : "s"} copied to the clipboard.`);
    } catch {
      setNotice("The browser refused to write to the clipboard.");
    }
  }, [hits, selection]);

  const exportCsv = useCallback(async () => {
    setExporting(true);
    setNotice(null);
    try {
      const selected = hits.filter((hit) => selection.has(rowKey(hit))).map(rowKey);
      const file = await downloadSearchExport({
        q,
        filters,
        selected: selected.length > 0 ? selected : undefined,
      });
      const url = URL.createObjectURL(file.blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = file.filename;
      document.body.appendChild(anchor);
      anchor.click();
      anchor.remove();
      URL.revokeObjectURL(url);
      setNotice(
        `Exported ${file.rows} row${file.rows === 1 ? "" : "s"} to ${file.filename}${
          file.truncated ? " (the file was capped)" : ""
        }.`,
      );
    } catch (cause) {
      setNotice(
        cause instanceof ApiError
          ? `The export failed: ${cause.message} (${cause.code})`
          : "The export failed.",
      );
    } finally {
      setExporting(false);
    }
  }, [filters, hits, q, selection]);

  // The keyboard map the screen advertises: `s` sort, `f` facets, `x` one row, `⌘A` the page,
  // `?` the list. Typing in a field is never hijacked.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement ||
        target?.isContentEditable === true;
      if (typing) {
        return;
      }
      if (event.key === "?" || (event.key === "/" && event.shiftKey)) {
        event.preventDefault();
        setShowShortcuts((value) => !value);
        return;
      }
      if (event.key === "Escape") {
        setShowShortcuts(false);
        setShowFilters(false);
        return;
      }
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "a") {
        if (hits.length === 0) return;
        event.preventDefault();
        setSelection(new Set(hits.map(rowKey)));
        return;
      }
      if (event.metaKey || event.ctrlKey || event.altKey) {
        return;
      }
      const key = event.key.toLowerCase();
      if (key === "s") {
        event.preventDefault();
        const index = SORT_ORDER.indexOf(sort);
        const next = SORT_ORDER[(index + 1) % SORT_ORDER.length];
        navigate(withFilter(params, "sort", next === "relevance" ? null : next));
        return;
      }
      if (key === "f") {
        event.preventDefault();
        searchBox.current?.blur();
        const first = document.querySelector<HTMLButtonElement>("[data-facet-value]");
        if (first) {
          first.focus();
        } else {
          setShowFilters(true);
        }
        return;
      }
      if (key === "x") {
        const focused = document.activeElement?.closest<HTMLElement>("[data-search-row]");
        const key2 = focused?.dataset.rowKey;
        const hit = key2 ? hits.find((row) => rowKey(row) === key2) : hits[0];
        if (hit) {
          event.preventDefault();
          toggleRow(hit, { shiftKey: false });
        }
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [hits, navigate, params, sort, toggleRow]);

  // A notice is a confirmation, not a screen: it leaves on its own.
  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(null), 6000);
    return () => window.clearTimeout(timer);
  }, [notice]);

  const total = result?.total ?? 0;
  const pageCount = Math.max(Math.ceil(total / PER_PAGE), 1);
  const first = total === 0 ? 0 : (page - 1) * PER_PAGE + 1;
  const last = Math.min(page * PER_PAGE, total);
  const providerLabel = (key: string) =>
    result?.counts.find((count) => count.provider === key)?.title ?? key;
  const selectionCount = selection.size;

  const rail = result?.facets?.length ? (
    <div className="flex flex-col gap-4" data-facet-rail>
      {(result.facets ?? []).map((group) => (
        <FacetGroupView
          key={group.key}
          group={group}
          active={activeFacets}
          onToggle={toggleFacet}
        />
      ))}
    </div>
  ) : null;

  return (
    <div className="flex flex-col gap-5">
      <form onSubmit={submit} className="flex flex-col gap-3 sm:flex-row sm:items-end">
        <label className="flex min-w-0 flex-1 flex-col gap-1.5">
          <span className="text-[12.5px] font-medium text-ink">Query</span>
          <span className="relative">
            <SearchIcon
              className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted"
              aria-hidden
            />
            <input
              ref={searchBox}
              value={draft}
              onChange={(event) => setDraft(event.target.value)}
              placeholder="Search Omnion…"
              name="q"
              data-search-query
              className="h-9 w-full rounded-lg border border-line bg-surface pr-3 pl-8 text-[13px] text-ink outline-none transition placeholder:text-muted focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
          </span>
        </label>

        <label className="flex flex-col gap-1.5">
          <span className="text-[12.5px] font-medium text-ink">Sort</span>
          <select
            value={sort}
            onChange={(event) =>
              applyFilter("sort", event.target.value === "relevance" ? null : event.target.value)
            }
            name="sort"
            data-search-sort
            className="h-9 rounded-lg border border-line bg-surface px-2.5 text-[13px] text-ink outline-none focus:border-accent"
          >
            {SORTS.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </select>
        </label>

        <button
          type="button"
          onClick={() => setShowFilters(true)}
          className="flex h-9 items-center justify-center gap-1.5 rounded-lg border border-line bg-surface px-3.5 text-[12.5px] font-medium transition hover:bg-quiet-soft lg:hidden"
          data-search-filters
        >
          <SlidersHorizontal className="size-3.5" aria-hidden />
          Filters
          {chips.length > 0 ? (
            <span className="rounded-full bg-accent px-1.5 text-[11px] text-white">
              {chips.length}
            </span>
          ) : null}
        </button>

        <button
          type="submit"
          className="flex h-9 items-center justify-center gap-1.5 rounded-lg bg-accent px-3.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
        >
          <SearchIcon className="size-3.5" aria-hidden />
          Search
        </button>

        <button
          type="button"
          onClick={() => setShowShortcuts(true)}
          className="hidden h-9 items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-[12.5px] text-muted transition hover:bg-quiet-soft lg:flex"
          aria-label="Keyboard shortcuts"
          data-search-shortcuts
        >
          <Keyboard className="size-3.5" aria-hidden />
          ?
        </button>
      </form>

      {chips.length > 0 ? (
        <div className="flex flex-wrap items-center gap-2" data-search-chips>
          {chips.map((chip) => (
            <span
              key={chip.param}
              data-chip={chip.param}
              className="flex items-center gap-1.5 rounded-full border border-line bg-surface px-2.5 py-1 text-[12px] text-ink"
            >
              {chip.label}
              <button
                type="button"
                onClick={() => removeChip(chip)}
                aria-label={`Remove ${chip.label} filter`}
                className="rounded-full p-0.5 text-muted transition hover:bg-quiet-soft hover:text-ink"
              >
                <X className="size-3" aria-hidden />
              </button>
            </span>
          ))}
          <button
            type="button"
            onClick={() => navigate(new URLSearchParams(q ? { q } : {}))}
            className="text-[12px] text-muted underline-offset-2 transition hover:text-ink hover:underline"
            data-search-clear-filters
          >
            Clear all
          </button>
        </div>
      ) : null}

      {notice ? (
        <p
          className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px] text-ink"
          role="status"
          data-search-notice
        >
          {notice}
        </p>
      ) : null}

      {status === "idle" ? (
        <div className="rounded-xl border border-line bg-surface px-6 py-10 text-center">
          <p className="text-[13.5px] font-medium">Search the whole platform</p>
          <p className="mx-auto mt-1 max-w-md text-[12.5px] text-muted">
            Type a query above, or open the palette with ⌘K from any screen. Results cover pages,
            media, activity, translations, settings, sites and accounts; narrow them with the
            filters, with type:page, or with ⌘A and the export.
          </p>
        </div>
      ) : null}

      {status === "error" && error ? (
        <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
          <p className="text-[13.5px] font-medium">Search is unavailable</p>
          <p className="max-w-md text-[12.5px] text-muted">
            {error.message} <span className="font-mono text-[11.5px]">({error.code})</span>
          </p>
          <button
            type="button"
            onClick={() => setAttempt((value) => value + 1)}
            className="mt-1 flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Try again
          </button>
        </div>
      ) : null}

      {status === "loading" ? (
        <div className="rounded-xl border border-line bg-surface p-4" aria-live="polite">
          <span className="sr-only">Searching…</span>
          {[0, 1, 2, 3].map((row) => (
            <div key={row} className="mb-2 h-10 animate-pulse rounded-lg bg-quiet-soft" />
          ))}
        </div>
      ) : null}

      {status === "ready" && result ? (
        <div className="flex flex-col gap-5 lg:flex-row lg:items-start">
          {rail ? (
            <aside className="hidden w-56 shrink-0 lg:block" aria-label="Filters">
              {rail}
            </aside>
          ) : null}

          <div className="flex min-w-0 flex-1 flex-col gap-4">
            <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-[12.5px] text-muted">
              <span data-search-total>
                <span className="font-medium text-ink">{result.total}</span> result
                {result.total === 1 ? "" : "s"} for “{result.query}”
              </span>
              <span>{result.took_ms} ms</span>
              {result.counts.map((count) => (
                <span key={count.provider}>
                  {count.title} {count.count}
                </span>
              ))}
            </div>

            {(result.hints ?? []).map((hint) => (
              <p key={hint} className="text-[12px] text-muted">
                {hint}
              </p>
            ))}

            {result.total === 0 ? (
              <div className="rounded-xl border border-line bg-surface px-6 py-10 text-center">
                <p className="text-[13.5px] font-medium">Nothing matched “{result.query}”</p>
                <ul className="mx-auto mt-2 flex max-w-md flex-col gap-1 text-left text-[12.5px] text-muted">
                  <li>Check the spelling of the words you searched for.</li>
                  <li>Try fewer or shorter words — one unusual word ranks best.</li>
                  <li>
                    Remove a filter above, or narrow the search with type:page, type:media or
                    type:sites.
                  </li>
                </ul>
              </div>
            ) : (
              <>
                {selectionCount > 0 ? (
                  <div
                    className="sticky top-2 z-10 flex flex-wrap items-center gap-2 rounded-xl border border-accent/40 bg-surface px-3 py-2 shadow-sm"
                    data-search-bulk
                  >
                    <span className="text-[12.5px] font-medium text-ink">
                      {selectionCount} selected
                    </span>
                    <button
                      type="button"
                      onClick={copyLinks}
                      data-bulk-copy
                      className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-quiet-soft"
                    >
                      <Copy className="size-3.5" aria-hidden />
                      Copy links
                    </button>
                    <button
                      type="button"
                      onClick={exportCsv}
                      disabled={exporting}
                      data-bulk-export
                      className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-quiet-soft disabled:opacity-60"
                    >
                      <Download className="size-3.5" aria-hidden />
                      Export CSV
                    </button>
                    <button
                      type="button"
                      onClick={() => setSelection(new Set())}
                      className="text-[12px] text-muted underline-offset-2 transition hover:text-ink hover:underline"
                    >
                      Clear selection
                    </button>
                  </div>
                ) : (
                  <div className="flex items-center justify-between gap-3">
                    <button
                      type="button"
                      onClick={() => setSelection(new Set(hits.map(rowKey)))}
                      className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium text-muted transition hover:bg-quiet-soft hover:text-ink"
                      data-search-select-page
                    >
                      <Check className="size-3.5" aria-hidden />
                      Select page (⌘A)
                    </button>
                    <button
                      type="button"
                      onClick={exportCsv}
                      disabled={exporting}
                      data-search-export-all
                      className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-quiet-soft disabled:opacity-60"
                    >
                      <Download className="size-3.5" aria-hidden />
                      Export all {total}
                    </button>
                  </div>
                )}

                <div className="overflow-hidden rounded-xl border border-line bg-surface">
                  {result.hits.map((hit) => (
                    <HitRow
                      key={rowKey(hit)}
                      hit={hit}
                      terms={result.terms}
                      providerLabel={providerLabel(hit.provider)}
                      selected={selection.has(rowKey(hit))}
                      onToggle={toggleRow}
                      onFocus={() => undefined}
                    />
                  ))}
                </div>
              </>
            )}

            {total > 0 ? (
              <div className="flex flex-wrap items-center justify-between gap-3">
                <p className="text-[12.5px] text-muted">
                  Showing {first}–{last} of {total}
                </p>
                <div className="flex items-center gap-2">
                  <button
                    type="button"
                    onClick={() =>
                      navigate(withFilter(params, "page", page <= 2 ? null : String(page - 1)))
                    }
                    disabled={page <= 1}
                    className="rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft disabled:opacity-50"
                  >
                    Previous
                  </button>
                  <span className="text-[12px] text-muted">
                    Page {page} of {pageCount}
                  </span>
                  <button
                    type="button"
                    onClick={() =>
                      navigate(
                        withFilter(params, "page", String(Math.min(page + 1, pageCount))),
                      )
                    }
                    disabled={page >= pageCount}
                    className="rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft disabled:opacity-50"
                  >
                    Next
                  </button>
                </div>
              </div>
            ) : null}
          </div>
        </div>
      ) : null}

      {/* The phone's filters live in a sheet; the rail itself needs a wide column. */}
      {showFilters ? (
        <div
          className="fixed inset-0 z-40 flex items-end bg-ink/30 lg:hidden"
          role="dialog"
          aria-modal="true"
          aria-label="Filters"
          data-search-filter-sheet
        >
          <div className="max-h-[80vh] w-full overflow-y-auto rounded-t-2xl border-t border-line bg-surface p-4">
            <div className="mb-3 flex items-center justify-between">
              <h2 className="text-[13.5px] font-semibold">Filters</h2>
              <button
                type="button"
                onClick={() => setShowFilters(false)}
                aria-label="Close filters"
                className="rounded-lg border border-line p-1.5 text-muted transition hover:bg-quiet-soft"
              >
                <X className="size-3.5" aria-hidden />
              </button>
            </div>
            {rail ?? <p className="text-[12.5px] text-muted">Run a search to see the filters.</p>}
          </div>
        </div>
      ) : null}

      {showShortcuts ? (
        <div
          className="fixed inset-0 z-40 flex items-center justify-center bg-ink/30 p-4"
          role="dialog"
          aria-modal="true"
          aria-label="Keyboard shortcuts"
          data-search-shortcuts-dialog
        >
          <div className="w-full max-w-sm rounded-xl border border-line bg-surface p-4">
            <div className="mb-2 flex items-center justify-between">
              <h2 className="text-[13.5px] font-semibold">Keyboard shortcuts</h2>
              <button
                type="button"
                onClick={() => setShowShortcuts(false)}
                aria-label="Close shortcuts"
                className="rounded-lg border border-line p-1.5 text-muted transition hover:bg-quiet-soft"
              >
                <X className="size-3.5" aria-hidden />
              </button>
            </div>
            <ul className="flex flex-col gap-1.5 text-[12.5px] text-muted">
              <li className="flex justify-between gap-3">
                <span>Open the palette</span> <kbd>⌘K</kbd>
              </li>
              <li className="flex justify-between gap-3">
                <span>Focus the search box</span> <kbd>/</kbd>
              </li>
              <li className="flex justify-between gap-3">
                <span>Cycle the sort order</span> <kbd>s</kbd>
              </li>
              <li className="flex justify-between gap-3">
                <span>Focus the filters</span> <kbd>f</kbd>
              </li>
              <li className="flex justify-between gap-3">
                <span>Select the focused row</span> <kbd>x</kbd>
              </li>
              <li className="flex justify-between gap-3">
                <span>Select the whole page</span> <kbd>⌘A</kbd>
              </li>
              <li className="flex justify-between gap-3">
                <span>This list</span> <kbd>?</kbd>
              </li>
            </ul>
          </div>
        </div>
      ) : null}
    </div>
  );
}
