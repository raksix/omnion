"use client";

/**
 * The sales report (REQ-052, slice 4b): `/sales/reports`.
 *
 * The screen a desk is judged on, and the one place in this module where a *presentation* choice
 * can quietly become a *number* the board believes. Five decisions are therefore worth naming,
 * because each of them is a way this screen could have lied to somebody:
 *
 * * **The client computes nothing.** Every figure — the four buckets, both conversion rates, the
 *   average, the won value — is the server's, read from one classification shared with the table
 *   below it and with the CSV. A screen that recomputed a conversion rate from the rows it happens
 *   to be showing would print a different number from the export the moment the table is capped,
 *   and the two files would be the argument. `formatPercent` therefore divides the server's
 *   integer by 100 and stops.
 * * **`—` is not `0%`.** A conversion rate is `null` — an em dash — when nothing was decided,
 *   because "nobody decided yet" and "nobody won anything" are different facts and only one of
 *   them is a 0%.
 * * **Cancelled is its own bucket, and the four add up.** The screen prints the check: the buckets
 *   sum to the number of quotes the filter matched, and the sum is shown next to the table so a
 *   reader can see it rather than take it on trust.
 * * **A capped table says so, in the row count itself.** The amber line is not a decoration: the
 *   export contains the same rows, so somebody reconciling a screen and a spreadsheet needs to
 *   know both are showing 1 000 of 412.
 * * **The CSV is fetched, not navigated to.** An `<a href>` to the export would hand a browser
 *   that got a 401 the API's JSON *as the file* — a seller whose session had just expired would
 *   find `sales-report.csv` containing `{"error": …}`. Fetching puts the refusal in the error
 *   path where it can be shown, and only writes a file when the response is really a CSV.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";

import { AlertTriangle, Download, Loader2, Users } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, describeError, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import { formatMoney } from "@/lib/sales";
import {
  SALES_OUTCOMES,
  SALES_REPORT_STATUS_FILTERS,
  downloadSalesReportCsv,
  fetchSalesReport,
  formatPercent,
  outcomeTone,
  type SalesReport,
  type SalesReportRow,
} from "@/lib/sales-reports";

import { SalesShortcutSheet, useSalesKeyboard } from "./sales-parts";

/** The columns, in the order the table draws them. */
const COLUMNS = ["Number", "Customer", "Owner", "Outcome", "Date", "Amount", "Order"];

/** The window a report opens on when the URL names none. */
const DEFAULT_DAYS = 30;

/** `today − n days`, in the module's own clock: a `Date` here would open on the seller's
 * yesterday for half the world. */
function isoDay(offsetDays: number): string {
  const now = new Date();
  const day = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() - offsetDays));
  return day.toISOString().slice(0, 10);
}

