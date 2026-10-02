"use client";

/**
 * `/deployment/history` and the checks screen (REQ-024, slice 1).
 *
 * The history table's columns and filters are the spec's own list — Started · Environment ·
 * From → To · Kind · Actor · Duration · Result · Log, filtered by environment, kind, result and
 * window — and rows expand to the step list. Two decisions are worth naming:
 *
 * * **The chips read the filter the API echoed back**, not the chip's own state. A panel that
 *   renders "week" because it is on "week" while the request carried a 30-day window is a
 *   screen that lies after a back-navigation, which is exactly when somebody is reading it.
 * * **An empty result says which filter produced it.** "No deployments yet" and "no deployments
 *   match this filter" are different facts with different next actions, and one empty-state
 *   string cannot be right for both.
 */

import { useCallback, useEffect, useState } from "react";

import { ChevronDown, ChevronRight, RefreshCw } from "lucide-react";
import { useSearchParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError, fetchDeploymentChecks, fetchDeploymentHistory } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type {
  DeploymentChecksResponse,
  DeploymentHistoryFilters,
  DeploymentHistoryResponse,
  DeploymentHistoryRow,
} from "@/lib/types";

import {
  CheckRowLine,
  HistoryRowSummary,
  StaleBanner,
  StepBadge,
  StepLine,
  formatDuration,
} from "./deployment-parts";

/** The window chips. `all` is the default rather than `7d`, because a fresh install has no
 *  history at all and "nothing in the last week" would be a lie about a table that is empty. */
const WINDOWS = [
  { key: "all", label: "All time" },
  { key: "24h", label: "Last 24 hours" },
  { key: "7d", label: "Last 7 days" },
  { key: "30d", label: "Last 30 days" },
  { key: "90d", label: "Last 90 days" },
] as const;

const KINDS = ["deploy", "rollback", "restart"] as const;
const STATUSES = ["succeeded", "failed", "cancelled"] as const;

