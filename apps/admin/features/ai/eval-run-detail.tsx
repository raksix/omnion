"use client";

/**
 * `/ai/evals/runs/[id]` — one run (REQ-107, slice 2).
 *
 * The screen has to answer three questions at once, and the layout is chosen by which of them
 * the operator opened it for:
 *
 * 1. **What did this run measure?** The header names the suite, the model under test, the judge
 *    and the threshold — the snapshot's *summary*, never the blob. `snapshot` is the
 *    reproduction data (model config, property set, judge prompt version) and it exists so a
 *    run can be explained months later; rendering it raw would be a wall of jsonb that nobody
 *    can act on.
 * 2. **What failed, and why?** One row per case with its checks, and — for a rubric case — the
 *    judge's own reasoning. A rubric case scored without its reason is a number nobody can
 *    argue with, which is the same as a number nobody trusts.
 * 3. **Did it get worse?** The diff against an explicitly chosen baseline, with the regressed
 *    cases named.
 *
 * Two decisions that are easy to get backwards:
 *
 * - **The baseline is always chosen, never implicit.** There is no "compare with the previous
 *   run" button. A diff whose baseline is inferred means something different on every call
 *   depending on what else has run since, and a regression report nobody can reproduce is a
 *   regression report nobody acts on. The server refuses a diff with no `base`, so the screen
 *   ships a picker and no shortcut.
 * - **Cancel does not pretend.** A run that has already settled answers `409`; the sheet stays
 *   open and says so, because a "cancelled" toast on a run that finished two minutes ago reads
 *   as success and the operator then waits for a stop that already happened.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import {
  ArrowDownRight,
  ArrowUpRight,
  Ban,
  CircleSlash,
  GitCompareArrows,
  Loader2,
  RefreshCw,
  Target,
  TriangleAlert,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError } from "@/lib/api";
import {
  cancelEvalRun,
  fetchEvalRun,
  fetchEvalRunDiff,
  setEvalBaseline,
  type EvalCaseResult,
  type EvalDiffRow,
  type EvalRunDetail,
  type EvalRunDiff,
} from "@/lib/eval-run-api";

/** How a result reads. A rubric case that failed is worse than one that errored. */
function resultTone(status: string): string {
  if (status === "passed") return "bg-success-soft text-success";
  if (status === "failed") return "bg-danger-soft text-danger";
  if (status === "error") return "bg-danger-soft text-danger";
  return "bg-canvas text-muted";
}

/** How a diff movement reads. `added`/`removed` are the pair that hides regressions by omission. */
function movementTone(movement: string): string {
  if (movement === "improved") return "text-success";
  if (movement === "regressed") return "text-danger";
  if (movement === "added") return "text-accent";
  if (movement === "removed") return "text-muted";
  return "text-muted";
}

/** The case's checks, as the evidence behind its score. */
function ChecksSummary({ checks }: { checks: unknown }) {
  if (!checks || typeof checks !== "object") return null;
  const entries = Object.entries(checks as Record<string, unknown>).filter(
    ([, value]) => value !== null && value !== undefined
  );
  if (entries.length === 0) return null;
  return (
    <ul className="mt-1 flex flex-wrap gap-1.5">
      {entries.map(([name, value]) => {
        const ok = value === true || value === "pass" || value === "passed";
        const bad = value === false || value === "fail" || value === "failed";
        return (
          <li
            key={name}
            className={`rounded px-1.5 py-0.5 text-[11px] ${
              ok ? "bg-success-soft text-success" : bad ? "bg-danger-soft text-danger" : "bg-canvas text-muted"
            }`}
          >
            {name}
            {typeof value === "string" && value !== "pass" && value !== "passed" ? `: ${value}` : ""}
          </li>
        );
      })}
    </ul>
  );
}

