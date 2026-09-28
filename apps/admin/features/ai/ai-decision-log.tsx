"use client";

/**
 * The route decision log (docs/requests/REQ-098, slice 3).
 *
 * Slices 1 and 2 made the routing deliberate; this screen makes it *explainable*. It answers the
 * question an operator actually has at 2am — "why did the expensive model answer the cheap task"
 * — and it is built around three rules that follow from that question existing.
 *
 * - **A row is a sentence, not a record.** Time, task, what was asked, what answered, and the
 *   reason in words. An id in a table is evidence, not an explanation.
 * - **The fallback badge is the point.** A row that quietly used the third candidate is the
 *   signal that a primary is degrading, which is the thing nobody notices until a bill arrives.
 * - **The export is the table.** Both read through one filter builder, so what a spreadsheet
 *   contains is what the screen showed — a mismatch there is what gets pasted into a ticket.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  AlertTriangle,
  Check,
  ChevronRight,
  Download,
  Loader2,
  RotateCw,
  Search,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiDecisionDetail,
  type AiDecisionFilter,
  type AiDecisionPage,
  type AiDecisionRow,
  type AiUnresolvedTask,
  fetchAiDecisions,
  fetchAiDecisionsCsv,
  fetchAiDecision,
  fetchAiUnresolved,
} from "@/lib/api";

/** The task keys the filter offers, in the order the routing screen lists them. */
const TASKS = [
  "cheap",
  "translation",
  "coding",
  "vision",
  "long_context",
  "embedding",
  "critical",
] as const;

/** Date options the range filter offers, as `[value, label]` pairs. */
const RANGES: [string, string][] = [
  ["", "Any time"],
  ["1", "Last 24 hours"],
  ["7", "Last 7 days"],
  ["30", "Last 30 days"],
  ["90", "Last 90 days"],
];