/** The history table. */
export function DeploymentHistory() {
  const search = useSearchParams();
  const [filters, setFilters] = useState<DeploymentHistoryFilters>({
    environment: search.get("environment") ?? undefined,
    window: "all",
  });
  const [data, setData] = useState<DeploymentHistoryResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setData(await fetchDeploymentHistory(filters));
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The deployment history could not be read.",
      );
    } finally {
      setLoading(false);
    }
  }, [filters]);

  useEffect(() => {
    void load();
  }, [load]);

  const set = useCallback((patch: Partial<DeploymentHistoryFilters>) => {
    setFilters((current) => ({ ...current, ...patch }));
  }, []);

  const toggle = useCallback((id: string) => {
    setExpanded((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const applied = data?.filter;
  const filtered =
    Boolean(applied?.environment) || Boolean(applied?.kind) || Boolean(applied?.status) ||
    (applied?.window !== undefined && applied.window !== "all");

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2">
        <FilterChip
          label="Environment"
          value={applied?.environment ?? "any"}
          options={["any", "production", "staging", "sandbox"]}
          onChange={(value) =>
            set({ environment: value === "any" ? undefined : value })
          }
        />
        <FilterChip
          label="Kind"
          value={applied?.kind ?? "any"}
          options={["any", ...KINDS]}
          onChange={(value) => set({ kind: value === "any" ? undefined : value })}
        />
        <FilterChip
          label="Result"
          value={applied?.status ?? "any"}
          options={["any", ...STATUSES]}
          onChange={(value) => set({ status: value === "any" ? undefined : value })}
        />
        <div className="flex flex-wrap items-center gap-1.5">
          {WINDOWS.map((window) => (
            <button
              key={window.key}
              type="button"
              aria-pressed={filters.window === window.key}
              onClick={() => set({ window: window.key })}
              className={`rounded-lg border px-2.5 py-1.5 text-[12px] font-medium ${
                filters.window === window.key
                  ? "border-accent bg-accent/10 text-ink"
                  : "border-line text-muted hover:bg-quiet-soft"
              }`}
            >
              {window.label}
            </button>
          ))}
        </div>
        <button
          type="button"
          onClick={() => void load()}
          className="ml-auto inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium hover:bg-quiet-soft"
        >
          <RefreshCw aria-hidden="true" className="size-3.5" />
          Refresh
        </button>
      </div>

      {error ? (
        <p role="alert" className="text-[12.5px] text-red-700 dark:text-red-300">
          {error}
        </p>
      ) : null}

      {loading ? (
        <LoadingTable columns={7} rows={6} />
      ) : data && data.rows.length === 0 ? (
        <div className="rounded-xl border border-line bg-surface">
          <EmptyState
            title={filtered ? "No deployment matches this filter" : "No deployment has been recorded"}
            hint={
              filtered
                ? "Widen the window or clear a chip — the history itself may well have rows."
                : "A deploy, a rollback or a restart writes a row here with its steps. Slice 1 ships the read; the wizard that writes the first row is slice 2."
            }
            action={
              filtered ? (
                <button
                  type="button"
                  onClick={() => setFilters({ window: "all" })}
                  className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium hover:bg-quiet-soft"
                >
                  Clear the filters
                </button>
              ) : undefined
            }
          />
        </div>
      ) : (
        <>
          <div className="overflow-x-auto rounded-xl border border-line">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[11px] tracking-wide text-muted uppercase">
                  <th scope="col" className="px-4 py-3 font-medium">
                    <span className="sr-only">Expand</span>
                  </th>
                  <th scope="col" className="px-4 py-3 font-medium">Started</th>
                  <th scope="col" className="px-4 py-3 font-medium">Environment</th>
                  <th scope="col" className="px-4 py-3 font-medium">From → To</th>
                  <th scope="col" className="px-4 py-3 font-medium">Kind</th>
                  <th scope="col" className="px-4 py-3 font-medium">Duration</th>
                  <th scope="col" className="px-4 py-3 font-medium">Result</th>
                </tr>
              </thead>
              <tbody>
                {data?.rows.map((row) => (
                  <HistoryTableRow
                    key={row.id}
                    row={row}
                    open={expanded.has(row.id)}
                    onToggle={() => toggle(row.id)}
                  />
                ))}
              </tbody>
            </table>
          </div>
          <p className="text-[12px] text-muted">
            {data?.rows.length} of {data?.total} row(s).
          </p>
        </>
      )}
    </div>
  );
}

/** One history row, expandable to its steps. */
function HistoryTableRow({
  row,
  open,
  onToggle,
}: {
  row: DeploymentHistoryRow;
  open: boolean;
  onToggle: () => void;
}) {
  const hasSteps = row.steps.length > 0;
  return (
    <>
      <tr className="border-b border-line">
        <td className="px-4 py-3">
          {/* The expander is a real button with `aria-expanded`, not a chevron that only looks
              clickable: the row's log is the only evidence of what a run did, and an
              unclickable chevron would hide it for ever. */}
          <button
            type="button"
            onClick={onToggle}
            disabled={!hasSteps}
            aria-expanded={open}
            aria-label={hasSteps ? `Steps for the ${row.kind} of ${row.environment}` : "No steps recorded"}
            className="inline-flex items-center rounded p-1 text-muted disabled:opacity-40 enabled:hover:bg-quiet-soft"
          >
            {open ? (
              <ChevronDown aria-hidden="true" className="size-4" />
            ) : (
              <ChevronRight aria-hidden="true" className="size-4" />
            )}
          </button>
        </td>
        <td className="px-4 py-3 whitespace-nowrap">{formatTimestamp(row.started_at)}</td>
        <td className="px-4 py-3">{row.environment}</td>
        <td className="px-4 py-3 tabular-nums">
          {row.from_version ?? "—"} → {row.to_version ?? "—"}
        </td>
        <td className="px-4 py-3">{row.kind}</td>
        <td className="px-4 py-3 text-muted tabular-nums">{formatDuration(row.duration_ms)}</td>
        <td className="px-4 py-3">
          <StepBadge status={row.status} />
          {row.error ? (
            <p className="mt-1 max-w-sm text-[11.5px] text-red-700 dark:text-red-300">
              {row.error}
            </p>
          ) : null}
        </td>
      </tr>
      {open ? (
        <tr className="border-b border-line bg-quiet-soft/40">
          <td colSpan={7} className="px-4 py-3">
            <HistoryRowSummary row={row} />
            {row.reason ? (
              <p className="mt-1.5 text-[12px] text-muted">Reason: {row.reason}</p>
            ) : null}
            <ul className="mt-2">
              {row.steps.map((step) => (
                <StepLine key={step.position} step={step} />
              ))}
            </ul>
          </td>
        </tr>
      ) : null}
    </>
  );
}

/** A labelled dropdown chip. */
function FilterChip({
  label,
  value,
  options,
  onChange,
}: {
  label: string;
  value: string;
  options: readonly string[];
  onChange: (value: string) => void;
}) {
  const id = `deployment-filter-${label.toLowerCase()}`;
  return (
    <span className="inline-flex items-center gap-1.5">
      {/* A real `<label htmlFor>`, not a placeholder and not an `aria-label` alone: the spec's
          form rule is that every field says what it is, and a filter chip that only announces
          itself to a screen reader is invisible to everyone reading the screen. */}
      <label htmlFor={id} className="text-[12px] text-muted">
        {label}
      </label>
      <select
        id={id}
        value={value}
        onChange={(event) => onChange(event.target.value)}
        className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px]"
      >
        {options.map((option) => (
          <option key={option} value={option}>
            {option === "any" ? "any" : option}
          </option>
        ))}
      </select>
    </span>
  );
}

/** `/deployment/checks` — what the update check has been doing. */
export function DeploymentChecks() {
  const [data, setData] = useState<DeploymentChecksResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setData(await fetchDeploymentChecks());
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The checks could not be read.");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  if (loading) return <LoadingTable columns={2} rows={4} />;

  if (error) {
    return (
      <p role="alert" className="text-[13.5px] font-medium text-red-800 dark:text-red-200">
        {error}
      </p>
    );
  }
  if (!data) return null;

  return (
    <div className="flex flex-col gap-5">
      {data.stale_banner ? <StaleBanner text={data.stale_banner} /> : null}

      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <Stat label="Channel" value={data.channel} hint="What this installation follows" />
        <Stat
          label="Last run"
          value={data.last_run_at ? formatTimestamp(data.last_run_at) : "never"}
          hint={
            data.last_status === "completed"
              ? "The last run completed."
              : data.last_status === "failed"
                ? "The last run failed."
                : "No check has run on this installation yet."
          }
        />
        <Stat
          label="Releases known"
          value={data.last_seen === null ? "—" : String(data.last_seen)}
          hint="What the last successful read carried"
        />
        <Stat
          label="Next run"
          value={
            data.due_in_seconds === null
              ? "not scheduled"
              : data.due_in_seconds < 60
                ? `in ${data.due_in_seconds}s`
                : `in ${Math.round(data.due_in_seconds / 60)} min`
          }
          hint="The scheduled check, on its own interval"
        />
      </div>

      {data.last_error ? (
        <p className="rounded-xl border border-amber-500/40 bg-amber-500/5 px-4 py-3 text-[12.5px] text-amber-900 dark:text-amber-200">
          The last check failed: {data.last_error}
        </p>
      ) : null}

      {data.last_announced.length > 0 ? (
        <p className="text-[12.5px]">
          The last successful run announced:{" "}
          <span className="font-medium">{data.last_announced.join(", ")}</span>. This instance
          has announced {data.announced_total} release(s) in total, so a feed that republishes
          the same manifest emits nothing further.
        </p>
      ) : null}

      <section>
        <h2 className="mb-2 text-[13px] font-medium">What the check did</h2>
        <ul className="rounded-xl border border-line bg-surface px-4 py-1">
          {data.rows.map((row) => (
            <CheckRowLine key={row.check} row={row} />
          ))}
        </ul>
      </section>
    </div>
  );
}

/** One number, with the sentence that makes it mean something. */
function Stat({ label, value, hint }: { label: string; value: string; hint: string }) {
  return (
    <div className="flex flex-col gap-1 rounded-xl border border-line bg-surface px-4 py-3.5">
      <span className="text-[11px] font-medium tracking-wide text-muted uppercase">{label}</span>
      <span className="text-[18px] leading-tight font-medium tabular-nums">{value}</span>
      <span className="text-[11.5px] text-muted">{hint}</span>
    </div>
  );
}
