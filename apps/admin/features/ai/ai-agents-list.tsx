"use client";

/**
 * The agents table (docs/requests/REQ-099, slice 1).
 *
 * An agent is a thing that spends money, and this table is where somebody decides whether it is
 * allowed to. Three rules follow, and each one closes a way a table of agents can mislead.
 *
 * 1. **The tool list is the risk, and the approvals count is the part of it that waits for a
 *    person.** "12 tools" says nothing about whether one of them is a deletion. The column
 *    therefore reads "7 · approvals: 2", and a tool that parks a run is the one an operator has
 *    to know about.
 * 2. **A disabled agent keeps its history.** Disable is not Delete, and the confirm says which
 *    one it is — the same distinction the webhook screen makes for an endpoint that is
 *    switched off but still has deliveries worth reading.
 * 3. **The search and the filters live in the query string.** A filtered list that cannot be
 *    pasted to a colleague is one that has to be rebuilt by hand, and the rebuild is where
 *    mistakes happen.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  Copy,
  Loader2,
  Play,
  Plus,
  RotateCw,
  Search,
  Trash2,
} from "lucide-react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiAgent,
  createAiAgent,
  deleteAiAgent,
  fetchAiAgents,
  fetchEffectivePermissions,
  updateAiAgent,
} from "@/lib/api";
import { useSession } from "@/lib/session";

import { RunSheet } from "./run-sheet";

/** How a tool count reads, with the approval half called out. */
function toolsLabel(agent: AiAgent): string {
  const total = agent.tools.length;
  if (total === 0) return "no tools";
  const base = `${total} tool${total === 1 ? "" : "s"}`;
  return agent.approvals_count > 0 ? `${base} · approvals: ${agent.approvals_count}` : base;
}

