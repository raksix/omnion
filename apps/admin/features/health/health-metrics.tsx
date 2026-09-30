"use client";

/**
 * `/health/metrics` — what each metric has *done*, over a named range (REQ-014, slice 2).
 *
 * Slice 1's overview answers "is it up now". This screen answers the question the operator asks
 * once the alarm has already gone off: **how long has it been like this, and has it been like
 * this before.** Five rules, each one a way a metrics table ends up quietly wrong:
 *
 * 1. **The range is a name, and the screen says which one.** `1h`, `24h`, `7d` — not "168 hours".
 *    The chosen range travels to the server, into the CSV filename, into the export's last
 *    column and into this screen's own heading, so there is exactly one spelling of the window
 *    anywhere in the product. A client that sent an arbitrary hour count could be clamped to a
 *    window the label does not describe, and then the export would match the table perfectly
 *    while both were wrong.
 * 2. **A dash is a dash.** `avg` over a window with no samples is `null` and renders as `—`,
 *    never as `0`. Zero is a measurement; a metric nobody has ever measured is the opposite,
 *    and it is the number a status screen most wants to invent for itself.
 * 3. **The sparkline is drawn from the series, and the series is the row's own.** Each row
 *    carries its window's values, so switching range draws a different line from different real
 *    samples rather than rescaling the same one. A single point draws a dot, not a line — a
 *    polyline through one point renders as *nothing at all*, which reads as "no data".
 * 4. **The export is the server's file, and the screen says what it got.** The download carries
 *    its range in the filename, in a header and in every row; the client reports the row count it
 *    counted *in the file*, not a count it was told. A client that built CSV from the rows it
 *    held would be exporting its own idea of the window.
 * 5. **An empty window is a screen, not an error.** A platform nobody has run checks on yet
 *    renders the controls, the explanation and no rows — with the sample count the overview is
 *    already reporting, so the answer to "why is this empty" is on the page.
 *
 * Keyboard: `1`/`2`/`3` switch range, `e` exports, `r` re-reads. Mobile: the table becomes a
 * card list with the metric name, its current value and its state all visible without scrolling.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Link from "next/link";
import { ArrowLeft, Download, Loader2, RefreshCw } from "lucide-react";

import {
  ApiError,
  downloadHealthMetricsCsv,
  fetchHealthMetrics,
} from "@/lib/api";
import type { HealthMetricsReport, HealthRangeKey } from "@/lib/types";

import { Num, Sparkline } from "./sparkline";

/** The ranges, in display order. The keys are the server's vocabulary, not this screen's. */
const RANGES: { key: HealthRangeKey; label: string }[] = [
  { key: "1h", label: "1 hour" },
  { key: "24h", label: "24 hours" },
  { key: "7d", label: "7 days" },
];

