/**
 * The reports client of REQ-054 slice 4b — the API half of `/accounting/reports`.
 *
 * ## Why the payloads are four types and not one
 *
 * The server answers with an **untagged** union, because a route has to return one
 * `Json<T>` and four different report shapes do not share a body. That makes the discriminator
 * a *field*, not the shape: `meta.kind`. This file narrows on it once, in one place
 * ([`reportRows`]), rather than letting every screen re-ask "which of these four is this?".
 * A screen that guessed wrong would render the tax summary's three columns with the aging
 * report's headers — no crash, no error, just numbers under the wrong names, which is the one
 * failure mode a financial screen must not have.
 *
 * ## The buckets and the definition are the SERVER's, never re-derived here
 *
 * `buckets[].label` and `meta.definition` travel with the rows. Recomputing "1-30 days"
 * in TypeScript would be a second definition of the same rule, and the two would part company
 * the day somebody fixes a boundary — with the screen still showing the old rule and nobody
 * noticing. The client formats; the module decides.
 *
 * ## Money crosses the wire as a string, always
 *
 * Same rule as `lib/accounting.ts`, and here the consequence is sharper: the acceptance box is
 * *"the buckets sum to the outstanding total"*, and that identity only holds in integer cents.
 * [`cents`] parses the decimal text into an integer and [`moneyFromCents`] puts it back, so
 * [`reportTotalsAgree`] can assert the identity **on the same numbers the screen shows** — a
 * check done in floating point would pass a report that is a cent out.
 */

import { ApiError, type ErrorBody } from "./api";

/** A `numeric(14,2)` amount, as the string the server sent. */
export type Money = string;

/** The four reports the screen offers, in the order a bookkeeper reads them. */
export const REPORT_KINDS = [
  { value: "income-expense", label: "Income & expense", blurb: "What came in against what went out, month by month." },
  { value: "aging", label: "Receivable aging", blurb: "Who owes what, and how long it has been owed." },
  { value: "cashflow", label: "Cashflow", blurb: "Money in against money out, week by week." },
  { value: "tax-summary", label: "Tax summary", blurb: "The taxable base and the tax collected, by rate." },
] as const;

export type ReportKind = (typeof REPORT_KINDS)[number]["value"];

/** The permission each report is read with — the tax summary's is NOT the section's. */
export const REPORT_PERMISSIONS: Record<ReportKind, string> = {
  "income-expense": "accounting.reports.read",
  aging: "accounting.reports.read",
  cashflow: "accounting.reports.read",
  "tax-summary": "accounting.reports.tax",
};

/** The label of a report name, falling back to the raw value for one the UI does not know. */
export function reportKindLabel(kind: string): string {
  return REPORT_KINDS.find((entry) => entry.value === kind)?.label ?? kind;
}

/** The window a report is read for. Both ends optional: an open-ended window is a real question. */
export type ReportPeriod = {
  /** `YYYY-MM-DD`, the first day counted. */
  from?: string;
  /** `YYYY-MM-DD`, the last day counted, inclusive. */
  to?: string;
};

/**
 * The header every report carries.
 *
 * `row_count` is the server's own count of what it returned — the screen prints it beside the
 * table rather than counting rows itself, because the number that matters is the one the
 * **server** says it produced, not the number that arrived.
 */
export type ReportMeta = {
  kind: ReportKind;
  period_label: string;
  from: string | null;
  to: string | null;
  generated_on: string;
  definition: string;
  row_count: number;
};

/** One invoice's row in the aging table. */
export type AgingRow = {
  /** The invoice's id, so the row can link to the invoice. NOT in the CSV — the export's column
   *  list is written out by hand, so a file opened later still carries what a reader needs. */
  invoice_id: string;
  number: string;
  customer_name: string;
  due_date: string | null;
  total: Money;
  paid: Money;
  outstanding: Money;
  bucket: string;
  /** `null` when there is no due date — NOT zero, which would file it as not-late. */
  days_past_due: number | null;
};

/** The total in one aging column, with the label the header shows. */
export type AgingBucketTotal = {
  bucket: string;
  label: string;
  invoice_count: number;
  amount: Money;
};

export type AgingReport = {
  meta: ReportMeta;
  rows: AgingRow[];
  buckets: AgingBucketTotal[];
};

export type IncomeExpenseRow = { month: string; income: Money; expense: Money; net: Money };
export type IncomeExpenseTotals = { income: Money; expense: Money; net: Money };
export type IncomeExpenseReport = {
  meta: ReportMeta;
  rows: IncomeExpenseRow[];
  totals: IncomeExpenseTotals;
};

