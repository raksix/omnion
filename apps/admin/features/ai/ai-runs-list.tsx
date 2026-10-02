"use client";

/**
 * The run history (docs/requests/REQ-099, slice 1).
 *
 * A run is a record of something that already happened, so this list is not a task list — it is
 * a log somebody reads after a bill arrived. Four rules follow from that, and each one closes a
 * way a run history misleads.
 *
 * 1. **A goal is shown at 60 characters, not truncated to nothing.** The full goal is the
 *    tooltip, because the first 60 characters are the *request* and the tail is usually the
 *    interesting half ("…find the three invoices in `/finance/2026` that do not reconcile").
 * 2. **A parked run is not a failed run.** `awaiting_approval` gets its own badge: a run
 *    waiting for a person is the one an operator has to act on, and colouring it as a failure
 *    either hides it or makes people cancel it.
 * 3. **The stop reason is the column, not a tooltip.** `loop_detected` and `max_steps` are the
 *    two that cost money while producing nothing, and an operator scanning for them needs them
 *    visible without a hover.
 * 4. **The filters live in the query string.** Same rule as every other list in the panel: a
 *    filtered log that cannot be pasted to a colleague is one that has to be rebuilt by hand.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { Loader2, RotateCw, Search } from "lucide-react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError, type AiAgent, type AiRun, fetchAiAgents, fetchAiRuns } from "@/lib/api";
import { useSession } from "@/lib/session";
import { cost, reasonLabel, tokens } from "./metrics";

/** The statuses the loop writes, in the order an operator cares about. */
const STATUSES = [
  ["", "Any status"],
  ["running", "Running"],
  ["awaiting_approval", "Awaiting approval"],
  ["queued", "Queued"],
  ["completed", "Completed"],
  ["failed", "Failed"],
  ["cancelled", "Cancelled"],
] as const;

/** The stop reasons the loop writes, from the REQ's own vocabulary. */
const STOP_REASONS = [
  ["", "Any reason"],
  ["final_answer", "Final answer"],
  ["max_steps", "Max steps"],
  ["deadline", "Deadline"],
  ["token_budget", "Token budget"],
  ["cancelled", "Cancelled"],
  ["loop_detected", "Loop detected"],
  ["error", "Error"],
] as const;

/** How a status reads, and — more importantly — how it is *coloured*. */
const STATUS_TONE: Record<string, string> = {
  running: "bg-accent-soft text-accent-strong",
  queued: "bg-quiet-soft text-muted",
  // Amber, not red: a run waiting for a person is the one the operator has to act on, and
  // colouring it as a failure either hides it or makes people cancel it.
  awaiting_approval: "bg-caution-soft text-caution",
  completed: "bg-positive-soft text-positive",
  failed: "bg-danger/10 text-danger",
  cancelled: "bg-quiet-soft text-muted",
};

/** A duration between two timestamps, as the run list shows it. */
function duration(started: string | null, finished: string | null): string {
  if (!started) return "—";
  const from = new Date(started).getTime();
  const to = finished ? new Date(finished).getTime() : Date.now();
  if (Number.isNaN(from) || Number.isNaN(to)) return "—";
  const seconds = Math.max(0, Math.round((to - from) / 1000));
  if (seconds < 60) return `${seconds}s`;
  return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}

type Filters = { q: string; agent: string; status: string; reason: string };

function filtersFrom(params: URLSearchParams): Filters {
  return {
    q: params.get("q") ?? "",
    agent: params.get("agent") ?? "",
    status: params.get("status") ?? "",
    reason: params.get("reason") ?? "",
  };
}

