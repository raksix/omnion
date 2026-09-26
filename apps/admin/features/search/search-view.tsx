"use client";

/**
 * `/search?q=…` — the results screen behind the palette's "See all" (REQ-002).
 *
 * The query lives in the URL, so a result set is a link: the palette, a bookmark and the back
 * button all land on the same search. The screen shows what the index answered — the hits, how
 * long the search took, and which provider contributed how many — and it is honest about every
 * other state: no query yet, loading, an API refusal (with its own code) and nothing matched.
 *
 * Facet chips, selection with copy/export and the ranking-weights screen arrive with slice 3;
 * this screen deliberately carries only what it can do for real.
 */
import { useEffect, useState, type FormEvent } from "react";

import { FileText, Globe, Images, RefreshCw, Search as SearchIcon } from "lucide-react";
import { useRouter, useSearchParams } from "next/navigation";

import {
  ApiError,
  searchAll,
  type SearchHit,
  type SearchResult,
  type SearchSort,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { splitHighlight } from "@/lib/search-palette";

/** Hits per page on this screen. */
const PER_PAGE = 25;

const SORTS: { value: SearchSort; label: string }[] = [
  { value: "relevance", label: "Relevance" },
  { value: "newest", label: "Newest first" },
  { value: "title", label: "Title A–Z" },
];

const PROVIDER_ICONS: Record<string, typeof FileText> = {
  pages: FileText,
  media: Images,
  sites: Globe,
};

/** One row: icon, title with the match emphasised, breadcrumb, provider and time. */
function HitRow({ hit, terms, providerLabel }: { hit: SearchHit; terms: string[]; providerLabel: string }) {
  const Icon = PROVIDER_ICONS[hit.provider] ?? SearchIcon;
  return (
    <a
      href={hit.url}
      className="flex items-start gap-3 border-t border-line px-4 py-3 transition first:border-t-0 hover:bg-canvas/60"
    >
      <span className="mt-0.5 flex size-6 shrink-0 items-center justify-center rounded-md border border-line bg-canvas text-muted">
        <Icon className="size-3.5" aria-hidden />
      </span>
      <span className="flex min-w-0 flex-1 flex-col gap-0.5">
        <span className="truncate text-[13.5px] font-medium text-ink">
          {splitHighlight(hit.title, terms).map((part, index) =>
            part.match ? (
              <mark key={index} className="rounded bg-accent-soft px-0.5 text-accent-strong">
                {part.text}
              </mark>
            ) : (
              <span key={index}>{part.text}</span>
            ),
          )}
        </span>
        {hit.subtitle ? (
          <span className="truncate text-[12px] text-muted">{hit.subtitle}</span>
        ) : null}
        <span className="flex flex-wrap items-center gap-2 text-[11.5px] text-muted">
          <span className="rounded-md border border-line bg-canvas px-1.5 py-0.5">{providerLabel}</span>
          {hit.updated_at ? <span>Updated {formatTimestamp(hit.updated_at)}</span> : null}
          {hit.tags.length > 0 ? <span>{hit.tags.join(" · ")}</span> : null}
        </span>
      </span>
    </a>
  );
}

/** The results screen. */
export function SearchView() {
  const router = useRouter();
  const params = useSearchParams();
  const q = (params.get("q") ?? "").trim();

  const [draft, setDraft] = useState(q);
  const [sort, setSort] = useState<SearchSort>("relevance");
  const [page, setPage] = useState(1);
  const [result, setResult] = useState<SearchResult | null>(null);
  const [status, setStatus] = useState<"idle" | "loading" | "ready" | "error">("idle");
  const [error, setError] = useState<{ code: string; message: string } | null>(null);
  const [attempt, setAttempt] = useState(0);

  // The box follows the URL — a palette jump into `/search?q=…` fills it too.
  useEffect(() => {
    setDraft(q);
  }, [q]);

  useEffect(() => {
    setPage(1);
  }, [q, sort]);

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
    searchAll({ q, page, per_page: PER_PAGE, sort })
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
  }, [q, page, sort, attempt]);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const next = draft.trim();
    router.replace(next ? `/search?q=${encodeURIComponent(next)}` : "/search");
  };

  const total = result?.total ?? 0;
  const pageCount = Math.max(Math.ceil(total / PER_PAGE), 1);
  const first = total === 0 ? 0 : (page - 1) * PER_PAGE + 1;
  const last = Math.min(page * PER_PAGE, total);
  const providerLabel = (key: string) =>
    result?.counts.find((count) => count.provider === key)?.title ?? key;

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
            onChange={(event) => setSort(event.target.value as SearchSort)}
            name="sort"
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
          type="submit"
          className="flex h-9 items-center gap-1.5 rounded-lg bg-accent px-3.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
        >
          <SearchIcon className="size-3.5" aria-hidden />
          Search
        </button>
      </form>

      {status === "idle" ? (
        <div className="rounded-xl border border-line bg-surface px-6 py-10 text-center">
          <p className="text-[13.5px] font-medium">Search the whole platform</p>
          <p className="mx-auto mt-1 max-w-md text-[12.5px] text-muted">
            Type a query above, or open the palette with ⌘K from any screen. Results cover pages,
            media and sites; narrow them with type:page, type:media or type:sites.
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
        <>
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-[12.5px] text-muted">
            <span>
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
                  Narrow the search with type:page, type:media or type:sites, or drop a filter if
                  you added one.
                </li>
              </ul>
            </div>
          ) : (
            <div className="overflow-hidden rounded-xl border border-line bg-surface">
              {result.hits.map((hit) => (
                <HitRow
                  key={`${hit.provider}-${hit.entity_type}-${hit.entity_id}`}
                  hit={hit}
                  terms={result.terms}
                  providerLabel={providerLabel(hit.provider)}
                />
              ))}
            </div>
          )}

          {total > 0 ? (
            <div className="flex flex-wrap items-center justify-between gap-3">
              <p className="text-[12.5px] text-muted">
                Showing {first}–{last} of {total}
              </p>
              <div className="flex items-center gap-2">
                <button
                  type="button"
                  onClick={() => setPage((value) => Math.max(value - 1, 1))}
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
                  onClick={() => setPage((value) => Math.min(value + 1, pageCount))}
                  disabled={page >= pageCount}
                  className="rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft disabled:opacity-50"
                >
                  Next
                </button>
              </div>
            </div>
          ) : null}
        </>
      ) : null}
    </div>
  );
}
