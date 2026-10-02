"use client";

/**
 * `/ai/telemetry` — the tool telemetry screen (REQ-107, slice 5).
 *
 * The question this screen exists to answer is one question: **which tool should I fix, and which
 * permission should I widen?** A denial is a configuration decision and a failure is a defect, and
 * a screen that merges them into one "error rate" cannot tell an operator which of the two they
 * are looking at — which is why denials are a column of their own and are not folded into the
 * failure count anywhere below.
 *
 * Four panels, and three decisions shape all of them:
 *
 * 1. **"—" and "0%" are different claims, and the screen keeps them apart.** `success_percent` is
 *    `null` for a tool nobody called and `0` for one that was called and never worked. The first
 *    renders as an em dash; the second renders as a red `0%`. Collapsing them would sort a tool
 *    nobody invoked to the bottom of a table sorted by success rate — a false alarm manufactured
 *    by a convenience.
 *
 * 2. **The window is the server's, not the picker's.** The route clamps `from`/`to` into
 *    `[today-365, today]` and *swaps* an inverted range rather than refusing it. Every label here
 *    reads `data.from` / `data.to`, so the header cannot claim a window the payload does not
 *    describe — which is the failure a range picker produces when the client trusts its own input.
 *
 * 3. **A tool's own history is beside its numbers.** `days_seen` says "3 of 30 days": a tool that
 *    appears on day 1 and vanishes was not as busy as one that answered every day, and a table
 *    that only showed call counts cannot express the difference.
 *
 * Each row links into `/ai/tools/{key}`, whose own screen carries that tool's recent calls — the
 * row's "why" is one click away rather than being reconstructed from a histogram.
 *
 * **The empty state is the state most likely to render as a confident lie.** A tenant with no
 * telemetry gets wording that says what has *not* happened yet ("no tool has been called in this
 * window"), never "every tool is healthy" — an empty table next to a green headline is a screen
 * claiming a measurement it never took.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import {
  Activity,
  ArrowRight,
  Ban,
  Filter,
  Gauge,
  Loader2,
  RefreshCw,
  TriangleAlert,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError } from "@/lib/api";
import {
  fetchToolTelemetry,
  latency,
  micros,
  TELEMETRY_WINDOWS,
  type SolvedDay,
  type StepBucket,
  type ToolTelemetry,
  type ToolTelemetryRow,
} from "@/lib/ai-telemetry-api";

/** An `YYYY-MM-DD` day, in UTC — the same frame the roll-up buckets on. */
function isoDay(date: Date): string {
  return date.toISOString().slice(0, 10);
}

/** Today's date, in the server's frame. Read once per render rather than per row. */
function today(): string {
  return isoDay(new Date());
}

/**
 * A share as a percentage, or an em dash.
 *
 * The em dash is the whole point: `null` means the rate does not exist (no calls), and `0` means
 * it exists and is zero. A `??` here would be a lie in the one direction that hurts.
 */
function percent(value: number | null): string {
  return value === null ? "—" : `${value.toFixed(1)}%`;
}

/** A day's date, shortened for a column that is mostly dates. */
function shortDay(day: string): string {
  return day.slice(5).replace("-", "/");
}

/** The headline tiles. Each names the window it counts, because the window is a choice here. */
function StatTiles({ data }: { data: ToolTelemetry }) {
  const success =
    data.totals.calls > 0
      ? `${((data.totals.successes / data.totals.calls) * 100).toFixed(1)}%`
      : "—";
  const tiles = [
    { label: "Tool calls", value: data.totals.calls.toLocaleString(), hint: `in ${data.days} days` },
    { label: "Succeeded", value: success, hint: "of every call made" },
    {
      label: "Denied",
      value: data.totals.denials.toLocaleString(),
      hint: "refused by policy, not broken",
    },
    {
      label: "Cost",
      value: micros(data.totals.cost_micros),
      hint: "in the window",
    },
  ];
  return (
    <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
      {tiles.map((tile) => (
        <div key={tile.label} className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">{tile.label}</p>
          <p className="mt-1 text-2xl font-semibold text-ink">{tile.value}</p>
          <p className="mt-0.5 text-[11px] text-muted">{tile.hint}</p>
        </div>
      ))}
    </div>
  );
}

