"use client";

/**
 * `/deployment/backfills` — the data migrations an operator runs BY HAND (REQ-129, slice 3).
 *
 * ## The three things this screen must never blur
 *
 * 1. **A declared descriptor is a backfill the operator has to run.** A migration that registered a
 *    backfill but whose job was never created is part of the answer, not a separate screen — on a
 *    fresh installation those rows are the ONLY rows, and hiding them makes "this release needs a
 *    data migration" invisible exactly when it matters most.
 *
 * 2. **`rows_done` is a self-report.** The progress bar is drawn from it and labelled with it, but
 *    nothing here claims the counter is *verified*. Slice 3's own proof watched that counter read
 *    **2097 for 250 rows** while every screen rendered it as healthy progress. So the cursor is
 *    always shown beside it: a cursor in the key column's own type and a row count are two
 *    independent witnesses, and an operator who sees one move without the other knows to look.
 *
 * 3. **A pause keeps the cursor; a reset would not.** The Pause button says "keeps its position"
 *    next to it, because "pause" and "start over" are one typo apart for anyone who has not read
 *    the crate, and the difference is a full re-run over every existing row.
 *
 * ## `failed` shows the database's own words
 *
 * A backfill's statement belongs to the migration author. `last_error` is rendered verbatim, in a
 * monospace block, and never summarised: "the migration failed" is not actionable and the operator
 * is the only person who can fix it.
 *
 * Keyboard: `/` focuses the filter, `r` refreshes, `Esc` clears. Under `sm:` the table becomes
 * cards.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  CircleDashed,
  Database,
  Gauge,
  Pause,
  Play,
  RefreshCw,
  Search,
  Table2,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  listBackfills,
  pauseBackfill,
  resumeBackfill,
  runBackfillBatch,
  type BackfillJob,
  type BackfillList,
  type BackfillRun,
  type BackfillState,
  type PendingBackfill,
} from "@/lib/deployment-api";
import { formatTimestamp } from "@/lib/format";

const STATE_WORDS: Record<BackfillState, string> = {
  pending: "not started",
  running: "running",
  paused: "paused",
  completed: "completed",
  failed: "failed",
};

/** Badge tone per state. `pending` is neutral, never green — nothing has run yet. */
function stateBadge(state: BackfillState) {
  switch (state) {
    case "running":
      return "bg-blue-100 text-blue-900 dark:bg-blue-900/40 dark:text-blue-100";
    case "completed":
      return "bg-emerald-100 text-emerald-900 dark:bg-emerald-900/40 dark:text-emerald-100";
    case "failed":
      return "bg-red-100 text-red-900 dark:bg-red-900/40 dark:text-red-100";
    case "paused":
      return "bg-amber-100 text-amber-900 dark:bg-amber-900/40 dark:text-amber-100";
    default:
      return "bg-muted px-2 py-0.5 text-xs font-normal";
  }
}

/**
 * How far along the job is, as far as this screen can honestly tell.
 *
 * There is no total to divide by — a backfill counts rows it has WRITTEN, and the rows still to do
 * are `count(*) where column is null`, which is a table scan this screen has no business running on
 * an operator's keystroke. So the bar is deliberately not a percentage: it is an indeterminate
 * track with the counter as the only number. Inventing a denominator is how a progress bar starts
 * reading 100% on a job that has not finished.
 */
function Progress({ job }: { job: BackfillJob }) {
  return (
    <div className="flex flex-col gap-1">
      <div
        className="h-1.5 w-full overflow-hidden rounded-full bg-muted"
        role="progressbar"
        aria-label={`${job.name}: ${job.rows_done} rows written`}
        aria-valuenow={job.rows_done}
        // No `aria-valuemax`: the API has no total, and a bar with a made-up maximum is worse than
        // a bar that says what it knows.
      >
        <div
          className={`h-full rounded-full ${
            job.state === "failed"
              ? "bg-red-500"
              : job.state === "completed"
                ? "bg-emerald-500"
                : "bg-blue-500"
          } ${job.state === "running" ? "animate-pulse" : ""}`}
          style={{ width: job.state === "completed" ? "100%" : "35%" }}
        />
      </div>
      <p className="text-[11.5px] text-muted">
        <span className="font-medium text-foreground">{job.rows_done.toLocaleString()}</span> rows
        written · cursor{" "}
        <code className="rounded bg-muted px-1 py-0.5 text-[11px]">{job.cursor_display}</code>
      </p>
    </div>
  );
}

