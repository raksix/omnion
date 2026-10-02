"use client";

/**
 * `/ai/evals/runs` — the run history (REQ-107, slice 2).
 *
 * The screen answers one question first: **is anything stuck?** That is why the status filter
 * leads with `incomplete` rather than a plain list, and why `queued` is coloured and worded as
 * what it is — nothing has claimed it — instead of being folded into a cheerful "in progress"
 * pill. A run list that renders `queued` and `running` alike cannot tell an operator that a
 * suite has been sitting unclaimed for six hours, which is the one thing this screen exists to
 * tell them.
 *
 * Three decisions shape it:
 *
 * 1. **The state comes from the server, and the client does not re-derive it.** Each row arrives
 *    with a `state` beside its stored `status`, because the mapping — merge `queued` and
 *    `running` for "not settled", split `queued` from `running` for "stuck or not" — is the sort
 *    of rule a second implementation gets subtly wrong, and a run list is exactly where being
 *    subtly wrong costs someone an afternoon.
 *
 * 2. **The tiles count the window they name.** The API takes `days` for the stats; the screen
 *    sends it with the filters rather than beside them, because a seven-day average next to a
 *    filtered list that shows only yesterday's failures is two different questions answered by
 *    two numbers on one screen with no labels.
 *
 * 3. **Nothing here starts a run.** A run costs real inference tokens, so `ai.evals.run` is a
 *    separate catalogue key from `ai.evals.read`; the button is on the suite screen, next to the
 *    suite, and this screen is for reading what happened. A list that also started runs would
 *    make the read permission look like a spend permission.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import {
  ArrowRight,
  Ban,
  CalendarClock,
  Filter,
  Loader2,
  PlayCircle,
  RefreshCw,
  ShieldAlert,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError } from "@/lib/api";
import {
  fetchEvalRuns,
  RUN_GATE_FILTERS,
  RUN_KIND_FILTERS,
  RUN_STATUS_FILTERS,
  type EvalRunFilter,
  type EvalRunList,
  type EvalRunSummary,
} from "@/lib/eval-run-api";

/**
 * How a run's state reads.
 *
 * Four that are not colour-only, because each answers a different question and only two of them
 * are bad news: `queued` is a wait, `running` is work, `passed`/`failed` are verdicts and
 * `cancelled` is a decision somebody made. An `error` is neither a verdict nor a decision — it
 * is the harness failing to answer, and it is the one a human must look at.
 */
function stateTone(state: string): { label: string; className: string } {
  switch (state) {
    case "passed":
      return { label: "Passed", className: "bg-success-soft text-success" };
    case "failed":
      return { label: "Failed", className: "bg-danger-soft text-danger" };
    case "running":
      return { label: "Running", className: "bg-accent-soft text-accent" };
    case "queued":
      // Deliberately not styled like progress. A queued run has no runner on it.
      return { label: "Queued", className: "bg-canvas text-muted" };
    case "cancelled":
      return { label: "Cancelled", className: "bg-canvas text-muted" };
    case "error":
      return { label: "Errored", className: "bg-danger-soft text-danger" };
    default:
      return { label: state, className: "bg-canvas text-muted" };
  }
}

/** A pass rate, or the reason there is not one yet. */
function rateText(rate: number | null): string {
  return rate === null ? "—" : `${rate.toFixed(1)}%`;
}

