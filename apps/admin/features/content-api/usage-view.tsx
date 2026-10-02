"use client";

/**
 * `/content-api/usage` — the Usage tab (REQ-019, slice 3).
 *
 * The screen a person opens to answer three questions, in this order: *is the integration
 * alive*, *what is it asking for*, and *is it being throttled*. It is built around one decision
 * that everything else follows from:
 *
 * **Flushed and pending are shown as two numbers, never summed.**
 *
 * Counters are incremented in Redis and flushed into `api_token_usage_daily` on an interval, so
 * at any moment the truth is split across a durable table and a live window. A single "1,204
 * requests" cannot be reconciled with either source — an operator comparing the panel against an
 * access log has nothing to compare. So the chart is durable, the table carries both columns, and
 * the pending column says what it is.
 *
 * Four more things this screen must not do:
 *
 * 1. **Say "unknown" when the live window is unreadable.** `pending_readable: false` is not zero
 *    pending; it is the counter being unreachable. The column renders an em dash with the reason,
 *    because a zero there says "nothing in flight" on an installation that cannot know.
 * 2. **Not re-derive the server's roll-ups.** `series`, `tokens` and `endpoints` arrive computed
 *    from the same rows by the same function. Summing `rows` in the browser is a second answer to
 *    the same question, and it is the one that drifts the moment the server's day filter changes.
 * 3. **Not invent an endpoint.** An empty leaderboard is an empty leaderboard; a "0 requests" row
 *    for a route nobody has called would be a route the platform invented.
 * 4. **Not pretend the data is live.** The window is only as fresh as the flush, and a person
 *    watching the chart move is watching a worker, not a subscription. The header says so.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Activity,
  AlertCircle,
  BarChart3,
  Loader2,
  RefreshCw,
  TriangleAlert,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ApiError, fetchContentApiUsage } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type {
  ContentApiUsage,
  ContentApiUsageDay,
  ContentApiUsageEndpoint,
  ContentApiUsageToken,
} from "@/lib/types";

/** The windows the tab offers. Short ones first, because that is the order a person reads them. */
const WINDOWS = [7, 14, 30, 90] as const;

/** The window the tab shows before anybody asks for another. */
const DEFAULT_WINDOW = 30;

/** Thousands-separated, with a dash for "no number" — never `0` for a number we do not have. */
function count(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return "—";
  return new Intl.NumberFormat("en").format(value);
}

/**
 * A day label short enough for a bar's axis: `2026-10-01` → `1 Oct`.
 *
 * Built from the ISO string's own parts rather than `new Date(day)`: a date-only string is parsed
 * as UTC midnight and then formatted in the browser's zone, which moves the label a day west of
 * Greenwich and turns a chart into a chart of the wrong days.
 */
function dayLabel(day: string): string {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(day);
  if (!match) return day;
  const months = [
    "Jan",
    "Feb",
    "Mar",
    "Apr",
    "May",
    "Jun",
    "Jul",
    "Aug",
    "Sep",
    "Oct",
    "Nov",
    "Dec",
  ];
  const month = months[Number(match[2]) - 1] ?? match[2];
  return `${Number(match[3])} ${month}`;
}

/**
 * `true` when the whole window is quiet.
 *
 * Used only to decide whether the *chart* says "nothing yet" or draws fourteen zero-height bars.
 * It is deliberately not used to suppress the token table: an installation with two tokens that
 * have never been called still needs to see that both rows exist.
 */
function seriesIsQuiet(series: ContentApiUsageDay[]): boolean {
  return series.every((bar) => bar.requests === 0);
}

