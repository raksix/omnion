"use client";

/**
 * The reports screen (REQ-053, slice 4b): `/inventory/reports`.
 *
 * The last screen the module needed, and the only one whose subject is a **number
 * somebody will quote to somebody else**. Three blocks, and each one has a way it could
 * mislead that the screen has to close rather than decorate:
 *
 * * **The value is printed with its incompleteness beside it.** `cost` is nullable and
 *   the module has never invented one, so an amount rendered on its own reads as the
 *   whole warehouse's worth. The note is not a tooltip: it is the sentence that makes the
 *   number true, and `pricingNote` refuses to render "100% of nothing is priced" for an
 *   empty scope.
 * * **A hold is not a loss of value.** The figure is `on_hand × cost`, and the reserved
 *   total is printed *beside* it — held stock is still on the shelf, and a report that
 *   subtracted availability would make confirming an order reduce what the warehouse is
 *   worth.
 * * **The idle block's count and its rows are separate numbers.** The rows are capped;
 *   the count and the quantity are the whole set's. A screen that showed twenty rows
 *   under a heading saying 186 would be reporting the wrong thing beautifully.
 *
 * The export button sends **the same filters the page is showing**, and the file is built
 * server-side from the same report object the JSON returns — so the two cannot disagree.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { AlertTriangle, Download, TrendingDown, Wallet } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  exportReportCsv,
  fetchReport,
  movementKindLabel,
  pricingNote,
  type InventoryReport,
  type ReportFilters,
} from "@/lib/inventory";

import { RelativeTime } from "./inventory-parts";

/** The idle windows the screen offers. A free-text field is not what a picker is for. */
const IDLE_WINDOWS = [30, 60, 90, 180, 365];

/** How many idle rows the screen asks for. The server caps it; this is a preference. */
const IDLE_ROWS = 50;

function daysAgo(days: number): string {
  const date = new Date();
  date.setDate(date.getDate() - days);
  return date.toISOString().slice(0, 10);
}