export function BackfillsView() {
  const [list, setList] = useState<BackfillList | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      setList(await listBackfills());
    } catch (caught) {
      setError(
        caught instanceof ApiError
          ? caught.message
          : "The backfill list could not be loaded.",
      );
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.isContentEditable === true;
      if (typing) return;
      if (event.key === "/") {
        event.preventDefault();
        searchRef.current?.focus();
      } else if (event.key === "r") {
        event.preventDefault();
        void load();
      } else if (event.key === "Escape" && filter) {
        event.preventDefault();
        setFilter("");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load, filter]);

  const visible = useMemo(() => {
    if (!list) return [];
    const needle = filter.trim().toLowerCase();
    if (!needle) return list.jobs;
    return list.jobs.filter((job) =>
      `${job.name} ${job.table_name} ${job.column_name} ${job.state}`
        .toLowerCase()
        .includes(needle),
    );
  }, [list, filter]);

  const pending: PendingBackfill[] = useMemo(() => {
    if (!list) return [];
    const needle = filter.trim().toLowerCase();
    if (!needle) return list.pending_descriptors;
    return list.pending_descriptors.filter((descriptor) =>
      `${descriptor.name} ${descriptor.table_name} ${descriptor.column_name}`
        .toLowerCase()
        .includes(needle),
    );
  }, [list, filter]);

  /**
   * Every write goes through here so the three reports cannot drift.
   *
   * The clamp case is the one worth the wrapper: `resumeBackfill` clamps 5000 batches to 10 and
   * echoes both numbers. Silently showing only what ran is what an operator reports as the platform
   * ignoring them, so when the two differ the screen says so.
   */
  const act = useCallback(
    async (id: string, verb: string, work: () => Promise<BackfillRun | { job: BackfillJob }>) => {
      setBusy(id);
      setNotice(null);
      try {
        const result = await work();
        const job = result.job;
        if (verb === "run") {
          const run = result as BackfillRun;
          setNotice(
            run.rows > 0
              ? `${job.name}: this request wrote ${run.rows.toLocaleString()} rows; the job is at ${job.rows_done.toLocaleString()}.`
              : `${job.name}: nothing ran — ${job.state === "paused" ? "it is paused, so resume it" : "the batch found no rows left"}.`,
          );
        } else if (verb === "resume") {
          const run = result as BackfillRun;
          const asked = run.requested_batches ?? run.batches ?? 1;
          const ran = run.batches ?? 0;
          const clamped =
            typeof run.requested_batches === "number" &&
            run.requested_batches > (list?.bounds.max_batches_per_request ?? Number.MAX_SAFE_INTEGER);
          setNotice(
            clamped
              ? `${job.name}: you asked for ${asked} batches and the API ran ${ran} — that is the ceiling, not a failure. The job is at ${job.rows_done.toLocaleString()} rows.`
              : `${job.name}: ran ${ran} batch(es), wrote ${run.rows.toLocaleString()} rows. The job is at ${job.rows_done.toLocaleString()}.`,
          );
        } else {
          setNotice(
            `${job.name} is paused with its cursor at ${job.cursor_display} — resume continues from there, not from the start of the table.`,
          );
        }
        await load();
      } catch (caught) {
        setNotice(
          caught instanceof ApiError
            ? `${verb} refused: ${caught.message}`
            : `${verb} could not be completed.`,
        );
        await load();
      } finally {
        setBusy(null);
      }
    },
    [load, list],
  );

  if (loading) return <LoadingTable columns={5} rows={3} />;

  return (
    <div className="flex flex-col gap-5" data-view="deployment-backfills">
      {error ? (
        <div
          role="alert"
          className="flex items-start gap-2 rounded-md border border-red-300 bg-red-50 px-3 py-2.5 text-[13px] text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200"
        >
          <AlertTriangle size={16} className="mt-0.5 shrink-0" aria-hidden />
          <span>
            {error}{" "}
            <button type="button" onClick={() => void load()} className="inline-flex underline">
              Try again
            </button>
          </span>
        </div>
      ) : null}

      {/* The API's own ceilings, shown rather than discovered. */}
      {list ? (
        <p className="text-[12.5px] text-muted">
          A request runs at most{" "}
          <span className="font-medium text-foreground">
            {list.bounds.max_batches_per_request} batches
          </span>
          . A batch is between {list.bounds.min_batch_size.toLocaleString()} and{" "}
          {list.bounds.max_batch_size.toLocaleString()} rows. No button here drains a table: a
          backfill is the one write that touches every existing row, so it is held in batches you
          can stop between.
        </p>
      ) : null}

      <div className="flex flex-wrap items-center gap-2">
        <label className="relative flex-1 min-w-[200px]">
          <Search
            size={15}
            aria-hidden
            className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-muted"
          />
          <span className="sr-only">Filter backfills</span>
          <input
            ref={searchRef}
            value={filter}
            onChange={(event) => setFilter(event.target.value)}
            placeholder="Filter by name, table, column or state  (press / )"
            className="w-full rounded-md border border-line bg-surface px-8 py-2 text-[13px] outline-none focus:border-accent"
          />
        </label>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-sm"
        >
          <RefreshCw size={15} aria-hidden />
          Refresh
          <kbd className="text-[10.5px] text-muted">r</kbd>
        </button>
      </div>

      {notice ? (
        <p
          role="status"
          className="rounded-md border border-line bg-surface px-3 py-2 text-[12.5px]"
        >
          {notice}
        </p>
      ) : null}

      {/* Descriptors with no job: an obligation the operator can see but not yet act on, said
          plainly rather than rendered as a disabled row. */}
      {pending.length > 0 ? (
        <section aria-labelledby="declared-heading" className="flex flex-col gap-2">
          <h2
            id="declared-heading"
            className="flex items-center gap-2 text-[13px] font-medium"
          >
            <CircleDashed size={15} aria-hidden className="text-muted" />
            Declared by a migration, not started yet
          </h2>
          <p className="text-[12.5px] text-muted">
            {pending.length} migration{pending.length === 1 ? "" : "s"} shipped a backfill
            descriptor. A job appears here once the migration has run on this installation and
            created it — until then the data is still the old shape.
          </p>
          <ul className="grid gap-2 sm:grid-cols-2">
            {pending.map((descriptor) => (
              <li
                key={descriptor.name}
                className="rounded-md border border-line bg-surface p-3 text-[13px]"
              >
                <p className="font-medium">{descriptor.name}</p>
                <p className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[12px] text-muted">
                  <span className="inline-flex items-center gap-1">
                    <Table2 size={13} aria-hidden />
                    {descriptor.table_name}.{descriptor.column_name}
                  </span>
                  <span className="inline-flex items-center gap-1">
                    <Gauge size={13} aria-hidden />
                    {descriptor.batch_size.toLocaleString()} rows/batch
                  </span>
                  <span className="rounded-full bg-muted px-2 py-0.5 text-xs">
                    version {descriptor.version}
                  </span>
                </p>
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      {visible.length === 0 && pending.length === 0 ? (
        <EmptyState
          title={filter ? "No backfill matches that filter" : "No backfill on this installation"}
          hint={
            filter
              ? "Clear the filter to see every job."
              : "Backfills appear when a migration adds a column to a table that already has rows. This installation has none, so nothing is waiting."
          }
        />
      ) : null}

      {visible.length > 0 ? (
        <>
          {/* Desktop: a table. */}
          <div className="hidden overflow-x-auto sm:block">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[12px] text-muted">
                  <th className="px-3 py-2 font-normal">Backfill</th>
                  <th className="px-3 py-2 font-normal">State</th>
                  <th className="px-3 py-2 font-normal">Progress</th>
                  <th className="px-3 py-2 font-normal">Started</th>
                  <th className="px-3 py-2 font-normal text-right">Actions</th>
                </tr>
              </thead>
              <tbody>
                {visible.map((job) => (
                  <tr key={job.id} className="border-b border-line align-top">
                    <td className="px-3 py-3">
                      <Link
                        href={`/deployment/backfills/${job.id}`}
                        className="font-medium underline-offset-2 hover:underline"
                      >
                        {job.name}
                      </Link>
                      <p className="mt-0.5 flex items-center gap-1 text-[11.5px] text-muted">
                        <Database size={12} aria-hidden />
                        {job.table_name}.{job.column_name}
                      </p>
                      {job.last_error ? (
                        <pre className="mt-2 max-w-md overflow-x-auto rounded border border-red-300 bg-red-50 px-2 py-1.5 text-[11.5px] text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200">
                          {job.last_error}
                        </pre>
                      ) : null}
                    </td>
                    <td className="px-3 py-3">
                      <span
                        className={`inline-block rounded-full px-2 py-0.5 text-xs ${stateBadge(
                          job.state,
                        )}`}
                      >
                        {STATE_WORDS[job.state]}
                      </span>
                    </td>
                    <td className="w-56 px-3 py-3">
                      <Progress job={job} />
                    </td>
                    <td className="px-3 py-3 text-[12px] text-muted">
                      {job.started_at ? formatTimestamp(job.started_at) : "—"}
                    </td>
                    <td className="px-3 py-3">
                      <div className="flex flex-wrap justify-end gap-1.5">
                        {job.can_pause ? (
                          <button
                            type="button"
                            disabled={busy === job.id}
                            onClick={() =>
                              void act(job.id, "pause", () => pauseBackfill(job.id))
                            }
                            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-60"
                          >
                            <Pause size={13} aria-hidden />
                            Pause
                          </button>
                        ) : null}
                        {job.can_resume ? (
                          <button
                            type="button"
                            disabled={busy === job.id}
                            onClick={() =>
                              void act(job.id, "resume", () => resumeBackfill(job.id, 1))
                            }
                            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-60"
                          >
                            <Play size={13} aria-hidden />
                            Resume 1 batch
                          </button>
                        ) : null}
                        {job.state === "running" ? (
                          <button
                            type="button"
                            disabled={busy === job.id}
                            onClick={() =>
                              void act(job.id, "run", () => runBackfillBatch(job.id))
                            }
                            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-60"
                          >
                            <RefreshCw size={13} aria-hidden />
                            Run one batch
                          </button>
                        ) : null}
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {/* Mobile: the same rows as cards, because a five-column table at 390px is a
              horizontal-scroll puzzle rather than a list. */}
          <ul className="flex flex-col gap-3 sm:hidden">
            {visible.map((job) => (
              <li key={job.id} className="rounded-md border border-line bg-surface p-3">
                <div className="flex items-start justify-between gap-2">
                  <Link
                    href={`/deployment/backfills/${job.id}`}
                    className="font-medium underline-offset-2 hover:underline"
                  >
                    {job.name}
                  </Link>
                  <span
                    className={`shrink-0 rounded-full px-2 py-0.5 text-xs ${stateBadge(
                      job.state,
                    )}`}
                  >
                    {STATE_WORDS[job.state]}
                  </span>
                </div>
                <p className="mt-1 flex items-center gap-1 text-[11.5px] text-muted">
                  <Database size={12} aria-hidden />
                  {job.table_name}.{job.column_name}
                </p>
                <div className="mt-2">
                  <Progress job={job} />
                </div>
                {job.last_error ? (
                  <pre className="mt-2 overflow-x-auto rounded border border-red-300 bg-red-50 px-2 py-1.5 text-[11.5px] text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200">
                    {job.last_error}
                  </pre>
                ) : null}
                <div className="mt-3 flex flex-wrap gap-1.5">
                  {job.can_pause ? (
                    <button
                      type="button"
                      disabled={busy === job.id}
                      onClick={() => void act(job.id, "pause", () => pauseBackfill(job.id))}
                      className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-60"
                    >
                      <Pause size={13} aria-hidden />
                      Pause
                    </button>
                  ) : null}
                  {job.can_resume ? (
                    <button
                      type="button"
                      disabled={busy === job.id}
                      onClick={() => void act(job.id, "resume", () => resumeBackfill(job.id, 1))}
                      className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-60"
                    >
                      <Play size={13} aria-hidden />
                      Resume 1 batch
                    </button>
                  ) : null}
                  {job.state === "running" ? (
                    <button
                      type="button"
                      disabled={busy === job.id}
                      onClick={() => void act(job.id, "run", () => runBackfillBatch(job.id))}
                      className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-60"
                    >
                      <RefreshCw size={13} aria-hidden />
                      Run one batch
                    </button>
                  ) : null}
                </div>
              </li>
            ))}
          </ul>
        </>
      ) : null}
    </div>
  );
}
