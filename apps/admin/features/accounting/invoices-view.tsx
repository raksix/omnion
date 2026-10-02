"use client";

/**
 * The invoice list (REQ-054, slice 2): `/accounting/invoices`.
 *
 * ## Why the status tabs and the "Overdue only" switch are both there
 *
 * They read the same column, and the module's own `overdue_only` filter is the *definition* —
 * `due_date < today` on a receivable. The tab and the red date on each row therefore cannot
 * disagree, which is the whole reason the filter is a server query rather than a client `filter()`
 * over rows that were already loaded. A client-side filter over a limited page would show "no
 * overdue invoices" on a page that simply did not contain one.
 *
 * ## Why the row's amounts are `total / paid / outstanding` and not a single number
 *
 * A finance screen that shows one number makes the reader do the subtraction, and the reader
 * gets it wrong. `outstanding` is the server's own figure (`grand_total - paid_total`,
 * computed rather than stored) and the three are printed together so the eye can check
 * `paid + outstanding = total` without arithmetic.
 *
 * ## Why "Check overdue now" is a button on this screen
 *
 * The sweep is normally the automation's job, and wave 3 owns the trigger. But an operator who
 * believes a late invoice has not turned red needs a way to ask rather than a way to file a
 * ticket, and the sweep is idempotent — the guard is `overdue_at is null` inside the UPDATE — so
 * pressing it twice is not a way to break anything.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { useRouter } from "next/navigation";
import { Plus, RefreshCw, Search } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  INVOICE_STATUSES,
  fetchInvoices,
  invoiceStatusLabel,
  sweepOverdue,
  type InvoiceFilters,
  type InvoiceSummary,
} from "@/lib/accounting-invoices";

/** The tab strip. "All" is not a status — it is the absence of one. */
const TABS: { value: string; label: string }[] = [
  { value: "", label: "All" },
  ...INVOICE_STATUSES.map((status) => ({ value: status.value, label: status.label })),
];

/** The badge's colour, by what the status means rather than by how it looks. */
function badgeClass(status: string): string {
  switch (status) {
    case "paid":
      return "bg-emerald-500/10 text-emerald-700 dark:text-emerald-400";
    case "overdue":
      return "bg-red-500/10 text-red-700 dark:text-red-400";
    case "sent":
    case "partially_paid":
      return "bg-amber-500/10 text-amber-700 dark:text-amber-400";
    case "draft":
      return "bg-muted text-muted-foreground";
    default:
      // Void. A withdrawn document is visible and must not read as active, but it is not an
      // error either — the number was issued and the audit trail keeps it.
      return "bg-muted text-muted-foreground line-through";
  }
}

/** The due-date hint: red when it is past, amber inside the week, plain otherwise. */
function dueClass(invoice: InvoiceSummary): string {
  if (invoice.status === "void" || invoice.status === "paid") {
    return "text-muted";
  }
  if (invoice.days_past_due > 0) {
    return "text-red-600 dark:text-red-400 font-medium";
  }
  return "text-muted";
}

function money(value: string, currency: string): string {
  return `${currency} ${value}`;
}

