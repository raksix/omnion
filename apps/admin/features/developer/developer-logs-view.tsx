"use client";

/**
 * `/developer/logs` — the request history (REQ-033, slice 1).
 *
 * The screen's one job is to make "why is my integration failing" answerable, and two facts about
 * it are stated on the screen rather than left to be discovered:
 *
 * - **No bodies are stored.** The table has no payload column *and* the migration has no payload
 *   column, so the absence is structural. Without this line an operator spends twenty minutes
 *   looking for a "view body" button that does not exist and concludes the platform is broken.
 * - **The status column carries a label, not only a colour.** A `403` in grey and a `403` in red
 *   are the same event; a screen that distinguishes them with hue alone fails for anyone who
 *   cannot read the hue.
 *
 * The filters are the request's own list — key, status class, path prefix, method, date range,
 * duration — and each maps to a column the migration indexes, so the screen degrades into "slow"
 * rather than "broken" as the table fills.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { Check, Copy, Filter, Loader2, RefreshCw, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  fetchApiKeys,
  fetchRequestLog,
  fetchRequestLogs,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { ApiKey, ApiRequestLog } from "@/lib/types";

/** The ranges the screen offers. The API's own default is 24 hours. */
const RANGES: { value: string; label: string; hours: number }[] = [
  { value: "1", label: "Last hour", hours: 1 },
  { value: "24", label: "Last 24 hours", hours: 24 },
  { value: "168", label: "Last 7 days", hours: 168 },
  { value: "720", label: "Last 30 days", hours: 720 },
];

/** Status classes as the API accepts them. */
const CLASSES = ["2xx", "3xx", "4xx", "5xx"] as const;

/** Methods a filter offers — the ones the platform actually routes. */
const METHODS = ["GET", "POST", "PUT", "PATCH", "DELETE"] as const;

const PAGE_SIZE = 50;

/** The tone of a status class, as a class name. Never the only signal — the number is beside it. */
function statusTone(status: number): string {
  if (status >= 500) return "text-accent-strong";
  if (status >= 400) return "text-caution";
  if (status >= 200 && status < 300) return "text-positive";
  return "text-muted";
}

/** The screen's filter state, which is also the query it sends. */
type Filters = {
  key_prefix: string;
  status_class: string;
  path_prefix: string;
  method: string;
  range: string;
  min_duration_ms: string;
};

const EMPTY_FILTERS: Filters = {
  key_prefix: "",
  status_class: "",
  path_prefix: "",
  method: "",
  range: "24",
  min_duration_ms: "",
};