/** The stat tiles. Each says which window it counts, because the API names one per request. */
function StatTiles({ data, days }: { data: EvalRunList; days: number }) {
  const tiles = [
    { label: `Suites with runs`, value: data.stats.suites, hint: "in the window" },
    {
      label: `Runs`,
      value: data.stats.runs,
      hint: days === 7 ? "last 7 days" : `last ${days} days`,
    },
    {
      label: "Average pass rate",
      value: rateText(data.stats.average_pass_rate),
      hint: "weighted, settled runs",
    },
    {
      label: "Judge + model cost",
      value: `${(data.stats.cost_micros / 1_000_000).toFixed(2)}`,
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

/** One row of the history. */
function RunRow({ row }: { row: EvalRunSummary }) {
  const tone = stateTone(row.state);
  const run = row.run;
  const regressed = run.gate === "block";
  return (
    <tr data-eval-run={run.id} className="border-b border-line last:border-0">
      <td className="px-3 py-2.5 align-top">
        <Link
          href={`/ai/evals/runs/${run.id}`}
          className="inline-flex items-center gap-1 text-[13px] font-medium text-ink hover:underline"
        >
          {run.suite_name}
          <ArrowRight aria-hidden size={13} className="text-muted" />
        </Link>
        <p className="mt-0.5 text-[11px] text-muted">
          <span className="font-mono">{run.suite_key}</span> · {run.kind}
          {run.started_at ? ` · ${new Date(run.started_at).toLocaleString()}` : ""}
        </p>
        <p className="mt-1 text-[12px] text-muted">{row.state_note}</p>
        {run.error ? (
          <p className="mt-1 flex items-start gap-1 text-[12px] text-danger">
            <ShieldAlert aria-hidden size={13} className="mt-0.5 shrink-0" />
            {run.error}
          </p>
        ) : null}
      </td>
      <td className="px-3 py-2.5 align-top">
        <span className={`rounded-full px-2 py-0.5 text-[11px] ${tone.className}`}>{tone.label}</span>
        {run.blocking ? (
          <span className="ml-1.5 inline-flex items-center gap-1 text-[11px] text-muted">
            <PlayCircle aria-hidden size={11} />
            gate
          </span>
        ) : null}
      </td>
      <td className="px-3 py-2.5 align-top text-[13px] text-ink">
        {rateText(run.pass_rate)}
        <p className="mt-0.5 text-[11px] text-muted">
          {run.passed_cases}/{run.total_cases} passed
          {run.error_cases > 0 ? ` · ${run.error_cases} errored` : ""}
        </p>
      </td>
      <td className="px-3 py-2.5 align-top text-[12px] text-muted">
        {regressed ? (
          <span className="inline-flex items-center gap-1 text-danger">
            <Ban aria-hidden size={12} />
            blocked
          </span>
        ) : run.gate === "pass" ? (
          "gate passed"
        ) : (
          "no gate"
        )}
        <p className="mt-0.5">threshold {run.threshold_percent}%</p>
      </td>
      <td className="px-3 py-2.5 align-top text-[12px] text-muted">
        {(run.cost_micros / 1_000_000).toFixed(3)}
        <p className="mt-0.5">
          {run.duration_ms === null ? "—" : `${(run.duration_ms / 1000).toFixed(1)}s`}
        </p>
      </td>
    </tr>
  );
}

/** The screen. */
export function EvalRunsScreen() {
  const [data, setData] = useState<EvalRunList | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState("");
  const [kind, setKind] = useState("");
  const [gate, setGate] = useState("");
  const [suite, setSuite] = useState("");
  const [days, setDays] = useState(7);

  const filter = useMemo<EvalRunFilter>(
    () => ({
      status: status || null,
      kind: kind || null,
      gate: gate || null,
      suite: suite || null,
      days,
    }),
    [status, kind, gate, suite, days]
  );

  const load = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      setData(await fetchEvalRuns(filter));
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The runs could not be loaded.");
    } finally {
      setBusy(false);
    }
  }, [filter]);

  useEffect(() => {
    void load();
  }, [load]);

  if (error) {
    return (
      <div data-eval-runs-error className="flex flex-col gap-3">
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

  if (!data) return <LoadingTable columns={5} rows={4} />;

  return (
    <div data-eval-runs className="flex flex-col gap-5">
      <StatTiles data={data} days={days} />

      <div className="flex flex-wrap items-center gap-2">
        <label className="flex items-center gap-1.5 text-[12px] text-muted">
          <Filter aria-hidden size={14} />
          <span className="sr-only">Status</span>
          <select
            value={status}
            onChange={(event) => setStatus(event.target.value)}
            aria-label="Filter by status"
            className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12px] text-ink"
          >
            {RUN_STATUS_FILTERS.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-1.5 text-[12px] text-muted">
          <span className="sr-only">Kind</span>
          <select
            value={kind}
            onChange={(event) => setKind(event.target.value)}
            aria-label="Filter by kind"
            className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12px] text-ink"
          >
            {RUN_KIND_FILTERS.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-1.5 text-[12px] text-muted">
          <span className="sr-only">Gate</span>
          <select
            value={gate}
            onChange={(event) => setGate(event.target.value)}
            aria-label="Filter by gate"
            className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12px] text-ink"
          >
            {RUN_GATE_FILTERS.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-1.5 text-[12px] text-muted">
          <CalendarClock aria-hidden size={14} />
          <span className="sr-only">Window</span>
          <select
            value={days}
            onChange={(event) => setDays(Number(event.target.value))}
            aria-label="Statistics window"
            className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12px] text-ink"
          >
            <option value={1}>Last 24 hours</option>
            <option value={7}>Last 7 days</option>
            <option value={30}>Last 30 days</option>
          </select>
        </label>
        <label className="flex min-w-[180px] flex-1 items-center gap-2 rounded-lg border border-line bg-canvas px-3 py-1.5">
          <span className="sr-only">Suite key</span>
          <input
            value={suite}
            onChange={(event) => setSuite(event.target.value)}
            placeholder="One suite key"
            aria-label="Filter by suite key"
            className="w-full bg-transparent text-[13px] text-ink outline-none"
          />
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

      {data.is_empty ? (
        <EmptyState
          title={status || kind || gate || suite ? "No run matches these filters" : "No run yet"}
          hint={
            status || kind || gate || suite
              ? "Clear a filter, or widen the window. A filter that hides every run is a filter that needs loosening, not a suite that needs running."
              : "Runs appear here once a suite is started — from its own screen, or on the schedule you gave it. Nothing has been started yet."
          }
        />
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full min-w-[760px] border-collapse text-left">
            <thead>
              <tr className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
                <th scope="col" className="px-3 py-2 font-medium">
                  Suite
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  State
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  Pass rate
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  Gate
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  Cost / time
                </th>
              </tr>
            </thead>
            <tbody>
              {data.runs.map((row) => (
                <RunRow key={row.run.id} row={row} />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
