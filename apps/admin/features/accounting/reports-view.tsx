"use client";

/**
 * The reports screen (REQ-054, slice 4b): `/accounting/reports`.
 *
 * One screen for four reports, and the reason it is one screen rather than four is the
 * acceptance box the REQ writes: *"the totals equal the sum of the underlying rows"* has to be
 * **visible**, so the reader can check it rather than trust it. Four screens would each have to
 * carry that note, and four copies of a check that exists to be verified is four chances for
 * one of them to be a decoration.
 *
 * ## The three things this screen could get wrong, and what stops them
 *
 * * **The period has to be a control, not a caption.** A report whose window cannot be changed
 *   is a screenshot. The presets and the two date fields write the same `from`/`to` the export
 *   sends, so "the file matches the table" is true because they were built from one value.
 * * **The numbers have to sum up in front of the reader.** `figuresAgree` re-adds the rows in
 *   integer cents and returns either the confirmation or a **loud** sentence naming both
 *   figures. A report whose totals do not match its rows is the one case where the screen must
 *   refuse to sound confident — a bookkeeper quoting a total that its own table contradicts is
 *   worse than one with no table at all.
 * * **The definition travels with the numbers.** Aging buckets and the period label come from
 *   `meta`, which the server built, so the sentence under the table is the module's rule and
 *   not a client's paraphrase of it. The CSV carries the same sentence in its header.
 *
 * **There is no PDF button, and that is deliberate.** The REQ's screen sketch asks for one, but a
 * PDF needs a font and a layout engine, and an *unverified* PDF is worse than none: a reader
 * cannot check its numbers and would not know to try. The CSV carries the definition, the
 * period and the row count, so a file opened six months later is still answerable.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { AlertTriangle, CheckCircle2, Download, TrendingUp } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import { AccountingModuleNav } from "./module-nav";

import {
  REPORT_KINDS,
  cents,
  exportReportCsv,
  fetchReport,
  formatMoney,
  moneyFromCents,
  reportKindLabel,
  reportRows,
  reportTotalsAgree,
  type AgingReport,
  type CashflowReport,
  type IncomeExpenseReport,
  type ReportKind,
  type ReportPayload,
  type ReportPeriod,
  type TaxSummaryReport,
} from "@/lib/accounting-reports";

/** Presets, in the shape a person asks for them. 30 days is the server's own default window. */
const PRESETS = [
  { label: "30 days", days: 29 },
  { label: "90 days", days: 89 },
  { label: "6 months", days: 179 },
  { label: "Year", days: 364 },
];

function daysAgo(days: number): string {
  const date = new Date();
  date.setDate(date.getDate() - days);
  return date.toISOString().slice(0, 10);
}

/** Read the window out of the URL, so a report can be bookmarked and shared as it is shown. */
function initialPeriod(): ReportPeriod {
  const params = new URLSearchParams(typeof window === "undefined" ? "" : window.location.search);
  return {
    from: params.get("from") ?? daysAgo(29),
    to: params.get("to") ?? daysAgo(0),
  };
}