/** One case result. */
function ResultRow({ row }: { row: EvalCaseResult }) {
  return (
    <tr data-eval-result={row.case_name} className="border-b border-line last:border-0">
      <td className="px-3 py-2.5 align-top">
        <p className="text-[13px] font-medium text-ink">{row.case_name}</p>
        <ChecksSummary checks={row.checks} />
        {/*
          The judge's reasoning is shown, not summarised. A rubric verdict without its reason is
          a number nobody can argue with, which is the same as a number nobody trusts — and the
          request's own risk note says a single rubric failure must never be the only evidence
          for a regression claim.
        */}
        {row.judge_reason ? (
          <p className="mt-1 border-l-2 border-line pl-2 text-[12px] text-muted">
            {row.judge_reason}
          </p>
        ) : null}
      </td>
      <td className="px-3 py-2.5 align-top">
        <span className={`rounded-full px-2 py-0.5 text-[11px] ${resultTone(row.status)}`}>
          {row.status}
        </span>
      </td>
      <td className="px-3 py-2.5 align-top text-[13px] text-ink">
        {row.score === null ? "—" : row.score.toFixed(2)}
      </td>
      <td className="px-3 py-2.5 align-top text-[12px] text-muted">
        {row.latency_ms === null ? "—" : `${row.latency_ms}ms`}
        <p className="mt-0.5">{(row.cost_micros / 1_000_000).toFixed(4)}</p>
      </td>
    </tr>
  );
}

/** One case in the diff, both runs side by side. */
function DiffRow({ row }: { row: EvalDiffRow }) {
  return (
    <tr data-eval-diff-row={row.case_name} className="border-b border-line last:border-0">
      <td className="px-3 py-2 align-top text-[13px] text-ink">{row.case_name}</td>
      <td className="px-3 py-2 align-top text-[12px] text-muted">
        {row.base_status ?? "—"}
        {row.base_score === null ? "" : ` · ${row.base_score.toFixed(2)}`}
      </td>
      <td className="px-3 py-2 align-top text-[12px] text-ink">
        {row.head_status}
        {row.head_score === null ? "" : ` · ${row.head_score.toFixed(2)}`}
      </td>
      <td className={`px-3 py-2 align-top text-[12px] ${movementTone(row.movement)}`}>
        <span className="inline-flex items-center gap-1">
          {row.movement === "improved" ? (
            <ArrowUpRight aria-hidden size={12} />
          ) : row.movement === "regressed" ? (
            <ArrowDownRight aria-hidden size={12} />
          ) : row.movement === "added" || row.movement === "removed" ? (
            <CircleSlash aria-hidden size={12} />
          ) : (
            <span className="inline-block w-3" />
          )}
          {row.movement}
        </span>
      </td>
    </tr>
  );
}