export function AiRunsList() {
  const router = useRouter();
  const params = useSearchParams();
  const { user } = useSession();
  const filters = useMemo(() => filtersFrom(new URLSearchParams(params?.toString() ?? "")), [params]);

  const [rows, setRows] = useState<AiRun[]>([]);
  const [agents, setAgents] = useState<AiAgent[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  const organizationId = user?.organization_id ?? undefined;

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      // The agents list is only here to name a run whose agent row was deleted: without it a
      // `null` agent id renders as a blank cell and the row looks corrupt.
      const [runRows, agentRows] = await Promise.all([
        fetchAiRuns({ organizationId, limit: 200 }),
        fetchAiAgents(organizationId),
      ]);
      setRows(runRows);
      setAgents(agentRows);
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The run history could not be loaded.");
    } finally {
      setLoading(false);
    }
  }, [organizationId]);

  useEffect(() => {
    void load();
  }, [load]);

  const setFilter = (key: keyof Filters, value: string) => {
    const next = new URLSearchParams(params?.toString() ?? "");
    if (value) next.set(key, value);
    else next.delete(key);
    const query = next.toString();
    router.replace(query ? `/ai/runs?${query}` : "/ai/runs");
  };

  const agentName = useMemo(() => {
    const map = new Map<string, string>();
    for (const agent of agents) map.set(agent.id, agent.name);
    return map;
  }, [agents]);

  const visible = useMemo(() => {
    const needle = filters.q.trim().toLowerCase();
    return rows.filter((run) => {
      if (needle && !run.goal.toLowerCase().includes(needle)) return false;
      if (filters.agent && run.agent_id !== filters.agent) return false;
      if (filters.status && run.status !== filters.status) return false;
      if (filters.reason && run.stop_reason !== filters.reason) return false;
      return true;
    });
  }, [rows, filters]);

  const totals = useMemo(() => {
    let costMicros = 0;
    let prompt = 0;
    let completion = 0;
    for (const run of visible) {
      costMicros += run.cost_micros;
      prompt += run.prompt_tokens;
      completion += run.completion_tokens;
    }
    return { costMicros, tokens: prompt + completion };
  }, [visible]);

  const act = async (run: AiRun, action: "cancel" | "resume") => {
    setBusyId(run.id);
    setError(null);
    try {
      const response = await fetch(`/api/v1/ai/runs/${encodeURIComponent(run.id)}/${action}`, {
        method: "POST",
        credentials: "same-origin",
        headers: { accept: "application/json" },
      });
      if (!response.ok) {
        const body = (await response.json().catch(() => null)) as {
          error?: { message?: string };
        } | null;
        throw new ApiError(response.status, "run.action_failed", body?.error?.message ?? "The run could not be changed.");
      }
      await load();
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The run could not be changed.");
    } finally {
      setBusyId(null);
    }
  };

  return (
    <div data-ai-runs className="space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <label className="relative flex min-w-48 flex-1 items-center">
          <span className="sr-only">Search goals</span>
          <Search className="pointer-events-none absolute left-2.5 size-3.5 text-muted" aria-hidden />
          <input
            ref={searchRef}
            data-runs-search
            value={filters.q}
            onChange={(event) => setFilter("q", event.target.value)}
            placeholder="Search goals"
            className="w-full rounded-lg border border-line bg-surface py-1.5 pl-8 pr-2.5 text-[13px] outline-none focus:border-accent"
          />
        </label>

        <select
          data-runs-filter-agent
          value={filters.agent}
          onChange={(event) => setFilter("agent", event.target.value)}
          aria-label="Filter by agent"
          className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] outline-none focus:border-accent"
        >
          <option value="">Any agent</option>
          {agents.map((agent) => (
            <option key={agent.id} value={agent.id}>
              {agent.name}
            </option>
          ))}
        </select>

        <select
          data-runs-filter-status
          value={filters.status}
          onChange={(event) => setFilter("status", event.target.value)}
          aria-label="Filter by status"
          className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] outline-none focus:border-accent"
        >
          {STATUSES.map(([value, label]) => (
            <option key={value} value={value}>
              {label}
            </option>
          ))}
        </select>

        <select
          data-runs-filter-reason
          value={filters.reason}
          onChange={(event) => setFilter("reason", event.target.value)}
          aria-label="Filter by stop reason"
          className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] outline-none focus:border-accent"
        >
          {STOP_REASONS.map(([value, label]) => (
            <option key={value} value={value}>
              {label}
            </option>
          ))}
        </select>

        <button
          type="button"
          data-runs-refresh
          onClick={() => void load()}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2 py-1.5 text-[12.5px] text-muted hover:text-ink"
        >
          <RotateCw className="size-3.5" aria-hidden />
          Refresh
        </button>
      </div>

      {rows.length > 0 ? (
        <p data-runs-totals className="text-[12.5px] text-muted">
          {visible.length} of {rows.length} runs · {tokens(totals.tokens)} tokens · {cost(totals.costMicros)}
        </p>
      ) : null}

      {error ? (
        <div
          data-runs-error
          role="alert"
          className="flex items-start justify-between gap-3 rounded-xl border border-danger/40 bg-danger/5 px-3.5 py-3"
        >
          <p className="text-[12.5px] text-danger">{error}</p>
          <button type="button" onClick={() => void load()} className="shrink-0 text-[12.5px] underline">
            Retry
          </button>
        </div>
      ) : null}

      {loading ? (
        <LoadingTable columns={7} />
      ) : visible.length === 0 ? (
        rows.length === 0 ? (
          <EmptyState
            title="No run yet"
            hint="A run is one agent working towards one goal. Start one from the agents table and its steps land here with a stop reason."
            action={
              <Link
                href="/ai/agents"
                className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
              >
                Go to agents
              </Link>
            }
          />
        ) : (
          <EmptyState
            title="No run matches these filters"
            hint="Clear the search or the filters to see the whole history."
            action={
              <button
                type="button"
                onClick={() => router.replace("/ai/runs")}
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px]"
              >
                Clear filters
              </button>
            }
          />
        )
      ) : (
        <>
          <div className="hidden overflow-x-auto lg:block">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                  <th className="px-2 py-2" scope="col">Started</th>
                  <th className="px-2 py-2" scope="col">Agent</th>
                  <th className="px-2 py-2" scope="col">Goal</th>
                  <th className="px-2 py-2" scope="col">Steps</th>
                  <th className="px-2 py-2" scope="col">Tokens</th>
                  <th className="px-2 py-2" scope="col">Cost</th>
                  <th className="px-2 py-2" scope="col">Duration</th>
                  <th className="px-2 py-2" scope="col">Stop reason</th>
                  <th className="px-2 py-2" scope="col">Status</th>
                </tr>
              </thead>
              <tbody>
                {visible.map((run) => (
                  <tr key={run.id} data-run-row={run.id} className="border-t border-line">
                    <td className="whitespace-nowrap px-2 py-2 text-[12.5px] text-muted">
                      {run.started_at ? new Date(run.started_at).toLocaleString() : "—"}
                    </td>
                    <td className="px-2 py-2">
                      {run.agent_id ? (
                        <Link
                          href={`/ai/agents/${run.agent_id}`}
                          className="text-[12.5px] underline-offset-2 hover:underline"
                        >
                          {agentName.get(run.agent_id) ?? "Deleted agent"}
                        </Link>
                      ) : (
                        <span className="text-[12.5px] text-muted">
                          {run.trigger === "chat" ? "Chat" : "Deleted agent"}
                        </span>
                      )}
                    </td>
                    <td className="px-2 py-2">
                      <Link
                        href={`/ai/runs/${run.id}`}
                        data-run-goal={run.goal}
                        title={run.goal}
                        className="block max-w-sm truncate text-[12.5px] underline-offset-2 hover:underline"
                      >
                        {run.goal.length > 60 ? `${run.goal.slice(0, 60)}…` : run.goal}
                      </Link>
                    </td>
                    <td className="px-2 py-2 text-[12.5px]">{run.current_step}</td>
                    <td className="px-2 py-2 whitespace-nowrap text-[12.5px]">
                      {tokens(run.prompt_tokens + run.completion_tokens)}
                    </td>
                    <td className="whitespace-nowrap px-2 py-2 text-[12.5px]">{cost(run.cost_micros)}</td>
                    <td className="whitespace-nowrap px-2 py-2 text-[12.5px] text-muted">
                      {duration(run.started_at, run.finished_at)}
                    </td>
                    <td className="whitespace-nowrap px-2 py-2 text-[12.5px]">
                      {run.stop_reason ? (reasonLabel(run.stop_reason)) : "—"}
                    </td>
                    <td className="px-2 py-2">
                      <span
                        data-run-status={run.status}
                        className={`inline-flex items-center whitespace-nowrap rounded-full px-2 py-0.5 text-[11px] font-medium ${
                          STATUS_TONE[run.status] ?? "bg-quiet-soft text-muted"
                        }`}
                      >
                        {run.status.replace(/_/g, " ")}
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <ul className="space-y-2 lg:hidden">
            {visible.map((run) => (
              <li key={run.id} data-run-card={run.id} className="space-y-1.5 rounded-xl border border-line bg-surface p-3">
                <div className="flex items-start justify-between gap-2">
                  <Link href={`/ai/runs/${run.id}`} className="min-w-0 flex-1 text-[13px]">
                    {run.goal.length > 60 ? `${run.goal.slice(0, 60)}…` : run.goal}
                  </Link>
                  <span
                    data-run-status={run.status}
                    className={`shrink-0 rounded-full px-2 py-0.5 text-[11px] font-medium ${
                      STATUS_TONE[run.status] ?? "bg-quiet-soft text-muted"
                    }`}
                  >
                    {run.status.replace(/_/g, " ")}
                  </span>
                </div>
                <dl className="grid grid-cols-[5.5rem_1fr] gap-x-3 gap-y-1 text-[12.5px]">
                  <dt className="text-muted">Agent</dt>
                  <dd>{run.agent_id ? (agentName.get(run.agent_id) ?? "Deleted agent") : run.trigger}</dd>
                  <dt className="text-muted">Steps</dt>
                  <dd>{run.current_step}</dd>
                  <dt className="text-muted">Tokens</dt>
                  <dd>{tokens(run.prompt_tokens + run.completion_tokens)}</dd>
                  <dt className="text-muted">Cost</dt>
                  <dd>{cost(run.cost_micros)}</dd>
                  <dt className="text-muted">Duration</dt>
                  <dd>{duration(run.started_at, run.finished_at)}</dd>
                  <dt className="text-muted">Reason</dt>
                  <dd>{run.stop_reason ? (reasonLabel(run.stop_reason)) : "—"}</dd>
                </dl>
                {run.status === "running" || run.status === "queued" ? (
                  <button
                    type="button"
                    data-run-cancel={run.id}
                    disabled={busyId === run.id}
                    onClick={() => void act(run, "cancel")}
                    className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12px] disabled:opacity-40"
                  >
                    {busyId === run.id ? <Loader2 className="size-3 animate-spin" aria-hidden /> : null}
                    Cancel
                  </button>
                ) : null}
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}