export function InvoicesView() {
  const router = useRouter();
  const [rows, setRows] = useState<InvoiceSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [status, setStatus] = useState("");
  const [overdueOnly, setOverdueOnly] = useState(false);
  const [search, setSearch] = useState("");
  const [applied, setApplied] = useState("");
  const [sweeping, setSweeping] = useState(false);

  const filters = useMemo<InvoiceFilters>(
    () => ({
      status: status || undefined,
      overdue_only: overdueOnly || undefined,
      search: applied || undefined,
    }),
    [status, overdueOnly, applied],
  );

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setRows(await fetchInvoices(filters));
    } catch (caught) {
      setError(toScreenError(caught, "The invoices could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [filters]);

  useEffect(() => {
    void load();
  }, [load]);

  const runSweep = useCallback(async () => {
    setSweeping(true);
    setNotice(null);
    try {
      const report = await sweepOverdue();
      setNotice(
        report.flipped === 0
          ? "Nothing is past its due date. Every open invoice was already checked."
          : `${report.flipped} invoice${report.flipped === 1 ? "" : "s"} marked overdue.`,
      );
      await load();
    } catch (caught) {
      setError(toScreenError(caught, "The overdue sweep could not be run."));
    } finally {
      setSweeping(false);
    }
  }, [load]);

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-lg font-semibold">Invoices</h1>
          <p className="text-[12.5px] text-muted">
            Draft, send, chase and void. Totals are computed by the server from the lines.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={() => void runSweep()}
            disabled={sweeping}
            data-qa-accounting-sweep
            className="inline-flex h-8 items-center gap-1.5 rounded-md border border-border px-2.5 text-sm disabled:opacity-60"
          >
            <RefreshCw className={`h-4 w-4 ${sweeping ? "animate-spin" : ""}`} aria-hidden />
            Check overdue now
          </button>
          <button
            type="button"
            onClick={() => router.push("/accounting/invoices/new")}
            data-qa-accounting-invoice-new
            className="inline-flex h-8 items-center gap-1.5 rounded-md bg-primary px-2.5 text-sm font-medium text-primary-foreground"
          >
            <Plus className="h-4 w-4" aria-hidden />
            New invoice
          </button>
        </div>
      </div>

      {notice ? (
        <p
          role="status"
          data-qa-accounting-notice
          className="rounded-md border border-border bg-muted/40 px-3 py-2 text-[12.5px]"
        >
          {notice}
        </p>
      ) : null}

      <div className="flex flex-wrap items-center gap-2">
        <div className="flex flex-wrap gap-1" role="tablist" aria-label="Invoice status">
          {TABS.map((tab) => (
            <button
              key={tab.value || "all"}
              type="button"
              role="tab"
              aria-selected={status === tab.value}
              onClick={() => setStatus(tab.value)}
              data-qa-accounting-invoice-tab={tab.value || "all"}
              className={`h-7 rounded-md px-2.5 text-[12.5px] ${
                status === tab.value
                  ? "bg-muted font-medium text-foreground"
                  : "text-muted-foreground hover:text-foreground"
              }`}
            >
              {tab.label}
            </button>
          ))}
        </div>

        <label className="ml-auto inline-flex items-center gap-1.5 text-[12.5px]">
          <input
            type="checkbox"
            checked={overdueOnly}
            onChange={(event) => setOverdueOnly(event.target.checked)}
            data-qa-accounting-invoice-overdue-only
          />
          Overdue only
        </label>

        <form
          className="inline-flex items-center gap-1.5"
          onSubmit={(event) => {
            event.preventDefault();
            setApplied(search.trim());
          }}
        >
          <label className="sr-only" htmlFor="accounting-invoice-search">
            Search invoices
          </label>
          <input
            id="accounting-invoice-search"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Number or customer"
            data-qa-accounting-invoice-search
            className="h-8 w-56 rounded-md border border-border bg-background px-2 text-sm"
          />
          <button
            type="submit"
            data-qa-accounting-invoice-search-submit
            className="inline-flex h-8 items-center gap-1 rounded-md border border-border px-2 text-sm"
          >
            <Search className="h-4 w-4" aria-hidden />
            Search
          </button>
        </form>
      </div>

      {error ? (
        <ErrorState error={error} onRetry={() => void load()} qa="accounting-invoice-error" />
      ) : loading ? (
        <LoadingTable columns={8} rows={5} />
      ) : rows.length === 0 ? (
        <div
          data-qa-accounting-invoices
          className="rounded-lg border border-border"
        >
          <EmptyState
            title={applied ? "No invoice matches that search." : "No invoice yet."}
            hint={
              applied
                ? "The search covers the number and the customer name."
                : "An invoice is a document with a lifecycle: create it as a draft, send it to the customer, then record what comes back."
            }
            action={
              <button
                type="button"
                onClick={() => router.push("/accounting/invoices/new")}
                className="inline-flex h-8 items-center gap-1.5 rounded-md bg-primary px-3 text-sm font-medium text-primary-foreground"
              >
                <Plus className="h-4 w-4" aria-hidden />
                New invoice
              </button>
            }
          />
        </div>
      ) : (
        <div
          data-qa-accounting-invoices
          className="overflow-x-auto rounded-lg border border-border"
        >
          <table className="w-full min-w-[900px] text-sm">
            <thead>
              <tr className="border-b border-border text-left text-[12px] text-muted">
                <th scope="col" className="px-3 py-2 font-medium">Number</th>
                <th scope="col" className="px-3 py-2 font-medium">Customer</th>
                <th scope="col" className="px-3 py-2 font-medium">Status</th>
                <th scope="col" className="px-3 py-2 font-medium">Issued</th>
                <th scope="col" className="px-3 py-2 font-medium">Due</th>
                <th scope="col" className="px-3 py-2 text-right font-medium">Total</th>
                <th scope="col" className="px-3 py-2 text-right font-medium">Paid</th>
                <th scope="col" className="px-3 py-2 text-right font-medium">Outstanding</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((invoice) => (
                <tr
                  key={invoice.id}
                  tabIndex={0}
                  role="link"
                  onClick={() => router.push(`/accounting/invoices/${invoice.id}`)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter" || event.key === " ") {
                      event.preventDefault();
                      router.push(`/accounting/invoices/${invoice.id}`);
                    }
                  }}
                  data-qa-accounting-invoice-row={invoice.number}
                  className="cursor-pointer border-b border-border/60 last:border-0 hover:bg-muted/40"
                >
                  <td className="px-3 py-2 font-medium">{invoice.number}</td>
                  <td className="px-3 py-2">{invoice.customer_name}</td>
                  <td className="px-3 py-2">
                    <span
                      className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11.5px] font-medium ${badgeClass(invoice.status)}`}
                      data-qa-accounting-invoice-status={invoice.number}
                    >
                      {invoiceStatusLabel(invoice.status)}
                    </span>
                  </td>
                  <td className="px-3 py-2 text-muted">{invoice.issue_date}</td>
                  <td className={`px-3 py-2 ${dueClass(invoice)}`}>
                    {invoice.due_date ?? "—"}
                    {invoice.days_past_due > 0 ? (
                      <span className="ml-1 text-[11.5px]">
                        ({invoice.days_past_due}d late)
                      </span>
                    ) : null}
                  </td>
                  <td className="px-3 py-2 text-right tabular-nums">
                    {money(invoice.grand_total, invoice.currency)}
                  </td>
                  <td className="px-3 py-2 text-right tabular-nums text-muted">
                    {money(invoice.paid_total, invoice.currency)}
                  </td>
                  <td className="px-3 py-2 text-right tabular-nums font-medium">
                    {money(invoice.outstanding, invoice.currency)}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
