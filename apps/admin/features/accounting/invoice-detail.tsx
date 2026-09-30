"use client";

/**
 * One invoice (REQ-054, slice 2): `/accounting/invoices/{id}`.
 *
 * ## The three actions, and why only a draft offers two of them
 *
 * Send, void and pay are three different commands with three different preconditions, and the
 * server owns all three. The screen shows what the *current* status permits rather than showing
 * everything greyed out: a void button on a paid invoice is a question the module has already
 * answered, and a screen that asks it anyway teaches the operator to click through refusals.
 * The rule is the server's `is_editable`, read from the status — never a second copy of it here.
 *
 * ## Why the void dialog demands a reason before it enables its button
 *
 * Voiding keeps the number, which means the hole in the sequence is permanent and the only
 * record of why it exists is the reason. An empty-string reason is a column that reads `''` on
 * every withdrawn invoice in the audit trail, which is exactly as informative as a blank. So the
 * field is required in the form and the button is disabled until it has something in it.
 *
 * ## Why the totals block is right-aligned with tabular figures
 *
 * `paid + outstanding = total` is the invariant the operator is really checking when they look at
 * an invoice, so the three are printed as a column of their own with digits that line up. A
 * proportional font puts `1,280.00` and `980.00` a few pixels apart per character and the eye
 * cannot verify the addition at a glance.
 */
import { useCallback, useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { ArrowLeft, Ban, Send, X } from "lucide-react";

import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  fetchInvoice,
  invoiceStatusLabel,
  sendInvoice,
  voidInvoice,
  type Invoice,
} from "@/lib/accounting-invoices";

/** How a status badge is coloured — the same rule the list uses, in one place. */
function badgeClass(status: string): string {
  switch (status) {
    case "paid":
      return "bg-emerald-500/10 text-emerald-700 dark:text-emerald-400";
    case "overdue":
      return "bg-red-500/10 text-red-700 dark:text-red-400";
    case "sent":
    case "partially_paid":
      return "bg-amber-500/10 text-amber-700 dark:text-amber-400";
    default:
      return "bg-muted text-muted-foreground";
  }
}

/** Only a draft may be sent or voided; anything else has left the office. */
function isDraft(invoice: Invoice): boolean {
  return invoice.status === "draft";
}