/**
 * The ranked failure codes of one tool, or an honest "none attributed".
 *
 * The distinction that matters: an empty histogram means no failure was given a code, which is not
 * the same as no failures — a tool can have ten failures and ten `unknown`s. So the empty case
 * says which one it is looking at rather than implying the tool is clean.
 */
function ErrorCodes({ row }: { row: ToolTelemetryRow }) {
  const ranked = Object.entries(row.error_codes).sort((a, b) => b[1] - a[1]);
  if (ranked.length === 0) {
    return (
      <span className="text-muted">
        {row.failures === 0 ? "none" : `${row.failures} unattributed`}
      </span>
    );
  }
  return (
    <span className="flex flex-wrap gap-1">
      {ranked.map(([code, hits]) => (
        <span
          key={code}
          className="rounded bg-danger-soft px-1.5 py-0.5 font-mono text-[11px] text-danger"
          title={`${hits} call${hits === 1 ? "" : "s"}`}
        >
          {code} · {hits}
        </span>
      ))}
    </span>
  );
}

/** The window's totals for the range picker — the words, not the arithmetic. */
function RangeLabel({ data }: { data: ToolTelemetry }) {
  return (
    <p data-telemetry-range className="text-[12px] text-muted">
      Showing <span className="font-medium text-ink">{shortDay(data.from)}</span> to{" "}
      <span className="font-medium text-ink">{shortDay(data.to)}</span> — {data.days} day
      {data.days === 1 ? "" : "s"}, as the server resolved it
    </p>
  );
}

/** One tool, as a table row. Percentages are coloured only where a number exists. */
function ToolRow({ row, days }: { row: ToolTelemetryRow; days: number }) {
  const rate = row.success_percent;
  const rateClass =
    rate === null
      ? "text-muted"
      : rate >= 95
        ? "text-success"
        : rate >= 80
          ? "text-ink"
          : "text-danger";
  return (
    <tr data-telemetry-tool={row.tool} className="border-b border-line last:border-0">
      <td className="px-3 py-2.5 align-top">
        <Link
          href={`/ai/tools/${encodeURIComponent(row.tool)}`}
          className="inline-flex items-center gap-1 text-[13px] font-medium text-ink hover:underline"
        >
          {row.tool}
          <ArrowRight aria-hidden size={13} className="text-muted" />
        </Link>
        {/* "3 of 30 days" beside the call count: a tool seen once is not a busy tool, and only
            this line can say so. */}
        <p className="mt-0.5 text-[11px] text-muted">
          seen {row.days_seen} of {days} day{days === 1 ? "" : "s"}
        </p>
      </td>
      <td className="px-3 py-2.5 align-top text-[13px] text-ink tabular-nums">
        {row.calls.toLocaleString()}
        <p className="mt-0.5 text-[11px] text-muted">
          {row.successes.toLocaleString()} ok · {row.failures.toLocaleString()} failed
        </p>
      </td>
      <td className={`px-3 py-2.5 align-top text-[13px] tabular-nums ${rateClass}`}>
        {percent(rate)}
      </td>
      <td className="px-3 py-2.5 align-top text-[13px] tabular-nums">
        {row.denials === 0 ? (
          <span className="text-muted">—</span>
        ) : (
          <span className="inline-flex items-center gap-1 text-warning">
            <Ban aria-hidden size={12} />
            {row.denials.toLocaleString()} ({percent(row.denial_percent)})
          </span>
        )}
      </td>
      <td className="px-3 py-2.5 align-top text-[12px]">
        <ErrorCodes row={row} />
      </td>
      <td className="px-3 py-2.5 align-top text-right text-[12px] tabular-nums text-muted">
        <p>{latency(row.p50_ms)}</p>
        <p className="mt-0.5">p95 {latency(row.p95_ms)}</p>
        <p className="mt-0.5">p99 {latency(row.p99_ms)}</p>
      </td>
    </tr>
  );
}

/** One tool as a card, for the width where the table cannot hold seven columns. */
function ToolCard({ row, days }: { row: ToolTelemetryRow; days: number }) {
  return (
    <li
      data-telemetry-tool-card={row.tool}
      className="flex flex-col gap-1 rounded-lg border border-line p-3"
    >
      <div className="flex items-start justify-between gap-2">
        <Link
          href={`/ai/tools/${encodeURIComponent(row.tool)}`}
          className="font-mono text-[13px] font-medium underline-offset-2 hover:underline"
        >
          {row.tool}
        </Link>
        <span className="text-[11px] text-muted">
          {row.days_seen}/{days}d
        </span>
      </div>
      <p className="text-[12px] text-muted">
        {row.calls.toLocaleString()} calls · {percent(row.success_percent)} succeeded ·{" "}
        {row.denials.toLocaleString()} denied
      </p>
      <p className="text-[11px] text-muted">
        p50 {latency(row.p50_ms)} · p95 {latency(row.p95_ms)} · p99 {latency(row.p99_ms)}
      </p>
      <div className="text-[12px]">
        <ErrorCodes row={row} />
      </div>
    </li>
  );
}