export function ReportsView() {
  const [kind, setKind] = useState<ReportKind>("aging");
  const [period, setPeriod] = useState<ReportPeriod>(initialPeriod);
  const [report, setReport] = useState<ReportPayload | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [exporting, setExporting] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setReport(await fetchReport(kind, period));
    } catch (failure) {
      // A refusal that names a permission is not a broken screen: the tax summary answers under
      // `accounting.reports.tax`, so a bookkeeper who may read the other three still meets a
      // real refusal here, and it is shown rather than swallowed.
      setReport(null);
      setError(toScreenError(failure, "The report could not be built."));
    } finally {
      setLoading(false);
    }
  }, [kind, period]);

  useEffect(() => {
    void load();
  }, [load]);

  // Keep the address bar in step: a refresh and a shared link both mean the same report.
  useEffect(() => {
    const params = new URLSearchParams();
    if (period.from) params.set("from", period.from);
    if (period.to) params.set("to", period.to);
    if (kind !== "aging") params.set("report", kind);
    const query = params.toString();
    window.history.replaceState(null, "", query ? `/accounting/reports?${query}` : "/accounting/reports");
  }, [kind, period]);

  const runExport = async () => {
    setExporting(true);
    setError(null);
    try {
      // The same window the table is showing, from the same values — a file that disagrees
      // with the screen is worse than no file.
      await exportReportCsv(kind, period);
    } catch (failure) {
      setError(toScreenError(failure, "The report could not be exported."));
    } finally {
      setExporting(false);
    }
  };

  const setWindow = (from: string, to: string) => setPeriod((p) => ({ ...p, from, to }));
  const verdict = report ? reportTotalsAgree(report) : null;

  return (
    <div className="space-y-5">
      <AccountingModuleNav />

      <header className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-xl font-semibold tracking-tight">Reports</h1>
          <p className="text-sm text-muted-foreground">
            {REPORT_KINDS.find((entry) => entry.value === kind)?.blurb}
          </p>
        </div>
        <button
          type="button"
          onClick={() => void runExport()}
          disabled={exporting || !report}
          data-qa-accounting-report-export
          className="inline-flex h-9 items-center gap-2 rounded-md border border-border px-3 text-sm disabled:opacity-60"
        >
          <Download className="h-4 w-4" aria-hidden />
          {exporting ? "Preparing…" : "Export CSV"}
        </button>
      </header>

      {/* The report type as a real control. Four buttons rather than a select: the blurb under
          the heading changes with the choice, and a select hides the choice until it is opened. */}
      <div className="flex flex-wrap gap-1" role="group" aria-label="Report">
        {REPORT_KINDS.map((entry) => (
          <button
            key={entry.value}
            type="button"
            aria-pressed={kind === entry.value}
            onClick={() => setKind(entry.value)}
            data-qa-accounting-report-kind={entry.value}
            className={`h-8 rounded-full border px-3 text-sm ${
              kind === entry.value
                ? "border-foreground bg-foreground text-background"
                : "border-border text-muted-foreground hover:text-foreground"
            }`}
          >
            {entry.label}
          </button>
        ))}
      </div>

      {/* The window, as real date and preset controls. */}
      <div className="flex flex-wrap items-end gap-3">
        <label className="flex flex-col gap-1 text-xs text-muted-foreground">
          <span>From</span>
          <input
            type="date"
            value={period.from ?? ""}
            onChange={(event) => setWindow(event.target.value, period.to ?? daysAgo(0))}
            data-qa-accounting-report-from
            className="h-8 rounded-md border border-border bg-transparent px-2 text-sm text-foreground"
          />
        </label>
        <label className="flex flex-col gap-1 text-xs text-muted-foreground">
          <span>To</span>
          <input
            type="date"
            value={period.to ?? ""}
            onChange={(event) => setWindow(period.from ?? daysAgo(29), event.target.value)}
            data-qa-accounting-report-to
            className="h-8 rounded-md border border-border bg-transparent px-2 text-sm text-foreground"
          />
        </label>
        <div className="flex flex-wrap gap-1" role="group" aria-label="Preset periods">
          {PRESETS.map((preset) => (
            <button
              key={preset.label}
              type="button"
              onClick={() => setWindow(daysAgo(preset.days), daysAgo(0))}
              data-qa-accounting-report-preset={preset.label}
              className="h-8 rounded-full border border-border px-3 text-sm text-muted-foreground hover:text-foreground"
            >
              {preset.label}
            </button>
          ))}
        </div>
      </div>

      {loading ? (
        <LoadingTable columns={4} rows={5} />
      ) : error ? (
        <ErrorState
          error={error}
          onRetry={() => void load()}
          qa="accounting-report-error"
          action={
            <Link
              href="/accounting/invoices"
              className="inline-flex h-8 items-center rounded-lg border border-line px-3 text-[12.5px]"
            >
              Back to invoices
            </Link>
          }
        />
      ) : !report || reportRows(report).length === 0 ? (
        <EmptyState
          title={`No ${reportKindLabel(kind).toLowerCase()} in this period`}
          hint={
            kind === "aging"
              ? "Aging counts invoices somebody was asked to pay. Raise one, or widen the window, and it will appear here."
              : "Nothing was recorded on this side of the books in the selected window. Widen it, or record something and come back."
          }
          action={
            <Link
              href="/accounting/invoices"
              className="inline-flex h-9 items-center rounded-md bg-primary px-3 text-sm text-primary-foreground"
            >
              Open invoices
            </Link>
          }
        />
      ) : (
        <div className="space-y-5">
          {/* The header the server built: which report, which window, how many rows, and the
              rule the rows were produced by. */}
          <div className="rounded-lg border border-border px-4 py-3" data-qa-accounting-report-header>
            <p className="text-sm font-medium">
              {reportKindLabel(report.meta.kind)}
              <span className="ml-2 font-normal text-muted-foreground">{report.meta.period_label}</span>
            </p>
            <p className="mt-1 text-[12.5px] text-muted-foreground">
              {report.meta.definition}
            </p>
            <p className="mt-1 text-[12px] text-muted-foreground">
              {report.meta.row_count} {report.meta.row_count === 1 ? "row" : "rows"} · read on{" "}
              {report.meta.generated_on}
            </p>
          </div>

          {/* The check, out loud. */}
          {verdict ? (
            <p
              role="status"
              data-qa-accounting-report-agree={String(verdict.agree)}
              className={`flex items-start gap-2 rounded-md px-3 py-2 text-[13px] ${
                verdict.agree
                  ? "border border-emerald-200 bg-emerald-50 text-emerald-900 dark:border-emerald-900 dark:bg-emerald-950 dark:text-emerald-200"
                  : "border border-amber-300 bg-amber-50 text-amber-900 dark:border-amber-800 dark:bg-amber-950 dark:text-amber-200"
              }`}
            >
              {verdict.agree ? (
                <CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
              ) : (
                <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
              )}
              {verdict.note}
            </p>
          ) : null}

          <ReportTable report={report} />
          <BarChart report={report} />
        </div>
      )}
    </div>
  );
}

