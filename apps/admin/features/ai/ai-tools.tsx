"use client";

/**
 * `/ai/tools` — the AI tool registry (docs/requests/REQ-100, slice 1).
 *
 * Every action an agent can take is a named, permissioned row in this table. Four decisions
 * shape the screen, and each of them exists because the obvious alternative fails an operator:
 *
 * 1. **The warning stripe is the screen's job, and it is data, not a derivation.** The API
 *    answers `ungated_high_risk`, and this screen renders it. A row that is high risk, enabled
 *    and ungated can run against production with no second pair of eyes, and an operator who
 *    built that configuration must *see* it — so the stripe is drawn on the row itself rather
 *    than hidden behind a "high risk tools" filter nobody opens.
 *
 * 2. **Disable names the agents it breaks.** The confirmation lists them by name, because
 *    "this may affect your agents" is not a warning, it is a shrug. The names come from the API
 *    (the same query the event carries), so the dialog cannot disagree with the `ai.tool.disabled`
 *    payload an operator would read in the audit trail.
 *
 * 3. **Never-called is an em dash, not `0%`.** A tool nobody has called has no error rate, and
 *    printing `0%` next to one that failed a quarter of its calls makes the first look healthier
 *    than it is measured to be.
 *
 * 4. **The seeding banner replaces the empty state.** The registry is seeded from compiled code
 *    on boot, so an empty table is a *diagnosis* — an upgrade whose seeder has not run — and it
 *    gets a banner saying so, not an empty state offering "Create a tool" for a table the
 *    operator may not extend.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";

import { Loader2, Search, TriangleAlert } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  type AiTool,
  type AiToolClass,
  fetchAiToolClasses,
  fetchAiTools,
  updateAiTool,
} from "@/lib/api";

/** The risk badge's three tones, keyed off the row's risk. */
const RISK_TONE: Record<string, string> = {
  low: "text-emerald-700 dark:text-emerald-300 bg-emerald-500/10",
  medium: "text-amber-700 dark:text-amber-300 bg-amber-500/10",
  high: "text-rose-700 dark:text-rose-300 bg-rose-500/10",
};

/** The call status badge's tones, keyed off the call log's vocabulary. */
const STATUS_TONE: Record<string, string> = {
  ok: "text-emerald-700 dark:text-emerald-300 bg-emerald-500/10",
  denied: "text-rose-700 dark:text-rose-300 bg-rose-500/10",
  failed: "text-rose-700 dark:text-rose-300 bg-rose-500/10",
  timeout: "text-amber-700 dark:text-amber-300 bg-amber-500/10",
  limited: "text-amber-700 dark:text-amber-300 bg-amber-500/10",
};

/** Named export, not default: every other feature module in this app exports its view by name,
 *  and a default export here would be the one file `import { AiToolsView } from …` cannot reach. */
