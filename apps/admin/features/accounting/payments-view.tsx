"use client";

/**
 * The payments list (REQ-054, slice 3): `/accounting/payments`.
 *
 * ## Why the recorder is a drawer and not a route
 *
 * Recording a payment is the most frequent thing a bookkeeper does in this module, and it always
 * starts from somewhere else: a list they are reading, an invoice they just opened. A route would
 * throw that context away and the back button would have to restore it. The drawer keeps the list
 * behind it, so after saving the operator is looking at the list with the new row in it — which is
 * also the only way they can see that the sweep applied the money to invoices they did not name.
 *
 * ## Why the row prints `amount / allocated / unallocated` and not one number
 *
 * The same argument as the invoice list, one level down: a payment that has not been applied yet is
 * not the same thing as a payment that has, and a screen that shows only the amount cannot tell the
 * reader which one they are looking at. `unallocated` is the money the customer is owed back — the
 * figure that decides whether somebody has to chase an application, and the one that is missing
 * entirely from a payments table with a single `amount` column.
 *
 * ## Why a reversed payment stays on the screen, struck through
 *
 * The row is a document, and a reversed document still happened. Deleting it would break the
 * numbering (`PAY-000007` would vanish from the sequence the audit trail quotes) and would leave
 * the counter entry in the journal pointing at nothing. It is hidden by default instead — the
 * `unreversed_only` switch is on — and the switch is on the screen so the operator who *is* looking
 * for it can find it.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { useRouter } from "next/navigation";
import { Banknote, Plus, Search } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  ALLOCATION_STATES,
  PAYMENT_METHODS,
  allocationStateLabel,
  fetchPayments,
  paymentMethodLabel,
  type AllocationState,
  type PaymentFilters,
  type PaymentSummary,
} from "@/lib/accounting-payments";

import { PaymentRecorder } from "./payment-recorder";

/** The badge's colour, by what the state means rather than by how it looks. */
function stateClass(state: string): string {
  switch (state) {
    case "applied":
      return "bg-emerald-500/10 text-emerald-700 dark:text-emerald-400";
    case "partial":
      // Money that arrived and is not fully placed is a thing somebody has to come back to, so it
      // is amber rather than neutral. Neutral would read as "nothing to do here".
      return "bg-amber-500/10 text-amber-700 dark:text-amber-400";
    default:
      // Unapplied: a deposit sitting in the account that no invoice has claimed. The most
      // outstanding state on the screen, and it must not read as quiet.
      return "bg-blue-500/10 text-blue-700 dark:text-blue-400";
  }
}

function methodClass(method: string): string {
  return method === "cash" ? "bg-muted text-muted-foreground" : "bg-muted/60 text-muted-foreground";
}

function money(value: string, currency: string): string {
  return `${currency} ${value}`;
}

