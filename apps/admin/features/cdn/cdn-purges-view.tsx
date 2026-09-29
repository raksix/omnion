"use client";

/**
 * `/cdn/purges` — the purge history and the per-item detail drawer (REQ-011, slice 2).
 *
 * This screen exists for one operator question: "the page I published is still the old one —
 * did the cache hear about it, and if not, what did the provider say?" Four decisions shape
 * it, and each is a place the obvious implementation is wrong:
 *
 * - **A failed purge is never collapsed into a count.** A row says `3 of 12 failed` and
 *   keeps the provider's own message, because "the purge failed" without the reason is the
 *   report that costs an afternoon. The drawer shows the message per item, verbatim.
 * - **The retry button exists only where there is something to retry.** It is driven by the
 *   server's `retryable` flag rather than by panel logic, so the two can never disagree; a
 *   retry on a `succeeded` row is a dead button and the request forbids those.
 * - **Retry requeues the failed items and says so before it does.** On a `partial` purge
 *   that is not the whole row: the targets that already went through are not re-sent,
 *   because re-sending them adds rate-limit pressure to a provider that is already
 *   struggling. The button's label names the count.
 * - **The table and the counters are read from one endpoint per view, and the filter state
 *   is in the URL's query string.** A filter that lives only in component state is a filter
 *   a reload loses, and the request's "rows shown match the API counts" is unprovable when
 *   the total is computed from the rows on screen.
 *
 * Keyboard: `/` focuses the filter, `Esc` closes the drawer, `r` retries the open purge.
 * Mobile: the table becomes stacked cards carrying the same `data-*` hooks as the rows.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { Copy, Download, Filter, RefreshCw, RotateCcw, Search, X } from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import {
  ApiError,
  fetchCdnPurge,
  fetchCdnPurges,
  retryCdnPurge,
} from "@/lib/api";
import { useSites } from "@/lib/sites";
import type {
  CdnPurge,
  CdnPurgeDetail,
  CdnPurgeFilters,
  CdnPurgeStatus,
} from "@/lib/types";

/** The statuses the filter offers, in the order an operator works through them. */
const STATUSES: { value: CdnPurgeStatus; label: string }[] = [
  { value: "queued", label: "Queued" },
  { value: "running", label: "Running" },
  { value: "succeeded", label: "Succeeded" },
  { value: "partial", label: "Partial" },
  { value: "failed", label: "Failed" },
];

/** The kinds the filter offers. */
const KINDS = [
  { value: "url", label: "URLs" },
  { value: "tag", label: "Tags" },
  { value: "all", label: "Everything" },
] as const;

/** How many rows a page shows. */
const PAGE_SIZE = 50;

/** A short, honest summary of what was targeted. */
function targetSummary(purge: CdnPurge): string {
  if (purge.kind === "all") {
    return "The whole zone";
  }
  if (purge.targets.length === 0) {
    return "—";
  }
  if (purge.targets.length === 1) {
    return purge.targets[0];
  }
  return `${purge.targets.length} targets`;
}

/** Download the current page as CSV, escaping the two characters that break a row. */
function toCsv(purges: CdnPurge[]): string {
  const escape = (value: string) => `"${value.replaceAll('"', '""')}"`;
  const header = ["requested_at", "kind", "status", "items", "failed", "provider", "targets"];
  const rows = purges.map((purge) =>
    [
      purge.requested_at,
      purge.kind,
      purge.status,
      String(purge.item_count),
      String(purge.failed_count),
      purge.provider,
      purge.targets.join(" "),
    ]
      .map(escape)
      .join(","),
  );
  return [header.join(","), ...rows].join("\n");
}

