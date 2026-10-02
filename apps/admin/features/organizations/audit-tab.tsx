"use client";

/**
 * The Audit tab (REQ-005, slice 3).
 *
 * A tenant's own trail: who did what, when, and from where — with the three filters the REQ names
 * (action, actor, date) and a CSV export of exactly what is on screen.
 *
 * Three decisions the screen depends on:
 *
 * * **The action filter is a picker, not a search box.** The list is built from the actions this
 *   tenant has actually performed (the same response carries it), so it cannot offer a filter
 *   that matches nothing and it cannot drift as new actions are added to the platform.
 * * **The row count is the filtered count.** A feed that says "12 of 12" while showing three is
 *   the kind of quiet wrong an audit screen must never have, so `total` comes from the same
 *   filtered statement as the rows.
 * * **`system` is a first-class actor.** The rows nobody performed — the bootstrap, the retention
 *   sweep — are real entries with no human behind them, and a filter that cannot reach them is a
 *   filter that hides the platform's own actions.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { Download, Filter, RefreshCw, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  downloadCsv,
  fetchOrganizationAudit,
  type OrganizationAuditEntry,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { Organization } from "@/lib/types";

/** A loadable tab: never both an error and a list, never a spinner over stale rows. */
type TabState = "loading" | "ready" | "error";

/** The rows a person could have acted as; `system` is the platform's own. */
interface ActorOption {
  id: string;
  name: string;
}

