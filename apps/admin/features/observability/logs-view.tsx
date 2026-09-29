"use client";

/**
 * `/observability/logs` — the bounded log explorer (docs/requests/REQ-126, slice 1).
 *
 * This screen is the jump an operator takes from an error banner, a support ticket or a dead
 * monitor to the lines of one request, so it is built around ONE thing working well: paste an id,
 * get that request's whole story in the order it happened, across the API and the workers.
 *
 * Five things this screen is careful about, each of which otherwise looks like a working screen:
 *
 * - **No telemetry is a different state from no matches.** The API answers both with an empty
 *   `entries` array. "The store holds 0 lines" and "your filter matched nothing out of 41 203"
 *   are different sentences, and an operator who cannot tell them apart concludes the platform
 *   stopped logging. `stored_total` is what separates them, and the empty state names which of
 *   the two it is.
 * - **The request timeline reads oldest first; the explorer reads newest first.** That is the
 *   API's contract, not this screen's preference: a list that is newest-first is what every list
 *   in the panel does, and a timeline that is newest-first is unreadable. When the request-id
 *   filter is on, the screen SAYS the order changed rather than leaving an operator to wonder
 *   why the rows are reversed.
 * - **A non-uuid request id is refused in the component.** The previous tick found this on the
 *   traces screen: `Number("12x")` became `NaN` and `URLSearchParams` stringified that into the
 *   query, so a typo produced a `400` the screen then rendered as "the log store could not be
 *   read" — the worst thing a debug screen can say to the person debugging. It is validated here,
 *   with a field-level message naming the shape, and the request is not sent.
 * - **Fields are rendered from an already-redacted object.** There is no masking pass in this file
 *   and none is needed: redaction happened when the line was written
 *   (`crates/telemetry::redact`), so the values here are the only values that exist. A screen that
 *   tried to "reveal" them would find nothing.
 * - **Retention is shown beside the results that it limits.** A window that silently hides older
 *   lines is indistinguishable from a platform that never recorded them.
 *
 * Keyboard: `/` focuses the search box, `r` refreshes, `Esc` clears the filters. Under `sm:` the
 * result rows become stacked cards rather than a horizontally-scrolling table, because a log
 * message and a request id on one 375 px line is not a row anybody can read.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { AlertTriangle, Copy, Loader2, RefreshCw, Search, Waypoints, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchLogs,
  fetchLogSettings,
  type LogEntry,
  type LogListResponse,
  type LogSettingsView,
} from "@/lib/api";

/** The closed level set. The API refuses anything else with the accepted list in the message. */
const LEVEL_TONE: Record<string, string> = {
  error: "bg-danger-soft text-danger",
  warn: "bg-caution-soft text-caution",
  info: "bg-quiet-soft text-muted",
  debug: "bg-quiet-soft text-muted",
  trace: "bg-quiet-soft text-muted",
};

/** Which process emitted a line. Worker lines are the ones a request-only search finds for you. */
const SOURCE_TONE: Record<string, string> = {
  api: "bg-quiet-soft text-muted",
  worker: "bg-accent-soft text-accent-strong",
  cli: "bg-quiet-soft text-muted",
};

const UUID_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** `12,043` — a count an operator compares against a dashboard. */
function count(value: number): string {
  return new Intl.NumberFormat("en").format(value);
}

/** The clock part only. The date is on its own column when a search spans days. */
function clock(iso: string): string {
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return iso;
  return at.toLocaleTimeString("en-GB", { hour12: false });
}

function day(iso: string): string {
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return "";
  return at.toLocaleDateString("en-GB", { day: "2-digit", month: "short" });
}

/**
 * The status class, not the code.
 *
 * `4xx` and `5xx` are what a column of log rows is scanned for; the exact code is one click away
 * in the expanded fields, where a row that is only there to be read does not spend its width on it.
 */
function statusTone(status: number | null): string {
  if (status === null) return "";
  if (status >= 500) return "text-danger";
  if (status >= 400) return "text-caution";
  return "text-positive";
}

/** The filter as the screen holds it. Empty strings mean "no filter", never "match empty". */
type Draft = {
  levels: string[];
  target: string;
  requestId: string;
  traceId: string;
  source: string;
  text: string;
};

const EMPTY_DRAFT: Draft = {
  levels: [],
  target: "",
  requestId: "",
  traceId: "",
  source: "",
  text: "",
};