/** A timestamp as a readable age: "4m", "3h", "2d". */
function relative(iso: string): string {
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return "—";
  const seconds = Math.max(0, Math.floor((Date.now() - then) / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}

function errorMessage(cause: unknown, fallback: string): string {
  return cause instanceof ApiError ? cause.message : fallback;
}

/** The filters the list reads out of the query string. */
type Filters = { q: string; status: string; tool: string };

function filtersFrom(params: URLSearchParams): Filters {
  return {
    q: params.get("q") ?? "",
    status: params.get("status") ?? "",
    tool: params.get("tool") ?? "",
  };
}

export function AiAgentsList() {
  const router = useRouter();
  const params = useSearchParams();
  const { user } = useSession();
  const filters = useMemo(() => filtersFrom(new URLSearchParams(params?.toString() ?? "")), [params]);

  const [rows, setRows] = useState<AiAgent[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [running, setRunning] = useState<AiAgent | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<{ agent: AiAgent; typed: string } | null>(null);
  /** `null` while the permission read is in flight; the form only offers a tool the editor holds. */
  const [mayManage, setMayManage] = useState<boolean | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  // A platform-level account has no organization of its own, so every agent call has to name
  // one. An organization account passes its own and the API refuses anything else, which is the
  // guard we want rather than a client-side re-implementation of it.
  const organizationId = user?.organization_id ?? undefined;

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setRows(await fetchAiAgents(organizationId));
    } catch (caught) {
      setError(errorMessage(caught, "The agents could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [organizationId]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const answer = await fetchEffectivePermissions({ organizationId });
        if (cancelled) return;
        // A permission both granted and denied resolves to a deny, so the granted list alone is
        // not the answer: the effective set is the difference.
        const denied = new Set(answer.denied.map((entry) => entry.key));
        const effective = answer.granted.map((entry) => entry.key).filter((key) => !denied.has(key));
        setMayManage(effective.includes("ai.agents.manage"));
      } catch {
        // A failed permission read leaves the buttons as they are; the API refuses the write
        // anyway, and a screen that hides its own controls because a read failed is a screen
        // that looks broken when nothing is.
        if (!cancelled) setMayManage(true);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [organizationId]);

  const setFilter = (key: keyof Filters, value: string) => {
    const next = new URLSearchParams(params?.toString() ?? "");
    if (value) next.set(key, value);
    else next.delete(key);
    const query = next.toString();
    router.replace(query ? `/ai/agents?${query}` : "/ai/agents");
  };

  const visible = useMemo(() => {
    const needle = filters.q.trim().toLowerCase();
    return rows.filter((agent) => {
      if (needle && !`${agent.name} ${agent.key} ${agent.description}`.toLowerCase().includes(needle)) {
        return false;
      }
      if (filters.status === "enabled" && !agent.enabled) return false;
      if (filters.status === "disabled" && agent.enabled) return false;
      if (filters.tool && !agent.tools.includes(filters.tool)) return false;
      return true;
    });
  }, [rows, filters]);

  /** Every tool key any agent holds, for the tool filter's own options. */
  const toolKeys = useMemo(() => {
    const keys = new Set<string>();
    for (const agent of rows) for (const tool of agent.tools) keys.add(tool);
    return [...keys].sort();
  }, [rows]);

  const applyBulk = async (enabled: boolean) => {
    const ids = [...selected];
    if (ids.length === 0) return;
    setBusy(true);
    setError(null);
    let changed = 0;
    try {
      for (const id of ids) {
        try {
          await updateAiAgent(id, { organizationId, enabled });
          changed += 1;
        } catch {
          // One refused row must not abandon the rest: the notice reports the honest count,
          // which is the only number the reader can act on.
        }
      }
      setNotice(`${changed} of ${ids.length} agents ${enabled ? "enabled" : "disabled"}.`);
      setSelected(new Set());
      await load();
    } finally {
      setBusy(false);
    }
  };

  /**
   * Copy an agent into a new disabled row.
   *
   * The copy is born disabled on purpose: a duplicate that is live the moment it is created is
   * a second thing that can spend money, and the reader has not decided yet whether it should
   * run. The key gets a `-copy` suffix and, if that is taken, a counter — the unique index on
   * `(organization_id, key)` is the real guarantee, and a name collision is not a reason to
   * fail the whole action.
   */
  const duplicate = async (agent: AiAgent) => {
    setBusy(true);
    setError(null);
    try {
      let attempt = 0;
      for (;;) {
        const key = attempt === 0 ? `${agent.key}-copy` : `${agent.key}-copy-${attempt + 1}`;
        try {
          const copy = await createAiAgent({
            organizationId,
            key,
            name: `${agent.name} (copy)`,
            description: agent.description,
            system_prompt: agent.system_prompt,
            model_id: agent.model_id,
            temperature: agent.temperature,
            max_steps: agent.max_steps,
            deadline_seconds: agent.deadline_seconds,
            token_budget: agent.token_budget,
            tools: agent.tools,
            approvals: agent.approvals,
            memory_scope: agent.memory_scope,
            enabled: false,
          });
          setNotice(`Duplicated as ${copy.name}. It is disabled until you enable it.`);
          await load();
          return;
        } catch (caught) {
          const error = caught as ApiError;
          // A 23505 for this key is the collision; anything else is the user's problem to read.
          if (error.code === "agent_exists" || error.status === 409) {
            attempt += 1;
            if (attempt > 20) throw error;
            continue;
          }
          throw error;
        }
      }
    } catch (caught) {
      setError(errorMessage(caught, "The agent could not be duplicated."));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (agent: AiAgent) => {
    setBusy(true);
    setError(null);
    try {
      await deleteAiAgent(agent.id, organizationId);
      setNotice(`Deleted ${agent.name}.`);
      setConfirmDelete(null);
      await load();
    } catch (caught) {
      setError(errorMessage(caught, "The agent could not be deleted."));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div data-ai-agents className="space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <label className="relative flex min-w-48 flex-1 items-center">
          <span className="sr-only">Search agents</span>
          <Search className="pointer-events-none absolute left-2.5 size-3.5 text-muted" aria-hidden />
          <input
            ref={searchRef}
            data-agents-search
            value={filters.q}
            onChange={(event) => setFilter("q", event.target.value)}
            placeholder="Search by name or key"
            className="w-full rounded-lg border border-line bg-surface py-1.5 pl-8 pr-2.5 text-[13px] outline-none focus:border-accent"
          />
        </label>

        <select
          data-agents-filter-status
          value={filters.status}
          onChange={(event) => setFilter("status", event.target.value)}
          aria-label="Filter by status"
          className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] outline-none focus:border-accent"
        >
          <option value="">Any status</option>
          <option value="enabled">Enabled</option>
          <option value="disabled">Disabled</option>
        </select>

        <select
          data-agents-filter-tool
          value={filters.tool}
          onChange={(event) => setFilter("tool", event.target.value)}
          aria-label="Filter by tool"
          className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] outline-none focus:border-accent"
        >
          <option value="">Any tool</option>
          {toolKeys.map((key) => (
            <option key={key} value={key}>
              {key}
            </option>
          ))}
        </select>

        <button
          type="button"
          data-agents-refresh
          onClick={() => void load()}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2 py-1.5 text-[12.5px] text-muted hover:text-ink"
        >
          <RotateCw className="size-3.5" aria-hidden />
          Refresh
        </button>

        <Link
          href="/ai/agents/new"
          data-agents-new
          className="ml-auto inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
        >
          <Plus className="size-3.5" aria-hidden />
          New agent
        </Link>
      </div>

      {error ? (
        <div
          data-agents-error
          role="alert"
          className="flex items-start justify-between gap-3 rounded-xl border border-danger/40 bg-danger/5 px-3.5 py-3"
        >
          <p className="text-[12.5px] text-danger">{error}</p>
          <button type="button" onClick={() => void load()} className="shrink-0 text-[12.5px] underline">
            Retry
          </button>
        </div>
      ) : null}

      {notice ? (
        <p data-agents-notice className="text-[12.5px] text-positive">
          {notice}
        </p>
      ) : null}

      {selected.size > 0 ? (
        <div
          data-agents-bulk
          className="flex items-center gap-2 rounded-xl border border-line bg-panel px-3 py-2"
        >
          <span className="text-[12.5px] font-medium">{selected.size} selected</span>
          <button
            type="button"
            data-agents-bulk-enable
            disabled={busy || mayManage === false}
            onClick={() => void applyBulk(true)}
            className="rounded-lg border border-line px-2 py-1 text-[12px] disabled:opacity-50"
          >
            Enable
          </button>
          <button
            type="button"
            data-agents-bulk-disable
            disabled={busy || mayManage === false}
            onClick={() => void applyBulk(false)}
            className="rounded-lg border border-line px-2 py-1 text-[12px] disabled:opacity-50"
          >
            Disable
          </button>
          <button
            type="button"
            onClick={() => setSelected(new Set())}
            className="ml-auto text-[12px] text-muted hover:text-ink"
          >
            Clear
          </button>
        </div>
      ) : null}

      {loading ? (
        <LoadingTable columns={7} />
      ) : visible.length === 0 ? (
        rows.length === 0 ? (
          <EmptyState
            title="No agent yet"
            hint="An agent is a model, a goal and the tools it may use. Create one and it can be run from here, from a workflow or from an API call."
            action={
              <Link
                href="/ai/agents/new"
                className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
              >
                <Plus className="size-3.5" aria-hidden />
                Create agent
              </Link>
            }
          />
        ) : (
          <EmptyState
            title="No agent matches these filters"
            hint="Clear the search or the filters to see every agent."
            action={
              <button
                type="button"
                onClick={() => router.replace("/ai/agents")}
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px]"
              >
                Clear filters
              </button>
            }
          />
        )
      ) : (
        <>
          {/* Below `lg` the table becomes label/value cards: a seven-column grid on a phone is a
              table nobody can read, and a card is what the same rows become. */}
          <div className="hidden overflow-x-auto lg:block">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                  <th className="w-8 px-2 py-2" scope="col">
                    <span className="sr-only">Select</span>
                  </th>
                  <th className="px-2 py-2" scope="col">Name</th>
                  <th className="px-2 py-2" scope="col">Model</th>
                  <th className="px-2 py-2" scope="col">Tools</th>
                  <th className="px-2 py-2" scope="col">Memory</th>
                  <th className="px-2 py-2" scope="col">Updated</th>
                  <th className="px-2 py-2" scope="col">Status</th>
                  <th className="px-2 py-2" scope="col">Actions</th>
                </tr>
              </thead>
              <tbody>
                {visible.map((agent) => (
                  <tr key={agent.id} data-agent-row={agent.id} className="border-t border-line">
                    <td className="px-2 py-2">
                      <input
                        type="checkbox"
                        aria-label={`Select ${agent.name}`}
                        data-agent-select={agent.id}
                        checked={selected.has(agent.id)}
                        onChange={(event) =>
                          setSelected((previous) => {
                            const next = new Set(previous);
                            if (event.target.checked) next.add(agent.id);
                            else next.delete(agent.id);
                            return next;
                          })
                        }
                      />
                    </td>
                    <td className="px-2 py-2">
                      <Link
                        href={`/ai/agents/${agent.id}`}
                        className="font-medium underline-offset-2 hover:underline"
                      >
                        {agent.name}
                      </Link>
                      <span className="ml-1.5 font-mono text-[11.5px] text-muted">{agent.key}</span>
                      {agent.description ? (
                        <span className="mt-0.5 block max-w-sm text-[12px] text-muted">
                          {agent.description}
                        </span>
                      ) : null}
                    </td>
                    <td className="px-2 py-2">
                      <span className="font-mono text-[12px]">
                        {agent.model_id ? agent.model_id.slice(0, 8) : "routed"}
                      </span>
                    </td>
                    <td className="px-2 py-2">
                      <span data-agent-tools={toolsLabel(agent)} className="text-[12.5px]">
                        {toolsLabel(agent)}
                      </span>
                    </td>
                    <td className="px-2 py-2 text-[12.5px] text-muted">{agent.memory_scope}</td>
                    <td className="px-2 py-2 text-[12.5px] text-muted">{relative(agent.updated_at)}</td>
                    <td className="px-2 py-2">
                      <span
                        data-agent-status={agent.enabled ? "enabled" : "disabled"}
                        className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${
                          agent.enabled ? "bg-positive-soft text-positive" : "bg-quiet-soft text-muted"
                        }`}
                      >
                        {agent.enabled ? "Enabled" : "Disabled"}
                      </span>
                    </td>
                    <td className="px-2 py-2">
                      <div className="flex items-center gap-1">
                        <button
                          type="button"
                          data-agent-run={agent.id}
                          disabled={!agent.enabled}
                          onClick={() => setRunning(agent)}
                          title={agent.enabled ? undefined : "Enable this agent before running it"}
                          className="inline-flex items-center gap-1 rounded-lg border border-line px-1.5 py-1 text-[12px] disabled:opacity-40"
                        >
                          <Play className="size-3" aria-hidden />
                          Run
                        </button>
                        <button
                          type="button"
                          data-agent-duplicate={agent.id}
                          disabled={busy || mayManage === false}
                          onClick={() => void duplicate(agent)}
                          className="inline-flex items-center gap-1 rounded-lg border border-line px-1.5 py-1 text-[12px] disabled:opacity-40"
                        >
                          <Copy className="size-3" aria-hidden />
                          Duplicate
                        </button>
                        <button
                          type="button"
                          data-agent-toggle={agent.id}
                          disabled={busy || mayManage === false}
                          onClick={() => void applyBulkFor(agent, !agent.enabled)}
                          className="rounded-lg border border-line px-1.5 py-1 text-[12px] disabled:opacity-40"
                        >
                          {agent.enabled ? "Disable" : "Enable"}
                        </button>
                        <button
                          type="button"
                          data-agent-delete={agent.id}
                          disabled={busy || mayManage === false}
                          onClick={() => setConfirmDelete({ agent, typed: "" })}
                          className="inline-flex items-center gap-1 rounded-lg border border-line px-1.5 py-1 text-[12px] text-danger disabled:opacity-40"
                        >
                          <Trash2 className="size-3" aria-hidden />
                          Delete
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {/* The same rows as cards on a phone — label, value, actions, no horizontal scroll. */}
          <ul className="space-y-2 lg:hidden">
            {visible.map((agent) => (
              <li
                key={agent.id}
                data-agent-card={agent.id}
                className="space-y-2 rounded-xl border border-line bg-surface p-3"
              >
                <div className="flex items-start justify-between gap-2">
                  <Link href={`/ai/agents/${agent.id}`} className="font-medium">
                    {agent.name}
                  </Link>
                  <span
                    data-agent-status={agent.enabled ? "enabled" : "disabled"}
                    className={`shrink-0 rounded-full px-2 py-0.5 text-[11px] font-medium ${
                      agent.enabled ? "bg-positive-soft text-positive" : "bg-quiet-soft text-muted"
                    }`}
                  >
                    {agent.enabled ? "Enabled" : "Disabled"}
                  </span>
                </div>
                <dl className="grid grid-cols-[6rem_1fr] gap-x-3 gap-y-1 text-[12.5px]">
                  <dt className="text-muted">Key</dt>
                  <dd className="font-mono text-[12px]">{agent.key}</dd>
                  <dt className="text-muted">Tools</dt>
                  <dd data-agent-tools={toolsLabel(agent)}>{toolsLabel(agent)}</dd>
                  <dt className="text-muted">Memory</dt>
                  <dd>{agent.memory_scope}</dd>
                  <dt className="text-muted">Updated</dt>
                  <dd>{relative(agent.updated_at)}</dd>
                </dl>
                <div className="flex flex-wrap gap-1.5">
                  <button
                    type="button"
                    data-agent-run={agent.id}
                    disabled={!agent.enabled}
                    onClick={() => setRunning(agent)}
                    className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12px] disabled:opacity-40"
                  >
                    <Play className="size-3" aria-hidden />
                    Run
                  </button>
                  <button
                    type="button"
                    data-agent-duplicate={agent.id}
                    disabled={busy || mayManage === false}
                    onClick={() => void duplicate(agent)}
                    className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12px] disabled:opacity-40"
                  >
                    <Copy className="size-3" aria-hidden />
                    Duplicate
                  </button>
                  <button
                    type="button"
                    data-agent-toggle={agent.id}
                    disabled={busy || mayManage === false}
                    onClick={() => void applyBulkFor(agent, !agent.enabled)}
                    className="rounded-lg border border-line px-2 py-1 text-[12px] disabled:opacity-40"
                  >
                    {agent.enabled ? "Disable" : "Enable"}
                  </button>
                  <button
                    type="button"
                    data-agent-delete={agent.id}
                    disabled={busy || mayManage === false}
                    onClick={() => setConfirmDelete({ agent, typed: "" })}
                    className="rounded-lg border border-line px-2 py-1 text-[12px] text-danger disabled:opacity-40"
                  >
                    Delete
                  </button>
                </div>
              </li>
            ))}
          </ul>
        </>
      )}

      {confirmDelete ? (
        <div
          data-agent-delete-confirm
          className="fixed inset-0 z-50 flex items-center justify-center bg-ink/25 p-4"
          role="dialog"
          aria-modal="true"
          aria-label={`Delete ${confirmDelete.agent.name}`}
        >
          <div className="w-full max-w-md rounded-2xl border border-line bg-surface p-4">
            <h2 className="text-[14px] font-semibold">Delete {confirmDelete.agent.name}?</h2>
            <p className="mt-1 text-[12.5px] text-muted">
              Its runs are kept and stop resolving to an agent, so the history stays readable. This
              cannot be undone.
            </p>
            <label className="mt-3 block text-[12.5px] font-medium" htmlFor="delete-confirm-input">
              Type the name to confirm
            </label>
            <input
              id="delete-confirm-input"
              data-agent-delete-input
              value={confirmDelete.typed}
              onChange={(event) => setConfirmDelete((previous) => (previous ? { ...previous, typed: event.target.value } : previous))}
              className="mt-1 w-full rounded-lg border border-line bg-canvas px-2.5 py-2 text-[13px] outline-none focus:border-accent"
            />
            <div className="mt-3 flex items-center gap-2">
              <button
                type="button"
                data-agent-delete-confirm-button
                disabled={busy || confirmDelete.typed !== confirmDelete.agent.name}
                onClick={() => void remove(confirmDelete.agent)}
                className="rounded-lg bg-danger px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-50"
              >
                {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
                Delete
              </button>
              <button
                type="button"
                onClick={() => setConfirmDelete(null)}
                className="text-[12.5px] text-muted hover:text-ink"
              >
                Keep it
              </button>
            </div>
          </div>
        </div>
      ) : null}

      {running ? (
        <RunSheet
          agent={running}
          organizationId={organizationId}
          onClose={() => {
            setRunning(null);
            void load();
          }}
          onStarted={() => void load()}
          onOpenRun={(runId) => router.push(`/ai/runs/${runId}`)}
        />
      ) : null}
    </div>
  );

  /** One row's enable/disable, sharing the bulk path so the notice reads the same either way. */
  async function applyBulkFor(agent: AiAgent, enabled: boolean) {
    setBusy(true);
    setError(null);
    try {
      await updateAiAgent(agent.id, { organizationId, enabled });
      setNotice(`${agent.name} ${enabled ? "enabled" : "disabled"}.`);
      await load();
    } catch (caught) {
      setError(errorMessage(caught, "The agent could not be changed."));
    } finally {
      setBusy(false);
    }
  }
}

/** Re-exported so the empty state can name the example skills the REQ points at. */
export const AGENT_EXAMPLE_SKILLS_LINK = "/ai/skills";