/**
 * How many steps the settled runs took.
 *
 * **Bars are scaled to the tallest bucket, and the tallest bar is labelled.** A histogram whose
 * bars are all the same height because each is scaled to itself says nothing; the point of the
 * panel is the *shape*, and the shape is only visible against a shared ceiling. The zero bucket
 * is kept because "answered without calling a tool" is a real and interesting run.
 */
function StepHistogram({ buckets }: { buckets: StepBucket[] }) {
  const tallest = Math.max(1, ...buckets.map((bucket) => bucket.runs));
  const total = buckets.reduce((sum, bucket) => sum + bucket.runs, 0);
  return (
    <section className="rounded-xl border border-line bg-surface">
      <header className="border-b border-line px-4 py-3">
        <h2 className="flex items-center gap-1.5 text-[13.5px] font-semibold">
          <Activity aria-hidden size={15} className="text-muted" />
          Steps per run
        </h2>
        <p className="text-[12px] text-muted">
          How long the window&rsquo;s settled runs took. Only finished runs count — a run still in
          flight would move this shape every time you looked at it.
        </p>
      </header>
      <div className="px-4 py-4">
        {buckets.length === 0 ? (
          <EmptyState
            title="No finished run in this window"
            hint="The shape appears once an agent run has started and settled. Nothing has finished in the range you are looking at."
          />
        ) : (
          <>
            <div data-telemetry-histogram className="flex h-32 items-end gap-1">
              {buckets.map((bucket) => (
                <div key={bucket.steps} className="flex min-w-0 flex-1 flex-col items-center gap-1">
                  <span className="text-[10px] tabular-nums text-muted">{bucket.runs}</span>
                  <div
                    title={`${bucket.runs} run${bucket.runs === 1 ? "" : "s"} took exactly ${bucket.steps} step${bucket.steps === 1 ? "" : "s"}`}
                    className="w-full rounded-t bg-accent"
                    style={{
                      // A bucket with one run still gets a visible bar: `max(2, …)` rather than
                      // a percentage of a ceiling, so a single run is a line rather than
                      // nothing at all.
                      height: `${Math.max(2, Math.round((bucket.runs / tallest) * 100))}%`,
                    }}
                  />
                  <span className="text-[10px] tabular-nums text-muted">{bucket.steps}</span>
                </div>
              ))}
            </div>
            <p className="mt-2 text-[11px] text-muted">
              {total} finished run{total === 1 ? "" : "s"}, by exact step count
            </p>
          </>
        )}
      </div>
    </section>
  );
}

/**
 * Cost per solved task, per day.
 *
 * A day with runs and no successes has **no dot on the y-axis**, and that absence is the finding:
 * the day is listed under the chart with what it spent and that it solved nothing. Painting it at
 * zero would put the worst day in the series at the cheapest point — the one inversion this chart
 * can make that actively inverts its own meaning. So the scatter draws only the days that solved
 * something, and the table beneath names the rest.
 */