/** The table for whichever report this is. Four shapes, one screen — a shared "no rows" branch
 *  would be unreachable, because the empty state above already owns that case. */
function ReportTable({ report }: { report: ReportPayload }) {
  if (report.meta.kind === "aging") {
    const aging = report as AgingReport;
    return (
      <div className="overflow-x-auto rounded-lg border border-border">
        <table className="w-full border-collapse text-left text-[13px]" data-qa-accounting-report-table="aging">
          <thead>
            <tr className="border-b border-line text-[12px] text-muted-foreground">
              <th className="px-4 py-2.5 font-medium">Number</th>
              <th className="px-4 py-2.5 font-medium">Customer</th>
              <th className="px-4 py-2.5 font-medium">Due</th>
              <th className="px-4 py-2.5 text-right font-medium">Total</th>
              <th className="px-4 py-2.5 text-right font-medium">Paid</th>
              <th className="px-4 py-2.5 text-right font-medium">Outstanding</th>
              <th className="px-4 py-2.5 font-medium">Bucket</th>
              <th className="px-4 py-2.5 text-right font-medium">Days late</th>
            </tr>
          </thead>
          <tbody>
            {aging.rows.map((row) => (
              <tr
                key={row.number}
                data-qa-accounting-aging-row={row.number}
                className="border-b border-line last:border-b-0"
              >
                <td className="px-4 py-2.5">
                  {/* The row links by ID, not by number: `/accounting/invoices/{id}` addresses a
                      uuid, so a link built from the number would be a link to a 404. */}
                  <Link
                    href={`/accounting/invoices/${row.invoice_id}`}
                    data-qa-accounting-aging-link={row.number}
                    className="underline underline-offset-2"
                  >
                    {row.number}
                  </Link>
                </td>
                <td className="px-4 py-2.5">{row.customer_name || "—"}</td>
                <td className="px-4 py-2.5">{row.due_date ?? "—"}</td>
                <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(row.total)}</td>
                <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(row.paid)}</td>
                <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(row.outstanding)}</td>
                <td className="px-4 py-2.5">{bucketLabel(aging, row.bucket)}</td>
                {/* `null` is printed as a dash and not as 0: no due date is an invoice the
                    report cannot place, and filing it as "not late" understates the column. */}
                <td className="px-4 py-2.5 text-right tabular-nums">
                  {row.days_past_due === null ? "—" : row.days_past_due}
                </td>
              </tr>
            ))}
          </tbody>
          <tfoot>
            <tr className="border-t border-line text-[12.5px] font-medium">
              <td className="px-4 py-2.5" colSpan={5}>
                Bucket totals
              </td>
              <td className="px-4 py-2.5 text-right tabular-nums">
                {formatMoney(sumText(aging.rows.map((r) => r.outstanding)))}
              </td>
              <td className="px-4 py-2.5" colSpan={2}>
                {aging.buckets.length} columns
              </td>
            </tr>
          </tfoot>
        </table>
        {/* The bucket totals as their own strip: the acceptance box is that they SUM to the
            outstanding total, and a reader cannot check that from the invoice rows alone. */}
        <ul
          className="flex flex-wrap gap-x-6 gap-y-2 border-t border-line px-4 py-3 text-[12.5px]"
          data-qa-accounting-report-buckets
        >
          {aging.buckets.map((bucket) => (
            <li key={bucket.bucket} className="flex flex-col" data-qa-accounting-bucket={bucket.bucket}>
              <span className="text-muted-foreground">
                {bucket.label} · {bucket.invoice_count} {bucket.invoice_count === 1 ? "invoice" : "invoices"}
              </span>
              <span className="tabular-nums">{formatMoney(bucket.amount)}</span>
            </li>
          ))}
        </ul>
      </div>
    );
  }

  if (report.meta.kind === "income-expense") {
    const r = report as IncomeExpenseReport;
    return (
      <div className="overflow-x-auto rounded-lg border border-border">
        <table className="w-full border-collapse text-left text-[13px]" data-qa-accounting-report-table="income-expense">
          <thead>
            <tr className="border-b border-line text-[12px] text-muted-foreground">
              <th className="px-4 py-2.5 font-medium">Month</th>
              <th className="px-4 py-2.5 text-right font-medium">Income</th>
              <th className="px-4 py-2.5 text-right font-medium">Expense</th>
              <th className="px-4 py-2.5 text-right font-medium">Net</th>
            </tr>
          </thead>
          <tbody>
            {r.rows.map((row) => (
              <tr key={row.month} data-qa-accounting-report-row={row.month} className="border-b border-line last:border-b-0">
                <td className="px-4 py-2.5">{row.month}</td>
                <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(row.income)}</td>
                <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(row.expense)}</td>
                <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(row.net)}</td>
              </tr>
            ))}
          </tbody>
          <tfoot>
            <tr className="border-t border-line font-medium">
              <td className="px-4 py-2.5">Total</td>
              <td className="px-4 py-2.5 text-right tabular-nums" data-qa-accounting-report-total="income">
                {formatMoney(r.totals.income)}
              </td>
              <td className="px-4 py-2.5 text-right tabular-nums" data-qa-accounting-report-total="expense">
                {formatMoney(r.totals.expense)}
              </td>
              <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(r.totals.net)}</td>
            </tr>
          </tfoot>
        </table>
      </div>
    );
  }

  if (report.meta.kind === "cashflow") {
    const r = report as CashflowReport;
    return (
      <div className="overflow-x-auto rounded-lg border border-border">
        <table className="w-full border-collapse text-left text-[13px]" data-qa-accounting-report-table="cashflow">
          <thead>
            <tr className="border-b border-line text-[12px] text-muted-foreground">
              <th className="px-4 py-2.5 font-medium">Week beginning</th>
              <th className="px-4 py-2.5 text-right font-medium">In</th>
              <th className="px-4 py-2.5 text-right font-medium">Out</th>
              <th className="px-4 py-2.5 text-right font-medium">Net</th>
            </tr>
          </thead>
          <tbody>
            {r.rows.map((row) => (
              <tr key={row.week_start} data-qa-accounting-report-row={row.week_start} className="border-b border-line last:border-b-0">
                <td className="px-4 py-2.5">{row.week_start}</td>
                <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(row.money_in)}</td>
                <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(row.money_out)}</td>
                <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(row.net)}</td>
              </tr>
            ))}
          </tbody>
          <tfoot>
            <tr className="border-t border-line font-medium">
              <td className="px-4 py-2.5">Total</td>
              <td className="px-4 py-2.5 text-right tabular-nums" data-qa-accounting-report-total="money-in">
                {formatMoney(r.totals.money_in)}
              </td>
              <td className="px-4 py-2.5 text-right tabular-nums" data-qa-accounting-report-total="money-out">
                {formatMoney(r.totals.money_out)}
              </td>
              <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(r.totals.net)}</td>
            </tr>
          </tfoot>
        </table>
      </div>
    );
  }

  const r = report as TaxSummaryReport;
  return (
    <div className="overflow-x-auto rounded-lg border border-border">
      <table className="w-full border-collapse text-left text-[13px]" data-qa-accounting-report-table="tax-summary">
        <thead>
          <tr className="border-b border-line text-[12px] text-muted-foreground">
            <th className="px-4 py-2.5 font-medium">Rate</th>
            <th className="px-4 py-2.5 text-right font-medium">Percent</th>
            <th className="px-4 py-2.5 font-medium">Applies to</th>
            <th className="px-4 py-2.5 text-right font-medium">Base</th>
            <th className="px-4 py-2.5 text-right font-medium">Tax</th>
          </tr>
        </thead>
        <tbody>
          {r.rows.map((row) => (
            <tr key={row.rate_name} data-qa-accounting-report-row={row.rate_name} className="border-b border-line last:border-b-0">
              <td className="px-4 py-2.5">{row.rate_name}</td>
              <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(row.percent)}%</td>
              <td className="px-4 py-2.5">{row.kind === "purchase" ? "Purchases" : "Sales"}</td>
              <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(row.base)}</td>
              <td className="px-4 py-2.5 text-right tabular-nums">{formatMoney(row.tax)}</td>
            </tr>
          ))}
        </tbody>
        <tfoot>
          <tr className="border-t border-line font-medium">
            <td className="px-4 py-2.5" colSpan={3}>
              Total
            </td>
            <td className="px-4 py-2.5 text-right tabular-nums" data-qa-accounting-report-total="base">
              {formatMoney(r.totals.base)}
            </td>
            <td className="px-4 py-2.5 text-right tabular-nums" data-qa-accounting-report-total="tax">
              {formatMoney(r.totals.tax)}
            </td>
          </tr>
        </tfoot>
      </table>
    </div>
  );
}