/** One expanded row: the line's structured detail, already redacted at write time. */
function FieldsPanel({ entry }: { entry: LogEntry }) {
  const rows = Object.entries((entry.fields ?? {}) as Record<string, unknown>).filter(
    ([, value]) => value !== null && value !== undefined && typeof value !== "object",
  );
  if (rows.length === 0) {
    return <p className="text-[12.5px] text-muted">This line carries no structured fields.</p>;
  }
  return (
    <dl className="grid grid-cols-[minmax(7rem,auto)_1fr] gap-x-3 gap-y-1 text-[12.5px]">
      {rows.map(([key, value]) => (
        <div key={key} className="contents">
          <dt className="truncate font-medium text-muted">{key}</dt>
          <dd className="min-w-0 break-words font-mono">{String(value)}</dd>
        </div>
      ))}
    </dl>
  );
}

/** One row of the explorer. */
function LogRow({
  entry,
  expanded,
  onToggle,
}: {
  entry: LogEntry;
  expanded: boolean;
  onToggle: () => void;
}) {
  const detail = [
    entry.user_id ? `user ${entry.user_id}` : null,
    entry.organization_id ? `org ${entry.organization_id}` : null,
    entry.host ?? null,
    entry.version ? `v${entry.version}` : null,
  ].filter(Boolean);

  return (
    <li className="border-t border-line first:border-t-0">
      <button
        type="button"
        data-log-row={entry.id}
        aria-expanded={expanded}
        onClick={onToggle}
        className="grid w-full grid-cols-1 gap-1 px-4 py-3 text-left hover:bg-quiet-soft sm:grid-cols-[5.5rem_4.5rem_1fr_auto] sm:items-start sm:gap-3"
      >
        <span className="flex items-center gap-1.5 text-[12px] tabular-nums text-muted">
          <span className="sm:hidden">{day(entry.ts)}</span>
          {clock(entry.ts)}
        </span>

        <span className="flex items-center gap-1.5">
          <span
            className={`rounded px-1.5 py-0.5 text-[11.5px] font-medium ${
              LEVEL_TONE[entry.level] ?? "bg-quiet-soft text-muted"
            }`}
          >
            {entry.level}
          </span>
          <span
            className={`rounded px-1.5 py-0.5 text-[11px] ${
              SOURCE_TONE[entry.source] ?? "bg-quiet-soft text-muted"
            }`}
          >
            {entry.source}
          </span>
        </span>

        <span className="min-w-0">
          <span className="block break-words font-mono text-[12.5px]">{entry.message}</span>
          <span className="mt-0.5 block truncate text-[11.5px] text-muted">
            {entry.target}
            {entry.route ? ` · ${entry.method ?? ""} ${entry.route}` : ""}
          </span>
        </span>

        <span className="flex items-center gap-2 text-[11.5px] text-muted sm:justify-end">
          {entry.duration_ms !== null ? `${entry.duration_ms} ms` : null}
          {entry.status !== null ? (
            <span className={`font-medium ${statusTone(entry.status)}`}>{entry.status}</span>
          ) : null}
          {entry.trace_id ? (
            <span className="font-mono" title={entry.trace_id}>
              {entry.trace_id.slice(0, 8)}
            </span>
          ) : null}
        </span>
      </button>

      {expanded ? (
        <div className="space-y-3 border-t border-line bg-quiet-soft/40 px-4 py-3">
          <div className="flex flex-wrap items-center gap-2 text-[11.5px]">
            {entry.request_id ? (
              <span className="inline-flex items-center gap-1.5">
                <span className="text-muted">request</span>
                <code className="rounded bg-quiet-soft px-1.5 py-0.5 font-mono">
                  {entry.request_id}
                </code>
                <button
                  type="button"
                  data-log-copy-request={entry.id}
                  onClick={() => void navigator.clipboard?.writeText(entry.request_id ?? "")}
                  className="inline-flex items-center gap-1 rounded border border-line px-1.5 py-0.5 hover:bg-panel"
                >
                  <Copy className="h-3 w-3" aria-hidden="true" />
                  copy
                </button>
              </span>
            ) : null}
            {detail.length > 0 ? (
              <span className="text-muted">{detail.join(" · ")}</span>
            ) : null}
          </div>
          <FieldsPanel entry={entry} />
        </div>
      ) : null}
    </li>
  );
}