export function AiToolsView({ organizationId }: { organizationId?: string | null }) {
  const [tools, setTools] = useState<AiTool[] | null>(null);
  const [classes, setClasses] = useState<AiToolClass[]>([]);
  const [seeded, setSeeded] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [q, setQ] = useState("");
  const [classFilter, setClassFilter] = useState<string>("all");
  const [riskFilter, setRiskFilter] = useState<string>("all");
  const [gatedOnly, setGatedOnly] = useState(false);
  const [busyKey, setBusyKey] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<AiTool | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      // The needle is sent to the API rather than filtered here: the list is small today, but
      // the registry screen is the one that grows with every tool the platform adds, and a
      // client-side filter over twenty rows that becomes two hundred is a rewrite nobody plans.
      const [list, classList] = await Promise.all([
        fetchAiTools({
          organizationId,
          q: q.trim() || undefined,
          class: classFilter,
          risk: riskFilter,
          gated: gatedOnly ? true : undefined,
        }),
        fetchAiToolClasses(organizationId),
      ]);
      setTools(list.tools);
      setSeeded(list.seeded);
      setClasses(classList);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setTools([]);
    }
  }, [organizationId, q, classFilter, riskFilter, gatedOnly]);

  useEffect(() => {
    void load();
  }, [load]);

  const ungated = useMemo(
    () => (tools ?? []).filter((tool) => tool.ungated_high_risk),
    [tools],
  );

  const toggle = useCallback(
    async (tool: AiTool, nextEnabled: boolean) => {
      setBusyKey(tool.key);
      setError(null);
      try {
        const updated = await updateAiTool(tool.key, { enabled: nextEnabled }, organizationId);
        // Replace in place rather than refetching: a bulk toggle that reloads the whole registry
        // makes the list jump under the operator's cursor, and the response already carries the
        // row's new state.
        setTools((current) =>
          (current ?? []).map((row) => (row.key === updated.key ? { ...row, ...updated } : row)),
        );
        setConfirm(null);
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setBusyKey(null);
      }
    },
    [organizationId],
  );

  const bulk = useCallback(
    async (nextEnabled: boolean) => {
      const rows = (tools ?? []).filter((tool) => tool.enabled !== nextEnabled);
      if (rows.length === 0) return;
      setBusyKey("__bulk__");
      setError(null);
      try {
        // Sequential, not parallel: each PATCH is a write against the same table and the
        // operator is watching. A burst of 20 concurrent requests is how a "Bulk disable"
        // button turns into a rate-limit error halfway through the list.
        for (const tool of rows) {
          const updated = await updateAiTool(tool.key, { enabled: nextEnabled }, organizationId);
          setTools((current) =>
            (current ?? []).map((row) =>
              row.key === updated.key ? { ...row, ...updated } : row,
            ),
          );
        }
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setBusyKey(null);
      }
    },
    [tools, organizationId],
  );

  return (
    <div className="flex flex-col gap-4" data-ai-tools>
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-lg font-semibold">Tool registry</h1>
          <p className="text-sm text-muted-foreground">
            Every action an agent may take, the permission it needs, and what it has cost.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            className="rounded-md border px-3 py-1.5 text-sm disabled:opacity-50"
            disabled={busyKey !== null || (tools ?? []).length === 0}
            onClick={() => void bulk(true)}
          >
            {busyKey === "__bulk__" ? <Loader2 className="size-4 animate-spin" /> : "Enable all"}
          </button>
          <button
            type="button"
            className="rounded-md border px-3 py-1.5 text-sm disabled:opacity-50"
            disabled={busyKey !== null || (tools ?? []).length === 0}
            onClick={() => void bulk(false)}
          >
            {busyKey === "__bulk__" ? <Loader2 className="size-4 animate-spin" /> : "Disable all"}
          </button>
        </div>
      </header>

      {!seeded && (
        <div className="flex items-start gap-2 rounded-md border border-amber-500/40 bg-amber-500/10 p-3 text-sm">
          <TriangleAlert className="mt-0.5 size-4 shrink-0 text-amber-600" />
          <div>
            <p className="font-medium">The registry has not been seeded yet.</p>
            <p className="text-muted-foreground">
              The tools are compiled into the API and written on boot. Restart the API process to
              seed the registry; nothing here is created by hand.
            </p>
          </div>
        </div>
      )}

      {ungated.length > 0 && (
        <div className="flex items-start gap-2 rounded-md border border-rose-500/40 bg-rose-500/10 p-3 text-sm">
          <TriangleAlert className="mt-0.5 size-4 shrink-0 text-rose-600" />
          <div>
            <p className="font-medium">
              {ungated.length} high-risk {ungated.length === 1 ? "tool is" : "tools are"} enabled
              with no approval gate.
            </p>
            <p className="text-muted-foreground">
              {ungated.map((tool) => tool.key).join(", ")} can act without a second pair of eyes.
            </p>
          </div>
        </div>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <label className="flex items-center gap-2 rounded-md border px-2 py-1.5 text-sm">
          <Search className="size-4 text-muted-foreground" />
          <span className="sr-only">Search tools by key or description</span>
          <input
            value={q}
            onChange={(event) => setQ(event.target.value)}
            placeholder="Search key or description"
            className="w-56 bg-transparent outline-none"
          />
        </label>
        <select
          value={classFilter}
          onChange={(event) => setClassFilter(event.target.value)}
          className="rounded-md border px-2 py-1.5 text-sm"
          aria-label="Filter by class"
        >
          <option value="all">All classes</option>
          {classes.map((entry) => (
            <option key={entry.key} value={entry.key}>
              {entry.label}
            </option>
          ))}
        </select>
        <select
          value={riskFilter}
          onChange={(event) => setRiskFilter(event.target.value)}
          className="rounded-md border px-2 py-1.5 text-sm"
          aria-label="Filter by risk"
        >
          <option value="all">All risks</option>
          <option value="low">Low</option>
          <option value="medium">Medium</option>
          <option value="high">High</option>
        </select>
        <label className="flex items-center gap-1.5 text-sm">
          <input
            type="checkbox"
            checked={gatedOnly}
            onChange={(event) => setGatedOnly(event.target.checked)}
          />
          Gated only
        </label>
      </div>

      {error !== null && (
        <div className="flex items-center justify-between gap-3 rounded-md border border-rose-500/40 bg-rose-500/10 p-3 text-sm">
          <span className="text-rose-700 dark:text-rose-300">{error}</span>
          <button
            type="button"
            onClick={() => void load()}
            className="rounded-md border px-2 py-1 text-sm"
          >
            Retry
          </button>
        </div>
      )}

      {tools === null ? (
        <LoadingTable columns={9} />
      ) : tools.length === 0 ? (
        <EmptyState
          title={q || classFilter !== "all" || riskFilter !== "all" || gatedOnly
            ? "No tool matches these filters"
            : "The registry is empty"}
          hint={
            q || classFilter !== "all" || riskFilter !== "all" || gatedOnly
              ? "Clear the filters to see every tool the installation offers."
              : "The API seeds this table from its compiled catalogue on boot."
          }
          action={
            <button
              type="button"
              onClick={() => {
                setQ("");
                setClassFilter("all");
                setRiskFilter("all");
                setGatedOnly(false);
              }}
              className="rounded-md border px-3 py-1.5 text-sm"
            >
              Clear filters
            </button>
          }
        />
      ) : (
        <>
          {/* Desktop: the full column set the spec's table names. */}
          <div className="hidden overflow-x-auto rounded-md border lg:block">
            <table className="w-full text-sm">
              <thead className="bg-muted/40 text-left text-xs uppercase tracking-wide text-muted-foreground">
                <tr>
                  <th className="px-3 py-2">Tool</th>
                  <th className="px-3 py-2">Class</th>
                  <th className="px-3 py-2">Permission</th>
                  <th className="px-3 py-2">Risk</th>
                  <th className="px-3 py-2">Gated</th>
                  <th className="px-3 py-2 text-right">Calls 30d</th>
                  <th className="px-3 py-2 text-right">Error %</th>
                  <th className="px-3 py-2">Last used</th>
                  <th className="px-3 py-2 text-right">Enabled</th>
                </tr>
              </thead>
              <tbody>
                {tools.map((tool) => (
                  <tr
                    key={tool.key}
                    data-ai-tool={tool.key}
                    data-ai-tool-ungated={String(tool.ungated_high_risk)}
                    className={tool.ungated_high_risk ? "border-l-4 border-rose-500" : ""}
                  >
                    <td className="px-3 py-2">
                      <Link
                        href={`/ai/tools/${encodeURIComponent(tool.key)}`}
                        className="font-mono text-xs underline-offset-2 hover:underline"
                      >
                        {tool.key}
                      </Link>
                      {tool.retired_note && (
                        <span className="ml-2 text-xs text-muted-foreground">retired</span>
                      )}
                    </td>
                    <td className="px-3 py-2">{tool.class}</td>
                    <td className="px-3 py-2 font-mono text-xs">{tool.permission}</td>
                    <td className="px-3 py-2">
                      <span className={`rounded px-1.5 py-0.5 text-xs ${RISK_TONE[tool.risk]}`}>
                        {tool.risk}
                      </span>
                    </td>
                    <td className="px-3 py-2">
                      {tool.requires_approval ? (
                        <span className="rounded bg-violet-500/10 px-1.5 py-0.5 text-xs text-violet-700 dark:text-violet-300">
                          gated
                        </span>
                      ) : (
                        <span className="text-xs text-muted-foreground">—</span>
                      )}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums">{tool.calls_30d}</td>
                    <td className="px-3 py-2 text-right tabular-nums">
                      {/* An em dash for never-called, not 0 %. See the header note. */}
                      {tool.error_rate_30d === null ? (
                        <span className="text-muted-foreground">—</span>
                      ) : (
                        `${tool.error_rate_30d.toFixed(1)}%`
                      )}
                    </td>
                    <td className="px-3 py-2 text-muted-foreground">
                      {tool.last_used ? new Date(tool.last_used).toLocaleString() : "—"}
                    </td>
                    <td className="px-3 py-2 text-right">
                      {tool.enabled ? (
                        <button
                          type="button"
                          className="rounded-md border px-2 py-1 text-xs disabled:opacity-50"
                          disabled={busyKey !== null}
                          onClick={() =>
                            tool.used_by_agents.length > 0
                              ? setConfirm(tool)
                              : void toggle(tool, false)
                          }
                        >
                          Disable
                        </button>
                      ) : (
                        <button
                          type="button"
                          className="rounded-md border px-2 py-1 text-xs disabled:opacity-50"
                          disabled={busyKey !== null}
                          onClick={() => void toggle(tool, true)}
                        >
                          {busyKey === tool.key ? (
                            <Loader2 className="size-3 animate-spin" />
                          ) : (
                            "Enable"
                          )}
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {/* Mobile: cards. Risk and gated stay visible — the spec requires both, and a card
              that hides the risk is a card that cannot be triaged from a phone. */}
          <ul className="flex flex-col gap-3 lg:hidden">
            {tools.map((tool) => (
              <li
                key={tool.key}
                data-ai-tool={tool.key}
                className={`rounded-md border p-3 ${tool.ungated_high_risk ? "border-rose-500" : ""}`}
              >
                <div className="flex items-start justify-between gap-2">
                  <Link
                    href={`/ai/tools/${encodeURIComponent(tool.key)}`}
                    className="font-mono text-sm underline-offset-2 hover:underline"
                  >
                    {tool.key}
                  </Link>
                  <div className="flex shrink-0 gap-1">
                    <span className={`rounded px-1.5 py-0.5 text-xs ${RISK_TONE[tool.risk]}`}>
                      {tool.risk}
                    </span>
                    {tool.requires_approval && (
                      <span className="rounded bg-violet-500/10 px-1.5 py-0.5 text-xs text-violet-700 dark:text-violet-300">
                        gated
                      </span>
                    )}
                  </div>
                </div>
                <p className="mt-1 text-sm text-muted-foreground">{tool.description}</p>
                <p className="mt-1 font-mono text-xs text-muted-foreground">{tool.permission}</p>
                <div className="mt-2 flex items-center justify-between text-xs text-muted-foreground">
                  <span>
                    {tool.calls_30d} calls ·{" "}
                    {tool.error_rate_30d === null ? "no errors" : `${tool.error_rate_30d.toFixed(1)}% errors`}
                  </span>
                  <button
                    type="button"
                    className="rounded-md border px-2 py-1 text-xs"
                    disabled={busyKey !== null}
                    onClick={() =>
                      tool.enabled
                        ? tool.used_by_agents.length > 0
                          ? setConfirm(tool)
                          : void toggle(tool, false)
                        : void toggle(tool, true)
                    }
                  >
                    {tool.enabled ? "Disable" : "Enable"}
                  </button>
                </div>
              </li>
            ))}
          </ul>
        </>
      )}

      {confirm !== null && (
        <div
          role="dialog"
          aria-modal="true"
          aria-label="Confirm disabling a tool"
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
          onKeyDown={(event) => {
            if (event.key === "Escape") setConfirm(null);
          }}
        >
          <div className="w-full max-w-md rounded-lg border bg-background p-4 shadow-lg">
            <h2 className="text-base font-semibold">Disable {confirm.key}?</h2>
            <p className="mt-2 text-sm text-muted-foreground">
              {confirm.used_by_agents.length === 0 ? (
                "No agent currently names this tool in its allow-list."
              ) : (
                <>
                  {confirm.used_by_agents.length}{" "}
                  {confirm.used_by_agents.length === 1 ? "agent" : "agents"} will lose it:{" "}
                  <span className="font-medium text-foreground">
                    {confirm.used_by_agents.map((agent) => agent.name).join(", ")}
                  </span>
                  . A run that names it anyway is refused with{" "}
                  <code className="font-mono text-xs">tool_not_granted</code>.
                </>
              )}
            </p>
            <div className="mt-4 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setConfirm(null)}
                className="rounded-md border px-3 py-1.5 text-sm"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={() => void toggle(confirm, false)}
                disabled={busyKey !== null}
                className="rounded-md border border-rose-500/50 bg-rose-500/10 px-3 py-1.5 text-sm"
              >
                {busyKey !== null ? <Loader2 className="size-4 animate-spin" /> : "Disable"}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
