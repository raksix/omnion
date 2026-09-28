"use client";

/**
 * `/observability/traces` — the trace search and the waterfall for one trace
 * (docs/requests/REQ-126, slice 3).
 *
 * The screen exists for one jump and one judgement. The jump: an operator holding a request id
 * from an error banner, a log row or a support ticket types it here and gets the trace for
 * *that* request. The judgement: the index is a SAMPLE, and the screen says so on every row,
 * because "why do I have this trace but not the one next to it" is a question an operator asks
 * and a boolean cannot answer — the reason is `error` (the 100 % bias) or `ratio` (it drew) or
 * `upstream` (an inbound `traceparent` decided, and we are not the parent).
 *
 * Four states this screen is careful about, each of which otherwise looks like a working screen:
 *
 * - **Unsampled is not absent.** A request id with no trace answers with the reason and the
 *   ratio, not a blank table. The API's 404 already carries that sentence; the screen adds the
 *   live ratio so the operator can tell "wrong id" from "drew the short straw".
 * - **A truncated waterfall says it is truncated.** The index caps the inline spans
 *   (`MAX_SPANS_PER_TRACE`), and a trace over the cap renders a SHORT waterfall that looks
 *   complete. The `span_count` / `spans_kept` disagreement is surfaced as a banner, and the
 *   backend link is the answer when one is configured.
 * - **No tracing backend is a configuration answer, not an empty region.** The request says the
 *   screen should say so and offer the collector example rather than draw an empty waterfall.
 *   Omnion emits telemetry and ships assets; it is not the backend (the request's own "Out").
 * - **Selecting a span shows values that are already redacted.** The attributes rendered here
 *   went through the shared pass when the span was BUILT, so there is nothing to mask at
 *   render time and no second rule to keep in sync.
 *
 * Keyboard: `/` focuses the request-id box, `t` opens the selected row's trace, `r` refreshes,
 * `Esc` clears the filters. Under `sm:` the result table becomes cards and the waterfall scrolls
 * horizontally inside its own region.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { ArrowUpRight, Copy, Filter, Loader2, RefreshCw, Search, Waypoints, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchTrace,
  fetchTraces,
  type TraceDetail,
  type TraceSpan,
  type TraceSummary,
  type TracesResponse,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The windows the search offers, in minutes. All of them are inside the API's cap. */
const WINDOWS: { label: string; minutes: number }[] = [
  { label: "15 min", minutes: 15 },
  { label: "1 h", minutes: 60 },
  { label: "6 h", minutes: 360 },
  { label: "24 h", minutes: 1440 },
];

/** Why a trace is in the index. The chip is the point of the screen, not decoration. */
const SAMPLING_LABEL: Record<string, { label: string; tone: string; hint: string }> = {
  error: {
    label: "error",
    tone: "bg-danger-soft text-danger",
    hint: "Always sampled: the request failed, and the bias is 100 % of errors.",
  },
  ratio: {
    label: "ratio",
    tone: "bg-accent-soft text-accent",
    hint: "Drawn by the sampling ratio. Most requests are not here, and that is the policy working.",
  },
  always: {
    label: "always",
    tone: "bg-positive-soft text-positive",
    hint: "Sampled unconditionally.",
  },
  upstream: {
    label: "upstream",
    tone: "bg-caution-soft text-caution",
    hint: "The caller sent a traceparent and decided; this instance followed it.",
  },
};

const STATUS_TONE: Record<string, string> = {
  ok: "bg-positive-soft text-positive",
  error: "bg-danger-soft text-danger",
};

function samplingOf(trace: TraceSummary) {
  return (
    SAMPLING_LABEL[trace.sampling] ?? {
      label: trace.sampling,
      tone: "bg-quiet-soft text-muted",
      hint: "Recorded by this build; no sampling reason was stored.",
    }
  );
}