export type CashflowRow = {
  week_start: string;
  money_in: Money;
  money_out: Money;
  net: Money;
};
export type CashflowTotals = { money_in: Money; money_out: Money; net: Money };
export type CashflowReport = {
  meta: ReportMeta;
  rows: CashflowRow[];
  totals: CashflowTotals;
};

export type TaxSummaryRow = {
  rate_name: string;
  percent: Money;
  kind: string;
  base: Money;
  tax: Money;
};
export type TaxTotals = { base: Money; tax: Money };
export type TaxSummaryReport = {
  meta: ReportMeta;
  rows: TaxSummaryRow[];
  totals: TaxTotals;
};

/** The whole union, exactly as the route returns it. */
export type ReportPayload =
  | IncomeExpenseReport
  | AgingReport
  | CashflowReport
  | TaxSummaryReport;

// ---------------------------------------------------------------------------------------------
// Integer-cent arithmetic
// ---------------------------------------------------------------------------------------------

/**
 * Decimal text into integer cents.
 *
 * Returns `null` for anything that is not a well-formed amount rather than throwing: the report
 * rows come off the wire and a row this cannot parse must not take the whole screen down — it
 * must show up as a figure that does not add up, which is the honest outcome.
 *
 * The arithmetic is deliberately done in **hundredths of a hundredth**, because an amount can
 * legitimately arrive with a sub-cent fraction (`numeric` columns are not rounded on read) and
 * `Math.round(x * 100)` on `1200.005` loses a half-cent through the float before it is rounded.
 */
export function cents(text: Money | number | null | undefined): number | null {
  if (text === null || text === undefined) return null;
  const raw = String(text).trim();
  if (raw === "") return null;
  const match = /^(-)?(\d*)(?:\.(\d*))?$/.exec(raw);
  if (!match) return null;
  const [, sign, whole, fraction = ""] = match;
  // Two digits carry the money; the rest are kept so a sub-cent amount does not silently round
  // away. Dividing the remainder by 10k keeps it in hundredths.
  const hundredths = Number.parseInt((fraction + "00").slice(0, 2) || "0", 10);
  const remainder = Number.parseInt((fraction + "0000").slice(2, 4) || "0", 10) / 10;
  const units = Number.parseInt(whole || "0", 10);
  const total = units * 100 + hundredths + remainder;
  if (!Number.isFinite(total)) return null;
  return sign ? -total : total;
}

/** Integer cents back into the two-decimal text the table shows. */
export function moneyFromCents(value: number): Money {
  const sign = value < 0 ? "-" : "";
  const absolute = Math.abs(Math.round(value));
  return `${sign}${Math.floor(absolute / 100)}.${String(absolute % 100).padStart(2, "0")}`;
}

/** Grouped for a table cell, still without a float. */
export function formatMoney(text: Money | number | null | undefined): string {
  const value = typeof text === "number" ? text : cents(text);
  if (value === null) return "—";
  const sign = value < 0 ? "-" : "";
  const absolute = Math.abs(value);
  const whole = String(Math.floor(absolute / 100)).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  return `${sign}${whole}.${String(absolute % 100).padStart(2, "0")}`;
}

// ---------------------------------------------------------------------------------------------
// The requests
// ---------------------------------------------------------------------------------------------

/** Build the query string a report read and its export share, so they cannot ask for different
 *  windows — the acceptance box is a row-count comparison, and a comparison across two windows
 *  proves nothing. */
function reportQuery(kind: ReportKind, period: ReportPeriod): string {
  const params = new URLSearchParams();
  if (period.from) params.set("from", period.from);
  if (period.to) params.set("to", period.to);
  const query = params.toString();
  return `/api/v1/accounting/reports/${kind}${query ? `?${query}` : ""}`;
}

/** A `Response` that is not `ok`, turned into the panel's own error. */
async function readFailure(response: Response): Promise<ApiError> {
  const text = await response.text();
  let code = "unknown_error";
  let message = `The API answered with status ${response.status}.`;
  let details: Record<string, unknown> | null = null;
  let requestId: string | null = response.headers.get("x-request-id");
  try {
    const body = JSON.parse(text) as ErrorBody;
    code = body.error?.code ?? code;
    message = body.error?.message ?? message;
    details = body.error?.details ?? null;
    requestId = body.error?.request_id ?? requestId;
  } catch {
    // A non-JSON body is still an error; the status stays in the message.
  }
  return new ApiError(response.status, code, message, details, requestId);
}