export function PaymentsView() {
  const router = useRouter();
  const [rows, setRows] = useState<PaymentSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [method, setMethod] = useState("");
  const [state, setState] = useState("");
  const [showReversed, setShowReversed] = useState(false);
  const [search, setSearch] = useState("");
  const [applied, setApplied] = useState("");
  const [recording, setRecording] = useState(false);

  const filters = useMemo<PaymentFilters>(
    () => ({
      method: method || undefined,
      search: applied || undefined,
      // The default view hides history. The switch is the only way to ask for it, so the screen
      // has to say that the rows exist rather than pretending they do not.
      unreversed_only: showReversed ? undefined : true,
    }),
    [method, applied, showReversed],
  );

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const page = await fetchPayments(filters);
      setRows(page.items);
    } catch (caught) {
      setError(toScreenError(caught, "The payments could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [filters]);

  useEffect(() => {
    void load();
  }, [load]);

  // The state filter is a *client* filter, and deliberately so: the API has no
  // `allocation_state` filter (the column is computed from two others, not stored), and a
  // server-side filter would need an index on an expression the module does not maintain. The
  // page is bounded by `limit`, so this filters what was loaded rather than pretending to filter
  // everything — the empty state below says "on this page" for exactly that reason.
  const visible = useMemo(
    () => (state ? rows.filter((row) => row.allocation_state === (state as AllocationState)) : rows),
    [rows, state],
  );

  const recorded = useCallback(
    (payment: PaymentSummary) => {
      setRecording(false);
      // The sentence is built from the server's own figures, not from the form: the sweep may have
      // applied the money to invoices the operator never named, so "what you typed" is not a
      // reliable summary of what happened.
      setNotice(
        payment.allocation_state === "applied"
          ? `${payment.number} recorded — ${money(payment.allocated, payment.currency)} applied.`
          : `${payment.number} recorded. ${money(payment.unallocated, payment.currency)} is unapplied and owed back until it is placed.`,
      );
      void load();
    },
    [load],
  );

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-lg font-semibold">Payments</h1>
          <p className="text-[12.5px] text-muted">
            Money that arrived. Applied against invoices, or held as customer credit until it is.
          </p>
        </div>
        <button
          type="button"
          onClick={() => setRecording(true)}
          data-qa-accounting-payment-record
          className="inline-flex h-8 items-center gap-1.5 rounded-md bg-primary px-2.5 text-sm font-medium text-primary-foreground"
        >
          <Plus className="h-4 w-4" aria-hidden />
          Record payment
        </button>
      </div>

      {notice ? (
        <p
          role="status"
          data-qa-accounting-payment-notice
          className="rounded-md border border-border bg-muted/40 px-3 py-2 text-[12.5px]"
        >
          {notice}
        </p>
      ) : null}

      <div className="flex flex-wrap items-center gap-2">
        <label className="inline-flex items-center gap-1.5 text-[12.5px]">
          <span className="text-muted">Method</span>
          <select
            value={method}
            onChange={(event) => setMethod(event.target.value)}
            data-qa-accounting-payment-method
            className="h-8 rounded-md border border-border bg-background px-2 text-sm"
          >
            <option value="">All</option>
            {PAYMENT_METHODS.map((entry) => (
              <option key={entry.value} value={entry.value}>
                {entry.label}
              </option>
            ))}
          </select>
        </label>

        <div className="flex flex-wrap gap-1" role="tablist" aria-label="Allocation state">
          <button
            type="button"
            role="tab"
            aria-selected={state === ""}
            onClick={() => setState("")}
            data-qa-accounting-payment-state-tab="all"
            className={`h-7 rounded-md px-2.5 text-[12.5px] ${
              state === "" ? "bg-muted font-medium text-foreground" : "text-muted-foreground hover:text-foreground"
            }`}
          >
            All
          </button>
          {ALLOCATION_STATES.map((entry) => (
            <button
              key={entry.value}
              type="button"
              role="tab"
              aria-selected={state === entry.value}
              onClick={() => setState(entry.value)}
              data-qa-accounting-payment-state-tab={entry.value}
              className={`h-7 rounded-md px-2.5 text-[12.5px] ${
                state === entry.value
                  ? "bg-muted font-medium text-foreground"
                  : "text-muted-foreground hover:text-foreground"
              }`}
            >
              {entry.label}
            </button>
          ))}
        </div>

        <label className="inline-flex items-center gap-1.5 text-[12.5px]">
          <input
            type="checkbox"
            checked={showReversed}
            onChange={(event) => setShowReversed(event.target.checked)}
            data-qa-accounting-payment-show-reversed
          />
          Include reversed
        </label>

        <form
          className="ml-auto inline-flex items-center gap-1.5"
          onSubmit={(event) => {
            event.preventDefault();
            setApplied(search.trim());
          }}
        >
          <label className="sr-only" htmlFor="accounting-payment-search">
            Search payments
          </label>
          <input
            id="accounting-payment-search"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Number, customer or reference"
            data-qa-accounting-payment-search
            className="h-8 w-56 rounded-md border border-border bg-background px-2 text-sm"
          />
          <button
            type="submit"
            data-qa-accounting-payment-search-submit
            className="inline-flex h-8 items-center gap-1 rounded-md border border-border px-2 text-sm"
          >
            <Search className="h-4 w-4" aria-hidden />
            Search
          </button>
        </form>
      </div>

      {error ? (
        <ErrorState error={error} onRetry={() => void load()} qa="accounting-payment-error" />
      ) : loading ? (
        <LoadingTable columns={7} rows={5} />
      ) : visible.length === 0 ? (
        <div data-qa-accounting-payments className="rounded-lg border border-border">
          <EmptyState
            title={
              rows.length > 0
                ? "No payment on this page has that state."
                : applied
                  ? "No payment matches that search."
                  : "No payment yet."
            }
            hint={
              rows.length > 0
                ? `All ${rows.length} payments on this page were filtered out. Clear the state tab to see them.`
                : applied
                  ? "The search covers the number, the customer and the bank reference."
                  : "A payment is money that arrived. Record it, then apply it to the invoices it settles — or let the oldest-first sweep do that for you."
            }
            action={
              <button
                type="button"
                onClick={() => setRecording(true)}
                className="inline-flex h-8 items-center gap-1.5 rounded-md bg-primary px-3 text-sm font-medium text-primary-foreground"
              >
                <Plus className="h-4 w-4" aria-hidden />
                Record payment
              </button>
            }
          />
        </div>
      ) : (
        <div data-qa-accounting-payments className="overflow-x-auto rounded-lg border border-border">
          <table className="w-full min-w-[900px] text-sm">
            <thead>
              <tr className="border-b border-border text-left text-[12px] text-muted">
                <th scope="col" className="px-3 py-2 font-medium">Number</th>
                <th scope="col" className="px-3 py-2 font-medium">Customer</th>
                <th scope="col" className="px-3 py-2 font-medium">Received</th>
                <th scope="col" className="px-3 py-2 font-medium">Method</th>
                <th scope="col" className="px-3 py-2 text-right font-medium">Amount</th>
                <th scope="col" className="px-3 py-2 text-right font-medium">Applied</th>
                <th scope="col" className="px-3 py-2 font-medium">State</th>
              </tr>
            </thead>
            <tbody>
              {visible.map((payment) => (
                <tr
                  key={payment.id}
                  tabIndex={0}
                  role="link"
                  onClick={() => router.push(`/accounting/payments/${payment.id}`)}
                  onKeyDown={(event) => {
                    // The row is the only way to reach the detail from a list, so Enter and Space
                    // both have to work — a `role="link"` that only answers the mouse is not a
                    // link to a keyboard user, and the table is the screen's primary surface.
                    if (event.key === "Enter" || event.key === " ") {
                      event.preventDefault();
                      router.push(`/accounting/payments/${payment.id}`);
                    }
                  }}
                  data-qa-accounting-payment-row={payment.number}
                  className={`cursor-pointer border-b border-border/60 last:border-0 hover:bg-muted/40 ${
                    payment.reversed ? "text-muted line-through" : ""
                  }`}
                >
                  <td className="px-3 py-2 font-medium">
                    {payment.number}
                    {payment.reversed ? (
                      <span className="ml-1.5 text-[11px] font-normal no-underline" title="This payment was reversed.">
                        reversed
                      </span>
                    ) : null}
                  </td>
                  <td className="px-3 py-2">{payment.customer_name || "—"}</td>
                  <td className="px-3 py-2 text-muted">{payment.paid_on}</td>
                  <td className="px-3 py-2">
                    <span
                      className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11.5px] font-medium ${methodClass(
                        payment.method,
                      )}`}
                      data-qa-accounting-payment-method-badge={payment.number}
                    >
                      {paymentMethodLabel(payment.method)}
                    </span>
                  </td>
                  <td className="px-3 py-2 text-right tabular-nums">
                    {money(payment.amount, payment.currency)}
                  </td>
                  <td className="px-3 py-2 text-right tabular-nums">
                    <div>{money(payment.allocated, payment.currency)}</div>
                    {payment.unallocated !== "0.00" ? (
                      <div className="text-[11.5px] text-muted no-underline">
                        {money(payment.unallocated, payment.currency)} unapplied
                      </div>
                    ) : null}
                  </td>
                  <td className="px-3 py-2">
                    <span
                      className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11.5px] font-medium ${stateClass(
                        payment.allocation_state,
                      )}`}
                      data-qa-accounting-payment-state={payment.number}
                    >
                      {allocationStateLabel(payment.allocation_state)}
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {recording ? (
        <PaymentRecorder
          onCancel={() => setRecording(false)}
          onRecorded={recorded}
        />
      ) : null}
    </div>
  );
}

/** The icon the module nav and the empty state share, so a new row does not drift. */
export const PaymentIcon = Banknote;