export function ContentApiUsageView() {
  const [days, setDays] = useState<number>(DEFAULT_WINDOW);
  const [usage, setUsage] = useState<ContentApiUsage | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);

  const load = useCallback(async () => {
    setRefreshing(true);
    setError(null);
    try {
      setUsage(await fetchContentApiUsage(days));
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setRefreshing(false);
    }
  }, [days]);

  useEffect(() => {
    void load();
  }, [load]);

  const totals = useMemo(() => (usage === null ? null : totalsOf(usage)), [usage]);

  if (error !== null && usage === null) {
    return <UsageError message={error} onRetry={() => void load()} />;
  }
  if (usage === null || totals === null) return <UsageSkeleton />;

  // `?? 0` is right **because** `null` means "unreadable", and an unreadable counter is not
  // evidence that the window is quiet — it is evidence that we cannot tell. Falling back to `0`
  // here would draw an empty chart; the warning about the unreadable counter is the totals row's
  // job, and it is where a reader will look for it.
  const quiet = seriesIsQuiet(usage.series) && (totals.pendingRequests ?? 0) === 0;

  return (
    <div className="space-y-6" data-content-api-usage="ready">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-[12.5px] text-muted">
          <span className="inline-flex items-center gap-1.5">
            <Activity className="h-3.5 w-3.5" aria-hidden />
            What the content API tokens have done in the last {usage.days} days.
          </span>
        </p>
        <div className="flex flex-wrap items-center gap-2">
          <label className="flex items-center gap-1.5 text-[12px] text-muted">
            <span>Window</span>
            <select
              data-content-api-usage-window
              value={days}
              onChange={(event) => setDays(Number(event.target.value))}
              className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[12.5px]"
            >
              {WINDOWS.map((window) => (
                <option key={window} value={window}>
                  {window} days
                </option>
              ))}
            </select>
          </label>
          <button
            type="button"
            data-content-api-usage-refresh
            onClick={() => void load()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <RefreshCw
              className={`h-3.5 w-3.5 ${refreshing ? "animate-spin" : ""}`}
              aria-hidden
            />
            Refresh
          </button>
        </div>
      </div>

      {error !== null ? <UsageError message={error} onRetry={() => void load()} /> : null}

      {/* The freshness note is not decoration. A person who watches the chart and expects it to
          move within a second has been told nothing about the flush interval, and concludes the
          feature is broken at the exact moment it is working. */}
      <p data-content-api-usage-freshness className="text-[11.5px] text-muted">
        Counts are incremented per request and written to the table on an interval, so the durable
        chart trails the live window by up to one flush. The two are never summed into one number,
        because neither half can be reconciled with the other on its own.
      </p>

      {/* A live window that cannot be read is a *different* state from a window with nothing in
          it, and it is stated before the numbers rather than in them. Every "counting" cell on the
          screen renders an em dash for the same reason; this strip is what tells the reader that
          the em dashes mean one thing. */}
      {usage.pending_readable ? null : (
        <p
          role="status"
          data-content-api-usage-unreadable
          className="flex items-center gap-2 rounded-lg border border-amber-500/40 px-3 py-2 text-[12.5px]"
        >
          <TriangleAlert className="h-3.5 w-3.5 shrink-0" aria-hidden />
          <span>
            The live request counter could not be read, so the &ldquo;counting&rdquo; columns are
            unknown rather than zero. Durable numbers below are unaffected. Reads are still being
            served — the limiter fails open when its counter is unreachable.
          </span>
        </p>
      )}

      <Totals usage={usage} totals={totals} />

      <UsageChart series={usage.series} quiet={quiet} />

      <div className="grid gap-6 lg:grid-cols-2">
        <EndpointLeaderboard endpoints={usage.endpoints} pendingReadable={usage.pending_readable} />
        <TokenTable tokens={usage.tokens} pendingReadable={usage.pending_readable} />
      </div>
    </div>
  );
}

/** The four numbers across the top. */
type Totals = {
  flushedRequests: number;
  /** `null` when the live window could not be read — never `0` in that case. */
  pendingRequests: number | null;
  errors: number;
  throttled: number;
};

