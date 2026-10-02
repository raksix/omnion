"use client";

/**
 * The payment detail (REQ-054, slice 3): one receipt, its allocations and the reversal.
 *
 * ## Why the allocations print the invoice's outstanding *before* this payment
 *
 * Because it makes the row self-checking. `10.00` on its own tells the operator nothing; `10.00 of
 * an invoice that owed 30.00` tells them the payment is a third of the bill and that the invoice
 * is now 20.00 short, without a second request. The server sends the figure (`AllocationView`
 * carries it) precisely so the client never has to fetch each invoice to render a row.
 *
 * ## Why "What this changed" is a strip and not a link
 *
 * The sweep can settle three invoices the operator never named. Leaving that as a row they have to
 * click through hides the most useful fact on the screen — which invoices are now closed — behind
 * three navigations. The strip names each one and its before/after status, and each is still a
 * link for anybody who wants the document.
 *
 * ## Why the reversal asks for a reason in a dialog
 *
 * The server refuses a reversal without one, and a button that fires a guaranteed 422 is a dead
 * button. The dialog is therefore the *only* path to reversing, the reason is required before it
 * can be submitted, and the row keeps the reason afterwards — a reversal is a second document, not
 * an edit, and the audit trail has to be able to read why.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { ArrowLeft, Ban, Loader2, X } from "lucide-react";

import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";

import { invoiceStatusLabel } from "@/lib/accounting-invoices";
import {
  allocationStateLabel,
  fetchPayment,
  paymentMethodLabel,
  reversePayment,
  type Payment,
} from "@/lib/accounting-payments";

function stateClass(state: string): string {
  switch (state) {
    case "applied":
      return "bg-emerald-500/10 text-emerald-700 dark:text-emerald-400";
    case "partial":
      return "bg-amber-500/10 text-amber-700 dark:text-amber-400";
    default:
      return "bg-blue-500/10 text-blue-700 dark:text-blue-400";
  }
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="space-y-0.5">
      <dt className="text-[11.5px] text-muted">{label}</dt>
      <dd className="text-[13px]">{children}</dd>
    </div>
  );
}

export function PaymentDetail({ id }: { id: string }) {
  const router = useRouter();
  const [payment, setPayment] = useState<Payment | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [confirming, setConfirming] = useState(false);
  const [reason, setReason] = useState("");
  const [reversing, setReversing] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setPayment(await fetchPayment(id));
    } catch (caught) {
      setError(toScreenError(caught, "The payment could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  const reverse = useCallback(async () => {
    if (reason.trim() === "") return;
    setReversing(true);
    setError(null);
    try {
      setPayment(await reversePayment(id, reason.trim()));
      setConfirming(false);
      setReason("");
    } catch (caught) {
      setError(toScreenError(caught, "The payment could not be reversed."));
    } finally {
      setReversing(false);
    }
  }, [id, reason]);

  if (error && !payment) {
    return <ErrorState error={error} onRetry={() => void load()} qa="accounting-payment-error" />;
  }
  if (loading) {
    return (
      <p className="py-10 text-center text-[13px] text-muted" data-qa-accounting-payment-loading>
        <Loader2 className="mx-auto mb-2 h-4 w-4 animate-spin" aria-hidden />
        Loading the payment…
      </p>
    );
  }
  if (!payment) {
    return (
      <ErrorState
        error="The payment is gone."
        onRetry={() => router.push("/accounting/payments")}
        qa="accounting-payment-missing"
      />
    );
  }

  return (
    <div className="space-y-4" data-qa-accounting-payment-detail={payment.number}>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={() => router.push("/accounting/payments")}
            aria-label="Back to payments"
            data-qa-accounting-payment-back
            className="rounded-md border border-border p-1.5"
          >
            <ArrowLeft className="h-4 w-4" aria-hidden />
          </button>
          <div>
            <h1 className="text-lg font-semibold">
              {payment.number}
              {payment.reversed ? <span className="ml-2 text-[13px] font-normal text-muted">reversed</span> : null}
            </h1>
            <p className="text-[12.5px] text-muted">
              {payment.customer_name || "Unidentified receipt"} · received {payment.paid_on}
            </p>
          </div>
        </div>
        <div className="flex items-center gap-2">
          <span
            className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11.5px] font-medium ${stateClass(
              payment.allocation_state,
            )}`}
            data-qa-accounting-payment-state={payment.number}
          >
            {allocationStateLabel(payment.allocation_state)}
          </span>
          {!payment.reversed ? (
            <button
              type="button"
              onClick={() => setConfirming(true)}
              data-qa-accounting-payment-reverse
              className="inline-flex h-8 items-center gap-1.5 rounded-md border border-border px-2.5 text-sm"
            >
              <Ban className="h-4 w-4" aria-hidden />
              Reverse
            </button>
          ) : null}
        </div>
      </div>

      {error ? <ErrorState error={error} onRetry={() => void load()} qa="accounting-payment-inline-error" /> : null}

      {payment.reversed ? (
        <div
          role="status"
          data-qa-accounting-payment-reversal-strip
          className="rounded-md border border-border bg-muted/40 px-3 py-2 text-[12.5px]"
        >
          <p className="font-medium">This payment was reversed.</p>
          <p className="text-muted">
            {payment.reversal_reason}
            {payment.reversed_at ? ` · ${payment.reversed_at.slice(0, 10)}` : ""}
          </p>
          <p className="mt-1 text-[11.5px] text-muted">
            The row stays: the money arrived, and the undo is a second document in the journal rather
            than an edit of this one.
          </p>
        </div>
      ) : null}

      <dl className="grid gap-3 rounded-lg border border-border p-4 sm:grid-cols-4">
        <Field label="Amount">
          <span className="tabular-nums">
            {payment.currency} {payment.amount}
          </span>
        </Field>
        <Field label="Applied">
          <span className="tabular-nums">
            {payment.currency} {payment.allocated}
          </span>
        </Field>
        <Field label="Unapplied">
          <span className="tabular-nums">
            {payment.currency} {payment.unallocated}
          </span>
        </Field>
        <Field label="Method">{paymentMethodLabel(payment.method)}</Field>
        {payment.reference ? <Field label="Reference">{payment.reference}</Field> : null}
        <Field label="Journal entry">
          {payment.journal_entry_id ? (
            <code className="font-mono text-[11.5px]">{payment.journal_entry_id.slice(0, 8)}</code>
          ) : (
            "—"
          )}
        </Field>
        {payment.note ? (
          <div className="space-y-0.5 sm:col-span-2">
            <dt className="text-[11.5px] text-muted">Note</dt>
            <dd className="text-[13px]">{payment.note}</dd>
          </div>
        ) : null}
      </dl>

      {payment.settled_invoices.length > 0 ? (
        <section className="rounded-lg border border-border" data-qa-accounting-payment-settled>
          <h2 className="border-b border-border px-3 py-2 text-[13px] font-medium">What this changed</h2>
          <ul className="divide-y divide-border/60">
            {payment.settled_invoices.map((settled) => (
              <li key={settled.invoice_id} className="flex flex-wrap items-center justify-between gap-2 px-3 py-2 text-[13px]">
                <Link
                  href={`/accounting/invoices/${settled.invoice_id}`}
                  data-qa-accounting-payment-settled-invoice={settled.invoice_number}
                  className="font-medium underline-offset-2 hover:underline"
                >
                  {settled.invoice_number}
                </Link>
                <span className="text-muted">
                  {invoiceStatusLabel(settled.status_before)} → {invoiceStatusLabel(settled.status_after)}
                </span>
                <span className="tabular-nums text-muted">
                  now owes {settled.outstanding === "0.00" ? "nothing" : settled.outstanding}
                </span>
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      <section className="rounded-lg border border-border" data-qa-accounting-payment-allocations>
        <h2 className="border-b border-border px-3 py-2 text-[13px] font-medium">
          Applied to {payment.allocations.length === 1 ? "1 invoice" : `${payment.allocations.length} invoices`}
        </h2>
        {payment.allocations.length === 0 ? (
          <p className="px-3 py-4 text-[12.5px] text-muted">
            Nothing is applied yet. This payment is a credit the customer is owed back until it is
            matched against an invoice.
          </p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[640px] text-sm">
              <thead>
                <tr className="border-b border-border text-left text-[12px] text-muted">
                  <th scope="col" className="px-3 py-2 font-medium">Invoice</th>
                  <th scope="col" className="px-3 py-2 font-medium">Customer</th>
                  <th scope="col" className="px-3 py-2 font-medium">Owed before</th>
                  <th scope="col" className="px-3 py-2 text-right font-medium">Applied</th>
                </tr>
              </thead>
              <tbody>
                {payment.allocations.map((allocation) => (
                  <tr
                    key={allocation.id}
                    data-qa-accounting-payment-allocation={allocation.invoice_number}
                    className="border-b border-border/60 last:border-0"
                  >
                    <td className="px-3 py-2">
                      <Link
                        href={`/accounting/invoices/${allocation.invoice_id}`}
                        className="font-medium underline-offset-2 hover:underline"
                      >
                        {allocation.invoice_number}
                      </Link>
                    </td>
                    <td className="px-3 py-2 text-muted">{allocation.invoice_customer}</td>
                    <td className="px-3 py-2 tabular-nums text-muted">
                      {allocation.invoice_outstanding_before}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums font-medium">
                      {allocation.amount}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>

      {confirming ? (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/30 p-4"
          onMouseDown={(event) => {
            if (event.target === event.currentTarget && !reversing) setConfirming(false);
          }}
        >
          <div
            role="dialog"
            aria-modal="true"
            aria-label="Reverse payment"
            className="w-full max-w-md space-y-3 rounded-lg border border-border bg-background p-4"
            data-qa-accounting-payment-reverse-dialog
            onKeyDown={(event) => {
              if (event.key === "Escape" && !reversing) setConfirming(false);
            }}
          >
            <div className="flex items-start justify-between gap-2">
              <h2 className="text-base font-semibold">Reverse {payment.number}</h2>
              <button
                type="button"
                onClick={() => setConfirming(false)}
                disabled={reversing}
                aria-label="Close"
                className="rounded-md border border-border p-1.5 disabled:opacity-60"
              >
                <X className="h-4 w-4" aria-hidden />
              </button>
            </div>
            <p className="text-[12.5px] text-muted">
              The payment stays on the record, the allocations are released and the invoices it
              settled go back to what they owed. A counter entry is written to the journal, dated on
              the original payment.
            </p>
            <label className="block">
              <span className="text-[12.5px] font-medium">Why is it being reversed?</span>
              <textarea
                autoFocus
                value={reason}
                onChange={(event) => setReason(event.target.value)}
                rows={2}
                placeholder="Required — a reversal without a reason is an edit with extra steps"
                data-qa-accounting-payment-reversal-reason
                className="mt-1 w-full rounded-md border border-border bg-background px-2 py-1.5 text-sm"
              />
            </label>
            <div className="flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setConfirming(false)}
                disabled={reversing}
                data-qa-accounting-payment-reversal-cancel
                className="inline-flex h-8 items-center rounded-md border border-border px-3 text-sm disabled:opacity-60"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={() => void reverse()}
                disabled={reason.trim() === "" || reversing}
                data-qa-accounting-payment-reversal-confirm
                className="inline-flex h-8 items-center gap-1.5 rounded-md bg-primary px-3 text-sm font-medium text-primary-foreground disabled:opacity-50"
              >
                {reversing ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
                {reversing ? "Reversing…" : "Reverse payment"}
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}