function CostScatter({ days }: { days: SolvedDay[] }) {
  const priced = days.filter((day) => day.solved > 0);
  const wasted = days.filter((day) => day.solved === 0);
  const priciest = Math.max(1, ...priced.map((day) => Math.round(day.cost_micros / day.solved)));

  return (
    <section className="rounded-xl border border-line bg-surface">
      <header className="border-b border-line px-4 py-3">
        <h2 className="flex items-center gap-1.5 text-[13.5px] font-semibold">
          <Gauge aria-hidden size={15} className="text-muted" />
          Cost per solved task
        </h2>
        <p className="text-[12px] text-muted">
          What each day&rsquo;s spend worked out to per run that reached an answer. A run that ran
          out of steps is charged for but does not count as solved — so a runaway week shows up here
          rather than being averaged away.
        </p>
      </header>
      <div className="px-4 py-4">
        {days.length === 0 ? (
          <EmptyState
            title="No run in this window"
            hint="A point appears per day that had at least one finished run. Nothing has run in the range you are looking at."
          />
        ) : (
          <>
            <div data-telemetry-scatter className="flex h-32 items-end gap-1">
              {priced.map((day) => {
                const per = Math.round(day.cost_micros / day.solved);
                return (
                  <div key={day.day} className="flex min-w-0 flex-1 flex-col items-center gap-1">
                    <span className="text-[10px] tabular-nums text-muted">{micros(per)}</span>
                    <div
                      title={`${day.day}: ${day.solved} of ${day.runs} runs solved, ${micros(per)} each`}
                      className="w-full rounded-t bg-success"
                      style={{ height: `${Math.max(2, Math.round((per / priciest) * 100))}%` }}
                    />
                    <span className="text-[10px] tabular-nums text-muted">{shortDay(day.day)}</span>
                  </div>
                );
              })}
            </div>
            {wasted.length > 0 ? (
              <p
                data-telemetry-wasted
                className="mt-3 flex items-start gap-1.5 rounded-lg bg-danger-soft px-3 py-2 text-[12px] text-danger"
              >
                <TriangleAlert aria-hidden size={14} className="mt-0.5 shrink-0" />
                {wasted.length} day{wasted.length === 1 ? "" : "s"} spent{" "}
                {micros(wasted.reduce((sum, day) => sum + day.cost_micros, 0))} and solved
                nothing — {wasted.map((day) => shortDay(day.day)).join(", ")}. They are off the
                chart rather than drawn at zero.
              </p>
            ) : null}
          </>
        )}
      </div>
    </section>
  );
}