/** One JSON call, with the same session and accept header the rest of the panel uses. */
async function reportsRequest<T>(path: string): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, { credentials: "same-origin", headers: { accept: "application/json" } });
  } catch {
    throw new ApiError(0, "network_error", "The Omnion API could not be reached.");
  }
  if (!response.ok) {
    throw await readFailure(response);
  }
  const text = await response.text();
  if (!text) {
    return null as T;
  }
  return JSON.parse(text) as T;
}

/**
 * Read one report.
 *
 * The server answers with an untagged union, so the narrowing happens here rather than in the
 * screen: a screen that trusted `meta.kind` without a guard would crash on a report kind added
 * to the module after this client was written, which is a louder failure than it needs to be.
 */
export async function fetchReport(kind: ReportKind, period: ReportPeriod = {}): Promise<ReportPayload> {
  const payload = await reportsRequest<ReportPayload>(reportQuery(kind, period));
  return narrowReport(payload, kind);
}

/** Reject a payload whose `meta.kind` is not the one that was asked for. */
function narrowReport(payload: ReportPayload, kind: ReportKind): ReportPayload {
  if (!payload || typeof payload !== "object") {
    throw new ApiError(0, "report_mismatch", `The ${kind} report came back empty.`);
  }
  const meta = (payload as { meta?: { kind?: string } }).meta;
  if (!meta || typeof meta.kind !== "string") {
    throw new ApiError(0, "report_mismatch", "The report came back without a header, so the screen cannot tell what it is.");
  }
  if (meta.kind !== kind) {
    throw new ApiError(
      0,
      "report_mismatch",
      `Asked for the ${reportKindLabel(kind)} report and the server answered with ${reportKindLabel(meta.kind)}.`,
    );
  }
  return payload;
}

/**
 * Download the CSV of the report the screen is showing.
 *
 * The server builds the report **once** and renders that same payload through the same
 * `to_csv` the unit test asserts on, so there is no second query to fall out of step — and the
 * response carries `x-omnion-row-count`, which is the acceptance box as a machine-readable fact.
 *
 * There is no PDF here and the screen must not offer one: a PDF needs a font and a layout
 * engine, and an **unverified** PDF is worse than none, because a reader cannot check its
 * numbers and would not know to try.
 */
export async function exportReportCsv(kind: ReportKind, period: ReportPeriod = {}): Promise<void> {
  const path = `${reportQuery(kind, period)}/export`;
  const response = await fetch(path, { credentials: "same-origin" });
  if (!response.ok) {
    throw await readFailure(response);
  }
  const disposition = response.headers.get("content-disposition") ?? "";
  const match = /filename="([^"]+)"/.exec(disposition);
  const blob = await response.blob();
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = match?.[1] ?? `${kind}.csv`;
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  URL.revokeObjectURL(url);
}

// ---------------------------------------------------------------------------------------------
// The one thing the screen must be able to say out loud
// ---------------------------------------------------------------------------------------------

/**
 * The "figures agree" note, or the sentence that says they do not.
 *
 * The REQ asks the screen to carry a note that the totals equal the sum of the rows. Checking
 * that here — over the **same numbers on screen**, in integer cents — is what makes the note a
 * fact rather than a claim: the module guarantees it, and the screen proves it against whatever
 * just arrived. A report whose totals disagree gets a loud sentence instead of a reassuring one.
 *
 * Aging is checked bucket-sum against outstanding-sum; the other three are totals against rows.
 * The export's row count is compared against `meta.row_count` too, because a table that renders
 * fewer rows than the server produced is a different defect from a total that is wrong.
 */
