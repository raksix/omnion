"use client";

/**
 * The invoice form (REQ-054, slice 2): `/accounting/invoices/new`.
 *
 * ## Why the totals are a preview and the payload cannot carry them
 *
 * `NewInvoicePayload` has no `subtotal` and no `grand_total` field. That is not an omission in
 * the client — the **server's** `NewInvoice` has no such field, so a total posted by a browser
 * would be rejected. Everything the totals block shows is priced in the browser for feedback
 * while typing, and the create call sends lines only; the server prices them again and the
 * screen re-reads its own figures after saving. A form that displayed the server's number before
 * the invoice existed would be showing a number nothing has vouched for.
 *
 * ## Why the discount comes off before the tax, in the preview as well
 *
 * The server's rule — the one the whole workspace inherited — is discount, then tax. The preview
 * runs the same order and the same single rounding, so the figure the operator watches while
 * typing is the figure that comes back. Getting this order wrong in the preview produces a
 * screen that is a cent off on exactly the invoices somebody is about to send, which is worse
 * than having no preview at all.
 *
 * ## Why "Add line" is the only way to grow the grid
 *
 * An empty row that silently contributes a zero line is a document with a heading on it, and the
 * server refuses a zero-quantity line precisely for that reason. So the form starts with one
 * real row and every line the operator leaves with a quantity is a line they meant.
 */
import { useCallback, useMemo, useState } from "react";
import { useRouter } from "next/navigation";
import { ArrowLeft, Plus, Trash2 } from "lucide-react";

import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";

import {
  createInvoice,
  previewTotals,
  type InvoiceLineDraft,
  type NewInvoicePayload,
} from "@/lib/accounting-invoices";

/** One editable row. A fixed key per row so React does not reuse a DOM node across deletions. */
type Row = { key: number; line: InvoiceLineDraft };

let nextKey = 0;
function freshRow(): Row {
  nextKey += 1;
  return {
    key: nextKey,
    line: { description: "", qty: "1", unit_price: "0", discount_percent: "0", tax_percent: "0" },
  };
}

/** Today, in the format the API's date fields want. */
function today(): string {
  return new Date().toISOString().slice(0, 10);
}