/**
 * The roll-up, summed from the **server's** per-token rows rather than from `rows`.
 *
 * Both are the same data and both are correct; the per-token rows are chosen because they already
 * have the live window folded in and the organization filter applied, so a client-side sum cannot
 * reintroduce a day filter the server no longer applies.
 *
 * **`pending_readable` decides whether `pendingRequests` is a number or `null` — it is not
 * inferred from the arithmetic.** A reducer summing zeroes cannot produce a `null` on its own, so
 * an implementation that left the field alone would render "0" on an installation whose counter is
 * unreachable: the one sentence this whole column exists to make impossible. `throttled` has no
 * such ambiguity because the server counts refusals in both halves and both are readable from the
 * table, so a durable-only figure is still a true statement about what was refused.
 */
function totalsOf(usage: ContentApiUsage): Totals {
  const sum = usage.tokens.reduce(
    (accumulator, token) => ({
      flushedRequests: accumulator.flushedRequests + token.flushed_requests,
      pendingRequests: accumulator.pendingRequests + token.pending_requests,
      errors: accumulator.errors + token.flushed_errors,
      throttled: accumulator.throttled + token.flushed_throttled + token.pending_throttled,
    }),
    { flushedRequests: 0, pendingRequests: 0, errors: 0, throttled: 0 },
  );

  return {
    ...sum,
    pendingRequests: usage.pending_readable ? sum.pendingRequests : null,
  };
}

function Totals({ usage, totals }: { usage: ContentApiUsage; totals: Totals }) {
  const cards = [
    {
      key: "requests",
      label: "Requests (flushed)",
      value: count(totals.flushedRequests),
      hint: "written to the usage table",
    },
    {
      key: "pending",
      label: "In the live window",
      // `null` renders as the em dash `count` produces, and the reason is spelled out below it.
      value: count(totals.pendingRequests),
      hint: usage.pending_readable ? "counted, not yet written" : "counter unreachable",
    },
    {
      key: "errors",
      label: "Errors (flushed)",
      value: count(totals.errors),
      hint: "answered 4xx or 5xx",
    },
    {
      key: "throttled",
      label: "Throttled",
      value: count(totals.throttled),
      hint: "refused with 429",
    },
  ];

  return (
    <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
      {cards.map((card) => (
        <div
          key={card.key}
          data-content-api-usage-total={card.key}
          className="rounded-lg border border-line px-3 py-2.5"
        >
          <p className="text-[11px] tracking-wide text-muted uppercase">{card.label}</p>
          <p className="font-mono text-[20px] leading-tight">{card.value}</p>
          <p className="text-[11px] text-muted">{card.hint}</p>
        </div>
      ))}
    </div>
  );
}

/**
 * The daily bar chart.
 *
 * Four rules, each one a way a small hand-drawn chart lies:
 *
 * 1. **Zero-filled days draw a visible stub, not nothing.** The server already zero-fills; without
 *    a minimum height a quiet day is an invisible bar and the axis reads as a gap, which is what a
 *    person interprets as "the integration stopped".
 * 2. **Scaled to the window's own maximum**, and the scale is exposed as `data-*` so a QA pass can
 *    tell a drawn series from a flat line pretending to be one.
 * 3. **Throttled requests are drawn as a second bar**, because a day whose traffic was all `429`s
 *    looks exactly like a healthy day otherwise — the request count is the same and the difference
 *    is entirely in the refusal.
 * 4. **The quiet case says so in words** rather than drawing fourteen stubs and letting the reader
 *    work out that none of them means anything.
 */
