"use client";

/**
 * The payment recorder (REQ-054, slice 3): the drawer that records money and applies it.
 *
 * ## Why this is the hardest form in the module, and what it refuses to do
 *
 * Recording a payment is where three rules meet, and a form that hid any of them would push the
 * failure into a round trip:
 *
 * 1. **An allocation can never exceed the invoice's outstanding.** The server enforces it (422 with
 *    the three numbers: the invoice, what is owed, what was asked) and this form shows the same
 *    arithmetic live, per row, so the operator sees the conflict while typing rather than after
 *    having named three invoices and been refused.
 * 2. **A payment cannot allocate more than it is for.** That is arithmetic, not a business rule, so
 *    the running total under the grid turns red the moment the rows exceed the amount.
 * 3. **Over-allocation of an invoice is a permission**, `accounting.payments.overpay`. The form
 *    sends `allow_overpayment` only when the caller says they hold it; the server refuses otherwise
 *    and the refusal is shown, not swallowed. A form that hid the flag would leave an operator
 *    wondering why the screen "lost" their over-application.
 *
 * ## Why the sweep is a mode rather than another row
 *
 * `auto_allocate` is a boolean in the API and a **radio** here, because the two ways of getting
 * there are mutually exclusive by intent: an operator who has named rows has already said what the
 * money is for, and silently overriding that with a sweep would be the worst possible answer. The
 * choice is exclusive, so it is drawn as an exclusive choice.
 *
 * ## Why the invoice picker is a text input with a list under it
 *
 * A `datalist` over the open invoices, not a `<select>`. A select closes after one choice and then
 * the operator has to reopen it for the second invoice of a three-invoice transfer — and the row
 * grid is where the multi-invoice case lives. The filter box narrows as they type, and the
 * outstanding figure travels with each option so nobody allocates 500.00 against an invoice that
 * owes 120.00.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Loader2, Plus, Trash2, X } from "lucide-react";

import { ApiError } from "@/lib/api";
import { describeError, toScreenError } from "@/components/error-state";

import { fetchInvoices, type InvoiceSummary } from "@/lib/accounting-invoices";
import {
  PAYMENT_METHODS,
  recordPayment,
  remainingCents,
  sumAllocations,
  type AllocationDraft,
  type Payment,
} from "@/lib/accounting-payments";

type Props = {
  onCancel: () => void;
  onRecorded: (payment: Payment) => void;
  /** An invoice to pre-apply against, when the recorder was opened from one. */
  presetInvoice?: { id: string; number: string; outstanding: string } | null;
  /** Whether the caller holds `accounting.payments.overpay`. The server is the arbiter. */
  canOverpay?: boolean;
};

type Mode = "manual" | "sweep" | "unapplied";

const today = () => new Date().toISOString().slice(0, 10);