/** The screen. */
export function EvalRunDetailView({ runId }: { runId: string }) {
  const [data, setData] = useState<EvalRunDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [base, setBase] = useState("");
  const [diff, setDiff] = useState<EvalRunDiff | null>(null);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [diffBusy, setDiffBusy] = useState(false);

  const load = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const next = await fetchEvalRun(runId);
      setData(next);
      // Pre-select the baseline the suite already carries, so the common case is one click.
      if (!base && next.baseline) setBase(next.baseline.run_id);
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The run could not be loaded.");
    } finally {
      setBusy(false);
    }
  }, [runId, base]);

  useEffect(() => {
    void load();
  }, [load]);

  const runDiff = useCallback(async () => {
    if (!base) return;
    setDiffBusy(true);
    setDiffError(null);
    setDiff(null);
    try {
      setDiff(await fetchEvalRunDiff(runId, base));
    } catch (cause: unknown) {
      setDiffError(cause instanceof ApiError ? cause.message : "The comparison failed.");
    } finally {
      setDiffBusy(false);
    }
  }, [runId, base]);

  const cancel = useCallback(async () => {
    setActionError(null);
    setMessage(null);
    try {
      await cancelEvalRun(runId);
      setMessage("The run was cancelled. Whatever it had already scored is kept below.");
      await load();
    } catch (cause: unknown) {
      setActionError(
        cause instanceof ApiError ? cause.message : "The run could not be cancelled."
      );
    }
  }, [runId, load]);

  const makeBaseline = useCallback(async () => {
    setActionError(null);
    setMessage(null);
    try {
      await setEvalBaseline(data!.run.suite_key, runId);
      setMessage("This run is now the suite's baseline. Later diffs measure against it.");
      await load();
    } catch (cause: unknown) {
      setActionError(
        cause instanceof ApiError ? cause.message : "The baseline could not be set."
      );
    }
  }, [data, runId, load]);

  const live = data?.state === "queued" || data?.state === "running";

  const gateLine = useMemo(() => {
    if (!data) return null;
    if (data.run.gate === "block") {
      return `Blocked — this run did not clear the ${data.run.threshold_percent}% threshold.`;
    }
    if (data.run.gate === "pass") return "The gate passed.";
    return null;
  }, [data]);

  if (error) {
    return (
      <div data-eval-run-error className="flex flex-col gap-3">
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

  if (!data) return <LoadingTable columns={4} rows={5} />;

  const run = data.run;

  return (
    <div data-eval-run-detail className="flex flex-col gap-5">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <Link
            href={`/ai/evals/${encodeURIComponent(run.suite_key)}`}
            className="text-[12px] text-muted hover:underline"
          >
            {run.suite_name}
          </Link>
          <h2 className="mt-0.5 text-lg font-semibold text-ink">
            Run of {new Date(run.started_at).toLocaleString()}
          </h2>
          <p className="mt-0.5 text-[12px] text-muted">
            <span className="font-mono">{run.suite_key}</span> · {run.kind} ·{" "}
            {run.pass_rate === null ? "not settled" : `${run.pass_rate.toFixed(1)}% pass rate`}
            {" · "}
            {run.passed_cases}/{run.total_cases} passed
            {run.error_cases > 0 ? ` · ${run.error_cases} errored` : ""}
            {run.duration_ms === null ? "" : ` · ${(run.duration_ms / 1000).toFixed(1)}s`}
          </p>
          {/*
            The state note is the sentence an operator acts on; the badge next to it is the colour.
            A `queued` run's note says nothing has claimed it, which is the fact a stuck suite
            turns on and the reason this screen never renders the two as one pill.
          */}
          <p className="mt-1 text-[12px] text-muted">{data.state_note}</p>
          {gateLine ? (
            <p className="mt-1 flex items-center gap-1.5 text-[12px] text-danger">
              <Target aria-hidden size={13} />
              {gateLine}
            </p>
          ) : null}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Link
            href="/ai/evals/runs"
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
          >
            All runs
          </Link>
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
          {/*
            Cancel is offered only while the run can still be stopped. On a settled run the
            store answers 409, and a button that could only ever fail is a button that teaches
            operators to ignore the panel.
          */}
          {live ? (
            <button
              type="button"
              onClick={cancel}
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
            >
              <Ban aria-hidden size={14} />
              Cancel run
            </button>
          ) : null}
          {!live && run.pass_rate !== null ? (
            <button
              type="button"
              onClick={makeBaseline}
              title="Later diffs measure against this run until another is set"
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
            >
              <Target aria-hidden size={14} />
              Make baseline
            </button>
          ) : null}
        </div>
      </div>

      {actionError ? (
        <p className="rounded-lg bg-danger-soft px-3 py-2 text-[13px] text-danger">{actionError}</p>
      ) : null}
      {message ? (
        <p className="rounded-lg bg-success-soft px-3 py-2 text-[13px] text-success">{message}</p>
      ) : null}

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Model under test</p>
          <p className="mt-1 truncate text-[13px] font-medium text-ink">
            {run.model_id ?? "not recorded"}
          </p>
        </div>
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Judge model</p>
          <p className="mt-1 truncate text-[13px] font-medium text-ink">
            {run.judge_model_id ?? "no rubric case in this suite"}
          </p>
        </div>
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Threshold</p>
          <p className="mt-1 text-[13px] font-medium text-ink">{run.threshold_percent}%</p>
        </div>
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Regression tolerance</p>
          <p className="mt-1 text-[13px] font-medium text-ink">
            {run.max_regression_points} points
          </p>
          <p className="mt-0.5 text-[11px] text-muted">as of this run — not the suite's current</p>
        </div>
      </div>

      {run.error ? (
        <p className="flex items-start gap-2 rounded-lg bg-danger-soft px-3 py-2 text-[13px] text-danger">
          <TriangleAlert aria-hidden size={14} className="mt-0.5 shrink-0" />
          {run.error}
        </p>
      ) : null}

      {/*
        The comparison block. The picker is mandatory and its emptiness is a first-class state:
        a suite with one run has nothing to compare against, and saying "no baseline yet" is
        more use than a disabled Compare button.
      */}
      <div className="rounded-xl border border-line bg-surface p-4">
        <h3 className="flex items-center gap-1.5 text-[13px] font-semibold text-ink">
          <GitCompareArrows aria-hidden size={14} />
          Compare with a baseline
        </h3>
        <p className="mt-1 text-[12px] text-muted">
          {data.baseline
            ? `The suite's baseline is the run from ${new Date(data.baseline.set_at).toLocaleString()} at ${data.baseline.pass_rate.toFixed(1)}%.`
            : "This suite has no baseline yet. Set a settled run as the yardstick, then later runs can be measured against it."}
        </p>
        <div className="mt-3 flex flex-wrap items-center gap-2">
          <label className="flex min-w-[240px] flex-1 items-center gap-2">
            <span className="sr-only">Baseline run</span>
            <select
              value={base}
              onChange={(event) => setBase(event.target.value)}
              data-eval-baseline-picker
              aria-label="Baseline run"
              className="w-full rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12px] text-ink"
            >
              <option value="">Choose a baseline run</option>
              {data.diff_candidates.map((option) => (
                <option key={option.id} value={option.id}>
                  {option.label}
                </option>
              ))}
            </select>
          </label>
          <button
            type="button"
            onClick={runDiff}
            disabled={!base || diffBusy}
            data-eval-compare
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-60"
          >
            {diffBusy ? (
              <Loader2 aria-hidden size={14} className="animate-spin" />
            ) : (
              <GitCompareArrows aria-hidden size={14} />
            )}
            Compare
          </button>
        </div>

        {diffError ? (
          <p className="mt-3 rounded-lg bg-danger-soft px-3 py-2 text-[13px] text-danger">
            {diffError}
          </p>
        ) : null}

        {diff ? (
          <div className="mt-4 flex flex-col gap-3" data-eval-diff>
            <p className="text-[12px] text-ink">{diff.summary}</p>
            <p
              className={`text-[12px] ${
                diff.gate.gate === "block" ? "text-danger" : "text-muted"
              }`}
            >
              {diff.gate.held_threshold ? "The threshold held." : "The threshold was not met."}
              {diff.gate.drop_points === null
                ? ""
                : ` ${diff.gate.drop_points.toFixed(2)} points below the baseline.`}
              {diff.gate.regressed ? " That drop is a regression." : ""}
            </p>
            <div className="overflow-x-auto">
              <table className="w-full min-w-[560px] border-collapse text-left">
                <thead>
                  <tr className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
                    <th scope="col" className="px-3 py-2 font-medium">
                      Case
                    </th>
                    <th scope="col" className="px-3 py-2 font-medium">
                      Baseline
                    </th>
                    <th scope="col" className="px-3 py-2 font-medium">
                      This run
                    </th>
                    <th scope="col" className="px-3 py-2 font-medium">
                      Movement
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {diff.diff.rows.map((row) => (
                    <DiffRow key={`${row.case_name}-${row.movement}`} row={row} />
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        ) : null}
      </div>

      {data.is_empty ? (
        <EmptyState
          title={data.state === "queued" ? "The run has not started" : "This run scored nothing"}
          hint={
            data.state === "queued"
              ? "A queued run is waiting for a runner to claim it. If it stays queued, no runner is running — that is the state to investigate, not the run."
              : "The run settled before its first case produced a result. The reason is above, if there is one."
          }
        />
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full min-w-[640px] border-collapse text-left">
            <thead>
              <tr className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
                <th scope="col" className="px-3 py-2 font-medium">
                  Case
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  Result
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  Score
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  Latency / cost
                </th>
              </tr>
            </thead>
            <tbody>
              {data.results.map((row) => (
                <ResultRow key={`${row.id}-${row.case_name}`} row={row} />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