export function reportTotalsAgree(report: ReportPayload): { agree: boolean; note: string } {
  const sum = (values: (Money | number | null | undefined)[]): number | null => {
    let total = 0;
    for (const value of values) {
      const parsed = cents(value);
      if (parsed === null) return null;
      total += parsed;
    }
    return total;
  };

  const shown = (report as { rows?: unknown[] }).rows?.length ?? 0;
  const counted = (report as { meta?: ReportMeta }).meta?.row_count ?? shown;
  const rowsNote =
    shown === counted
      ? `${counted} ${counted === 1 ? "row" : "rows"}`
      : `the server counted ${counted} ${counted === 1 ? "row" : "rows"} but ${shown} arrived`;

  const byKind = (() => {
    const meta = (report as { meta?: ReportMeta }).meta?.kind;
    if (meta === "aging") {
      const aging = report as AgingReport;
      const buckets = sum(aging.buckets?.map((b) => b.amount));
      const outstanding = sum(aging.rows?.map((r) => r.outstanding));
      if (buckets === null || outstanding === null) {
        return { agree: false, note: `A figure could not be read as an amount, so the ${rowsNote} cannot be added up.` };
      }
      if (buckets !== outstanding) {
        return {
          agree: false,
          note: `The buckets add up to ${formatMoney(buckets)} but the invoices are owed ${formatMoney(outstanding)}. Do not quote either figure until this is settled.`,
        };
      }
      return {
        agree: true,
        note: `The buckets add up to ${formatMoney(buckets)}, which is exactly what the ${rowsNote} say is outstanding.`,
      };
    }
    if (meta === "income-expense") {
      const r = report as IncomeExpenseReport;
      const income = sum(r.rows?.map((row) => row.income));
      const expense = sum(r.rows?.map((row) => row.expense));
      if (income === null || expense === null) {
        return { agree: false, note: `A figure could not be read as an amount, so the ${rowsNote} cannot be added up.` };
      }
      if (income !== cents(r.totals?.income) || expense !== cents(r.totals?.expense)) {
        return {
          agree: false,
          note: `The ${rowsNote} add up to ${formatMoney(income)} in and ${formatMoney(expense)} out, which is not what the totals line says.`,
        };
      }
      return {
        agree: true,
        note: `The totals line is the sum of the ${rowsNote}: ${formatMoney(income)} in, ${formatMoney(expense)} out.`,
      };
    }
    if (meta === "cashflow") {
      const r = report as CashflowReport;
      const moneyIn = sum(r.rows?.map((row) => row.money_in));
      const moneyOut = sum(r.rows?.map((row) => row.money_out));
      if (moneyIn === null || moneyOut === null) {
        return { agree: false, note: `A figure could not be read as an amount, so the ${rowsNote} cannot be added up.` };
      }
      if (moneyIn !== cents(r.totals?.money_in) || moneyOut !== cents(r.totals?.money_out)) {
        return {
          agree: false,
          note: `The ${rowsNote} add up to ${formatMoney(moneyIn)} in and ${formatMoney(moneyOut)} out, which is not what the totals line says.`,
        };
      }
      return {
        agree: true,
        note: `The weekly series adds up to ${formatMoney(moneyIn)} in and ${formatMoney(moneyOut)} out, which is the period total.`,
      };
    }
    const r = report as TaxSummaryReport;
    const base = sum(r.rows?.map((row) => row.base));
    const tax = sum(r.rows?.map((row) => row.tax));
    if (base === null || tax === null) {
      return { agree: false, note: `A figure could not be read as an amount, so the ${rowsNote} cannot be added up.` };
    }
    if (base !== cents(r.totals?.base) || tax !== cents(r.totals?.tax)) {
      return {
        agree: false,
        note: `The ${rowsNote} add up to a base of ${formatMoney(base)} and ${formatMoney(tax)} of tax, which is not what the totals line says.`,
      };
    }
    return {
      agree: true,
      note: `The ${rowsNote} add up to a base of ${formatMoney(base)} and ${formatMoney(tax)} of tax, which is the period total.`,
    };
  })();

  return byKind;
}

/**
 * The rows of any report as one flat, printable shape — the table the screen renders and the
 * chart both read, so the chart cannot show a series the table does not have.
 */
export function reportRows(report: ReportPayload): { label: string; values: { name: string; amount: number }[] }[] {
  const meta = (report as { meta?: ReportMeta }).meta?.kind;
  if (meta === "aging") {
    return (report as AgingReport).rows.map((row) => ({
      label: row.number,
      values: [{ name: "Outstanding", amount: cents(row.outstanding) ?? 0 }],
    }));
  }
  if (meta === "income-expense") {
    return (report as IncomeExpenseReport).rows.map((row) => ({
      label: row.month,
      values: [
        { name: "Income", amount: cents(row.income) ?? 0 },
        { name: "Expense", amount: -(cents(row.expense) ?? 0) },
      ],
    }));
  }
  if (meta === "cashflow") {
    return (report as CashflowReport).rows.map((row) => ({
      label: row.week_start,
      values: [
        { name: "In", amount: cents(row.money_in) ?? 0 },
        { name: "Out", amount: -(cents(row.money_out) ?? 0) },
      ],
    }));
  }
  return (report as TaxSummaryReport).rows.map((row) => ({
    label: row.rate_name,
    values: [{ name: "Tax", amount: cents(row.tax) ?? 0 }],
  }));
}