/** The costliest failing tool of each day — the table under the scatter. */
function CostliestFailingTable({ data }: { data: ToolTelemetry }) {
  return (
    <section className="rounded-xl border border-line bg-surface">
      <header className="border-b border-line px-4 py-3">
        <h2 className="text-[13.5px] font-semibold">Costliest failing tool per day</h2>
        <p className="text-[12px] text-muted">
          One row per day: the most expensive tool that <em>failed</em> that day. A healthy tool
          is never ranked here, however much it costs — the question is where money went while
          something was broken.
        </p>
      </header>
      <div className="px-4 py-4">
        {data.costliest_failing.length === 0 ? (
          <EmptyState
            title="No tool failed in this window"
            hint="Nothing broke in the range you are looking at, so there is nothing to rank. This is the good version of an empty table."
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[420px] border-collapse text-left">
              <thead>
                <tr className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
                  <th scope="col" className="px-3 py-2 font-medium">Day</th>
                  <th scope="col" className="px-3 py-2 font-medium">Tool</th>
                  <th scope="col" className="px-3 py-2 font-medium">Failures</th>
                  <th scope="col" className="px-3 py-2 text-right font-medium">Cost</th>
                </tr>
              </thead>
              <tbody>
                {data.costliest_failing.map((day) => (
                  <tr
                    key={`${day.day}-${day.tool}`}
                    data-telemetry-costliest={day.tool}
                    className="border-b border-line last:border-0"
                  >
                    <td className="px-3 py-2 text-[12px] tabular-nums text-muted">{day.day}</td>
                    <td className="px-3 py-2 font-mono text-[12px] text-ink">{day.tool}</td>
                    <td className="px-3 py-2 text-[12px] tabular-nums text-danger">{day.failures}</td>
                    <td className="px-3 py-2 text-right text-[12px] tabular-nums text-ink">
                      {micros(day.cost_micros)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </section>
  );
}

/** The screen. */
export function AiTelemetry() {
  const [data, setData] = useState<ToolTelemetry | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [days, setDays] = useState(30);
  const [failingOnly, setFailingOnly] = useState(false);

  // The window is computed once, here, and sent explicitly. Two reasons it is not left to the
  // server's default: the picker says "last 30 days", so the screen should *ask* for 30 days
  // rather than agree with the server by coincidence; and a reader that changed the server's
  // default would silently change what this picker means. The labels below still come from the
  // response — the picker is a request, never an authority.
  const filter = useMemo(() => {
    const end = new Date();
    const start = new Date(end.getTime() - (days - 1) * 86_400_000);
    return {
      from: isoDay(start),
      to: isoDay(end),
      failing: failingOnly ? true : null,
    };
  }, [days, failingOnly]);

  const load = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      setData(await fetchToolTelemetry(filter));
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "Telemetry could not be loaded.");
    } finally {
      setBusy(false);
    }
  }, [filter]);

  useEffect(() => {
    void load();
  }, [load]);

  if (error) {
    return (
      <div data-telemetry-error className="flex flex-col gap-3">
        <p className="rounded-lg bg-danger-soft px-3 py-2 text-[13px] text-danger">{error}</p>
        <button
          type="button"
          onClick={load}
          className="self-start rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
        >
          Retry
        </button>
      </div>
    );
  }

  if (!data) return <LoadingTable columns={6} rows={4} />;

  const filtered = failingOnly && data.tools.length === 0;

  return (
    <div data-ai-telemetry className="flex flex-col gap-5">
      <StatTiles data={data} />
      <RangeLabel data={data} />

      <div className="flex flex-wrap items-center gap-2">
        <label className="flex items-center gap-1.5 text-[12px] text-muted">
          <Filter aria-hidden size={14} />
          <span className="sr-only">Window</span>
          <select
            value={days}
            onChange={(event) => setDays(Number(event.target.value))}
            aria-label="Telemetry window"
            className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12px] text-ink"
          >
            {TELEMETRY_WINDOWS.map((option) => (
              <option key={option.days} value={option.days}>
                {option.label}
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-1.5 text-[12px] text-muted">
          <input
            type="checkbox"
            checked={failingOnly}
            onChange={(event) => setFailingOnly(event.target.checked)}
            aria-label="Only tools that failed"
            className="rounded border-line"
          />
          Only tools that failed
        </label>
        <button
          type="button"
          onClick={load}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
        >
          {busy ? (
            <Loader2 aria-hidden size={14} className="animate-spin" />
          ) : (
            <RefreshCw aria-hidden size={14} />
          )}
          Refresh
        </button>
      </div>

      <section className="rounded-xl border border-line bg-surface">
        <header className="border-b border-line px-4 py-3">
          <h2 className="text-[13.5px] font-semibold">Tools</h2>
          <p className="text-[12px] text-muted">
            Per tool, over the window. Denials are counted apart from failures on purpose: a denial
            is a permission saying no, a failure is a tool breaking, and the fix is different.
          </p>
        </header>
        <div className="px-4 py-4">
          {data.tools.length === 0 ? (
            <EmptyState
              title={
                filtered
                  ? "No tool failed in this window"
                  : data.days > 0
                    ? "No tool has been called in this window"
                    : "No telemetry yet"
              }
              hint={
                filtered
                  ? "The failing-only filter is on and nothing failed. Clear it to see every tool's numbers."
                  : `Nothing was called between ${data.from} and ${data.to}. This is an absence of measurements, not a measurement of health — the numbers appear as soon as an agent or copilot calls a tool.`
              }
            />
          ) : (
            <>
              <div className="hidden overflow-x-auto lg:block">
                <table className="w-full min-w-[820px] border-collapse text-left">
                  <thead>
                    <tr className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
                      <th scope="col" className="px-3 py-2 font-medium">Tool</th>
                      <th scope="col" className="px-3 py-2 font-medium">Calls</th>
                      <th scope="col" className="px-3 py-2 font-medium">Success</th>
                      <th scope="col" className="px-3 py-2 font-medium">Denied</th>
                      <th scope="col" className="px-3 py-2 font-medium">Error codes</th>
                      <th scope="col" className="px-3 py-2 text-right font-medium">Latency</th>
                    </tr>
                  </thead>
                  <tbody>
                    {data.tools.map((row) => (
                      <ToolRow key={row.tool} row={row} days={data.days} />
                    ))}
                  </tbody>
                </table>
              </div>
              <ul className="flex flex-col gap-3 lg:hidden">
                {data.tools.map((row) => (
                  <ToolCard key={row.tool} row={row} days={data.days} />
                ))}
              </ul>
            </>
          )}
        </div>
      </section>

      <div className="grid gap-5 xl:grid-cols-2">
        <StepHistogram buckets={data.step_histogram} />
        <CostScatter days={data.cost_per_solved} />
      </div>

      <CostliestFailingTable data={data} />

      <p className="text-[11px] text-muted">
        Window ends {data.to} ({today() === data.to ? "today" : "a past day"}); the roll-up is
        written by the telemetry runner, so the most recent hour may not be summarised yet.
      </p>
    </div>
  );
}