export function AuditTab({ organization }: { organization: Organization }) {
  const [entries, setEntries] = useState<OrganizationAuditEntry[]>([]);
  const [actions, setActions] = useState<string[]>([]);
  const [actors, setActors] = useState<ActorOption[]>([]);
  const [total, setTotal] = useState(0);
  const [state, setState] = useState<TabState>("loading");
  const [error, setError] = useState<string | null>(null);
  const [exporting, setExporting] = useState(false);
  const [downloaded, setDownloaded] = useState<string | null>(null);

  const [action, setAction] = useState("");
  const [actor, setActor] = useState("");
  const [since, setSince] = useState("");

  const load = useCallback(async () => {
    setState("loading");
    setError(null);
    try {
      const body = await fetchOrganizationAudit(organization.id, { action, actor, since });
      setEntries(body.entries);
      setActions(body.actions);
      setTotal(body.total);
      setState("ready");
    } catch (cause) {
      setState("error");
      setError(
        cause instanceof ApiError ? cause.message : "The audit trail could not be loaded.",
      );
    }
  }, [organization.id, action, actor, since]);

  useEffect(() => {
    void load();
  }, [load]);

  // The actor list is derived from the page on screen rather than fetched separately: it names
  // the people this tenant's trail actually contains, and a filter offering an account with no
  // rows behind it is a filter that leads nowhere.
  const actorOptions = useMemo(() => {
    const seen = new Map<string, string>();
    for (const entry of entries) {
      if (entry.actor_user_id && entry.actor_name) {
        seen.set(entry.actor_user_id, entry.actor_name);
      }
    }
    return [...seen.entries()].map(([id, name]) => ({ id, name }));
  }, [entries]);

  const download = async () => {
    setExporting(true);
    setDownloaded(null);
    try {
      // The same filters as the screen, so the file is the page and not a different query's idea
      // of it — a CSV that disagrees with the table above it is worse than no export.
      const params = new URLSearchParams({ format: "csv" });
      if (action) params.set("action", action);
      if (actor) params.set("actor", actor);
      if (since) params.set("since", `${since}T00:00:00Z`);

      const result = await downloadCsv(
        `/api/v1/organizations/${encodeURIComponent(organization.id)}/audit?${params.toString()}`,
        `omnion-audit-${organization.slug}.csv`,
      );
      setDownloaded(`Exported ${result.rows} rows to ${result.filename}.`);
    } catch (cause) {
      setDownloaded(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The export failed.",
      );
    } finally {
      setExporting(false);
    }
  };

  const anyFilter = action !== "" || actor !== "" || since !== "";

  return (
    <div className="flex flex-col gap-4">
      <div
        className="flex flex-wrap items-center justify-between gap-3 rounded-xl border border-line bg-surface px-4 py-3"
        data-audit-filters="1"
      >
        <div className="flex flex-wrap items-center gap-2">
          <Filter className="size-3.5 text-muted" aria-hidden />
          <label className="flex items-center gap-1.5">
            <span className="sr-only">Filter by action</span>
            <select
              value={action}
              onChange={(event) => setAction(event.target.value)}
              data-audit-action-filter
              className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
            >
              <option value="">All actions</option>
              {actions.map((value) => (
                <option key={value} value={value}>
                  {value}
                </option>
              ))}
            </select>
          </label>

          <label className="flex items-center gap-1.5">
            <span className="sr-only">Filter by actor</span>
            <select
              value={actor}
              onChange={(event) => setActor(event.target.value)}
              data-audit-actor-filter
              className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
            >
              <option value="">Anyone</option>
              <option value="system">Platform (system)</option>
              {actorOptions.map((option) => (
                <option key={option.id} value={option.id}>
                  {option.name}
                </option>
              ))}
            </select>
          </label>

          <label className="flex items-center gap-1.5 text-[12px] text-muted">
            Since
            <input
              type="date"
              value={since}
              onChange={(event) => setSince(event.target.value)}
              data-audit-since-filter
              className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
            />
          </label>

          {anyFilter ? (
            <button
              type="button"
              onClick={() => {
                setAction("");
                setActor("");
                setSince("");
              }}
              data-audit-clear-filters
              className="flex items-center gap-1 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              <X className="size-3" aria-hidden />
              Clear
            </button>
          ) : null}
        </div>

        <div className="flex items-center gap-2">
          <span className="text-[12px] text-muted" data-audit-count="1">
            {state === "ready" ? `${entries.length} of ${total}` : "Loading…"}
          </span>
          <button
            type="button"
            onClick={() => void load()}
            aria-label="Reload the audit trail"
            className="rounded-lg border border-line bg-surface p-2 text-muted transition hover:text-ink"
          >
            <RefreshCw className="size-3.5" aria-hidden />
          </button>
          <button
            type="button"
            onClick={() => void download()}
            disabled={exporting || entries.length === 0}
            data-audit-export="1"
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-60"
          >
            <Download className="size-3.5" aria-hidden />
            {exporting ? "Exporting…" : "Download CSV"}
          </button>
        </div>
      </div>

      {downloaded ? (
        <p role="status" className="rounded-xl border border-line bg-surface px-4 py-2.5 text-[12.5px]">
          {downloaded}
        </p>
      ) : null}

      <div className="overflow-hidden rounded-xl border border-line bg-surface">
        {error ? (
          <div className="flex flex-col items-center gap-3 px-6 py-10 text-center">
            <p role="alert" className="text-[12.5px] text-accent-strong">
              {error}
            </p>
            <button
              type="button"
              onClick={() => void load()}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              Try again
            </button>
          </div>
        ) : state !== "ready" ? (
          <LoadingTable columns={4} rows={4} />
        ) : entries.length === 0 ? (
          <EmptyState
            title={anyFilter ? "Nothing matches those filters" : "No activity yet"}
            hint={
              anyFilter
                ? "Clear the action, actor or date filter to see the whole trail."
                : "Every privileged change in this organization is recorded here as it happens."
            }
          />
        ) : (
          <>
            {/* A five-column trail (action, actor, target, address, time) is a table on a desktop
                and a card on a phone. Both render from the same `entries`, and each carries the
                same `data-audit-row`, so the QA pass's row assertions mean the same thing at
                either width. */}
            <div className="hidden overflow-x-auto md:block">
              <table className="w-full border-collapse text-left text-[13px]">
                <thead>
                  <tr className="bg-canvas/60 text-[11px] font-medium tracking-wide text-muted uppercase">
                    <th scope="col" className="px-4 py-2.5">Action</th>
                    <th scope="col" className="px-4 py-2.5">Actor</th>
                    <th scope="col" className="px-4 py-2.5">Target</th>
                    <th scope="col" className="px-4 py-2.5">From</th>
                    <th scope="col" className="px-4 py-2.5">When</th>
                  </tr>
                </thead>
                <tbody>
                  {entries.map((entry) => (
                    <tr
                      key={entry.id}
                      data-audit-row={entry.action}
                      className="border-t border-line transition hover:bg-canvas/60"
                    >
                      <td className="px-4 py-3">
                        <span className="font-mono text-[12px]">{entry.action}</span>
                      </td>
                      <td className="px-4 py-3">
                        {entry.actor_type === "system" ? (
                          <span className="text-muted">Platform (system)</span>
                        ) : (
                          <span className="truncate">{entry.actor_name ?? "Unknown account"}</span>
                        )}
                      </td>
                      <td className="max-w-48 truncate px-4 py-3 text-muted">
                        {entry.target_type
                          ? `${entry.target_type}${entry.target_id ? ` ${entry.target_id.slice(0, 8)}` : ""}`
                          : "—"}
                      </td>
                      <td className="px-4 py-3 font-mono text-[12px] text-muted">
                        {entry.ip_address ?? "—"}
                      </td>
                      <td className="px-4 py-3 text-muted">{formatTimestamp(entry.created_at)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>

            <ul className="flex flex-col gap-2 px-3 py-3 md:hidden">
              {entries.map((entry) => (
                <li
                  key={entry.id}
                  data-audit-row={entry.action}
                  className="rounded-lg border border-line px-3 py-2.5"
                >
                  <p className="font-mono text-[12px] break-words">{entry.action}</p>
                  <p className="mt-1 text-[12.5px]">
                    {entry.actor_type === "system" ? (
                      <span className="text-muted">Platform (system)</span>
                    ) : (
                      entry.actor_name ?? "Unknown account"
                    )}
                  </p>
                  <p className="mt-0.5 flex flex-wrap gap-x-3 text-[11.5px] text-muted">
                    {entry.target_type ? (
                      <span>
                        {entry.target_type}
                        {entry.target_id ? ` ${entry.target_id.slice(0, 8)}` : ""}
                      </span>
                    ) : null}
                    {entry.ip_address ? <span className="font-mono">{entry.ip_address}</span> : null}
                    <span>{formatTimestamp(entry.created_at)}</span>
                  </p>
                </li>
              ))}
            </ul>
          </>
        )}
      </div>
    </div>
  );
}