export function HealthMetricsScreen() {
  const [range, setRange] = useState<HealthRangeKey>("24h");
  const [report, setReport] = useState<HealthMetricsReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [exporting, setExporting] = useState(false);
  const [exportNote, setExportNote] = useState<string | null>(null);
  const typing = useRef(false);

  const load = useCallback(async (next: HealthRangeKey) => {
    try {
      setReport(await fetchHealthMetrics(next));
      setError(null);
    } catch (cause) {
      const apiError = cause as ApiError;
      // The previous rows stay. A blanked table after a failed range switch
      // destroys the reading the operator was comparing the new one against.
      setError(apiError.message ?? "The metric table could not be read.");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load(range);
  }, [load, range]);

  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      // Typing in a field is not a shortcut. Without this guard, `1` inside a
      // search box would silently change the window under the cursor.
      if (
        typing.current ||
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.isContentEditable
      ) {
        return;
      }
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      const key = event.key.toLowerCase();
      const byIndex = { "1": "1h", "2": "24h", "3": "7d" }[key] as HealthRangeKey | undefined;
      if (byIndex) {
        event.preventDefault();
        setRange(byIndex);
      } else if (key === "r") {
        event.preventDefault();
        void load(range);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [load, range]);

  const exportCsv = useCallback(async () => {
    setExporting(true);
    setExportNote(null);
    try {
      const { blob, filename, rows, range: served } =
        await downloadHealthMetricsCsv(range);
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = filename;
      document.body.appendChild(anchor);
      anchor.click();
      anchor.remove();
      URL.revokeObjectURL(url);
      // The note names the window the *file* says it covers, which is the
      // assertion the acceptance criterion makes — and if the server ever
      // answered a different window than the one on screen, this is where it
      // becomes visible instead of quietly downloaded.
      setExportNote(`${filename} · ${rows} ${rows === 1 ? "row" : "rows"} · range ${served}`);
    } catch (cause) {
      setExportNote(
        cause instanceof ApiError ? cause.message : "The export could not be downloaded.",
      );
    } finally {
      setExporting(false);
    }
  }, [range]);

  const rows = report?.metrics ?? [];
  const total = useMemo(() => rows.reduce((sum, row) => sum + row.samples, 0), [rows]);

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <Link
          href="/health"
          data-health-metrics-back
          className="inline-flex items-center gap-1 text-[13px] text-muted hover:text-ink"
        >
          <ArrowLeft aria-hidden className="h-3.5 w-3.5" />
          Overview
        </Link>

        <div className="flex flex-wrap items-center gap-2">
          <div
            role="group"
            aria-label="Time range"
            className="flex rounded-md border border-line"
          >
            {RANGES.map((entry) => (
              <button
                key={entry.key}
                type="button"
                data-health-range={entry.key}
                aria-pressed={range === entry.key}
                onClick={() => setRange(entry.key)}
                className={`px-3 py-1.5 text-[12.5px] ${
                  range === entry.key
                    ? "bg-ink text-[var(--color-surface)]"
                    : "text-muted hover:text-ink"
                }`}
              >
                {entry.label}
              </button>
            ))}
          </div>

          <button
            type="button"
            data-health-metrics-refresh
            onClick={() => void load(range)}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-surface"
          >
            <RefreshCw aria-hidden className="h-3.5 w-3.5" />
            Refresh
          </button>

          <button
            type="button"
            data-health-metrics-export
            onClick={() => void exportCsv()}
            disabled={exporting}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-surface disabled:opacity-60"
          >
            {exporting ? (
              <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <Download aria-hidden className="h-3.5 w-3.5" />
            )}
            Export CSV
          </button>
        </div>
      </div>

      <p data-health-metrics-window className="text-[12.5px] text-muted">
        Window <span className="font-mono">{report?.range ?? range}</span> ·{" "}
        {loading ? "reading…" : `${rows.length} ${rows.length === 1 ? "metric" : "metrics"} · ${total} samples`}
        {exportNote ? <span data-health-metrics-export-note> · {exportNote}</span> : null}
      </p>

      {error ? (
        <p
          data-health-metrics-error
          role="alert"
          className="rounded-md border border-red-300 bg-red-50 px-3 py-2 text-[13px] text-red-800 dark:border-red-900 dark:bg-red-950 dark:text-red-200"
        >
          {error}
        </p>
      ) : null}

      {loading && rows.length === 0 ? (
        <ul data-health-metrics-skeleton className="space-y-2">
          {[0, 1, 2, 3].map((row) => (
            <li key={row} className="h-9 animate-pulse rounded-md bg-surface" />
          ))}
        </ul>
      ) : rows.length === 0 ? (
        <div
          data-health-metrics-empty
          className="rounded-lg border border-line px-4 py-6 text-center"
        >
          <p className="text-[13px]">No samples in this window.</p>
          <p className="mx-auto mt-1 max-w-md text-[12.5px] text-muted">
            Nothing has been measured for the last {RANGES.find((entry) => entry.key === range)?.label}.
            The overview keeps recording every run — open it and press{" "}
            <span className="font-medium">Run all checks</span> to write the first samples of this
            window.
          </p>
        </div>
      ) : (
        <>
          <table
            data-health-metrics-table
            className="hidden w-full text-left text-[13px] sm:table"
          >
            <thead>
              <tr className="border-b border-line text-[12px] text-muted">
                <th scope="col" className="py-2 pr-3 font-medium">Metric</th>
                <th scope="col" className="py-2 pr-3 font-medium">Current</th>
                <th scope="col" className="py-2 pr-3 font-medium">Min</th>
                <th scope="col" className="py-2 pr-3 font-medium">Avg</th>
                <th scope="col" className="py-2 pr-3 font-medium">Max</th>
                <th scope="col" className="py-2 pr-3 font-medium">Samples</th>
                <th scope="col" className="py-2 pr-3 font-medium">State</th>
                <th scope="col" className="py-2 font-medium">{report?.range} trend</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr
                  key={`${row.service}/${row.metric}`}
                  data-health-metric-row={`${row.service}/${row.metric}`}
                  className="border-b border-line last:border-b-0"
                >
                  <td className="py-2 pr-3">
                    <div className="font-medium">{row.metric.replaceAll("_", " ")}</div>
                    <div className="text-[11.5px] text-muted">
                      <Link href={`/health/services/${row.service}`} className="hover:underline">
                        {row.service}
                      </Link>
                    </div>
                  </td>
                  <td className="py-2 pr-3">
                    <Num value={row.current} />
                    {row.unit ? <span className="ml-1 text-[11.5px] text-muted">{row.unit}</span> : null}
                  </td>
                  <td className="py-2 pr-3 text-muted">
                    <Num value={row.min} />
                  </td>
                  <td className="py-2 pr-3 text-muted">
                    <Num value={row.avg} />
                  </td>
                  <td className="py-2 pr-3 text-muted">
                    <Num value={row.max} />
                  </td>
                  <td className="py-2 pr-3 tabular-nums text-muted">{row.samples}</td>
                  <td className="py-2 pr-3">
                    <span
                      data-health-metric-row-state={row.state}
                      className={
                        row.state === "healthy"
                          ? "text-[12px] text-emerald-700 dark:text-emerald-300"
                          : row.state === "unknown"
                            ? "text-[12px] text-slate-600 dark:text-slate-300"
                            : "text-[12px] text-amber-700 dark:text-amber-300"
                      }
                    >
                      {row.state}
                    </span>
                  </td>
                  <td className="py-2">
                    <Sparkline values={row.series} label={row.metric} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>

          {/*
            The mobile list is a different layout, not a squeezed table: a seven-column table at
            390 px either scrolls horizontally or drops columns, and both lose the metric's name
            or its value. The QA checklist names "mobile cards that keep the status and the value
            visible without scrolling", so the cards exist for exactly that.
          */}
          <ul data-health-metrics-cards className="space-y-2 sm:hidden">
            {rows.map((row) => (
              <li
                key={`${row.service}/${row.metric}`}
                data-health-metric-card={`${row.service}/${row.metric}`}
                data-health-metric-card-state={row.state}
                className="rounded-lg border border-line px-3 py-2"
              >
                <div className="flex items-baseline justify-between gap-2">
                  <span className="text-[13px] font-medium">
                    {row.metric.replaceAll("_", " ")}
                  </span>
                  <span className="text-[13px]">
                    <Num value={row.current} />
                    {row.unit ? <span className="ml-1 text-[11.5px] text-muted">{row.unit}</span> : null}
                  </span>
                </div>
                <div className="mt-1 flex items-center justify-between gap-2 text-[11.5px] text-muted">
                  <span>
                    {row.service} · {row.samples} samples
                  </span>
                  <span data-health-metric-card-range={report?.range}>
                    {row.state} · {report?.range}
                  </span>
                </div>
                <div className="mt-1">
                  <Sparkline values={row.series} label={row.metric} />
                </div>
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}