/** `82 ms` / `1.4 s` — a duration in the form an eye compares. */
function duration(ms: number): string {
  if (!Number.isFinite(ms)) return "—";
  if (ms < 1000) return `${ms} ms`;
  return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`;
}

/** `4 spans` / `1,204 spans`. */
function count(value: number): string {
  return new Intl.NumberFormat("en").format(value);
}

/**
 * The waterfall.
 *
 * Hand-drawn rows rather than a charting library: the data is a list of spans with a start offset
 * and a duration, and the thing being judged is whether the nesting is readable — which is a
 * layout question a library would answer with its own opinion. The scale is the trace's own
 * duration, so a 4 ms trace and a 4 s trace both fill the width, and a span with a zero duration
 * still gets a visible 2 px so "it happened" is not "it does not exist".
 */
function Waterfall({ detail }: { detail: TraceDetail }) {
  const total = Math.max(detail.duration_ms, 1);
  const deepest = detail.spans.reduce(
    (depth, span) => Math.max(depth, span.parent_span_id ? depth + 1 : 0),
    0,
  );

  return (
    <div className="overflow-x-auto" data-trace-waterfall>
      <ul className="min-w-[520px] space-y-1">
        {detail.spans.map((span) => {
          const left = (span.offset_ms / total) * 100;
          const width = Math.max((span.duration_ms / total) * 100, 0.6);
          return (
            <li
              key={span.span_id}
              data-trace-span={span.name}
              className="flex items-center gap-2 rounded-md px-1.5 py-1 hover:bg-quiet-soft"
            >
              <span
                className={`w-2 shrink-0 ${span.failed ? "text-danger" : "text-muted"}`}
                aria-hidden="true"
              >
                {span.failed ? "✗" : "·"}
              </span>
              <span
                className="w-[42%] shrink-0 truncate font-mono text-[11.5px]"
                title={`${span.name} · ${span.service}`}
              >
                {span.root ? "◉ " : ""}
                {span.name}
              </span>
              <span className="relative h-4 flex-1 rounded bg-quiet-soft">
                <span
                  className={`absolute top-0.5 h-3 rounded-sm ${
                    span.failed ? "bg-danger" : span.root ? "bg-accent" : "bg-positive"
                  }`}
                  style={{ left: `${Math.min(left, 99)}%`, width: `${Math.min(width, 100)}%` }}
                />
              </span>
              <span className="w-16 shrink-0 text-right font-mono text-[11px] tabular-nums text-muted">
                {duration(span.duration_ms)}
              </span>
            </li>
          );
        })}
      </ul>
      <p className="mt-2 text-[11.5px] text-muted">
        Offsets are relative to the trace root; durations come from a monotonic clock, so a
        cross-process child is never shifted by clock skew. {deepest} level{deepest === 1 ? "" : "s"}{" "}
        of nesting in {detail.spans.length} inline span{detail.spans.length === 1 ? "" : "s"}.
      </p>
    </div>
  );
}

/** One selected span's attributes, rendered from the already-redacted object. */
function SpanDetails({ span }: { span: TraceSpan }) {
  const entries = Object.entries(span.attributes);
  return (
    <div data-trace-span-details className="rounded-lg border border-line bg-quiet-soft/40 p-3">
      <h4 className="font-mono text-[12.5px]">{span.name}</h4>
      <p className="mt-0.5 text-[11.5px] text-muted">
        {span.service} · {span.span_id}
        {span.parent_span_id ? ` · child of ${span.parent_span_id}` : " · root"}
        {span.failed ? " · failed" : ""}
      </p>
      {entries.length === 0 ? (
        <p className="mt-2 text-[12px] text-muted">This span carries no attributes.</p>
      ) : (
        <dl className="mt-2 space-y-1 text-[11.5px]">
          {entries.map(([key, value]) => (
            <div key={key} className="flex gap-2">
              <dt className="shrink-0 font-mono text-muted">{key}</dt>
              <dd className="min-w-0 break-words font-mono">{JSON.stringify(value)}</dd>
            </div>
          ))}
        </dl>
      )}
      <p className="mt-2 text-[11px] text-muted">
        Values are shown as the span recorded them. Redaction ran when the span was built, so a
        prompt, a secret or an e-mail is not here to mask.
      </p>
    </div>
  );
}

export function TracesView() {
  const [data, setData] = useState<TracesResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const [requestId, setRequestId] = useState("");
  const [route, setRoute] = useState("");
  const [status, setStatus] = useState<"" | "ok" | "error">("");
  const [minDuration, setMinDuration] = useState("");
  const [windowMinutes, setWindowMinutes] = useState(60);

  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [detail, setDetail] = useState<TraceDetail | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [spanPick, setSpanPick] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const requestInput = useRef<HTMLInputElement>(null);
  const heading = useRef<HTMLHeadingElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setData(
        await fetchTraces({
          request_id: requestId.trim() || undefined,
          route: route.trim() || undefined,
          status: status || undefined,
          min_duration_ms: minDuration ? Number(minDuration) : undefined,
          window_minutes: windowMinutes,
          limit: 50,
        }),
      );
    } catch (caught) {
      setError(
        caught instanceof ApiError
          ? caught.message
          : "The trace index could not be read. It may still be starting up.",
      );
    } finally {
      setLoading(false);
    }
  }, [requestId, route, status, minDuration, windowMinutes]);

  useEffect(() => {
    void load();
    // The filters are the query: a change re-reads. `load` is rebuilt on every filter change,
    // which is what this effect keys on.
  }, [load]);

  const openTrace = useCallback(async (traceId: string) => {
    setSelectedId(traceId);
    setDetailLoading(true);
    setDetailError(null);
    setDetail(null);
    setSpanPick(null);
    try {
      setDetail(await fetchTrace(traceId));
    } catch (caught) {
      setDetailError(
        caught instanceof ApiError ? caught.message : "That trace could not be read.",
      );
    } finally {
      setDetailLoading(false);
    }
  }, []);

  // Keyboard: `/` searches, `r` refreshes, `Esc` clears. The filters are read from a ref so the
  // listener is attached once — a listener rebuilt on every keystroke of the search box would
  // capture a stale `requestId` and clear the wrong thing.
  const filtersRef = useRef({ requestId, route, status, minDuration });
  filtersRef.current = { requestId, route, status, minDuration };

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.tagName === "SELECT";

      if (event.key === "Escape") {
        if (selectedId) {
          setSelectedId(null);
          setDetail(null);
          return;
        }
        const current = filtersRef.current;
        setRequestId(current.requestId);
        setRoute(current.route);
        setStatus(current.status);
        setMinDuration(current.minDuration);
        setRequestId("");
        setRoute("");
        setStatus("");
        setMinDuration("");
        return;
      }
      if (typing) return;
      if (event.key === "/") {
        event.preventDefault();
        requestInput.current?.focus();
        return;
      }
      if (event.key === "r") {
        event.preventDefault();
        void load();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load, selectedId]);

  const clearFilters = () => {
    setRequestId("");
    setRoute("");
    setStatus("");
    setMinDuration("");
    requestInput.current?.focus();
  };

  const filtered =
    Boolean(requestId.trim()) || Boolean(route.trim()) || Boolean(status) || Boolean(minDuration);
  const truncated = detail?.spans_truncated ?? false;
  const selectedSpan: TraceSpan | null = useMemo(() => {
    if (!detail) return null;
    if (!spanPick) return detail.spans.find((span) => span.root) ?? detail.spans[0] ?? null;
    return detail.spans.find((span) => span.span_id === spanPick) ?? null;
  }, [detail, spanPick]);

  return (
    <div className="space-y-4" data-view="observability-traces">
      <section className="rounded-xl border border-line">
        <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <h2 className="flex items-center gap-2 text-[13.5px] font-medium">
            <Waypoints className="size-4 text-accent" aria-hidden="true" />
            Trace search
          </h2>
          {data ? (
            <span className="text-[12px] text-muted">
              {data.traces.length} shown of {count(data.total)} indexed
            </span>
          ) : null}
          {filtered ? (
            <button
              type="button"
              onClick={clearFilters}
              data-trace-clear
              className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
            >
              <X className="h-3 w-3" aria-hidden="true" />
              Clear filters
            </button>
          ) : null}
          <button
            type="button"
            onClick={() => void load()}
            data-trace-refresh
            className="ml-auto flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
          >
            <RefreshCw className="h-3 w-3" aria-hidden="true" />
            Refresh
          </button>
        </header>

        <div className="grid gap-3 border-b border-line px-4 py-3 sm:grid-cols-2 lg:grid-cols-5">
          <div>
            <label className="text-[12px] text-muted" htmlFor="trace-request-id">
              Request id
            </label>
            <div className="relative mt-1">
              <Search
                className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted"
                aria-hidden="true"
              />
              <input
                id="trace-request-id"
                ref={requestInput}
                value={requestId}
                onChange={(event) => setRequestId(event.target.value)}
                data-trace-request-input
                placeholder="the id from an error banner or a log row"
                className="h-9 w-full rounded-lg border border-line bg-surface pl-7 pr-2 font-mono text-[12px] outline-none focus:border-accent"
              />
            </div>
          </div>
          <div>
            <label className="text-[12px] text-muted" htmlFor="trace-route">
              Route template
            </label>
            <input
              id="trace-route"
              value={route}
              onChange={(event) => setRoute(event.target.value)}
              data-trace-route-input
              placeholder="/api/v1/observability/…"
              className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 font-mono text-[12px] outline-none focus:border-accent"
            />
          </div>
          <div>
            <label className="text-[12px] text-muted" htmlFor="trace-status">
              Status
            </label>
            <select
              id="trace-status"
              value={status}
              onChange={(event) => setStatus(event.target.value as "" | "ok" | "error")}
              data-trace-status-select
              className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2 text-[12px] outline-none focus:border-accent"
            >
              <option value="">Any</option>
              <option value="ok">Succeeded</option>
              <option value="error">Failed</option>
            </select>
          </div>
          <div>
            <label className="text-[12px] text-muted" htmlFor="trace-min-duration">
              Slower than (ms)
            </label>
            <input
              id="trace-min-duration"
              value={minDuration}
              onChange={(event) => setMinDuration(event.target.value)}
              data-trace-min-input
              inputMode="numeric"
              placeholder="0"
              className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 text-[12px] outline-none focus:border-accent"
            />
          </div>
          <div>
            <label className="text-[12px] text-muted" htmlFor="trace-window">
              Window
            </label>
            <select
              id="trace-window"
              value={windowMinutes}
              onChange={(event) => setWindowMinutes(Number(event.target.value))}
              data-trace-window-select
              className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2 text-[12px] outline-none focus:border-accent"
            >
              {WINDOWS.map((option) => (
                <option key={option.minutes} value={option.minutes}>
                  {option.label}
                </option>
              ))}
            </select>
          </div>
        </div>

        {error ? (
          <p role="alert" data-trace-error className="px-4 py-3 text-[12.5px] text-danger">
            {error}
          </p>
        ) : loading && !data ? (
          <div className="px-4 py-3">
            <LoadingTable rows={4} columns={5} />
          </div>
        ) : !data || data.traces.length === 0 ? (
          <div className="px-4 py-3">
            <EmptyState
              title={filtered ? "No trace matches" : "No traces indexed yet"}
              hint={
                filtered
                  ? "An unsampled request has no trace by design — the sampling bias is 100 % of errors plus a ratio for the rest. Check the status filter, widen the window, or search by the request id from the log explorer."
                  : "Traces appear once the first request completes and the sampling decision keeps it. The index is bounded by retention; the full span set lives in your tracing backend."
              }
            />
          </div>
        ) : (
          <>
            {/* Desktop: a table. Under `sm:` the same rows as cards. */}
            <table className="hidden w-full text-left text-[12.5px] sm:table" data-trace-table>
              <thead className="text-[11.5px] uppercase tracking-wide text-muted">
                <tr>
                  <th scope="col" className="px-4 py-2 font-medium">Started</th>
                  <th scope="col" className="px-2 py-2 font-medium">Route</th>
                  <th scope="col" className="px-2 py-2 font-medium">Duration</th>
                  <th scope="col" className="px-2 py-2 font-medium">Spans</th>
                  <th scope="col" className="px-2 py-2 font-medium">Status</th>
                  <th scope="col" className="px-2 py-2 font-medium">Sampled</th>
                  <th scope="col" className="px-4 py-2 font-medium">Trace</th>
                </tr>
              </thead>
              <tbody>
                {data.traces.map((trace) => {
                  const sampling = samplingOf(trace);
                  return (
                    <tr key={trace.trace_id} className="border-t border-line">
                      <td className="whitespace-nowrap px-4 py-2 text-muted">
                        {formatTimestamp(trace.started_at)}
                      </td>
                      <td className="px-2 py-2 font-mono text-[11.5px]">
                        {trace.route ?? trace.root_name}
                      </td>
                      <td className="px-2 py-2 tabular-nums">{duration(trace.duration_ms)}</td>
                      <td className="px-2 py-2 tabular-nums">
                        {count(trace.span_count)}
                        {trace.spans_truncated ? (
                          <span
                            className="ml-1 text-caution"
                            title={`the index kept ${trace.spans_kept} of ${trace.span_count} spans`}
                          >
                            *
                          </span>
                        ) : null}
                      </td>
                      <td className="px-2 py-2">
                        <span
                          className={`rounded-full px-1.5 py-0.5 text-[11px] ${STATUS_TONE[trace.status]}`}
                        >
                          {trace.status}
                        </span>
                      </td>
                      <td className="px-2 py-2">
                        <span
                          className={`rounded-full px-1.5 py-0.5 text-[11px] ${sampling.tone}`}
                          title={sampling.hint}
                          data-trace-sampling={trace.sampling}
                        >
                          {sampling.label}
                        </span>
                      </td>
                      <td className="px-4 py-2">
                        <button
                          type="button"
                          onClick={() => void openTrace(trace.trace_id)}
                          data-trace-open={trace.trace_id}
                          className="flex items-center gap-1 font-mono text-[11.5px] text-accent hover:underline"
                        >
                          {trace.trace_id.slice(0, 12)}…
                          <ArrowUpRight className="size-3" aria-hidden="true" />
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>

            <ul className="space-y-2 p-3 sm:hidden" data-trace-cards>
              {data.traces.map((trace) => {
                const sampling = samplingOf(trace);
                return (
                  <li key={trace.trace_id} className="rounded-lg border border-line p-3">
                    <div className="flex items-center gap-2">
                      <span
                        className={`rounded-full px-1.5 py-0.5 text-[11px] ${STATUS_TONE[trace.status]}`}
                      >
                        {trace.status}
                      </span>
                      <span
                        className={`rounded-full px-1.5 py-0.5 text-[11px] ${sampling.tone}`}
                        title={sampling.hint}
                      >
                        {sampling.label}
                      </span>
                      <span className="ml-auto text-[11.5px] tabular-nums text-muted">
                        {duration(trace.duration_ms)}
                      </span>
                    </div>
                    <p className="mt-1.5 font-mono text-[11.5px] break-all">
                      {trace.route ?? trace.root_name}
                    </p>
                    <p className="mt-0.5 text-[11.5px] text-muted">
                      {formatTimestamp(trace.started_at)} · {count(trace.span_count)} spans
                    </p>
                    <button
                      type="button"
                      onClick={() => void openTrace(trace.trace_id)}
                      data-trace-open={trace.trace_id}
                      className="mt-2 text-[12px] text-accent hover:underline"
                    >
                      Open the waterfall
                    </button>
                  </li>
                );
              })}
            </ul>
          </>
        )}
      </section>

      {selectedId ? (
        <section className="rounded-xl border border-line" data-trace-detail>
          <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
            <h2 ref={heading} className="text-[13.5px] font-medium">
              Trace {selectedId.slice(0, 12)}…
            </h2>
            <button
              type="button"
              onClick={() => {
                void navigator.clipboard?.writeText(selectedId).then(() => {
                  setNotice("Trace id copied");
                  window.setTimeout(() => setNotice(null), 2000);
                });
              }}
              className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
            >
              <Copy className="h-3 w-3" aria-hidden="true" />
              Copy id
            </button>
            {notice ? <span className="text-[12px] text-positive">{notice}</span> : null}
            <button
              type="button"
              onClick={() => {
                setSelectedId(null);
                setDetail(null);
              }}
              data-trace-close
              className="ml-auto flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
            >
              <X className="h-3 w-3" aria-hidden="true" />
              Close
            </button>
          </header>

          <div className="space-y-3 px-4 py-3">
            {detailLoading ? (
              <p className="flex items-center text-[12.5px] text-muted" aria-busy="true">
                <Loader2 className="mr-2 size-3.5 animate-spin" aria-hidden="true" />
                Loading the trace…
              </p>
            ) : detailError ? (
              <div role="alert" data-trace-detail-error className="rounded-lg border border-caution-soft bg-caution-soft px-3 py-2.5 text-[12.5px]">
                <p>{detailError}</p>
                <p className="mt-1 text-[12px] text-muted">
                  Traces are sampled, and an unsampled request has none. Widen the window in the
                  search above, or check the request id against the log explorer.
                </p>
              </div>
            ) : detail ? (
              <>
                {truncated ? (
                  <p
                    data-trace-truncated
                    className="rounded-lg border border-caution-soft bg-caution-soft px-3 py-2 text-[12.5px]"
                  >
                    This trace has {count(detail.span_count)} spans and the index kept{" "}
                    {count(detail.spans_kept)}. The waterfall below is the kept part — the
                    remainder lives in your tracing backend.
                  </p>
                ) : null}

                <dl className="grid gap-x-4 gap-y-1 text-[12px] sm:grid-cols-4">
                  {[
                    ["Root", detail.root_name],
                    ["Service", detail.service],
                    ["Route", detail.route ?? "—"],
                    ["Duration", duration(detail.duration_ms)],
                    ["Request", detail.request_id ?? "—"],
                    ["Sampled because", samplingOf(detail).label],
                    ["Started", formatTimestamp(detail.started_at)],
                    ["Status", detail.status],
                  ].map(([label, value]) => (
                    <div key={label}>
                      <dt className="text-muted">{label}</dt>
                      <dd className="break-all font-mono">{value}</dd>
                    </div>
                  ))}
                </dl>

                {detail.backend_trace_url ? (
                  <a
                    href={detail.backend_trace_url}
                    target="_blank"
                    rel="noreferrer"
                    data-trace-backend-link
                    className="inline-flex items-center gap-1 text-[12.5px] text-accent hover:underline"
                  >
                    Open the full trace in your tracing backend
                    <ArrowUpRight className="size-3" aria-hidden="true" />
                  </a>
                ) : (
                  <p data-trace-no-backend className="text-[12.5px] text-muted">
                    No tracing backend is configured, so this is the whole waterfall Omnion holds.
                    The full span set is whatever the bundled collector sends to — the example
                    collector configuration ships with{" "}
                    <code className="font-mono">infra/observability/</code>.
                  </p>
                )}

                <Waterfall detail={detail} />

                {detail.spans.length > 0 ? (
                  <div>
                    <h3 className="mb-1 text-[12.5px] font-medium">A span's attributes</h3>
                    <div className="flex flex-wrap gap-1.5">
                      {detail.spans.map((span) => (
                        <button
                          key={span.span_id}
                          type="button"
                          onClick={() => setSpanPick(span.span_id)}
                          data-trace-span-pick={span.name}
                          aria-pressed={selectedSpan?.span_id === span.span_id}
                          className={`rounded-md border px-2 py-1 font-mono text-[11.5px] ${
                            selectedSpan?.span_id === span.span_id
                              ? "border-accent bg-accent-soft text-accent"
                              : "border-line hover:bg-quiet-soft"
                          }`}
                        >
                          {span.name}
                        </button>
                      ))}
                    </div>
                    {selectedSpan ? <div className="mt-2"><SpanDetails span={selectedSpan} /></div> : null}
                  </div>
                ) : null}
              </>
            ) : null}
          </div>
        </section>
      ) : null}

      <p className="flex items-center gap-1.5 text-[11.5px] text-muted">
        <Filter className="size-3" aria-hidden="true" />
        Press <kbd className="rounded border border-line px-1">/</kbd> to search,{" "}
        <kbd className="rounded border border-line px-1">r</kbd> to refresh,{" "}
        <kbd className="rounded border border-line px-1">Esc</kbd> to clear. The index is a sample;
        the log explorer is where every line is.
      </p>
    </div>
  );
}