function UsageChart({ series, quiet }: { series: ContentApiUsageDay[]; quiet: boolean }) {
  const max = useMemo(
    () => Math.max(1, ...series.flatMap((bar) => [bar.requests, bar.throttled])),
    [series],
  );

  if (quiet) {
    return (
      <section
        data-content-api-usage-chart="empty"
        className="rounded-lg border border-line px-4 py-6"
        aria-label="Requests per day"
      >
        <h2 className="flex items-center gap-1.5 text-[13px] font-medium">
          <BarChart3 className="h-3.5 w-3.5" aria-hidden />
          Requests per day
        </h2>
        <p className="py-6 text-center text-[12.5px] text-muted">
          No requests yet — try the Explorer.
        </p>
      </section>
    );
  }

  const labelEvery = Math.max(1, Math.ceil(series.length / 8));

  return (
    <section
      data-content-api-usage-chart="bars"
      data-content-api-usage-bars={series.length}
      data-content-api-usage-max={max}
      className="rounded-lg border border-line px-4 py-3"
      aria-label={`Requests per day over ${series.length} days, busiest day ${max} requests`}
    >
      <div className="flex flex-wrap items-center gap-3">
        <h2 className="flex items-center gap-1.5 text-[13px] font-medium">
          <BarChart3 className="h-3.5 w-3.5" aria-hidden />
          Requests per day
        </h2>
        <span className="flex items-center gap-1.5 text-[11.5px] text-muted">
          <span aria-hidden className="inline-block size-2 rounded-sm bg-accent" />
          requests
        </span>
        <span className="flex items-center gap-1.5 text-[11.5px] text-muted">
          <span aria-hidden className="inline-block size-2 rounded-sm bg-amber-500" />
          throttled
        </span>
      </div>

      <div className="mt-3 flex h-32 items-end gap-[3px]" role="presentation">
        {series.map((bar) => {
          const height = (bar.requests / max) * 100;
          const throttledHeight = (bar.throttled / max) * 100;
          return (
            <div
              key={bar.day}
              data-content-api-usage-bar={bar.day}
              data-content-api-usage-bar-requests={bar.requests}
              data-content-api-usage-bar-throttled={bar.throttled}
              title={`${dayLabel(bar.day)} — ${bar.requests} request${bar.requests === 1 ? "" : "s"}, ${bar.throttled} throttled`}
              className="flex h-full min-w-0 flex-1 flex-col justify-end gap-[2px]"
            >
              {bar.throttled > 0 ? (
                <span
                  aria-hidden
                  className="w-full rounded-sm bg-amber-500"
                  style={{ height: `${Math.max(2, throttledHeight)}%` }}
                />
              ) : null}
              <span
                aria-hidden
                // The 2% floor is what makes a quiet-but-nonzero day visible; below it the bar is
                // sub-pixel on a retina display and the day reads as a gap.
                className="w-full rounded-sm bg-accent"
                style={{ height: `${Math.max(bar.requests > 0 ? 2 : 1, height)}%` }}
              />
            </div>
          );
        })}
      </div>

      <div className="mt-1.5 flex gap-[3px]" role="presentation">
        {series.map((bar, index) => (
          <span
            key={bar.day}
            className="min-w-0 flex-1 truncate text-center text-[10px] text-muted"
            aria-hidden
          >
            {index % labelEvery === 0 ? dayLabel(bar.day) : ""}
          </span>
        ))}
      </div>
    </section>
  );
}

/**
 * What the traffic is made of.
 *
 * The `pending` chip on each row is the reconciliation aid: "312 requests · 12 still counting"
 * can be compared against a log, a bare 312 cannot. An endpoint that only exists in the live
 * window still appears — it is real traffic, and hiding it until the next flush would make the
 * panel disagree with the rate limiter that is refusing the 429s right now.
 */