export function InvoiceDetail({ id }: { id: string }) {
  const router = useRouter();
  const [invoice, setInvoice] = useState<Invoice | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [actionError, setActionError] = useState<ScreenErrorValue>(null);
  const [busy, setBusy] = useState(false);
  const [voiding, setVoiding] = useState(false);
  const [reason, setReason] = useState("");

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setInvoice(await fetchInvoice(id));
    } catch (caught) {
      setError(toScreenError(caught, "The invoice could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  const send = useCallback(async () => {
    setBusy(true);
    setActionError(null);
    try {
      setInvoice(await sendInvoice(id));
    } catch (caught) {
      setActionError(toScreenError(caught, "The invoice could not be sent."));
    } finally {
      setBusy(false);
    }
  }, [id]);

  const confirmVoid = useCallback(async () => {
    setBusy(true);
    setActionError(null);
    try {
      setInvoice(await voidInvoice(id, reason.trim()));
      setVoiding(false);
      setReason("");
    } catch (caught) {
      setActionError(toScreenError(caught, "The invoice could not be voided."));
    } finally {
      setBusy(false);
    }
  }, [id, reason]);

  if (loading) {
    return <LoadingTable columns={4} rows={5} />;
  }

  if (error || !invoice) {
    return (
      <ErrorState
        error={error ?? "This invoice is not here."}
        onRetry={() => void load()}
        action={
          <button
            type="button"
            onClick={() => router.push("/accounting/invoices")}
            className="h-8 rounded-md border border-border px-3 text-sm"
          >
            Back to invoices
          </button>
        }
      />
    );
  }

  const draft = isDraft(invoice);

  return (
    <div className="space-y-4" data-qa-accounting-invoice-detail={invoice.number}>
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <button
            type="button"
            onClick={() => router.push("/accounting/invoices")}
            data-qa-accounting-invoice-back
            className="mb-1 inline-flex items-center gap-1 text-[12.5px] text-muted hover:text-foreground"
          >
            <ArrowLeft className="h-3.5 w-3.5" aria-hidden />
            Invoices
          </button>
          <div className="flex items-center gap-2">
            <h1 className="text-lg font-semibold">{invoice.number}</h1>
            <span
              data-qa-accounting-invoice-detail-status
              className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11.5px] font-medium ${badgeClass(invoice.status)}`}
            >
              {invoiceStatusLabel(invoice.status)}
            </span>
          </div>
          <p className="text-[12.5px] text-muted">
            {invoice.customer_name}
            {invoice.order_number ? ` · converted from ${invoice.order_number}` : ""}
          </p>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          {draft ? (
            <>
              <button
                type="button"
                onClick={() => void send()}
                disabled={busy || invoice.lines.length === 0}
                data-qa-accounting-invoice-send
                className="inline-flex h-8 items-center gap-1.5 rounded-md bg-primary px-3 text-sm font-medium text-primary-foreground disabled:opacity-60"
              >
                <Send className="h-4 w-4" aria-hidden />
                Send to customer
              </button>
              <button
                type="button"
                onClick={() => setVoiding((open) => !open)}
                aria-expanded={voiding}
                data-qa-accounting-invoice-void-open
                className="inline-flex h-8 items-center gap-1.5 rounded-md border border-border px-3 text-sm"
              >
                <Ban className="h-4 w-4" aria-hidden />
                Void
              </button>
            </>
          ) : null}
        </div>
      </div>

      {actionError ? (
        <ErrorState error={actionError} onRetry={() => setActionError(null)} qa="accounting-invoice-action-error" />
      ) : null}

      {voiding && draft ? (
        <div
          data-qa-accounting-invoice-void
          className="space-y-2 rounded-lg border border-border bg-muted/30 p-3"
        >
          <p className="text-[13px] font-medium">Void {invoice.number}</p>
          <p className="text-[12.5px] text-muted">
            The number is kept and the document stays visible under the Void tab. It stops counting
            toward the receivables. A reason is required — it is the only record of why this number
            was withdrawn.
          </p>
          <label className="block text-[12.5px]" htmlFor="accounting-void-reason">
            Reason
          </label>
          <input
            id="accounting-void-reason"
            value={reason}
            onChange={(event) => setReason(event.target.value)}
            placeholder="Duplicate of INV-0004"
            data-qa-accounting-invoice-void-reason
            className="h-8 w-full max-w-md rounded-md border border-border bg-background px-2 text-sm"
          />
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={() => void confirmVoid()}
              disabled={busy || reason.trim().length === 0}
              data-qa-accounting-invoice-void-confirm
              className="inline-flex h-8 items-center gap-1.5 rounded-md bg-destructive px-3 text-sm font-medium text-destructive-foreground disabled:opacity-60"
            >
              Void the invoice
            </button>
            <button
              type="button"
              onClick={() => {
                setVoiding(false);
                setReason("");
              }}
              data-qa-accounting-invoice-void-cancel
              className="inline-flex h-8 items-center gap-1.5 rounded-md border border-border px-3 text-sm"
            >
              <X className="h-4 w-4" aria-hidden />
              Cancel
            </button>
          </div>
        </div>
      ) : null}

      {invoice.status === "void" && invoice.void_reason ? (
        <p
          data-qa-accounting-invoice-void-reason-read
          className="rounded-md border border-border bg-muted/40 px-3 py-2 text-[12.5px]"
        >
          <span className="font-medium">Voided.</span> {invoice.void_reason}
        </p>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_18rem]">
        <div className="overflow-x-auto rounded-lg border border-border">
          <table className="w-full min-w-[640px] text-sm">
            <caption className="sr-only">Lines of {invoice.number}</caption>
            <thead>
              <tr className="border-b border-border text-left text-[12px] text-muted">
                <th scope="col" className="px-3 py-2 font-medium">Description</th>
                <th scope="col" className="px-3 py-2 text-right font-medium">Qty</th>
                <th scope="col" className="px-3 py-2 text-right font-medium">Unit</th>
                <th scope="col" className="px-3 py-2 text-right font-medium">Disc.</th>
                <th scope="col" className="px-3 py-2 text-right font-medium">Tax</th>
                <th scope="col" className="px-3 py-2 text-right font-medium">Total</th>
              </tr>
            </thead>
            <tbody>
              {invoice.lines.map((line) => (
                <tr
                  key={line.id}
                  data-qa-accounting-invoice-line={line.position}
                  className="border-b border-border/60 last:border-0"
                >
                  <td className="px-3 py-2">{line.description}</td>
                  <td className="px-3 py-2 text-right tabular-nums">{line.qty}</td>
                  <td className="px-3 py-2 text-right tabular-nums">{line.unit_price}</td>
                  <td className="px-3 py-2 text-right tabular-nums text-muted">
                    {Number(line.discount_percent) === 0 ? "—" : `${line.discount_percent}%`}
                  </td>
                  <td className="px-3 py-2 text-right tabular-nums text-muted">
                    {Number(line.tax_percent) === 0 ? "—" : `${line.tax_percent}%`}
                  </td>
                  <td className="px-3 py-2 text-right tabular-nums font-medium">{line.line_total}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>

        <div className="space-y-3">
          <div className="rounded-lg border border-border p-3">
            <h2 className="mb-2 text-[13px] font-medium">Totals</h2>
            <dl className="space-y-1 text-[12.5px]">
              <div className="flex justify-between">
                <dt className="text-muted">Subtotal</dt>
                <dd className="tabular-nums">{invoice.subtotal}</dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-muted">Discount</dt>
                <dd className="tabular-nums">−{invoice.discount_total}</dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-muted">Tax</dt>
                <dd className="tabular-nums">{invoice.tax_total}</dd>
              </div>
              <div className="flex justify-between border-t border-border pt-1 text-[13px] font-medium">
                <dt>Total</dt>
                <dd className="tabular-nums" data-qa-accounting-invoice-total>
                  {invoice.currency} {invoice.grand_total}
                </dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-muted">Paid</dt>
                <dd className="tabular-nums" data-qa-accounting-invoice-paid>
                  {invoice.paid_total}
                </dd>
              </div>
              <div className="flex justify-between text-[13px] font-medium">
                <dt>Outstanding</dt>
                <dd className="tabular-nums" data-qa-accounting-invoice-outstanding>
                  {invoice.outstanding}
                </dd>
              </div>
            </dl>
          </div>

          <div className="space-y-1.5 rounded-lg border border-border p-3 text-[12.5px]">
            <h2 className="text-[13px] font-medium">Document</h2>
            <div className="flex justify-between gap-2">
              <span className="text-muted">Issued</span>
              <span className="tabular-nums">{invoice.issue_date}</span>
            </div>
            <div className="flex justify-between gap-2">
              <span className="text-muted">Due</span>
              <span className="tabular-nums">
                {invoice.due_date ?? "—"}
                {invoice.days_past_due > 0 ? (
                  <span className="ml-1 text-red-600 dark:text-red-400">
                    {invoice.days_past_due}d late
                  </span>
                ) : null}
              </span>
            </div>
            <div className="flex justify-between gap-2">
              <span className="text-muted">Terms</span>
              <span>{invoice.payment_terms || "—"}</span>
            </div>
            <div className="flex justify-between gap-2">
              <span className="text-muted">Reference</span>
              <span>{invoice.reference || "—"}</span>
            </div>
            <div className="flex justify-between gap-2">
              <span className="text-muted">Sent</span>
              <span className="tabular-nums">
                {invoice.sent_at ? invoice.sent_at.slice(0, 10) : "—"}
              </span>
            </div>
          </div>

          {invoice.notes ? (
            <div className="rounded-lg border border-border p-3">
              <h2 className="mb-1 text-[13px] font-medium">Notes</h2>
              <p className="whitespace-pre-line text-[12.5px] text-muted">{invoice.notes}</p>
            </div>
          ) : null}
        </div>
      </div>
    </div>
  );
}