export function SalesReportsView() {
  const router = useRouter();
  const params = useSearchParams();

  // The filter lives in the URL, so a report somebody is looking at over somebody's shoulder is a
  // link and not a story about what they had typed.
  const from = params.get("from") ?? isoDay(DEFAULT_DAYS - 1);
  const to = params.get("to") ?? isoDay(0);
  const status = params.get("status") ?? "all";
  const unassigned = params.get("unassigned") === "1";

  const [report, setReport] = useState<SalesReport | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue | null>(null);
  const [exporting, setExporting] = useState(false);
  const [exported, setExported] = useState<string | null>(null);
  const [exportError, setExportError] = useState<ScreenErrorValue | null>(null);
  const [selected, setSelected] = useState(0);
  const searchRef = useRef<HTMLInputElement | null>(null);
  const requestRef = useRef(0);

  const query = useMemo(
    () => ({ from, to, status: status === "all" ? undefined : status, unassigned }),
    [from, to, status, unassigned],
  );

  const load = useCallback(async () => {
    const ticket = ++requestRef.current;
    setLoading(true);
    setError(null);
    try {
      const next = await fetchSalesReport(query);
      // A slow answer for a filter the seller has already changed must not replace the newer
      // one; without this, widening the window while it loads flips the table back.
      if (ticket !== requestRef.current) return;
      setReport(next);
      setSelected(0);
    } catch (caught) {
      if (ticket !== requestRef.current) return;
      setReport(null);
      setError(toScreenError(caught, "The report could not be read."));
    } finally {
      if (ticket === requestRef.current) setLoading(false);
    }
  }, [query]);

  useEffect(() => {
    void load();
  }, [load]);

  const setFilter = useCallback(
    (next: Record<string, string | null>) => {
      const search = new URLSearchParams(params.toString());
      for (const [key, value] of Object.entries(next)) {
        if (value === null || value === "" || value === "all") search.delete(key);
        else search.set(key, value);
      }
      const text = search.toString();
      router.push(`/sales/reports${text ? `?${text}` : ""}`);
    },
    [params, router],
  );

  const rows = report?.rows ?? [];

  const onOpen = useCallback(
    (index: number) => {
      const row = rows[index];
      if (row) router.push(`/sales/quotes/${row.quote_id}`);
    },
    [rows, router],
  );

  const { shortcutsOpen, setShortcutsOpen } = useSalesKeyboard(
    { count: rows.length, selected, onSelect: setSelected, onOpen, onEdit: undefined, onNew: undefined },
    searchRef,
  );

  const onExport = useCallback(async () => {
    setExporting(true);
    setExportError(null);
    setExported(null);
    try {
      setExported(await downloadSalesReportCsv(query));
    } catch (caught) {
      setExportError(toScreenError(caught, "The export could not be prepared."));
    } finally {
      setExporting(false);
    }
  }, [query]);

  // The arithmetic the screen asks the reader to check, computed from the server's own counts.
  // It is summed here rather than printed by the server because it is the *check* — the server
  // saying "my parts add up" is not a check, it is a claim.
  const bucketSum = report
    ? report.totals.won + report.totals.lost + report.totals.pending + report.totals.cancelled
    : 0;
  const addsUp = report ? bucketSum === report.totals.quotes_seen : true;

  return (
    <div className="space-y-6" data-qa-sales-reports>
      <header className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-xl font-semibold">Sales report</h1>
          <p className="text-sm text-muted-foreground">
            {report
              ? `${report.from} to ${report.to} · ${report.totals.quotes_seen} quote${
                  report.totals.quotes_seen === 1 ? "" : "s"
                }`
              : "Loading the report…"}
          </p>
        </div>
        <button
          type="button"
          className="inline-flex items-center gap-2 rounded-md border px-3 py-2 text-sm"
          data-qa-sales-export
          onClick={() => void onExport()}
          disabled={exporting}
        >
          {exporting ? <Loader2 className="h-4 w-4 animate-spin" aria-hidden /> : <Download className="h-4 w-4" aria-hidden />}
          {exporting ? "Preparing…" : "Export CSV"}
        </button>
      </header>

      {/* The filter bar. Every control is a real filter that changes the numbers above it; a
          period picker that only re-sorts the table would be a control that lies about its
          name. */}
      <div className="flex flex-wrap items-end gap-3 rounded-lg border p-3" data-qa-sales-report-filters>
        <label className="flex flex-col gap-1 text-sm">
          <span className="text-muted-foreground">From</span>
          <input
            type="date"
            data-qa-sales-report-from
            value={from}
            onChange={(event) => setFilter({ from: event.target.value })}
            className="rounded-md border px-2 py-1.5"
          />
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span className="text-muted-foreground">To</span>
          <input
            type="date"
            data-qa-sales-report-to
            value={to}
            onChange={(event) => setFilter({ to: event.target.value })}
            className="rounded-md border px-2 py-1.5"
          />
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span className="text-muted-foreground">Status</span>
          <select
            data-qa-sales-report-status
            value={status}
            onChange={(event) => setFilter({ status: event.target.value })}
            className="rounded-md border px-2 py-1.5"
          >
            {SALES_REPORT_STATUS_FILTERS.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            data-qa-sales-report-unassigned
            checked={unassigned}
            onChange={(event) => setFilter({ unassigned: event.target.checked ? "1" : null })}
          />
          Only quotes nobody owns
        </label>
        <div className="flex gap-2">
          {[
            { label: "7 days", days: 7 },
            { label: "30 days", days: 30 },
            { label: "90 days", days: 90 },
          ].map((preset) => (
            <button
              key={preset.days}
              type="button"
              data-qa-sales-report-preset={preset.days}
              className="rounded-md border px-2 py-1.5 text-sm"
              onClick={() => setFilter({ from: isoDay(preset.days - 1), to: isoDay(0) })}
            >
              {preset.label}
            </button>
          ))}
        </div>
      </div>

      {exportError ? (
        <p className="rounded-md border border-destructive px-3 py-2 text-sm" data-qa-sales-export-error>
          {describeError(exportError).message}
        </p>
      ) : null}
      {exported ? (
        <p className="rounded-md border px-3 py-2 text-sm" data-qa-sales-export-notice>
          Downloaded {exported} — the same rows as the table below.
        </p>
      ) : null}

      {error ? (
        <ErrorState
          error={error}
          onRetry={() => void load()}
          busy={loading}
          qa="sales-reports-error"
        />
      ) : loading && !report ? (
        <LoadingTable columns={COLUMNS.length} rows={6} />
      ) : (
        <>
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4" data-qa-sales-report-cards>
            {SALES_OUTCOMES.map((bucket) => (
              <div
                key={bucket.value}
                className="rounded-lg border p-4"
                data-qa-sales-report-card={bucket.value}
              >
                <p className="text-sm text-muted-foreground">{bucket.label}</p>
                <p className="text-2xl font-semibold">{report?.totals[bucket.value] ?? 0}</p>
              </div>
            ))}
          </div>

          <div className="grid gap-3 sm:grid-cols-3">
            <div className="rounded-lg border p-4" data-qa-sales-report-conversion>
              <p className="text-sm text-muted-foreground">Won ÷ decided</p>
              <p className="text-2xl font-semibold">{formatPercent(report?.conversion_bps)}</p>
              <p className="text-xs text-muted-foreground">
                Accepted orders against accepted and declined quotes.
              </p>
            </div>
            <div className="rounded-lg border p-4" data-qa-sales-report-order-conversion>
              <p className="text-sm text-muted-foreground">Quote → order</p>
              <p className="text-2xl font-semibold">
                {formatPercent(report?.order_conversion_bps)}
              </p>
              <p className="text-xs text-muted-foreground">
                Accepted quotes that have become an order.
              </p>
            </div>
            <div className="rounded-lg border p-4" data-qa-sales-report-average>
              <p className="text-sm text-muted-foreground">Average deal</p>
              <p className="text-2xl font-semibold">
                {report?.average_deal ? formatMoney(report.average_deal, report.currency) : "—"}
              </p>
              <p className="text-xs text-muted-foreground">
                Won deals only · {report ? formatMoney(report.won_value, report.currency) : "—"} won
              </p>
            </div>
          </div>

          {/* The check, made visible. The four buckets come from one `group by`, so this cannot
              drift — but a reader has no way to know that, and a report whose parts do not add
              up to its total is one nobody trusts. */}
          <p
            className={`text-sm ${addsUp ? "text-muted-foreground" : "text-destructive"}`}
            data-qa-sales-report-sum
            data-adds-up={addsUp ? "true" : "false"}
          >
            {report
              ? `${report.totals.won} won + ${report.totals.lost} lost + ${report.totals.pending} pending + ${report.totals.cancelled} cancelled = ${bucketSum} of ${report.totals.quotes_seen} quotes`
              : ""}
          </p>

          <section aria-labelledby="sales-report-owners">
            <h2 id="sales-report-owners" className="mb-2 flex items-center gap-2 text-base font-medium">
              <Users className="h-4 w-4" aria-hidden />
              By owner
            </h2>
            {report && report.by_owner.length > 0 ? (
              <div className="overflow-x-auto rounded-lg border">
                <table className="w-full text-sm">
                  <caption className="sr-only">Quotes and won value per seller</caption>
                  <thead className="border-b text-left">
                    <tr>
                      <th className="px-3 py-2">Owner</th>
                      <th className="px-3 py-2">Won</th>
                      <th className="px-3 py-2">Lost</th>
                      <th className="px-3 py-2">Pending</th>
                      <th className="px-3 py-2">Average</th>
                      <th className="px-3 py-2">Won value</th>
                    </tr>
                  </thead>
                  <tbody>
                    {report.by_owner.map((owner) => (
                      <tr
                        key={owner.owner_user_id ?? "unassigned"}
                        className="border-b last:border-0"
                        data-qa-sales-report-owner={owner.owner_name}
                      >
                        <td className="px-3 py-2">{owner.owner_name}</td>
                        <td className="px-3 py-2">{owner.totals.won}</td>
                        <td className="px-3 py-2">{owner.totals.lost}</td>
                        <td className="px-3 py-2">{owner.totals.pending}</td>
                        <td className="px-3 py-2">
                          {owner.average_deal
                            ? formatMoney(owner.average_deal, report.currency)
                            : "—"}
                        </td>
                        <td className="px-3 py-2">{formatMoney(owner.won_value, report.currency)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            ) : (
              <EmptyState
                title="Nothing to break down"
                hint="No quote in this window has an owner, or none was written at all."
              />
            )}
          </section>

          <section aria-labelledby="sales-report-rows">
            <h2 id="sales-report-rows" className="mb-2 text-base font-medium">
              Quotes
            </h2>
            {report && report.truncated ? (
              <p
                className="mb-2 flex items-center gap-2 rounded-md border border-amber-500 px-3 py-2 text-sm"
                data-qa-sales-report-truncated
              >
                <AlertTriangle className="h-4 w-4" aria-hidden />
                Showing the first {rows.length} of {report.totals.quotes_seen} quotes — narrow the
                window to see the rest. The export holds the same {rows.length} rows.
              </p>
            ) : null}
            {rows.length === 0 ? (
              <EmptyState
                title="No quotes in this window"
                hint="Widen the period or clear the status filter."
                action={
                  <button
                    type="button"
                    className="rounded-md border px-3 py-2 text-sm"
                    onClick={() => setFilter({ from: null, to: null, status: null, unassigned: null })}
                  >
                    Show the last 30 days
                  </button>
                }
              />
            ) : (
              <div className="overflow-x-auto rounded-lg border">
                <table className="w-full text-sm">
                  <caption className="sr-only">The quotes behind the numbers</caption>
                  <thead className="border-b text-left">
                    <tr>
                      {COLUMNS.map((column) => (
                        <th key={column} className="px-3 py-2">
                          {column}
                        </th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {rows.map((row: SalesReportRow, index: number) => (
                      <tr
                        key={row.quote_id}
                        className={`cursor-pointer border-b last:border-0 ${
                          index === selected ? "bg-muted" : ""
                        }`}
                        data-qa-sales-report-row={row.number}
                        data-outcome={row.outcome}
                        onClick={() => {
                          setSelected(index);
                          onOpen(index);
                        }}
                      >
                        <td className="px-3 py-2 font-medium">{row.number}</td>
                        <td className="px-3 py-2">{row.customer || "—"}</td>
                        <td className="px-3 py-2">{row.owner_name}</td>
                        <td className="px-3 py-2">
                          <span
                            className="rounded-full border px-2 py-0.5 text-xs"
                            data-qa-sales-report-outcome={row.outcome}
                            data-tone={outcomeTone(row.outcome)}
                          >
                            {row.outcome}
                          </span>
                        </td>
                        <td className="px-3 py-2">{row.date}</td>
                        <td className="px-3 py-2 text-right">{formatMoney(row.grand_total, row.currency)}</td>
                        <td className="px-3 py-2">{row.order_number ?? "—"}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </section>
        </>
      )}

      {shortcutsOpen ? <SalesShortcutSheet onClose={() => setShortcutsOpen(false)} /> : null}
    </div>
  );
}