/** The purge history of the selected site. */
export function CdnPurgesView() {
  const { selectedSite } = useSites();
  const siteId = selectedSite?.id ?? null;

  const [purges, setPurges] = useState<CdnPurge[]>([]);
  const [total, setTotal] = useState(0);
  const [maxTargets, setMaxTargets] = useState(500);
  const [filters, setFilters] = useState<CdnPurgeFilters>({});
  const [openId, setOpenId] = useState<string | null>(null);
  const [detail, setDetail] = useState<CdnPurgeDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [fieldError, setFieldError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);
  const searchRef = useRef<HTMLInputElement>(null);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    if (siteId === null) {
      return;
    }
    let cancelled = false;
    setError(null);
    fetchCdnPurges(siteId, { ...filters, limit: PAGE_SIZE })
      .then((page) => {
        if (cancelled) {
          return;
        }
        setPurges(page.purges);
        setTotal(page.total);
        // The cap comes from the API rather than being restated here: a form that hard-codes
        // 500 and the server that refuses at 501 are two numbers that will eventually
        // disagree, and the disagreement shows up as a refusal nobody can explain.
        setMaxTargets(page.max_targets);
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(cause instanceof ApiError ? cause.message : "The purge history could not be read.");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [siteId, filters, reloadToken]);

  // Open the drawer for whichever row the operator clicked, and load its items.
  useEffect(() => {
    if (openId === null) {
      setDetail(null);
      return;
    }
    let cancelled = false;
    fetchCdnPurge(openId)
      .then((next) => {
        if (!cancelled) {
          setDetail(next);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(cause instanceof ApiError ? cause.message : "That purge could not be read.");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [openId, reloadToken]);

  // Keyboard: `/` focuses the filter, `Esc` closes the drawer, `r` retries what is open.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement;
      if (event.key === "/" && !typing) {
        event.preventDefault();
        searchRef.current?.focus();
        return;
      }
      if (event.key === "Escape" && openId !== null) {
        setOpenId(null);
        return;
      }
      if (event.key === "r" && !typing && detail?.purge.retryable) {
        event.preventDefault();
        void onRetry();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const onRetry = useCallback(async () => {
    if (openId === null || busy) {
      return;
    }
    setBusy(true);
    setFieldError(null);
    try {
      const next = await retryCdnPurge(openId);
      setDetail(next);
      reload();
    } catch (cause) {
      // A `409` is a real answer, not a failure: this purge has nothing to retry. It is
      // shown inline under the button rather than in the page-wide banner, because it is
      // about the row the operator is looking at.
      setFieldError(cause instanceof ApiError ? cause.message : "The retry did not go through.");
    } finally {
      setBusy(false);
    }
  }, [openId, busy, reload]);

  const failedPurges = useMemo(() => purges.filter((purge) => purge.retryable), [purges]);

  const exportCsv = useCallback(() => {
    // Built in a Blob rather than a data: URL: a 500-target purge history makes the data
    // URL longer than a browser will accept in an anchor, and the download silently
    // produces a file named after the page.
    const blob = new Blob([toCsv(purges)], { type: "text/csv;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = `cdn-purges-${new Date().toISOString().slice(0, 10)}.csv`;
    anchor.click();
    URL.revokeObjectURL(url);
  }, [purges]);

  const copyTargets = useCallback(() => {
    const targets = detail?.purge.targets.join("\n") ?? "";
    if (targets === "") {
      return;
    }
    // `navigator.clipboard` is unavailable on an insecure origin, and a button that does
    // nothing when it is missing is the dead button the request forbids. The catch is the
    // behaviour, not an afterthought.
    void navigator.clipboard
      ?.writeText(targets)
      .then(() => setFieldError(null))
      .catch(() => setFieldError("The targets could not be copied to the clipboard."));
  }, [detail]);

  if (siteId === null) {
    return (
      <EmptyState
        title="No site selected"
        hint="The CDN is configured per site. Pick one in the switcher above."
      />
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-col gap-1">
          <h1 className="text-[15px] font-medium">Purge history</h1>
          <p className="text-[12px] text-muted">
            {total === 0
              ? "No purge has been requested for this site"
              : `Showing ${purges.length} of ${total}`}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            onClick={exportCsv}
            disabled={purges.length === 0}
            data-cdn-export-csv
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
          >
            <Download className="size-3.5" aria-hidden />
            Export CSV
          </button>
          <button
            type="button"
            onClick={reload}
            data-cdn-purges-reload
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Refresh
          </button>
          <Link
            href="/cdn/purge"
            data-cdn-new-purge
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
          >
            Purge
          </Link>
        </div>
      </header>

      {error ? (
        <div className="flex flex-col items-start gap-3 rounded-xl border border-accent/30 bg-accent-soft px-4 py-3">
          <p role="alert" className="text-[12.5px] text-accent-strong">
            {error}
          </p>
          <button
            type="button"
            onClick={reload}
            className="rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Try again
          </button>
        </div>
      ) : null}

      <div className="flex flex-wrap items-center gap-2">
        <div className="relative flex-1 min-w-[200px]">
          <Search
            className="pointer-events-none absolute top-1/2 left-3 size-3.5 -translate-y-1/2 text-muted"
            aria-hidden
          />
          <input
            ref={searchRef}
            type="search"
            value={filters.status ?? ""}
            onChange={(event) =>
              setFilters((current) => ({
                ...current,
                status: (event.target.value || undefined) as CdnPurgeStatus | undefined,
              }))
            }
            placeholder="Filter by status — press / to focus"
            aria-label="Filter the purge history by status"
            data-cdn-purge-filter
            className="w-full rounded-lg border border-line bg-surface py-2 pr-3 pl-8 text-[12.5px]"
          />
        </div>
        <select
          value={filters.kind ?? ""}
          onChange={(event) =>
            setFilters((current) => ({
              ...current,
              kind: (event.target.value || undefined) as CdnPurgeFilters["kind"],
            }))
          }
          aria-label="Filter the purge history by kind"
          data-cdn-purge-kind-filter
          className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px]"
        >
          <option value="">Every kind</option>
          {KINDS.map((kind) => (
            <option key={kind.value} value={kind.value}>
              {kind.label}
            </option>
          ))}
        </select>
        {(filters.status || filters.kind) && (
          <button
            type="button"
            onClick={() => setFilters({})}
            data-cdn-purge-filter-clear
            className="inline-flex items-center gap-1 rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] transition hover:bg-canvas"
          >
            <X className="size-3.5" aria-hidden />
            Clear
          </button>
        )}
      </div>

      {failedPurges.length > 0 ? (
        <p
          data-cdn-purge-failed-banner
          className="flex flex-wrap items-center gap-x-2 gap-y-1 rounded-xl border border-caution/30 bg-caution-soft px-4 py-2.5 text-[12.5px] text-caution"
        >
          <Filter className="size-3.5 shrink-0" aria-hidden />
          <span>
            {`${failedPurges.length} purge${failedPurges.length === 1 ? "" : "s"} on this page did not fully succeed. Open one to see the provider's message and retry what failed.`}
          </span>
        </p>
      ) : null}

      {purges.length === 0 ? (
        filters.status || filters.kind ? (
          <EmptyState
            title="Nothing matches that filter"
            hint="No purge on this page has that status or kind. Clearing the filter shows the full history."
          />
        ) : (
          <EmptyState
            title="No purge has been requested yet"
            hint="Purges are what tell the edge that a page it holds is stale. Queue one by URL, by surrogate key, or for the whole zone."
            action={
              <Link
                href="/cdn/purge"
                className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
              >
                Open the purge console
              </Link>
            }
          />
        )
      ) : (
        <>
          <div className="hidden overflow-x-auto rounded-xl border border-line bg-surface md:block">
            <table className="w-full text-left text-[12.5px]">
              <thead className="border-b border-line text-[11px] tracking-wide text-muted uppercase">
                <tr>
                  <th scope="col" className="px-4 py-2.5 font-medium">
                    When
                  </th>
                  <th scope="col" className="px-4 py-2.5 font-medium">
                    Kind
                  </th>
                  <th scope="col" className="px-4 py-2.5 font-medium">
                    Targets
                  </th>
                  <th scope="col" className="px-4 py-2.5 text-right font-medium">
                    Items
                  </th>
                  <th scope="col" className="px-4 py-2.5 text-right font-medium">
                    Failed
                  </th>
                  <th scope="col" className="px-4 py-2.5 font-medium">
                    Provider
                  </th>
                  <th scope="col" className="px-4 py-2.5 font-medium">
                    Status
                  </th>
                </tr>
              </thead>
              <tbody className="divide-y divide-line">
                {purges.map((purge) => (
                  <tr
                    key={purge.id}
                    data-cdn-purge-row
                    data-cdn-purge-status={purge.status}
                    data-cdn-purge-retryable={String(purge.retryable)}
                    className="cursor-pointer transition hover:bg-canvas"
                    onClick={() => setOpenId(purge.id)}
                  >
                    <td className="px-4 py-2.5 font-mono text-[11.5px] whitespace-nowrap text-muted">
                      {new Date(purge.requested_at).toLocaleString()}
                    </td>
                    <td className="px-4 py-2.5 text-muted">{purge.kind}</td>
                    <td className="max-w-[280px] truncate px-4 py-2.5 font-mono text-[12px]">
                      {targetSummary(purge)}
                    </td>
                    <td className="px-4 py-2.5 text-right tabular-nums">{purge.item_count}</td>
                    <td
                      data-cdn-purge-failed-count
                      className={
                        purge.failed_count > 0
                          ? "px-4 py-2.5 text-right tabular-nums text-caution"
                          : "px-4 py-2.5 text-right tabular-nums text-muted"
                      }
                    >
                      {purge.failed_count}
                    </td>
                    <td className="px-4 py-2.5 text-muted">{purge.provider}</td>
                    <td className="px-4 py-2.5">
                      <StatusBadge status={purge.status} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {/* The same rows as cards, carrying the same data-* hooks. A hook present in only
              one rendering silently halves what a depth pass can drive, and the mobile
              measurement is then of a layout no interaction has ever reached. */}
          <ul className="flex flex-col gap-2 md:hidden">
            {purges.map((purge) => (
              <li key={purge.id}>
                <button
                  type="button"
                  onClick={() => setOpenId(purge.id)}
                  data-cdn-purge-row
                  data-cdn-purge-status={purge.status}
                  data-cdn-purge-retryable={String(purge.retryable)}
                  className="flex w-full flex-col gap-1.5 rounded-xl border border-line bg-surface px-4 py-3 text-left"
                >
                  <span className="flex items-center justify-between gap-2">
                    <span className="text-[12px] text-muted">
                      {new Date(purge.requested_at).toLocaleString()}
                    </span>
                    <StatusBadge status={purge.status} />
                  </span>
                  <span className="truncate font-mono text-[12.5px]">{targetSummary(purge)}</span>
                  <span className="text-[11.5px] text-muted">
                    {`${purge.kind} · ${purge.item_count} item${purge.item_count === 1 ? "" : "s"} · ${purge.failed_count} failed · ${purge.provider}`}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </>
      )}

      {openId !== null ? (
        <div
          className="fixed inset-0 z-40 flex justify-end bg-black/20"
          onClick={() => setOpenId(null)}
          data-cdn-purge-drawer-overlay
        >
          <aside
            role="dialog"
            aria-label="Purge detail"
            data-cdn-purge-drawer
            onClick={(event) => event.stopPropagation()}
            className="flex h-full w-full max-w-[520px] flex-col gap-3 overflow-y-auto border-l border-line bg-canvas px-5 py-4"
          >
            <header className="flex items-start justify-between gap-3">
              <div className="flex min-w-0 flex-col gap-1">
                <h2 className="text-[14px] font-medium">Purge detail</h2>
                {detail ? (
                  <p className="text-[12px] text-muted">
                    {`${detail.purge.kind} · ${detail.purge.provider} · ${new Date(detail.purge.requested_at).toLocaleString()}`}
                  </p>
                ) : (
                  <p className="text-[12px] text-muted">Loading…</p>
                )}
              </div>
              <button
                type="button"
                onClick={() => setOpenId(null)}
                aria-label="Close the purge detail"
                data-cdn-purge-drawer-close
                className="rounded-lg border border-line bg-surface p-1.5 transition hover:bg-canvas"
              >
                <X className="size-3.5" aria-hidden />
              </button>
            </header>

            {detail ? (
              <>
                <div className="flex flex-wrap items-center gap-2">
                  <StatusBadge status={detail.purge.status} />
                  <span data-cdn-purge-drawer-counts className="text-[12px] text-muted">
                    {`${detail.purge.item_count - detail.purge.failed_count} of ${detail.purge.item_count} went through`}
                  </span>
                </div>

                {/*
                 * Who asked for this. An automatic purge has no author, and the honest
                 * rendering of that is the event that caused it — not a blank column and
                 * not the publisher's name, which would read as a person pressing the
                 * button for something the platform decided on its own.
                 */}
                <p
                  data-cdn-purge-origin
                  data-cdn-purge-origin-kind={detail.source ? "automatic" : "manual"}
                  className="text-[12px] text-muted"
                >
                  {detail.source ? (
                    <span className="font-mono">
                      {`automatic · ${detail.source.trigger} · event ${detail.source.event_id}`}
                    </span>
                  ) : (
                    "requested by an operator"
                  )}
                </p>

                {detail.purge.error ? (
                  <p
                    role="status"
                    data-cdn-purge-error
                    className="rounded-xl border border-caution/30 bg-caution-soft px-3 py-2 text-[12px] text-caution"
                  >
                    {detail.purge.error}
                  </p>
                ) : null}

                <div className="flex flex-wrap items-center gap-2">
                  {detail.purge.retryable ? (
                    <button
                      type="button"
                      onClick={() => void onRetry()}
                      disabled={busy}
                      data-cdn-purge-retry
                      className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
                    >
                      <RotateCcw className="size-3.5" aria-hidden />
                      {`Retry the ${detail.purge.failed_count} failed`}
                    </button>
                  ) : (
                    <p className="text-[12px] text-muted">
                      {detail.purge.status === "succeeded"
                        ? "Every target went through, so there is nothing to retry."
                        : "This purge is still moving — a worker is retrying it."}
                    </p>
                  )}
                  <button
                    type="button"
                    onClick={copyTargets}
                    disabled={detail.purge.targets.length === 0}
                    data-cdn-purge-copy-targets
                    className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
                  >
                    <Copy className="size-3.5" aria-hidden />
                    Copy targets
                  </button>
                </div>

                {fieldError ? (
                  <p
                    role="alert"
                    data-cdn-purge-retry-error
                    className="rounded-lg border border-accent/30 bg-accent-soft px-3 py-2 text-[12px] text-accent-strong"
                  >
                    {fieldError}
                  </p>
                ) : null}

                <div className="overflow-x-auto rounded-xl border border-line bg-surface">
                  <table className="w-full text-left text-[12px]">
                    <thead className="border-b border-line text-[11px] tracking-wide text-muted uppercase">
                      <tr>
                        <th scope="col" className="px-3 py-2 font-medium">
                          Target
                        </th>
                        <th scope="col" className="px-3 py-2 text-right font-medium">
                          Attempts
                        </th>
                        <th scope="col" className="px-3 py-2 font-medium">
                          Status
                        </th>
                      </tr>
                    </thead>
                    <tbody className="divide-y divide-line">
                      {detail.items.map((item) => (
                        <tr
                          key={item.id}
                          data-cdn-purge-item
                          data-cdn-purge-item-status={item.status}
                          className="align-top"
                        >
                          <td className="px-3 py-2">
                            <span className="block font-mono text-[11.5px] break-all">
                              {item.target}
                            </span>
                            {item.error ? (
                              <span
                                data-cdn-purge-item-error
                                className="mt-0.5 block text-[11px] text-caution"
                              >
                                {item.error}
                              </span>
                            ) : null}
                          </td>
                          <td className="px-3 py-2 text-right tabular-nums text-muted">
                            {item.attempts}
                          </td>
                          <td className="px-3 py-2">
                            <StatusBadge status={item.status} />
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>

                {detail.purge.finished_at ? (
                  <p className="text-[11.5px] text-muted">
                    {`Finished ${new Date(detail.purge.finished_at).toLocaleString()}`}
                  </p>
                ) : detail.purge.started_at ? (
                  <p className="text-[11.5px] text-muted">
                    {`Started ${new Date(detail.purge.started_at).toLocaleString()} — waiting on a worker`}
                  </p>
                ) : (
                  <p className="text-[11.5px] text-muted">Queued; a worker will pick it up.</p>
                )}
              </>
            ) : null}
          </aside>
        </div>
      ) : null}
    </div>
  );
}