export function InvoiceForm() {
  const router = useRouter();
  const [rows, setRows] = useState<Row[]>([freshRow()]);
  const [customerName, setCustomerName] = useState("");
  const [reference, setReference] = useState("");
  const [notes, setNotes] = useState("");
  const [issueDate, setIssueDate] = useState(today);
  const [dueDate, setDueDate] = useState("");
  const [terms, setTerms] = useState("net 30");
  const [currency, setCurrency] = useState("USD");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<ScreenErrorValue>(null);

  const lines = useMemo(() => rows.map((row) => row.line), [rows]);
  const totals = useMemo(() => previewTotals(lines), [lines]);

  const update = useCallback((key: number, patch: Partial<InvoiceLineDraft>) => {
    setRows((current) =>
      current.map((row) => (row.key === key ? { ...row, line: { ...row.line, ...patch } } : row)),
    );
  }, []);

  const remove = useCallback((key: number) => {
    // The last row stays: an invoice with no line row is not a form, it is a blank page.
    setRows((current) => (current.length === 1 ? current : current.filter((row) => row.key !== key)));
  }, []);

  /** A row is worth sending when it names something and prices it above zero. */
  const usable = useCallback(
    (line: InvoiceLineDraft) =>
      (line.description ?? "").trim().length > 0 && Number(line.qty ?? "0") !== 0,
    [],
  );

  const submit = useCallback(async () => {
    setSaving(true);
    setError(null);
    const payload: NewInvoicePayload = {
      customer_name: customerName.trim() || null,
      issue_date: issueDate || null,
      // Omitted rather than sent empty: the server fills `issue_date + 30` itself, and sending
      // `""` would be a parse error rather than a default.
      due_date: dueDate || null,
      currency: currency.trim().toUpperCase() || null,
      payment_terms: terms.trim() || null,
      reference: reference.trim() || null,
      notes: notes.trim() || null,
      lines: lines.filter(usable),
    };
    try {
      const created = await createInvoice(payload);
      router.push(`/accounting/invoices/${created.id}`);
    } catch (caught) {
      setError(toScreenError(caught, "The invoice could not be created."));
    } finally {
      setSaving(false);
    }
  }, [customerName, issueDate, dueDate, currency, terms, reference, notes, lines, usable, router]);

  return (
    <div className="space-y-4" data-qa-accounting-invoice-form>
      <div>
        <button
          type="button"
          onClick={() => router.push("/accounting/invoices")}
          data-qa-accounting-invoice-form-back
          className="mb-1 inline-flex items-center gap-1 text-[12.5px] text-muted hover:text-foreground"
        >
          <ArrowLeft className="h-3.5 w-3.5" aria-hidden />
          Invoices
        </button>
        <h1 className="text-lg font-semibold">New invoice</h1>
        <p className="text-[12.5px] text-muted">
          Saved as a draft. Nothing is sent until the invoice is issued from its own screen.
        </p>
      </div>

      {error ? <ErrorState error={error} onRetry={() => setError(null)} qa="accounting-invoice-form-error" /> : null}

      <div className="grid gap-3 rounded-lg border border-border p-3 sm:grid-cols-2 lg:grid-cols-3">
        <label className="block text-[12.5px]">
          <span className="mb-1 block font-medium">Customer name</span>
          <input
            value={customerName}
            onChange={(event) => setCustomerName(event.target.value)}
            placeholder="Northwind Trading"
            data-qa-accounting-invoice-customer
            className="h-8 w-full rounded-md border border-border bg-background px-2 text-sm"
          />
          <span className="mt-1 block text-[11.5px] text-muted">
            The name this document is issued under. Converting an order copies it for you.
          </span>
        </label>
        <label className="block text-[12.5px]">
          <span className="mb-1 block font-medium">Issue date</span>
          <input
            type="date"
            value={issueDate}
            onChange={(event) => setIssueDate(event.target.value)}
            data-qa-accounting-invoice-issue-date
            className="h-8 w-full rounded-md border border-border bg-background px-2 text-sm"
          />
        </label>
        <label className="block text-[12.5px]">
          <span className="mb-1 block font-medium">Due date</span>
          <input
            type="date"
            value={dueDate}
            min={issueDate || undefined}
            onChange={(event) => setDueDate(event.target.value)}
            data-qa-accounting-invoice-due-date
            className="h-8 w-full rounded-md border border-border bg-background px-2 text-sm"
          />
          <span className="mt-1 block text-[11.5px] text-muted">
            Left empty, the server sets it 30 days after the issue date.
          </span>
        </label>
        <label className="block text-[12.5px]">
          <span className="mb-1 block font-medium">Currency</span>
          <input
            value={currency}
            onChange={(event) => setCurrency(event.target.value)}
            maxLength={3}
            data-qa-accounting-invoice-currency
            className="h-8 w-24 rounded-md border border-border bg-background px-2 text-sm uppercase"
          />
        </label>
        <label className="block text-[12.5px]">
          <span className="mb-1 block font-medium">Payment terms</span>
          <input
            value={terms}
            onChange={(event) => setTerms(event.target.value)}
            placeholder="net 30"
            data-qa-accounting-invoice-terms
            className="h-8 w-full rounded-md border border-border bg-background px-2 text-sm"
          />
        </label>
        <label className="block text-[12.5px]">
          <span className="mb-1 block font-medium">Customer reference</span>
          <input
            value={reference}
            onChange={(event) => setReference(event.target.value)}
            placeholder="PO-2291"
            data-qa-accounting-invoice-reference
            className="h-8 w-full rounded-md border border-border bg-background px-2 text-sm"
          />
        </label>
      </div>

      <div className="overflow-x-auto rounded-lg border border-border">
        <table className="w-full min-w-[820px] text-sm">
          <caption className="sr-only">Invoice lines</caption>
          <thead>
            <tr className="border-b border-border text-left text-[12px] text-muted">
              <th scope="col" className="px-3 py-2 font-medium">Description</th>
              <th scope="col" className="px-3 py-2 text-right font-medium">Qty</th>
              <th scope="col" className="px-3 py-2 text-right font-medium">Unit price</th>
              <th scope="col" className="px-3 py-2 text-right font-medium">Discount %</th>
              <th scope="col" className="px-3 py-2 text-right font-medium">Tax %</th>
              <th scope="col" className="px-3 py-2 text-right font-medium">Line total</th>
              <th scope="col" className="px-3 py-2">
                <span className="sr-only">Remove</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row, index) => {
              const priced = previewTotals([row.line]);
              return (
                <tr key={row.key} data-qa-accounting-invoice-row={index}>
                  <td className="px-3 py-1.5">
                    <input
                      value={row.line.description ?? ""}
                      onChange={(event) => update(row.key, { description: event.target.value })}
                      placeholder="Consultancy, day rate"
                      data-qa-accounting-invoice-line-description={index}
                      className="h-8 w-full rounded-md border border-border bg-background px-2 text-sm"
                    />
                  </td>
                  <td className="px-3 py-1.5">
                    <input
                      value={row.line.qty ?? ""}
                      onChange={(event) => update(row.key, { qty: event.target.value })}
                      inputMode="decimal"
                      data-qa-accounting-invoice-line-qty={index}
                      className="h-8 w-20 rounded-md border border-border bg-background px-2 text-right text-sm tabular-nums"
                    />
                  </td>
                  <td className="px-3 py-1.5">
                    <input
                      value={row.line.unit_price ?? ""}
                      onChange={(event) => update(row.key, { unit_price: event.target.value })}
                      inputMode="decimal"
                      data-qa-accounting-invoice-line-unit={index}
                      className="h-8 w-24 rounded-md border border-border bg-background px-2 text-right text-sm tabular-nums"
                    />
                  </td>
                  <td className="px-3 py-1.5">
                    <input
                      value={row.line.discount_percent ?? ""}
                      onChange={(event) => update(row.key, { discount_percent: event.target.value })}
                      inputMode="decimal"
                      data-qa-accounting-invoice-line-discount={index}
                      className="h-8 w-20 rounded-md border border-border bg-background px-2 text-right text-sm tabular-nums"
                    />
                  </td>
                  <td className="px-3 py-1.5">
                    <input
                      value={row.line.tax_percent ?? ""}
                      onChange={(event) => update(row.key, { tax_percent: event.target.value })}
                      inputMode="decimal"
                      data-qa-accounting-invoice-line-tax={index}
                      className="h-8 w-20 rounded-md border border-border bg-background px-2 text-right text-sm tabular-nums"
                    />
                  </td>
                  <td className="px-3 py-1.5 text-right tabular-nums text-muted">
                    {priced.grandTotal}
                  </td>
                  <td className="px-3 py-1.5 text-right">
                    <button
                      type="button"
                      onClick={() => remove(row.key)}
                      disabled={rows.length === 1}
                      aria-label={`Remove line ${index + 1}`}
                      data-qa-accounting-invoice-line-remove={index}
                      className="inline-flex h-8 w-8 items-center justify-center rounded-md border border-border disabled:opacity-40"
                    >
                      <Trash2 className="h-4 w-4" aria-hidden />
                    </button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          onClick={() => setRows((current) => [...current, freshRow()])}
          data-qa-accounting-invoice-add-line
          className="inline-flex h-8 items-center gap-1.5 rounded-md border border-border px-3 text-sm"
        >
          <Plus className="h-4 w-4" aria-hidden />
          Add line
        </button>
        <button
          type="button"
          onClick={() => void submit()}
          disabled={saving || lines.filter(usable).length === 0}
          data-qa-accounting-invoice-save
          className="ml-auto inline-flex h-8 items-center gap-1.5 rounded-md bg-primary px-3 text-sm font-medium text-primary-foreground disabled:opacity-60"
        >
          {saving ? "Saving…" : "Save draft"}
        </button>
      </div>

      <div className="ml-auto max-w-xs rounded-lg border border-border p-3">
        <h2 className="mb-2 text-[13px] font-medium">Preview</h2>
        <dl className="space-y-1 text-[12.5px]">
          <div className="flex justify-between">
            <dt className="text-muted">Subtotal</dt>
            <dd className="tabular-nums" data-qa-accounting-invoice-preview-subtotal>
              {totals.subtotal}
            </dd>
          </div>
          <div className="flex justify-between">
            <dt className="text-muted">Discount</dt>
            <dd className="tabular-nums">−{totals.discountTotal}</dd>
          </div>
          <div className="flex justify-between">
            <dt className="text-muted">Tax</dt>
            <dd className="tabular-nums">{totals.taxTotal}</dd>
          </div>
          <div className="flex justify-between border-t border-border pt-1 text-[13px] font-medium">
            <dt>Total</dt>
            <dd className="tabular-nums" data-qa-accounting-invoice-preview-total>
              {currency} {totals.grandTotal}
            </dd>
          </div>
        </dl>
        <p className="mt-2 text-[11.5px] text-muted">
          Priced in the browser for feedback. The server recomputes every figure on save.
        </p>
      </div>

      <label className="block max-w-xl text-[12.5px]">
        <span className="mb-1 block font-medium">Notes on the document</span>
        <textarea
          value={notes}
          onChange={(event) => setNotes(event.target.value)}
          rows={3}
          placeholder="Payable by transfer, quoting the invoice number."
          data-qa-accounting-invoice-notes
          className="w-full rounded-md border border-border bg-background px-2 py-1.5 text-sm"
        />
      </label>
    </div>
  );
}