/** The start of the window, as the API wants it (RFC 3339). */
function windowStart(days: string): string | undefined {
  if (!days) return undefined;
  const from = new Date(Date.now() - Number(days) * 24 * 60 * 60 * 1000);
  return from.toISOString();
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

// -------------------------------------------------------------------------------------------
// The warning banner
// -------------------------------------------------------------------------------------------

/**
 * The tasks that could not resolve.
 *
 * Amber, not red, and deliberately so: the installation keeps serving everything that *does*
 * resolve, and colouring this as an error would train operators to ignore the one banner that
 * does mean something. An empty list renders nothing at all rather than an all-clear — a green
 * "everything is fine" box is a box that covers half the screen to say nothing.
 */
function UnresolvedBanner({
  tasks,
  onOpen,
}: {
  tasks: AiUnresolvedTask[];
  onOpen: (task: string) => void;
}) {
  if (tasks.length === 0) return null;

  return (
    <section
      data-log-unresolved
      className="rounded-xl border border-warning/40 bg-warning/5 p-3.5"
      role="status"
    >
      <div className="flex items-start gap-2.5">
        <AlertTriangle className="mt-0.5 size-4 shrink-0 text-warning" aria-hidden />
        <div className="min-w-0 flex-1">
          <p className="text-[13px] font-medium text-warning-ink">
            {tasks.length} task{tasks.length === 1 ? "" : "s"} could not resolve
          </p>
          <p className="mt-0.5 text-[12.5px] text-muted">
            Requests for {tasks.length === 1 ? "this task" : "these tasks"} were refused. The
            other tasks keep serving.
          </p>
          <ul className="mt-2 space-y-1">
            {tasks.map((entry) => (
              <li key={entry.task} className="flex flex-wrap items-baseline gap-x-2 text-[12.5px]">
                <button
                  type="button"
                  data-log-unresolved-task={entry.task}
                  onClick={() => onOpen(entry.task)}
                  className="font-medium underline underline-offset-2 hover:text-ink"
                >
                  {entry.task}
                </button>
                <span className="text-muted">{entry.reason}</span>
                <span className="text-muted">
                  {entry.occurrences} request{entry.occurrences === 1 ? "" : "s"}
                  {entry.last_failed_at ? ` · ${relative(entry.last_failed_at)} ago` : ""}
                </span>
              </li>
            ))}
          </ul>
        </div>
      </div>
    </section>
  );
}

// -------------------------------------------------------------------------------------------
// One row of the table
// -------------------------------------------------------------------------------------------

function DecisionRowView({
  row,
  onOpen,
}: {
  row: AiDecisionRow;
  onOpen: (id: number) => void;
}) {
  return (
    <tr data-log-row={row.id} className="border-t border-line">
      <td className="px-3 py-2 align-top whitespace-nowrap">
        <span data-log-age={relative(row.created_at)} className="text-[12.5px] text-muted">
          {relative(row.created_at)} ago
        </span>
      </td>
      <td className="px-3 py-2 align-top">
        <span data-log-task={row.task ?? ""} className="text-[12.5px] font-medium">
          {row.task ?? row.feature ?? "—"}
        </span>
        {row.feature && row.feature !== row.task ? (
          <span className="ml-1.5 text-[12px] text-muted">via {row.feature}</span>
        ) : null}
      </td>
      <td className="px-3 py-2 align-top">
        {row.requested ? (
          <code data-log-requested className="font-mono text-[12px] text-muted">
            {row.requested}
          </code>
        ) : (
          <span className="text-[12px] text-muted">nothing asked</span>
        )}
      </td>
      <td className="px-3 py-2 align-top">
        {row.resolved_label ? (
          <code data-log-resolved={row.resolved_label} className="font-mono text-[12px]">
            {row.resolved_label}
          </code>
        ) : (
          <span data-log-unresolved-value className="text-[12px] text-muted">
            nothing answered
          </span>
        )}
      </td>
      <td className="px-3 py-2 align-top">
        {row.used_fallback ? (
          <span
            data-log-fallback={row.fallback_index}
            className="rounded-full bg-warning/12 px-1.5 py-0.5 text-[11.5px] font-medium text-warning-ink"
            title={`The ${row.fallback_index + 1}${row.fallback_index === 1 ? "st" : "nd"} candidate answered`}
          >
            fallback {row.fallback_index}
          </span>
        ) : row.unresolved ? (
          <span
            data-log-unresolved-badge
            className="rounded-full bg-danger/10 px-1.5 py-0.5 text-[11.5px] font-medium text-danger"
          >
            unresolved
          </span>
        ) : (
          <span className="text-[12px] text-muted">—</span>
        )}
      </td>
      <td className="px-3 py-2 align-top">
        {/* Two lines on the log row, the rest in the drawer: a reason that wraps to six lines
            turns the table into a wall of prose nobody scans. */}
        <p data-log-reason className="line-clamp-2 max-w-md text-[12.5px] text-muted">
          {row.reason}
        </p>
      </td>
      <td className="px-3 py-2 align-top text-right">
        <button
          type="button"
          data-log-open={row.id}
          onClick={() => onOpen(row.id)}
          className="inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-[12.5px] text-muted hover:bg-canvas hover:text-ink"
          aria-label={`Open decision ${row.id}`}
        >
          Walk
          <ChevronRight className="size-3.5" aria-hidden />
        </button>
      </td>
    </tr>
  );
}

// -------------------------------------------------------------------------------------------
// The detail drawer
// -------------------------------------------------------------------------------------------

/**
 * One decision's full candidate walk.
 *
 * Every candidate that was *considered* is here with its reason, including the ones that were
 * never reached. A walk that only lists the winner is a walk that cannot answer "why not the
 * second model", which is the question that brings an operator to this screen.
 */
function DecisionDrawer({
  id,
  onClose,
}: {
  id: number;
  onClose: () => void;
}) {
  const [detail, setDetail] = useState<AiDecisionDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [token, setToken] = useState(0);

  useEffect(() => {
    let cancelled = false;
    setDetail(null);
    setError(null);

    fetchAiDecision(id)
      .then((found) => {
        if (!cancelled) setDetail(found);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setError(
          errorMessage(cause, "That decision could not be read. It may have been pruned."),
        );
      });

    return () => {
      cancelled = true;
    };
  }, [id, token]);

  // `Escape` closes the drawer, as every overlay in the panel does. A drawer that can only be
  // dismissed with the mouse is a drawer a keyboard user is trapped in.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div
      data-log-drawer
      className="fixed inset-0 z-50 flex items-end justify-center bg-black/40 p-4 sm:items-center"
      role="dialog"
      aria-modal="true"
      aria-label={`Decision ${id}`}
      onClick={onClose}
    >
      <div
        data-log-drawer-panel
        className="max-h-[80vh] w-full max-w-2xl overflow-y-auto rounded-xl border border-line bg-surface p-4"
        onClick={(event) => event.stopPropagation()}
      >
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0">
            <p className="text-[14px] font-medium">Decision {id}</p>
            {detail ? (
              <p className="mt-0.5 text-[12.5px] text-muted">
                {detail.task ?? detail.feature ?? "no task"} · {detail.scope} ·{" "}
                {detail.rule}
              </p>
            ) : null}
          </div>
          <button
            type="button"
            data-log-drawer-close
            onClick={onClose}
            className="rounded-md p-1 text-muted hover:bg-canvas hover:text-ink"
            aria-label="Close"
          >
            <X className="size-4" aria-hidden />
          </button>
        </div>

        {error ? (
          <div
            data-log-drawer-error
            className="mt-3 flex items-start gap-2 rounded-lg border border-danger/40 bg-danger/5 p-2.5"
          >
            <p className="flex-1 text-[12.5px] text-danger">{error}</p>
            <button
              type="button"
              onClick={() => setToken((current) => current + 1)}
              className="rounded-md border border-line px-2 py-1 text-[12px] hover:bg-canvas"
            >
              Retry
            </button>
          </div>
        ) : null}

        {!error && !detail ? (
          <div className="mt-4 flex items-center gap-2 text-[12.5px] text-muted">
            <Loader2 className="size-3.5 animate-spin" aria-hidden />
            Loading the walk…
          </div>
        ) : null}

        {detail ? (
          <>
            <p data-log-drawer-reason className="mt-3 text-[12.5px]">
              {detail.reason}
            </p>
            {detail.requirements.length > 0 ? (
              <p className="mt-1.5 text-[12px] text-muted">
                Required: {detail.requirements.join(", ")}
              </p>
            ) : null}

            <ol data-log-walk className="mt-3 space-y-1.5">
              {detail.walk.map((entry, index) => (
                <li
                  key={`${entry.position ?? index}-${entry.model_id ?? "none"}`}
                  data-log-walk-entry={entry.outcome}
                  className="flex items-start gap-2 rounded-lg border border-line bg-panel px-2.5 py-2"
                >
                  <span
                    className={
                      entry.outcome === "chosen"
                        ? "mt-0.5 text-success"
                        : "mt-0.5 text-muted"
                    }
                  >
                    {entry.outcome === "chosen" ? (
                      <Check className="size-3.5" aria-hidden />
                    ) : (
                      <span className="block size-3.5 rounded-full border border-line" />
                    )}
                  </span>
                  <div className="min-w-0 flex-1">
                    <p className="text-[12.5px]">
                      <span className="text-muted">
                        {entry.position ? `#${entry.position}` : entry.source.replace(/_/g, " ")}
                      </span>{" "}
                      <code className="font-mono">
                        {entry.model_id ?? "no model — removed"}
                      </code>
                    </p>
                    <p className="text-[12px] text-muted">{entry.reason}</p>
                  </div>
                </li>
              ))}
            </ol>

            <p className="mt-3 text-[11.5px] text-muted">
              Resolution order: {detail.rules.join(" → ")}
            </p>
            {detail.run_id ? (
              <p className="mt-1 text-[11.5px] text-muted">
                From agent run <code className="font-mono">{detail.run_id}</code>
              </p>
            ) : null}
          </>
        ) : null}
      </div>
    </div>
  );
}