export function PaymentRecorder({ onCancel, onRecorded, presetInvoice = null, canOverpay = false }: Props) {
  const [amount, setAmount] = useState("");
  const [currency, setCurrency] = useState("USD");
  const [method, setMethod] = useState<string>("bank_transfer");
  const [paidOn, setPaidOn] = useState(today());
  const [reference, setReference] = useState("");
  const [customer, setCustomer] = useState("");
  const [note, setNote] = useState("");
  const [mode, setMode] = useState<Mode>(presetInvoice ? "manual" : "sweep");
  const [rows, setRows] = useState<AllocationDraft[]>(
    presetInvoice ? [{ invoice_id: presetInvoice.id, amount: presetInvoice.outstanding }] : [],
  );
  const [allowOverpay, setAllowOverpay] = useState(false);
  const [open, setOpen] = useState<InvoiceSummary[]>([]);
  const [picker, setPicker] = useState("");
  const [loadingInvoices, setLoadingInvoices] = useState(true);
  const [saving, setSaving] = useState(false);
  // The caught value, held as the type the error state accepts. `unknown` would be the honest
  // annotation for a `catch`, but `describeError` narrows to these four and a screen that stores
  // `unknown` makes every render re-narrow — the narrowing belongs at the catch site, once.
  const [error, setError] = useState<string | Error | ApiError | null>(null);
  const amountRef = useRef<HTMLInputElement>(null);

  // The open invoices, once. They are the only candidates a payment can be applied to — the server
  // refuses a draft, a void and a paid one by name — so a picker that offered anything else would be
  // offering a row that is guaranteed to fail.
  useEffect(() => {
    let live = true;
    setLoadingInvoices(true);
    void fetchInvoices({ limit: 100 })
      .then((list) => {
        if (live) setOpen(list);
      })
      .catch((caught) => {
        if (live) setError(toScreenError(caught, "The open invoices could not be loaded."));
      })
      .finally(() => {
        if (live) setLoadingInvoices(false);
      });
    return () => {
      live = false;
    };
  }, []);

  const byId = useMemo(() => new Map(open.map((invoice) => [invoice.id, invoice])), [open]);

  // What is left to allocate. A negative figure is the one case the form refuses to submit, because
  // the server would refuse it as arithmetic and there is nothing for a permission to change.
  const left = useMemo(() => remainingCents(amount, rows), [amount, rows]);
  const allocated = useMemo(() => sumAllocations(rows), [rows]);

  // A row whose amount is above the invoice's outstanding, named. The same refusal the server
  // writes, in the same three numbers, before the round trip.
  const overRows = useMemo(() => {
    const out: { index: number; invoice: string; outstanding: string; attempted: string }[] = [];
    rows.forEach((row, index) => {
      const invoice = byId.get(row.invoice_id);
      if (!invoice) return;
      const value = Number(row.amount);
      if (!Number.isFinite(value)) return;
      if (Math.round(value * 100) > Math.round(Number(invoice.outstanding) * 100)) {
        out.push({
          index,
          invoice: invoice.number,
          outstanding: invoice.outstanding,
          attempted: row.amount,
        });
      }
    });
    return out;
  }, [rows, byId]);

  const overTotal = left < 0;
  const canSave = amount.trim() !== "" && Number(amount) > 0 && !overTotal && !saving;

  const setRow = useCallback((index: number, patch: Partial<AllocationDraft>) => {
    setRows((current) =>
      current.map((row, at) => (at === index ? { ...row, ...patch } : row)),
    );
  }, []);

  const addRow = useCallback(() => {
    setRows((current) => [...current, { invoice_id: "", amount: "" }]);
  }, []);

  const dropRow = useCallback((index: number) => {
    setRows((current) => current.filter((_, at) => at !== index));
  }, []);

  /** The open invoices the filter still matches, minus the ones already named. */
  const candidates = useMemo(() => {
    const taken = new Set(rows.map((row) => row.invoice_id).filter(Boolean));
    const needle = picker.trim().toLowerCase();
    return open
      .filter((invoice) => !taken.has(invoice.id))
      .filter(
        (invoice) =>
          needle === "" ||
          invoice.number.toLowerCase().includes(needle) ||
          invoice.customer_name.toLowerCase().includes(needle),
      )
      .slice(0, 6);
  }, [open, picker, rows]);

  const save = useCallback(async () => {
    setSaving(true);
    setError(null);
    try {
      const payment = await recordPayment({
        amount: amount.trim(),
        currency: currency.trim() || "USD",
        method,
        paid_on: paidOn || null,
        reference: reference.trim() || null,
        customer_name: customer.trim() || null,
        note: note.trim() || null,
        // The three modes, sent as the API's two booleans plus the rows. `unapplied` is
        // `auto_allocate: false` with no rows — the money is recorded and applied later, which is a
        // real thing a business does and not an incomplete form.
        auto_allocate: mode === "sweep",
        allow_overpayment: allowOverpay || undefined,
        allocations:
          mode === "manual"
            ? rows
                .filter((row) => row.invoice_id && row.amount.trim() !== "")
                .map((row) => ({ invoice_id: row.invoice_id, amount: row.amount.trim() }))
            : [],
      });
      // The **whole** payment, not a summary: the list reloads from the server, and the drawer is
      // not a second source of truth about what the sweep did.
      onRecorded(payment);
    } catch (caught) {
      setError(toScreenError(caught, "The payment could not be recorded."));
    } finally {
      setSaving(false);
    }
  }, [amount, currency, method, paidOn, reference, customer, note, mode, rows, allowOverpay, onRecorded]);

  // Escape closes, and the dialog takes focus on open so the keyboard path starts inside it rather
  // than wherever the page happened to be. A drawer that swallows Escape and strands focus behind
  // the overlay is worse than no drawer.
  const panelRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !saving) {
        event.preventDefault();
        onCancel();
      }
    };
    document.addEventListener("keydown", onKey);
    amountRef.current?.focus();
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel, saving]);

  const described = describeError(error);

  return (
    <div
      className="fixed inset-0 z-50 flex justify-end bg-black/30"
      data-qa-accounting-payment-recorder
      onMouseDown={(event) => {
        if (event.target === event.currentTarget && !saving) onCancel();
      }}
    >
      <div
        ref={panelRef}
        role="dialog"
        aria-modal="true"
        aria-label="Record payment"
        className="flex h-full w-full max-w-[560px] flex-col gap-4 overflow-y-auto border-l border-border bg-background p-5"
      >
        <div className="flex items-start justify-between gap-3">
          <div>
            <h2 className="text-base font-semibold">Record payment</h2>
            <p className="text-[12.5px] text-muted">
              Money that arrived. Apply it here, or leave it unapplied until it is matched.
            </p>
          </div>
          <button
            type="button"
            onClick={onCancel}
            disabled={saving}
            aria-label="Close"
            data-qa-accounting-payment-cancel
            className="rounded-md border border-border p-1.5 disabled:opacity-60"
          >
            <X className="h-4 w-4" aria-hidden />
          </button>
        </div>

        {error ? (
          <div
            role="alert"
            data-qa-accounting-payment-recorder-error
            className="rounded-md border border-red-300 bg-red-50 px-3 py-2 text-[12.5px] text-red-800 dark:border-red-900 dark:bg-red-950/40 dark:text-red-300"
          >
            <p className="font-medium">{described.message}</p>
            {described.code ? <p className="text-[11.5px] opacity-80">{described.code}</p> : null}
            {described.requestId ? (
              <p className="text-[11.5px] opacity-80">
                request <code className="select-all font-mono">{described.requestId}</code>
              </p>
            ) : null}
          </div>
        ) : null}

        <div className="grid gap-3 sm:grid-cols-2">
          <label className="block">
            <span className="text-[12.5px] font-medium">Amount</span>
            <input
              ref={amountRef}
              type="text"
              inputMode="decimal"
              value={amount}
              onChange={(event) => setAmount(event.target.value)}
              placeholder="0.00"
              data-qa-accounting-payment-amount
              className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-sm tabular-nums"
            />
          </label>
          <label className="block">
            <span className="text-[12.5px] font-medium">Received on</span>
            <input
              type="date"
              value={paidOn}
              onChange={(event) => setPaidOn(event.target.value)}
              data-qa-accounting-payment-date
              className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-sm"
            />
          </label>
          <label className="block">
            <span className="text-[12.5px] font-medium">Method</span>
            <select
              value={method}
              onChange={(event) => setMethod(event.target.value)}
              data-qa-accounting-payment-recorder-method
              className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-sm"
            >
              {PAYMENT_METHODS.map((entry) => (
                <option key={entry.value} value={entry.value}>
                  {entry.label}
                </option>
              ))}
            </select>
          </label>
          <label className="block">
            <span className="text-[12.5px] font-medium">Currency</span>
            <input
              type="text"
              value={currency}
              onChange={(event) => setCurrency(event.target.value.toUpperCase().slice(0, 3))}
              data-qa-accounting-payment-currency
              className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-sm uppercase"
            />
          </label>
          <label className="block">
            <span className="text-[12.5px] font-medium">Customer</span>
            <input
              type="text"
              value={customer}
              onChange={(event) => setCustomer(event.target.value)}
              placeholder="Optional — an unidentified receipt is a real row"
              data-qa-accounting-payment-customer
              className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-sm"
            />
          </label>
          <label className="block">
            <span className="text-[12.5px] font-medium">Reference</span>
            <input
              type="text"
              value={reference}
              onChange={(event) => setReference(event.target.value)}
              placeholder="Bank reference or cheque number"
              data-qa-accounting-payment-reference
              className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-sm"
            />
          </label>
        </div>

        <fieldset className="space-y-2">
          <legend className="text-[12.5px] font-medium">How is it applied?</legend>
          <div className="flex flex-wrap gap-1.5">
            {(
              [
                { value: "sweep", label: "Oldest invoices first", hint: "The server walks the open invoices by due date." },
                { value: "manual", label: "Name the invoices", hint: "Apply it to specific invoices below." },
                { value: "unapplied", label: "Leave unapplied", hint: "Record the money; match it later." },
              ] as { value: Mode; label: string; hint: string }[]
            ).map((option) => (
              <label
                key={option.value}
                className={`cursor-pointer rounded-md border px-2.5 py-1.5 text-[12.5px] ${
                  mode === option.value
                    ? "border-primary bg-primary/5 font-medium"
                    : "border-border hover:bg-muted/40"
                }`}
              >
                <input
                  type="radio"
                  name="allocation-mode"
                  value={option.value}
                  checked={mode === option.value}
                  onChange={() => setMode(option.value)}
                  data-qa-accounting-payment-mode={option.value}
                  className="mr-1.5"
                />
                {option.label}
              </label>
            ))}
          </div>
          <p className="text-[11.5px] text-muted">
            {mode === "sweep"
              ? "The open invoices are swept by due date, oldest first. The server decides which ones it takes."
              : mode === "manual"
                ? "Name each invoice and how much of the payment goes to it."
                : "The money is recorded and nothing is applied. It shows as unapplied until you match it."}
          </p>
        </fieldset>

        {mode === "manual" ? (
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <h3 className="text-[12.5px] font-medium">Applied to</h3>
              <button
                type="button"
                onClick={addRow}
                data-qa-accounting-payment-add-allocation
                className="inline-flex h-7 items-center gap-1 rounded-md border border-border px-2 text-[12.5px]"
              >
                <Plus className="h-3.5 w-3.5" aria-hidden />
                Add invoice
              </button>
            </div>

            {rows.length === 0 ? (
              <p
                data-qa-accounting-payment-no-allocations
                className="rounded-md border border-dashed border-border px-3 py-3 text-[12.5px] text-muted"
              >
                No invoice named. The payment will be recorded as unapplied — which is a real state,
                not a half-finished form.
              </p>
            ) : null}

            {rows.map((row, index) => {
              const invoice = byId.get(row.invoice_id);
              const conflict = overRows.find((entry) => entry.index === index);
              return (
                <div key={index} className="space-y-1.5">
                  <div className="flex items-end gap-2">
                    <label className="block min-w-0 flex-1">
                      <span className="text-[11.5px] text-muted">Invoice {index + 1}</span>
                      <input
                        type="text"
                        value={invoice ? `${invoice.number} — ${invoice.customer_name}` : row.invoice_id}
                        onChange={(event) => {
                          const needle = event.target.value;
                          const match = open.find(
                            (candidate) =>
                              `${candidate.number} — ${candidate.customer_name}`
                                .toLowerCase()
                                .startsWith(needle.toLowerCase()),
                          );
                          setRow(index, { invoice_id: match ? match.id : "" });
                          setPicker("");
                        }}
                        placeholder={loadingInvoices ? "Loading open invoices…" : "Number or customer"}
                        data-qa-accounting-payment-allocation-invoice={index}
                        className="mt-0.5 h-8 w-full rounded-md border border-border bg-background px-2 text-sm"
                      />
                    </label>
                    <label className="block w-32">
                      <span className="text-[11.5px] text-muted">Amount</span>
                      <input
                        type="text"
                        inputMode="decimal"
                        value={row.amount}
                        onChange={(event) => setRow(index, { amount: event.target.value })}
                        placeholder={invoice ? `max ${invoice.outstanding}` : "0.00"}
                        data-qa-accounting-payment-allocation-amount={index}
                        className="mt-0.5 h-8 w-full rounded-md border border-border bg-background px-2 text-sm tabular-nums"
                      />
                    </label>
                    <button
                      type="button"
                      onClick={() => dropRow(index)}
                      aria-label={`Remove invoice ${index + 1}`}
                      data-qa-accounting-payment-remove-allocation={index}
                      className="h-8 rounded-md border border-border px-2 text-muted hover:text-danger"
                    >
                      <Trash2 className="h-3.5 w-3.5" aria-hidden />
                    </button>
                  </div>
                  {invoice ? (
                    <p className="text-[11.5px] text-muted">
                      {invoice.customer_name} — owes {invoice.currency} {invoice.outstanding}
                    </p>
                  ) : null}
                  {conflict ? (
                    <p
                      role="alert"
                      data-qa-accounting-payment-over-allocation={conflict.index}
                      className="flex items-start gap-1.5 rounded-md bg-amber-500/10 px-2 py-1.5 text-[11.5px] text-amber-800 dark:text-amber-300"
                    >
                      <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden />
                      {conflict.invoice} is owed {conflict.outstanding}, and this row applies {conflict.attempted}.
                      {canOverpay
                        ? " The override is on for this save."
                        : " Recording it needs the over-payment permission."}
                    </p>
                  ) : null}
                </div>
              );
            })}

            {candidates.length > 0 ? (
              <div className="rounded-md border border-border">
                <p className="border-b border-border px-2 py-1.5 text-[11.5px] text-muted">
                  Open invoices you can still apply to
                </p>
                <ul>
                  {candidates.map((candidate) => (
                    <li key={candidate.id}>
                      <button
                        type="button"
                        onClick={() => {
                          setRows((current) => [
                            ...current,
                            { invoice_id: candidate.id, amount: candidate.outstanding },
                          ]);
                          setPicker("");
                        }}
                        data-qa-accounting-payment-candidate={candidate.number}
                        className="flex w-full items-center justify-between gap-2 px-2 py-1.5 text-left text-[12.5px] hover:bg-muted/50"
                      >
                        <span>
                          {candidate.number} — {candidate.customer_name}
                        </span>
                        <span className="tabular-nums text-muted">
                          owes {candidate.currency} {candidate.outstanding}
                        </span>
                      </button>
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}
          </div>
        ) : null}

        <label className="block">
          <span className="text-[12.5px] font-medium">Note</span>
          <textarea
            value={note}
            onChange={(event) => setNote(event.target.value)}
            rows={2}
            placeholder="Optional"
            data-qa-accounting-payment-note
            className="mt-1 w-full rounded-md border border-border bg-background px-2 py-1.5 text-sm"
          />
        </label>

        {canOverpay ? (
          <label className="inline-flex items-center gap-1.5 text-[12.5px]">
            <input
              type="checkbox"
              checked={allowOverpay}
              onChange={(event) => setAllowOverpay(event.target.checked)}
              data-qa-accounting-payment-allow-overpay
            />
            Allow applying more than an invoice is owed
          </label>
        ) : null}

        <div className="mt-auto flex flex-wrap items-center justify-between gap-2 border-t border-border pt-3">
          <p
            data-qa-accounting-payment-recorder-totals
            className={`text-[12.5px] tabular-nums ${
              overTotal ? "font-medium text-danger" : "text-muted"
            }`}
          >
            {rows.length === 0 ? (
              <>Nothing applied yet — the whole amount stays unapplied.</>
            ) : (
              <>
                {currency} {allocated} of {currency} {amount || "0.00"} applied
                {left < 0
                  ? ` — ${currency} ${(Math.abs(left) / 100).toFixed(2)} more than the payment is for.`
                  : left > 0
                    ? ` — ${currency} ${(left / 100).toFixed(2)} will stay unapplied.`
                    : "."}
              </>
            )}
          </p>
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={onCancel}
              disabled={saving}
              data-qa-accounting-payment-cancel-footer
              className="inline-flex h-8 items-center rounded-md border border-border px-3 text-sm disabled:opacity-60"
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={() => void save()}
              disabled={!canSave}
              data-qa-accounting-payment-save
              className="inline-flex h-8 items-center gap-1.5 rounded-md bg-primary px-3 text-sm font-medium text-primary-foreground disabled:opacity-50"
            >
              {saving ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
              {saving ? "Recording…" : "Record payment"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

/** The reason `ApiError` is imported here even though the type flows through `error`. */
export type RecorderError = ApiError;