function EndpointLeaderboard({
  endpoints,
  pendingReadable,
}: {
  endpoints: ContentApiUsageEndpoint[];
  /** Whether the "still counting" chip may be shown at all. */
  pendingReadable: boolean;
}) {
  const total = endpoints.reduce((sum, entry) => sum + entry.requests, 0);

  return (
    <section
      data-content-api-usage-endpoints={endpoints.length}
      className="rounded-lg border border-line"
      aria-label="Busiest endpoints"
    >
      <header className="border-b border-line px-3 py-2">
        <h2 className="text-[13px] font-medium">Busiest endpoints</h2>
        <p className="text-[11.5px] text-muted">
          Across every token. The matched route, not the full URL — one page read is one row however
          many slugs it asked for.
        </p>
      </header>
      {endpoints.length === 0 ? (
        <EmptyState
          title="No endpoint has been called"
          hint="Send a request from the Explorer and it appears here on the next refresh."
        />
      ) : (
        <ul className="divide-y divide-line">
          {endpoints.map((entry) => {
            const pending = entry.requests - entry.flushed_requests;
            const share = total > 0 ? entry.requests / total : 0;
            return (
              <li
                key={entry.endpoint}
                data-content-api-usage-endpoint={entry.endpoint}
                className="px-3 py-2.5"
              >
                <div className="flex items-baseline justify-between gap-2">
                  <code className="truncate text-[12px]">{entry.endpoint}</code>
                  <span className="shrink-0 font-mono text-[12px] text-muted">
                    {count(entry.requests)}
                    {entry.throttled > 0 ? (
                      <span className="ml-1.5 text-amber-600 dark:text-amber-400">
                        {count(entry.throttled)} throttled
                      </span>
                    ) : null}
                  </span>
                </div>
                <div className="mt-1 flex items-center gap-2">
                  <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-quiet-soft">
                    <div
                      className="h-full rounded-full bg-accent"
                      style={{ width: `${Math.max(1, share * 100).toFixed(1)}%` }}
                    />
                  </div>
                  {pendingReadable && pending > 0 ? (
                    <span
                      data-content-api-usage-pending
                      className="shrink-0 text-[11px] text-muted"
                      title="Counted in the live window; the next flush writes it."
                    >
                      {count(pending)} counting
                    </span>
                  ) : null}
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}

/**
 * The per-token table.
 *
 * Six columns, and the two that matter are the *split* ones: a token's total is only meaningful
 * next to how much of it is still in flight. `Last used` comes from the server because the
 * question the screen is asked — "is this token doing anything" — cannot be answered by a column
 * that is blank, and it is the one column that can disagree with the rest of the row: a token
 * whose every call failed still has a `last_used_at`, because authentication runs first.
 *
 * A token with no usage is shown, not hidden. "This token has never been called" is exactly the
 * thing an operator opens this screen to find.
 */
function TokenTable({
  tokens,
  pendingReadable,
}: {
  tokens: ContentApiUsageToken[];
  /** Whether the "counting" column may hold a number. */
  pendingReadable: boolean;
}) {
  return (
    <section
      data-content-api-usage-tokens={tokens.length}
      className="rounded-lg border border-line"
      aria-label="Per-token usage"
    >
      <header className="border-b border-line px-3 py-2">
        <h2 className="text-[13px] font-medium">Per token</h2>
        <p className="text-[11.5px] text-muted">
          Every token of this organization, including the ones nobody has called.
        </p>
      </header>
      {tokens.length === 0 ? (
        <EmptyState
          title="No tokens yet"
          hint="A token is how a headless frontend reads published content. Mint one on the Tokens tab."
        />
      ) : (
        <>
          <div className="hidden overflow-x-auto sm:block">
            <table
              data-content-api-usage-table
              className="w-full min-w-[640px] text-left text-[12.5px]"
            >
              <thead className="text-[11px] tracking-wide text-muted uppercase">
                <tr>
                  <th scope="col" className="px-3 py-2 font-medium">Token</th>
                  <th scope="col" className="px-3 py-2 text-right font-medium">Requests</th>
                  <th scope="col" className="px-3 py-2 text-right font-medium">Counting</th>
                  <th scope="col" className="px-3 py-2 text-right font-medium">Errors</th>
                  <th scope="col" className="px-3 py-2 text-right font-medium">Throttled</th>
                  <th scope="col" className="px-3 py-2 font-medium">Last used</th>
                </tr>
              </thead>
              <tbody>
                {tokens.map((token) => (
                  <tr
                    key={token.token_id}
                    data-content-api-usage-token={token.token_id}
                    className="border-t border-line"
                  >
                    <td className="px-3 py-2">{token.name}</td>
                    <td className="px-3 py-2 text-right font-mono">
                      {count(token.flushed_requests)}
                    </td>
                    <td className="px-3 py-2 text-right font-mono text-muted">
                      {pendingReadable ? count(token.pending_requests) : "—"}
                    </td>
                    <td className="px-3 py-2 text-right font-mono text-muted">
                      {token.flushed_errors > 0 ? count(token.flushed_errors) : "—"}
                    </td>
                    <td className="px-3 py-2 text-right font-mono text-muted">
                      {token.flushed_throttled + token.pending_throttled > 0
                        ? count(token.flushed_throttled + token.pending_throttled)
                        : "—"}
                    </td>
                    <td className="px-3 py-2 text-muted">
                      {token.last_used_at ? formatTimestamp(token.last_used_at) : "never"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {/* Below `sm` the same rows as cards, through the same data — a seven-column table on a
              390 px screen is a horizontal scrollbar, and the spec's own mobile rule is a card
              list. Two layouts over one array cannot disagree about a number. */}
          <ul data-content-api-usage-cards className="flex flex-col gap-2 p-3 sm:hidden">
            {tokens.map((token) => (
              <li
                key={token.token_id}
                data-content-api-usage-card={token.token_id}
                className="rounded-lg border border-line px-3 py-2.5"
              >
                <p className="text-[12.5px] font-medium">{token.name}</p>
                <dl className="mt-1.5 grid grid-cols-2 gap-x-3 gap-y-1 text-[11.5px]">
                  <dt className="text-muted">Requests</dt>
                  <dd className="text-right font-mono">
                    {count(token.flushed_requests)}
                  </dd>
                  <dt className="text-muted">Counting</dt>
                  <dd className="text-right font-mono">
                    {pendingReadable ? count(token.pending_requests) : "—"}
                  </dd>
                  <dt className="text-muted">Errors</dt>
                  <dd className="text-right font-mono">
                    {token.flushed_errors > 0 ? count(token.flushed_errors) : "—"}
                  </dd>
                  <dt className="text-muted">Throttled</dt>
                  <dd className="text-right font-mono">
                    {token.flushed_throttled + token.pending_throttled > 0
                      ? count(token.flushed_throttled + token.pending_throttled)
                      : "—"}
                  </dd>
                </dl>
                <p className="mt-1.5 text-[11.5px] text-muted">
                  Last used{" "}
                  {token.last_used_at ? formatTimestamp(token.last_used_at) : "never"}
                </p>
              </li>
            ))}
          </ul>
        </>
      )}
    </section>
  );
}

function UsageSkeleton() {
  return (
    <div className="space-y-3" data-content-api-usage="loading" aria-busy="true">
      <div className="h-4 w-64 animate-pulse rounded bg-quiet-soft" />
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        {Array.from({ length: 4 }, (_, index) => (
          <div key={index} className="h-16 animate-pulse rounded-lg bg-quiet-soft" />
        ))}
      </div>
      <div className="h-40 animate-pulse rounded-lg bg-quiet-soft" />
    </div>
  );
}

function UsageError({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div
      role="alert"
      data-content-api-usage-error
      className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-red-500/40 px-3 py-2.5 text-[12.5px]"
    >
      <span className="flex items-center gap-2">
        <AlertCircle className="h-3.5 w-3.5" aria-hidden />
        {message}
      </span>
      <button
        type="button"
        onClick={onRetry}
        className="inline-flex items-center gap-1.5 rounded border border-line px-2 py-1"
      >
        <RefreshCw className="h-3 w-3" aria-hidden />
        Try again
      </button>
    </div>
  );
}