export function ReportsView() {
  const [report, setReport] = useState<InventoryReport | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [exporting, setExporting] = useState(false);
  // The filters live here rather than in the URL: this screen is a *report*, and a
  // report you can bookmark with its period in the query string is worth having — so
  // they are initialised from the URL and pushed back onto every change.
  const [filters, setFilters] = useState<ReportFilters>(() => {
    const params = new URLSearchParams(
      typeof window === "undefined" ? "" : window.location.search,
    );
    const from = params.get("from");
    const to = params.get("to");
    const idle = params.get("idle_days");
    return {
      from: from ?? daysAgo(29),
      to: to ?? daysAgo(0),
      idle_days: idle ? Number.parseInt(idle, 10) : 30,
      limit: IDLE_ROWS,
    };
  });

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setReport(await fetchReport(filters));
    } catch (failure) {
      setError(toScreenError(failure, "The inventory report could not be built."));
    } finally {
      setLoading(false);
    }
  }, [filters]);

  useEffect(() => {
    void load();
  }, [load]);

  // Keep the address bar in step, so a refresh and a shared link both mean the same
  // report. `replaceState` rather than a navigation: this is not a new page.
  useEffect(() => {
    const params = new URLSearchParams();
    if (filters.from) params.set("from", filters.from);
    if (filters.to) params.set("to", filters.to);
    if (filters.idle_days) params.set("idle_days", String(filters.idle_days));
    const query = params.toString();
    window.history.replaceState(
      null,
      "",
      query ? `/inventory/reports?${query}` : "/inventory/reports",
    );
  }, [filters]);

  const runExport = async () => {
    setExporting(true);
    setError(null);
    try {
      await exportReportCsv(filters);
    } catch (failure) {
      setError(toScreenError(failure, "The report could not be exported."));
    } finally {
      setExporting(false);
    }
  };

  const setWindow = (from: string, to: string) => setFilters((f) => ({ ...f, from, to }));
  const note = report ? pricingNote(report.value) : "";

  return (
    <div className="space-y-6">
      <header className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-xl font-semibold tracking-tight">Reports</h1>
          <p className="text-sm text-muted-foreground">
            What the stock is worth, what moved in the period, and what has not moved at all.
          </p>
        </div>
        <button
          type="button"
          onClick={runExport}
          disabled={exporting || !report}
          data-qa-inventory-report-export
          className="inline-flex h-9 items-center gap-2 rounded-md border border-border px-3 text-sm disabled:opacity-60"
        >
          <Download className="h-4 w-4" aria-hidden />
          {exporting ? "Preparing…" : "Export CSV"}
        </button>
      </header>

      {/* The period and the idle window, as real date and select controls. A report that
          cannot be re-run for last quarter is a screenshot, not a report. */}
      <div className="flex flex-wrap items-end gap-3">
        <label className="flex flex-col gap-1 text-xs text-muted-foreground">
          <span>From</span>
          <input
            type="date"
            value={filters.from ?? ""}
            onChange={(event) => setWindow(event.target.value, filters.to ?? daysAgo(0))}
            data-qa-inventory-report-from
            className="h-8 rounded-md border border-border bg-transparent px-2 text-sm text-foreground"
          />
        </label>
        <label className="flex flex-col gap-1 text-xs text-muted-foreground">
          <span>To</span>
          <input
            type="date"
            value={filters.to ?? ""}
            onChange={(event) => setWindow(filters.from ?? daysAgo(29), event.target.value)}
            data-qa-inventory-report-to
            className="h-8 rounded-md border border-border bg-transparent px-2 text-sm text-foreground"
          />
        </label>
        <label className="flex flex-col gap-1 text-xs text-muted-foreground">
          <span>Idle after</span>
          <select
            value={filters.idle_days ?? 30}
            onChange={(event) =>
              setFilters((f) => ({ ...f, idle_days: Number.parseInt(event.target.value, 10) }))
            }
            data-qa-inventory-report-idle
            className="h-8 rounded-md border border-border bg-transparent px-2 text-sm text-foreground"
          >
            {IDLE_WINDOWS.map((days) => (
              <option key={days} value={days}>
                {days} days
              </option>
            ))}
          </select>
        </label>
        <div className="flex flex-wrap gap-1" role="group" aria-label="Preset periods">
          {[
            { label: "7 days", days: 6 },
            { label: "30 days", days: 29 },
            { label: "90 days", days: 89 },
            { label: "Year", days: 364 },
          ].map((preset) => (
            <button
              key={preset.label}
              type="button"
              onClick={() => setWindow(daysAgo(preset.days), daysAgo(0))}
              data-qa-inventory-report-preset={preset.label}
              className="h-8 rounded-full border border-border px-3 text-sm text-muted-foreground hover:text-foreground"
            >
              {preset.label}
            </button>
          ))}
        </div>
      </div>

      {loading ? (
        <LoadingTable columns={4} rows={4} />
      ) : error ? (
        <ErrorState error={error} onRetry={load} />
      ) : !report ? null : report.scoped_lines === 0 && report.idle.total === 0 ? (
        <EmptyState
          title="Nothing to report on yet"
          hint={
            "This warehouse has no stock rows in the selected scope. Record a receipt, or " +
            "widen the period, and the report will fill in."
          }
          action={
            <Link
              href="/inventory/movements"
              className="inline-flex h-9 items-center rounded-md bg-primary px-3 text-sm text-primary-foreground"
            >
              Record a movement
            </Link>
          }
        />
      ) : (
        <div className="space-y-6">
          {/* ---- the valuation ---- */}
          <section aria-labelledby="inventory-report-value" className="space-y-2">
            <h2
              id="inventory-report-value"
              className="flex items-center gap-2 text-sm font-medium text-muted-foreground"
            >
              <Wallet className="h-4 w-4" aria-hidden />
              Stock value
            </h2>
            <div className="rounded-lg border border-border p-4">
              <p className="text-2xl font-semibold tracking-tight" data-qa-inventory-report-value>
                {report.value.valued_amount}{" "}
                <span className="text-base font-normal text-muted-foreground">
                  {report.value.currency}
                </span>
              </p>
              <p className="mt-1 text-sm text-muted-foreground">
                {report.value.valued_quantity} across{" "}
                {report.value.scoped_lines - report.value.unpriced_lines} priced stock lines ·{" "}
                {report.value.reserved_quantity} held for orders
              </p>
              {note ? (
                <p
                  data-qa-inventory-report-pricing-note
                  className="mt-3 flex items-start gap-2 rounded-md bg-amber-50 px-3 py-2 text-sm text-amber-900 dark:bg-amber-950 dark:text-amber-200"
                >
                  <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
                  {note}
                </p>
              ) : null}
            </div>
          </section>

          {/* ---- the period's movements ---- */}
          <section aria-labelledby="inventory-report-movements" className="space-y-2">
            <h2
              id="inventory-report-movements"
              className="flex items-center gap-2 text-sm font-medium text-muted-foreground"
            >
              <TrendingDown className="h-4 w-4" aria-hidden />
              Movements · {report.from} to {report.to}
            </h2>
            <div className="rounded-lg border border-border">
              {report.movements.by_kind.length === 0 ? (
                <p
                  data-qa-inventory-report-no-movements
                  className="px-4 py-6 text-sm text-muted-foreground"
                >
                  Nothing moved between {report.from} and {report.to}. Reservations are not
                  counted here — they hold stock without moving it.
                </p>
              ) : (
                <table className="w-full text-sm" data-qa-inventory-report-kinds>
                  <thead>
                    <tr className="border-b border-border text-left text-xs text-muted-foreground">
                      <th className="px-4 py-2 font-medium">Kind</th>
                      <th className="px-4 py-2 text-right font-medium">Rows</th>
                      <th className="px-4 py-2 text-right font-medium">Quantity</th>
                    </tr>
                  </thead>
                  <tbody>
                    {report.movements.by_kind.map((row) => (
                      <tr key={row.kind} className="border-b border-border last:border-0">
                        <td className="px-4 py-2">{movementKindLabel(row.kind)}</td>
                        <td className="px-4 py-2 text-right tabular-nums">{row.count}</td>
                        <td className="px-4 py-2 text-right tabular-nums">{row.quantity}</td>
                      </tr>
                    ))}
                    <tr className="font-medium">
                      <td className="px-4 py-2">Net</td>
                      <td className="px-4 py-2 text-right tabular-nums">
                        {report.movements.rows}
                      </td>
                      <td
                        className="px-4 py-2 text-right tabular-nums"
                        data-qa-inventory-report-net
                      >
                        {report.movements.net_quantity}
                      </td>
                    </tr>
                  </tbody>
                </table>
              )}
            </div>
          </section>

          {/* ---- idle stock ---- */}
          <section aria-labelledby="inventory-report-idle" className="space-y-2">
            <h2
              id="inventory-report-idle"
              className="flex items-center gap-2 text-sm font-medium text-muted-foreground"
            >
              <AlertTriangle className="h-4 w-4" aria-hidden />
              Idle for {report.idle.days} days
            </h2>
            <div className="rounded-lg border border-border">
              <p className="border-b border-border px-4 py-2 text-sm text-muted-foreground">
                <strong className="text-foreground" data-qa-inventory-report-idle-total>
                  {report.idle.total}
                </strong>{" "}
                stock {report.idle.total === 1 ? "line has" : "lines have"} not moved ·{" "}
                {report.idle.quantity} sitting on the shelf
                {report.idle.truncated
                  ? ` · showing the ${report.idle.rows.length} most valuable`
                  : ""}
              </p>
              {report.idle.rows.length === 0 ? (
                <p className="px-4 py-6 text-sm text-muted-foreground">
                  Nothing has been idle for {report.idle.days} days. Every line in this scope
                  has moved recently.
                </p>
              ) : (
                <div className="overflow-x-auto">
                  <table className="w-full text-sm" data-qa-inventory-report-idle-table>
                    <thead>
                      <tr className="border-b border-border text-left text-xs text-muted-foreground">
                        <th className="px-4 py-2 font-medium">SKU</th>
                        <th className="px-4 py-2 font-medium">Item</th>
                        <th className="px-4 py-2 font-medium">Location</th>
                        <th className="px-4 py-2 text-right font-medium">On hand</th>
                        <th className="px-4 py-2 text-right font-medium">Value</th>
                        <th className="px-4 py-2 font-medium">Last movement</th>
                      </tr>
                    </thead>
                    <tbody>
                      {report.idle.rows.map((row) => (
                        <tr
                          key={row.id}
                          className="border-b border-border last:border-0"
                          data-qa-inventory-report-idle-row
                        >
                          <td className="px-4 py-2">
                            <Link
                              href={`/inventory/items/${row.item_id}`}
                              className="font-mono text-xs hover:underline"
                            >
                              {row.sku}
                            </Link>
                          </td>
                          <td className="px-4 py-2">{row.name}</td>
                          <td className="px-4 py-2 text-muted-foreground">
                            {row.location_code}
                          </td>
                          <td className="px-4 py-2 text-right tabular-nums">
                            {row.on_hand} {row.unit}
                          </td>
                          <td className="px-4 py-2 text-right tabular-nums">
                            {/* "Unpriced" is spelled out rather than rendered as a dash:
                                a dash beside a column of numbers reads as a zero. */}
                            {row.value === null ? (
                              <span className="text-xs text-muted-foreground">Unpriced</span>
                            ) : (
                              row.value
                            )}
                          </td>
                          <td className="px-4 py-2 text-xs text-muted-foreground">
                            {row.last_movement_at ? (
                              <RelativeTime at={row.last_movement_at} />
                            ) : (
                              <span data-qa-inventory-report-never-moved>Never moved</span>
                            )}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
            </div>
          </section>
        </div>
      )}
    </div>
  );
}