export function DeveloperLogsView() {
  const [logs, setLogs] = useState<ApiRequestLog[] | null>(null);
  const [total, setTotal] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [offset, setOffset] = useState(0);
  const [reloadToken, setReloadToken] = useState(0);

  const [keys, setKeys] = useState<ApiKey[]>([]);
  const [filters, setFilters] = useState<Filters>(() => {
    // The key row's "View logs" link arrives here as a query parameter, which is the whole point
    // of that link: it must not need the operator to re-pick the key from a dropdown.
    if (typeof window === "undefined") {
      return EMPTY_FILTERS;
    }
    const requested = new URLSearchParams(window.location.search).get("key_prefix");
    return requested && requested.length > 0 ? { ...EMPTY_FILTERS, key_prefix: requested } : EMPTY_FILTERS;
  });

  const [selected, setSelected] = useState<ApiRequestLog | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const rangeHours = useMemo(
    () => RANGES.find((entry) => entry.value === filters.range)?.hours ?? 24,
    [filters.range],
  );

  const load = useCallback(() => {
    setBusy(true);
    setError(null);
    const minimum = Number(filters.min_duration_ms);
    fetchRequestLogs({
      key_prefix: filters.key_prefix || undefined,
      status_class: (filters.status_class || undefined) as "2xx" | undefined,
      path_prefix: filters.path_prefix || undefined,
      method: filters.method || undefined,
      since_hours: rangeHours,
      min_duration_ms:
        Number.isFinite(minimum) && minimum > 0 ? Math.trunc(minimum) : undefined,
      limit: PAGE_SIZE,
      offset,
    })
      .then((page) => {
        setLogs(page.items);
        setTotal(page.total);
      })
      .catch((cause: unknown) => {
        setLogs([]);
        setError(
          cause instanceof ApiError ? cause.message : "The request log could not be loaded.",
        );
      })
      .finally(() => setBusy(false));
  }, [filters.key_prefix, filters.status_class, filters.path_prefix, filters.method, filters.min_duration_ms, rangeHours, offset]);

  useEffect(() => {
    load();
  }, [load]);

  useEffect(() => {
    fetchApiKeys()
      .then((response) => setKeys(response.keys))
      // The filter's key dropdown is a convenience; a failure to load it must not take the log
      // screen down with it, so this one failure is deliberately silent — the prefix filter is
      // still a text input and still works.
      .catch(() => setKeys([]));
  }, [reloadToken]);

  const setFilter = (patch: Partial<Filters>) => {
    setOffset(0);
    setFilters((current) => ({ ...current, ...patch }));
  };

  const filtersActive =
    filters.key_prefix !== "" ||
    filters.status_class !== "" ||
    filters.path_prefix !== "" ||
    filters.method !== "" ||
    filters.min_duration_ms !== "";

  const openRow = async (row: ApiRequestLog) => {
    setSelected(row);
    setDetailError(null);
    setCopied(false);
    // The list row already carries everything the drawer shows, but the single-row endpoint is
    // what the request asks for and what a support conversation quotes — so it is fetched, and
    // the drawer keeps the list's row if the fetch fails rather than going blank.
    try {
      const full = await fetchRequestLog(row.id);
      setSelected(full);
    } catch (cause: unknown) {
      setDetailError(
        cause instanceof ApiError ? cause.message : "This request's detail could not be loaded.",
      );
    }
  };

  if (logs === null) {
    return error ? (
      <div className="flex flex-col items-center gap-3 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <p className="text-[12.5px] text-accent-strong">{error}</p>
        <button
          type="button"
          onClick={() => setReloadToken((token) => token + 1)}
          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px]"
        >
          Try again
        </button>
      </div>
    ) : (
      <div className="flex items-center gap-2 px-1 py-8 text-[12.5px] text-muted">
        <Loader2 className="size-3.5 animate-spin" aria-hidden />
        Loading the request log…
      </div>
    );
  }

  const showingFrom = total === 0 ? 0 : offset + 1;
  const showingTo = offset + logs.length;

  return (
    <div className="flex flex-col gap-4">
      {/* The absence of bodies, stated before the reader goes looking for them. */}
      <p
        data-developer-logs-no-bodies
        className="rounded-xl border border-line bg-canvas/60 px-4 py-2.5 text-[11.5px] text-muted"
      >
        This log stores request <span className="font-medium text-ink">metadata only</span> —
        method, path, status, duration, sizes and the request id. Request and response bodies are
        never written, so there is nothing to expand here and nothing was captured to leak.
      </p>

      <section
        aria-label="Filters"
        data-developer-logs-filters
        className="flex flex-wrap items-end gap-3 rounded-xl border border-line bg-surface px-4 py-3"
      >
        <label className="flex flex-col gap-1 text-[12px] text-muted">
          <span className="inline-flex items-center gap-1">
            <Filter className="size-3" aria-hidden />
            Range
          </span>
          <select
            value={filters.range}
            onChange={(event) => setFilter({ range: event.target.value })}
            data-developer-logs-range
            className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
          >
            {RANGES.map((entry) => (
              <option key={entry.value} value={entry.value}>
                {entry.label}
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1 text-[12px] text-muted">
          Key
          <select
            value={filters.key_prefix}
            onChange={(event) => setFilter({ key_prefix: event.target.value })}
            data-developer-logs-key
            className="min-h-9 max-w-56 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
          >
            <option value="">Any key</option>
            {keys.map((key) => (
              <option key={key.id} value={key.prefix}>
                {key.name} ({key.prefix})
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1 text-[12px] text-muted">
          Status
          <select
            value={filters.status_class}
            onChange={(event) => setFilter({ status_class: event.target.value })}
            data-developer-logs-class
            className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
          >
            <option value="">Any status</option>
            {CLASSES.map((entry) => (
              <option key={entry} value={entry}>
                {entry}
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1 text-[12px] text-muted">
          Method
          <select
            value={filters.method}
            onChange={(event) => setFilter({ method: event.target.value })}
            data-developer-logs-method
            className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
          >
            <option value="">Any method</option>
            {METHODS.map((entry) => (
              <option key={entry} value={entry}>
                {entry}
              </option>
            ))}
          </select>
        </label>
        <label className="flex min-w-40 flex-1 flex-col gap-1 text-[12px] text-muted">
          Path starts with
          <input
            value={filters.path_prefix}
            onChange={(event) => setFilter({ path_prefix: event.target.value })}
            data-developer-logs-path
            placeholder="/api/v1/pages"
            className="min-h-9 rounded-lg border border-line bg-canvas px-2 font-mono text-[12px] text-ink"
          />
        </label>
        <label className="flex w-28 flex-col gap-1 text-[12px] text-muted">
          Min ms
          <input
            value={filters.min_duration_ms}
            onChange={(event) => setFilter({ min_duration_ms: event.target.value })}
            data-developer-logs-duration
            inputMode="numeric"
            placeholder="0"
            className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
          />
        </label>
        {filtersActive ? (
          <button
            type="button"
            data-developer-logs-clear
            onClick={() => {
              setOffset(0);
              setFilters(EMPTY_FILTERS);
            }}
            className="min-h-9 rounded-lg border border-line px-2.5 text-[12.5px] transition hover:bg-canvas"
          >
            Clear
          </button>
        ) : null}
        <button
          type="button"
          onClick={() => setReloadToken((token) => token + 1)}
          className="inline-flex min-h-9 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12.5px] transition hover:bg-canvas"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Refresh
        </button>
      </section>

      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-[12.5px] text-muted" data-developer-logs-count>
          {busy
            ? "Loading…"
            : `Showing ${showingFrom}–${showingTo} of ${total} request${total === 1 ? "" : "s"}.`}
        </p>
        <div className="flex items-center gap-2">
          <button
            type="button"
            disabled={offset === 0 || busy}
            onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
            data-developer-logs-prev
            className="min-h-9 rounded-lg border border-line px-2.5 text-[12.5px] disabled:opacity-40"
          >
            Newer
          </button>
          <button
            type="button"
            disabled={showingTo >= total || busy}
            onClick={() => setOffset(offset + PAGE_SIZE)}
            data-developer-logs-next
            className="min-h-9 rounded-lg border border-line px-2.5 text-[12.5px] disabled:opacity-40"
          >
            Older
          </button>
        </div>
      </div>

      {error ? (
        <p
          role="alert"
          className="rounded-xl border border-accent/30 bg-accent-soft px-4 py-3 text-[12.5px] text-accent-strong"
        >
          {error}
        </p>
      ) : null}

      {logs.length === 0 ? (
        <div className="rounded-xl border border-line bg-surface">
          <EmptyState
            title="Bu aralıkta istek yok"
            hint={
              filtersActive
                ? "Nothing matched these filters. Widen the range or clear a filter — an integration that is genuinely silent looks identical to one filtered out."
                : "Nothing has called this platform through an API key in this window. That is either a quiet period or an integration that has not been pointed here yet."
            }
            action={
              filtersActive ? (
                <button
                  type="button"
                  onClick={() => {
                    setOffset(0);
                    setFilters(EMPTY_FILTERS);
                  }}
                  className="min-h-9 rounded-lg border border-line px-3 text-[12.5px]"
                >
                  Widen to the last 24 hours
                </button>
              ) : undefined
            }
          />
        </div>
      ) : (
        <>
          <div className="hidden overflow-x-auto rounded-xl border border-line bg-surface md:block">
            <table className="w-full text-left text-[12.5px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                  <th className="px-3 py-2 font-medium">Time</th>
                  <th className="px-3 py-2 font-medium">Method</th>
                  <th className="px-3 py-2 font-medium">Path</th>
                  <th className="px-3 py-2 font-medium">Status</th>
                  <th className="px-3 py-2 font-medium">Duration</th>
                  <th className="px-3 py-2 font-medium">Key</th>
                  <th className="px-3 py-2 font-medium">Request id</th>
                </tr>
              </thead>
              <tbody>
                {logs.map((row) => (
                  <tr
                    key={row.id}
                    data-developer-log-row
                    onClick={() => openRow(row)}
                    className="cursor-pointer border-b border-line last:border-b-0 hover:bg-canvas"
                  >
                    <td className="whitespace-nowrap px-3 py-2 text-muted">
                      {formatTimestamp(row.created_at)}
                    </td>
                    <td className="px-3 py-2 font-mono text-[11.5px]">{row.method}</td>
                    <td className="px-3 py-2 font-mono text-[11.5px]">{row.path}</td>
                    <td className={`px-3 py-2 font-medium ${statusTone(row.status)}`}>
                      {row.status}
                      {row.error_code ? (
                        <span className="ml-1.5 text-[11px] font-normal text-muted">
                          {row.error_code}
                        </span>
                      ) : null}
                    </td>
                    <td className="px-3 py-2 text-muted">{row.duration_ms} ms</td>
                    <td className="px-3 py-2 font-mono text-[11px] text-muted">
                      {row.api_key_id ? row.api_key_id.slice(0, 8) : "session"}
                    </td>
                    <td className="px-3 py-2 font-mono text-[11px] text-muted">{row.request_id}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <div className="flex flex-col gap-2 md:hidden">
            {logs.map((row) => (
              <button
                key={row.id}
                type="button"
                data-developer-log-card
                onClick={() => openRow(row)}
                className="rounded-xl border border-line bg-surface px-4 py-3 text-left"
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="font-mono text-[12px]">
                    {row.method} {row.path}
                  </span>
                  <span className={`text-[12.5px] font-medium ${statusTone(row.status)}`}>
                    {row.status}
                  </span>
                </div>
                <p className="mt-1 text-[11.5px] text-muted">
                  {formatTimestamp(row.created_at)} · {row.duration_ms} ms
                </p>
              </button>
            ))}
          </div>
        </>
      )}

      {selected ? (
        <div
          role="dialog"
          aria-modal="true"
          aria-label="Request detail"
          data-developer-log-drawer
          className="fixed inset-0 z-50 flex justify-end bg-black/40"
        >
          <div className="flex h-full w-full max-w-md flex-col gap-3 overflow-y-auto border-l border-line bg-surface px-5 py-4">
            <div className="flex items-center justify-between">
              <h2 className="text-[13.5px] font-medium">Request detail</h2>
              <button
                type="button"
                data-developer-log-drawer-close
                onClick={() => setSelected(null)}
                className="inline-flex min-h-11 min-w-11 items-center justify-center rounded-lg border border-line"
              >
                <X className="size-4" aria-hidden />
                <span className="sr-only">Close</span>
              </button>
            </div>

            <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1.5 text-[12.5px]">
              <dt className="text-muted">Request</dt>
              <dd className="font-mono text-[12px]">
                {selected.method} {selected.path}
              </dd>
              <dt className="text-muted">Status</dt>
              <dd className={statusTone(selected.status)}>
                {selected.status}
                {selected.error_code ? ` · ${selected.error_code}` : ""}
              </dd>
              <dt className="text-muted">Duration</dt>
              <dd>{selected.duration_ms} ms</dd>
              <dt className="text-muted">Time</dt>
              <dd>{formatTimestamp(selected.created_at)}</dd>
              <dt className="text-muted">Request id</dt>
              <dd className="font-mono text-[11.5px] break-all">{selected.request_id}</dd>
              <dt className="text-muted">Key</dt>
              <dd className="font-mono text-[11.5px] break-all">
                {selected.api_key_id ?? "session-authenticated"}
              </dd>
              <dt className="text-muted">User</dt>
              <dd className="font-mono text-[11.5px] break-all">
                {selected.actor_user_id ?? "—"}
              </dd>
              <dt className="text-muted">Bytes in</dt>
              <dd>{selected.bytes_in ?? "—"}</dd>
              <dt className="text-muted">Bytes out</dt>
              <dd>{selected.bytes_out ?? "—"}</dd>
            </dl>

            <p className="rounded-lg border border-line bg-canvas/60 px-3 py-2 text-[11.5px] text-muted">
              No request or response body was stored for this call. The columns above are the
              whole of what the platform knows about it once it is over.
            </p>

            {detailError ? (
              <p role="alert" className="text-[12px] text-caution">
                {detailError}
              </p>
            ) : null}

            <button
              type="button"
              data-developer-log-copy-curl
              onClick={async () => {
                const command = `curl -H "Authorization: Bearer omn_<prefix>.<secret>" "${
                  typeof window === "undefined" ? "" : window.location.origin
                }/api/v1${selected.path.replace(/^\/api\/v1/, "")}"`;
                try {
                  await navigator.clipboard.writeText(command);
                  setCopied(true);
                } catch {
                  setCopied(false);
                }
              }}
              className="inline-flex min-h-11 items-center justify-center gap-1.5 rounded-lg border border-line text-[12.5px]"
            >
              {copied ? <Check className="size-3.5 text-positive" aria-hidden /> : <Copy className="size-3.5" aria-hidden />}
              {copied ? "Copied" : "Copy as curl"}
            </button>
          </div>
        </div>
      ) : null}
    </div>
  );
}