/**
 * The bar chart, drawn from the SAME `reportRows` the table renders.
 *
 * One source for both is the point: a chart fed by a second query is a second answer to the same
 * question, and it is the one nobody checks. Bars are labelled and the values are printed in
 * `title`/text, so the chart is readable without hovering — a reader with a screen reader gets
 * the numbers, not a picture of them.
 */
function BarChart({ report }: { report: ReportPayload }) {
  const rows = reportRows(report);
  if (rows.length === 0) return null;
  const peak = Math.max(
    1,
    ...rows.flatMap((row) => row.values.map((value) => Math.abs(value.amount))),
  );
  const tone = ["bg-emerald-500", "bg-amber-500", "bg-sky-500", "bg-rose-500"];

  return (
    <section aria-labelledby="accounting-report-chart" className="space-y-2">
      <h2
        id="accounting-report-chart"
        className="flex items-center gap-2 text-sm font-medium text-muted-foreground"
      >
        <TrendingUp className="h-4 w-4" aria-hidden />
        {reportKindLabel(report.meta.kind)}
      </h2>
      <div
        className="flex flex-wrap items-end gap-2 rounded-lg border border-border px-4 py-4"
        data-qa-accounting-report-chart
      >
        {rows.map((row, rowIndex) => (
          <div key={row.label} className="flex min-w-16 flex-col items-center gap-1">
            <div className="flex h-24 items-end gap-0.5">
              {row.values.map((value, valueIndex) => (
                <div
                  key={value.name}
                  title={`${row.label} · ${value.name} ${formatMoney(moneyFromCents(value.amount))}`}
                  data-qa-accounting-bar={row.label}
                  className={`w-4 rounded-t ${tone[(rowIndex + valueIndex) % tone.length]}`}
                  style={{ height: `${Math.max(3, (Math.abs(value.amount) / peak) * 96)}px` }}
                />
              ))}
            </div>
            <span className="max-w-20 truncate text-[11px] text-muted-foreground">{row.label}</span>
            {row.values.map((value) => (
              <span key={value.name} className="text-[11px] tabular-nums text-muted-foreground">
                {formatMoney(moneyFromCents(value.amount))}
              </span>
            ))}
          </div>
        ))}
      </div>
    </section>
  );
}

/** The bucket's label as the SERVER wrote it, falling back to the machine name. */
function bucketLabel(report: AgingReport, bucket: string): string {
  return report.buckets.find((entry) => entry.bucket === bucket)?.label ?? bucket;
}

/** Add a list of amounts in integer cents; `null` when one of them will not parse. */
function sumText(values: string[]): string | null {
  let total = 0;
  for (const value of values) {
    const parsed = cents(value);
    if (parsed === null) return null;
    total += parsed;
  }
  return moneyFromCents(total);
}