/** The explorer. */
export function LogsView() {
  const [draft, setDraft] = useState<Draft>(EMPTY_DRAFT);
  const [data, setData] = useState<LogListResponse | null>(null);
  const [settings, setSettings] = useState<LogSettingsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [expanded, setExpanded] = useState<number | null>(null);
  const [idError, setIdError] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  /**
   * A request id is validated BEFORE it is sent.
   *
   * The API's `Uuid` extractor answers a `400` for a malformed path segment, and a screen that
   * forwards whatever the operator typed turns a typo into an error page instead of a field
   * message. The traces screen had exactly this defect; the check lives here for the same reason.
   */
  const requestIdValid = draft.requestId.trim() === "" || UUID_PATTERN.test(draft.requestId.trim());

  const load = useCallback(async () => {
    const id = draft.requestId.trim();
    if (id !== "" && !UUID_PATTERN.test(id)) {
      setIdError(
        "A request id is a UUID — eight-four-four-four-twelve hex characters, as printed on the line. Nothing was sent, so the filters below still describe the store.",
      );
      return;
    }
    setIdError(null);
    setLoading(true);
    setError(null);
    try {
      const [rows, row] = await Promise.all([fetchLogs({ ...draft }), fetchLogSettings()]);
      setData(rows);
      setSettings(row);
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setLoading(false);
    }
  }, [draft]);

  useEffect(() => {
    void load();
  }, [load]);

  // `/` focuses the search box and `r` refreshes, unless the operator is typing into a field —
  // a global `r` that fires while somebody types a request id into a box is a filter that changes
  // itself.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement;
      if (event.key === "/" && !typing) {
        event.preventDefault();
        searchRef.current?.focus();
      }
      if (event.key === "r" && !typing) {
        event.preventDefault();
        void load();
      }
      if (event.key === "Escape") {
        setDraft(EMPTY_DRAFT);
        setIdError(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load]);

  // The id message is cleared the moment the field changes, so a corrected paste does not leave a
  // stale complaint next to a valid value.
  const update = (patch: Partial<Draft>) => {
    setDraft((current) => ({ ...current, ...patch }));
    if (patch.requestId !== undefined) setIdError(null);
  };

  const toggleLevel = (level: string) =>
    setDraft((current) => ({
      ...current,
      levels: current.levels.includes(level)
        ? current.levels.filter((each) => each !== level)
        : [...current.levels, level],
    }));

  const isRequestView = draft.requestId.trim() !== "";
  const levels = data?.levels ?? ["error", "warn", "info", "debug", "trace"];
  const filtered = useMemo(() => data?.entries ?? [], [data]);

  const clear = (
    <button
      type="button"
      data-logs-clear
      onClick={() => {
        setDraft(EMPTY_DRAFT);
        setIdError(null);
      }}
      className="inline-flex items-center gap-1.5 rounded border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-quiet-soft"
    >
      <X className="h-3.5 w-3.5" aria-hidden="true" />
      Clear filters
    </button>
  );

  return (
    <div className="space-y-4" data-view="observability-logs">
      <section className="space-y-3 rounded-lg border border-line p-4">
        <div className="flex flex-wrap items-center gap-2">
          <div className="relative min-w-0 flex-1">
            <Search
              className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted"
              aria-hidden="true"
            />
            <input
              ref={searchRef}
              data-logs-text
              value={draft.text}
              onChange={(event) => update({ text: event.target.value })}
              placeholder="Search messages — press / to focus"
              aria-label="Search log messages"
              className="w-full rounded border border-line bg-panel py-1.5 pl-8 pr-2.5 text-[13px]"
            />
          </div>

          <label className="flex items-center gap-1.5 text-[12.5px] text-muted">
            <span className="sr-only sm:not-sr-only">Request id</span>
            <input
              data-logs-request-id
              value={draft.requestId}
              onChange={(event) => update({ requestId: event.target.value })}
              placeholder="Request id"
              aria-label="Filter by request id"
              aria-invalid={idError !== null}
              className="w-44 rounded border border-line bg-panel px-2.5 py-1.5 font-mono text-[12.5px]"
            />
          </label>

          <label className="flex items-center gap-1.5 text-[12.5px] text-muted">
            <span className="sr-only sm:not-sr-only">Trace id</span>
            <input
              data-logs-trace-id
              value={draft.traceId}
              onChange={(event) => update({ traceId: event.target.value })}
              placeholder="Trace id"
              aria-label="Filter by trace id"
              className="w-40 rounded border border-line bg-panel px-2.5 py-1.5 font-mono text-[12.5px]"
            />
          </label>

          <select
            data-logs-target
            value={draft.target}
            onChange={(event) => update({ target: event.target.value })}
            aria-label="Filter by module"
            className="rounded border border-line bg-panel px-2 py-1.5 text-[12.5px]"
          >
            <option value="">Every module</option>
            {(data?.targets ?? []).map((target) => (
              <option key={target} value={target}>
                {target}
              </option>
            ))}
          </select>

          <select
            data-logs-source
            value={draft.source}
            onChange={(event) => update({ source: event.target.value })}
            aria-label="Filter by process"
            className="rounded border border-line bg-panel px-2 py-1.5 text-[12.5px]"
          >
            <option value="">Every process</option>
            <option value="api">api</option>
            <option value="worker">worker</option>
            <option value="cli">cli</option>
          </select>

          <button
            type="button"
            data-logs-refresh
            onClick={() => void load()}
            disabled={loading}
            className="inline-flex items-center gap-1.5 rounded border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-quiet-soft disabled:opacity-60"
          >
            {loading ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
            ) : (
              <RefreshCw className="h-3.5 w-3.5" aria-hidden="true" />
            )}
            Refresh
          </button>
          {clear}
        </div>

        <div className="flex flex-wrap items-center gap-1.5" role="group" aria-label="Filter by level">
          {levels.map((level) => {
            const on = draft.levels.includes(level);
            return (
              <button
                key={level}
                type="button"
                data-logs-level={level}
                aria-pressed={on}
                onClick={() => toggleLevel(level)}
                className={`rounded px-2 py-1 text-[12px] ${
                  on ? LEVEL_TONE[level] ?? "bg-accent-soft text-accent-strong" : "bg-quiet-soft text-muted"
                }`}
              >
                {level}
              </button>
            );
          })}
        </div>

        {idError ? (
          <p data-logs-request-error className="flex items-start gap-1.5 text-[12.5px] text-danger">
            <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden="true" />
            {idError}
          </p>
        ) : null}

        {isRequestView && requestIdValid ? (
          <p className="flex items-center gap-1.5 text-[12.5px] text-muted">
            <Waypoints className="h-3.5 w-3.5" aria-hidden="true" />
            Showing one request&apos;s lines, oldest first, across the API and the workers.
          </p>
        ) : null}

        {settings ? (
          <p className="text-[12px] text-muted">
            This store keeps {settings.logs_retention_days} days (up to {settings.max_retention_days}).
            Anything older belongs to the operator&apos;s own log backend.
          </p>
        ) : null}
      </section>

      {error ? (
        <p data-logs-error className="rounded-lg border border-danger/40 bg-danger-soft px-4 py-3 text-[13px] text-danger">
          {error}
        </p>
      ) : null}

      <section className="overflow-hidden rounded-lg border border-line">
        {loading && !data ? (
          <LoadingTable columns={4} />
        ) : isRequestView && !requestIdValid ? null : filtered.length === 0 ? (
          <EmptyState
            title={
              (data?.stored_total ?? 0) === 0
                ? "No telemetry yet — the store is empty"
                : "No line matches these filters"
            }
            hint={
              (data?.stored_total ?? 0) === 0
                ? `The log store holds no lines at all. Once a request is served — or an exporter is pointed somewhere — lines appear here. Kept for ${data?.max_window_days ?? 14} days.`
                : `The store holds ${count(data?.stored_total ?? 0)} lines; none of them match this filter. Widen the level, drop the module filter, or clear everything and start again.`
            }
            action={clear}
          />
        ) : (
          <>
            <ul>
              {filtered.map((entry) => (
                <LogRow
                  key={entry.id}
                  entry={entry}
                  expanded={expanded === entry.id}
                  onToggle={() => setExpanded((current) => (current === entry.id ? null : entry.id))}
                />
              ))}
            </ul>
            <p className="border-t border-line px-4 py-2.5 text-[12px] text-muted">
              {count(filtered.length)} of {count(data?.stored_total ?? 0)} stored lines
              {data ? ` · this search is capped at ${count(data.max_rows)} rows` : ""}
            </p>
          </>
        )}
      </section>
    </div>
  );
}