// -------------------------------------------------------------------------------------------
// The screen
// -------------------------------------------------------------------------------------------

export function AiDecisionLogScreen() {
  const [page, setPage] = useState<AiDecisionPage | null>(null);
  const [unresolved, setUnresolved] = useState<AiUnresolvedTask[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [token, setToken] = useState(0);
  const [open, setOpen] = useState<number | null>(null);

  const [task, setTask] = useState("");
  const [range, setRange] = useState("");
  const [fallbackOnly, setFallbackOnly] = useState(false);
  const [unresolvedOnly, setUnresolvedOnly] = useState(false);
  const [search, setSearch] = useState("");
  const [exported, setExported] = useState<string | null>(null);
  const [exportError, setExportError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // The filter object is memoized so the effect below depends on its *contents* rather than on
  // a fresh object identity: an inline literal would re-fetch on every render and the log would
  // flicker between states while the operator types in the search box.
  const filter = useMemo<AiDecisionFilter>(
    () => ({
      ...(task ? { task } : {}),
      ...(fallbackOnly ? { fallback: true } : {}),
      ...(unresolvedOnly ? { unresolved: true } : {}),
      ...(windowStart(range) ? { from: windowStart(range) as string } : {}),
      limit: 50,
    }),
    [task, range, fallbackOnly, unresolvedOnly],
  );

  const load = useCallback(() => {
    setLoading(true);
    setError(null);
    fetchAiDecisions(filter)
      .then((found) => {
        setPage(found);
        setLoading(false);
      })
      .catch((cause: unknown) => {
        setError(errorMessage(cause, "The decision log could not be read."));
        setLoading(false);
      });
    // The unresolved banner is a second read because it is a *different question* ("what is
    // broken now") than the table's ("what happened"). A failure here degrades to no banner
    // rather than to an error over a table that loaded fine.
    fetchAiUnresolved()
      .then((found) => setUnresolved(found.unresolved))
      .catch(() => setUnresolved([]));
  }, [filter]);

  useEffect(load, [load, token]);

  const onExport = async () => {
    setBusy(true);
    setExportError(null);
    setExported(null);
    try {
      const csv = await fetchAiDecisionsCsv(filter);
      const lines = csv.trim() ? csv.trim().split("\n").length : 0;
      const shown = page?.rows.length ?? 0;
      setExported(
        `Exported ${Math.max(0, lines - 1)} row${lines - 1 === 1 ? "" : "s"}` +
          (shown > 0 && lines - 1 !== shown
            ? ` — the screen shows ${shown} of them.`
            : "."),
      );
    } catch (cause: unknown) {
      setExportError(errorMessage(cause, "The export could not be produced."));
    } finally {
      setBusy(false);
    }
  };

  // The search box filters the *rendered* rows by model, because the API has no free-text
  // filter on the model column and adding one for a client-side convenience would be a round
  // trip per keystroke. The result line says how many are hidden, so a search that appears to
  // do nothing cannot be mistaken for a log that has nothing.
  const visible = useMemo(() => {
    const rows = page?.rows ?? [];
    const needle = search.trim().toLowerCase();
    if (!needle) return rows;
    return rows.filter((row) =>
      [row.resolved_label, row.requested, row.task, row.feature, row.reason]
        .filter((value): value is string => Boolean(value))
        .some((value) => value.toLowerCase().includes(needle)),
    );
  }, [page, search]);

  const hidden = (page?.rows.length ?? 0) - visible.length;

  return (
    <div className="space-y-3.5" data-ai-decision-log>
      <UnresolvedBanner
        tasks={unresolved}
        onOpen={(value) => {
          setTask(value);
          setUnresolvedOnly(false);
          setFallbackOnly(false);
        }}
      />

      <section className="rounded-xl border border-line bg-panel p-3.5">
        <div className="flex flex-wrap items-end gap-2.5">
          <label className="text-[12.5px]">
            <span className="mb-1 block text-muted">Task</span>
            <select
              data-log-filter-task
              value={task}
              onChange={(event) => setTask(event.target.value)}
              className="rounded-md border border-line bg-surface px-2 py-1.5 text-[12.5px]"
            >
              <option value="">Every task</option>
              {TASKS.map((key) => (
                <option key={key} value={key}>
                  {key.replace(/_/g, " ")}
                </option>
              ))}
            </select>
          </label>

          <label className="text-[12.5px]">
            <span className="mb-1 block text-muted">When</span>
            <select
              data-log-filter-range
              value={range}
              onChange={(event) => setRange(event.target.value)}
              className="rounded-md border border-line bg-surface px-2 py-1.5 text-[12.5px]"
            >
              {RANGES.map(([value, label]) => (
                <option key={value} value={value}>
                  {label}
                </option>
              ))}
            </select>
          </label>

          <label className="text-[12.5px]">
            <span className="mb-1 block text-muted">Search</span>
            <span className="flex items-center gap-1.5 rounded-md border border-line bg-surface px-2 py-1.5">
              <Search className="size-3.5 text-muted" aria-hidden />
              <input
                data-log-filter-search
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder="model, task or reason"
                className="w-44 bg-transparent text-[12.5px] outline-none"
              />
            </span>
          </label>

          <label className="flex items-center gap-1.5 pb-1.5 text-[12.5px]">
            <input
              data-log-filter-fallback
              type="checkbox"
              checked={fallbackOnly}
              onChange={(event) => setFallbackOnly(event.target.checked)}
              className="size-3.5"
            />
            Fallback only
          </label>

          <label className="flex items-center gap-1.5 pb-1.5 text-[12.5px]">
            <input
              data-log-filter-unresolved
              type="checkbox"
              checked={unresolvedOnly}
              onChange={(event) => setUnresolvedOnly(event.target.checked)}
              className="size-3.5"
            />
            Unresolved only
          </label>

          <div className="ml-auto flex items-center gap-1.5 pb-1">
            <button
              type="button"
              data-log-export
              onClick={onExport}
              disabled={busy}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-canvas disabled:opacity-50"
            >
              {busy ? (
                <Loader2 className="size-3.5 animate-spin" aria-hidden />
              ) : (
                <Download className="size-3.5" aria-hidden />
              )}
              Export CSV
            </button>
            <button
              type="button"
              data-log-retry
              onClick={() => setToken((current) => current + 1)}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-canvas"
            >
              <RotateCw className="size-3.5" aria-hidden />
              Refresh
            </button>
          </div>
        </div>

        {exported ? (
          <p data-log-export-notice className="mt-2 text-[12.5px] text-success">
            {exported}
          </p>
        ) : null}
        {exportError ? (
          <p data-log-export-error className="mt-2 text-[12.5px] text-danger">
            {exportError}
          </p>
        ) : null}
      </section>

      <section className="rounded-xl border border-line bg-surface">
        {error ? (
          <div
            data-log-error
            className="m-3.5 flex items-start gap-2 rounded-lg border border-danger/40 bg-danger/5 p-2.5"
            role="alert"
          >
            <p className="flex-1 text-[12.5px] text-danger">{error}</p>
            <button
              type="button"
              data-log-error-retry
              onClick={() => setToken((current) => current + 1)}
              className="rounded-md border border-line px-2 py-1 text-[12px] hover:bg-canvas"
            >
              Retry
            </button>
          </div>
        ) : null}

        {loading && !page ? (
          <LoadingTable rows={5} columns={7} />
        ) : !error && page && page.total === 0 ? (
          // The empty state names the *cause* rather than the symptom: an empty log means no
          // request has been resolved yet, and the way to change that is to make one.
          <EmptyState
            title="No route decisions yet"
            hint="Every resolved request writes a row here, with the model that answered and why. Nothing has been resolved since the log started."
            action={
              <p className="text-[12.5px] text-muted">
                Send a request from the AI hub, or run the dry run on the routing screen.
              </p>
            }
          />
        ) : page && page.total > 0 && visible.length === 0 ? (
          <EmptyState
            title="Nothing matches these filters"
            hint={`${page.total} decision${page.total === 1 ? "" : "s"} are in this window, and none of them match the search.`}
            action={
              <button
                type="button"
                onClick={() => setSearch("")}
                className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-canvas"
              >
                Clear the search
              </button>
            }
          />
        ) : (
          <>
            <div className="flex items-center justify-between px-3.5 pt-3">
              <p data-log-count className="text-[12.5px] text-muted">
                {page?.total} decision{page?.total === 1 ? "" : "s"}
                {hidden > 0 ? ` · ${hidden} hidden by the search` : ""}
              </p>
            </div>
            <div className="overflow-x-auto">
              <table className="w-full text-left">
                <thead>
                  <tr className="text-[11.5px] text-muted">
                    <th className="px-3 py-2 font-medium">When</th>
                    <th className="px-3 py-2 font-medium">Task</th>
                    <th className="px-3 py-2 font-medium">Requested</th>
                    <th className="px-3 py-2 font-medium">Resolved</th>
                    <th className="px-3 py-2 font-medium">Fallback</th>
                    <th className="px-3 py-2 font-medium">Reason</th>
                    <th className="px-3 py-2" />
                  </tr>
                </thead>
                <tbody>
                  {visible.map((row) => (
                    <DecisionRowView key={row.id} row={row} onOpen={setOpen} />
                  ))}
                </tbody>
              </table>
            </div>
          </>
        )}
      </section>

      {open !== null ? <DecisionDrawer id={open} onClose={() => setOpen(null)} /> : null}
    </div>
  );
}
