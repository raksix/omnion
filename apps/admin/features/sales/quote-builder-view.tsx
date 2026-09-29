"use client";

/**
 * The quote builder (REQ-052, slice 2): `/sales/quotes/new` and the editable state of
 * `/sales/quotes/{id}`.
 *
 * One component for both, on purpose: a "new quote" screen and an "edit this draft" screen that
 * are two implementations of the same grid are two screens that will drift, and the drift shows up
 * as a draft that saves differently from the quote it was copied from.
 *
 * The three things this screen is careful about:
 *
 * * **It shows the server's totals, not its own.** The footer reads the numbers the API echoed
 *   back after each save. A builder that added up the lines in the browser would let a seller see
 *   one number and send another — and the difference would only appear on a quote with awkward
 *   fractions, which is to say on the quote that mattered.
 * * **The line total shown per row is the one the server stored for that row**, for the same
 *   reason. While a row is being typed it shows a dash, because a number computed locally for an
 *   unsaved row would be a guess.
 * * **The approval banner appears while the seller is still typing**, from the organization's own
 *   threshold, so "this will need a manager" is known before they press send rather than as a
 *   refusal afterwards. Slice 3 adds the request that the banner promises.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useRouter } from "next/navigation";

import { ArrowDown, ArrowUp, Loader2, Plus, Save, Send, Trash2, TriangleAlert } from "lucide-react";

import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";

import { formatMoney, fetchSalesProducts, fieldOf, type SalesProduct } from "@/lib/sales";
import {
  createSalesQuote,
  fetchSalesQuote,
  fetchSalesQuoteVocabulary,
  saveSalesQuoteLines,
  sendSalesQuote,
  updateSalesQuoteHeader,
  blankQuoteLine,
  type SalesQuoteDetail,
  type SalesQuoteLineDraft,
  type SalesQuoteVocabulary,
} from "@/lib/sales-quotes";

import { useSales } from "./sales-parts";

/** The grid's form state. Every field is a string, because every field arrives as a string. */
type Draft = {
  customer_id: string;
  customer_name: string;
  customer_type: "company" | "contact";
  title: string;
  currency: string;
  valid_until: string;
  payment_terms: string;
  reference: string;
  notes: string;
  lines: SalesQuoteLineDraft[];
};

/** Today's day as the API wants it: `YYYY-MM-DD`, read in UTC so it is the API's today too. */
function today(): string {
  const now = new Date();
  return `${now.getUTCFullYear()}-${String(now.getUTCMonth() + 1).padStart(2, "0")}-${String(
    now.getUTCDate(),
  ).padStart(2, "0")}`;
}

/** A day `days` after `from`, in the same UTC calendar. */
function plusDays(from: string, days: number): string {
  const day = new Date(`${from}T00:00:00Z`);
  day.setUTCDate(day.getUTCDate() + days);
  return day.toISOString().slice(0, 10);
}

/**
 * `/sales/quotes/new`, and `/sales/quotes/{id}` when that quote is still editable.
 *
 * `quoteId` decides which: present means the grid edits an existing draft through
 * `PUT /lines` (and the header through `PATCH`), absent means the create posts the whole document.
 */
export function QuoteBuilderView({ quoteId }: { quoteId?: string }) {
  const router = useRouter();
  const { organizationId } = useSales();

  const [draft, setDraft] = useState<Draft | null>(null);
  const [detail, setDetail] = useState<SalesQuoteDetail | null>(null);
  const [products, setProducts] = useState<SalesProduct[]>([]);
  const [vocabulary, setVocabulary] = useState<SalesQuoteVocabulary | null>(null);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [formField, setFormField] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);
  const gridRef = useRef<HTMLDivElement | null>(null);

  // Load the vocabulary and the products the picker draws from. The catalog is fetched once with a
  // generous page size rather than searched per keystroke: a seller picking a product for a quote
  // scans the list, and a combobox that queries on every letter is slower and harder to read.
  useEffect(() => {
    fetchSalesQuoteVocabulary(organizationId)
      .then(setVocabulary)
      .catch(() => setVocabulary(null));
    fetchSalesProducts({ limit: 200, active: true, organization_id: organizationId ?? undefined })
      .then((page) => setProducts(page.items))
      .catch(() => setProducts([]));
  }, [organizationId]);

  useEffect(() => {
    setError(null);
    if (!quoteId) {
      const currency = vocabulary?.default_currency ?? "TRY";
      const validity = vocabulary?.validity_days ?? 30;
      setDraft({
        customer_id: "",
        customer_name: "",
        customer_type: "company",
        title: "",
        currency,
        valid_until: plusDays(today(), validity),
        payment_terms: "",
        reference: "",
        notes: "",
        lines: [blankQuoteLine()],
      });
      setDetail(null);
      return;
    }
    fetchSalesQuote(quoteId, organizationId)
      .then((loaded) => {
        setDetail(loaded);
        setDraft({
          customer_id: loaded.quote.customer.id ?? "",
          customer_name: loaded.quote.customer.name,
          customer_type: loaded.quote.customer.kind,
          title: loaded.quote.title,
          currency: loaded.quote.currency,
          valid_until: loaded.quote.valid_until,
          payment_terms: "",
          reference: loaded.reference,
          notes: loaded.notes,
          lines: loaded.lines.map((line) => ({
            product_id: line.product_id ?? "",
            description: line.description,
            unit: line.unit,
            quantity: line.quantity,
            unit_price: line.unit_price,
            discount_percent: String(line.discount_percent),
            tax_percent: String(line.tax_percent),
          })),
        });
      })
      .catch((problem) => setError(toScreenError(problem, "That quote could not be opened.")));
  }, [quoteId, organizationId, vocabulary, reloadToken]);

  /** The largest discount typed anywhere in the grid, which is what the threshold is compared to. */
  const maxDiscount = useMemo(
    () =>
      (draft?.lines ?? []).reduce((largest, line) => {
        const value = Number(line.discount_percent || "0");
        return Number.isFinite(value) && value > largest ? value : largest;
      }, 0),
    [draft],
  );

  const threshold = vocabulary?.discount_approval_threshold ?? 15;
  const needsApproval = maxDiscount > threshold;

  const setLine = useCallback((index: number, patch: Partial<SalesQuoteLineDraft>) => {
    setDraft((current) => {
      if (!current) return current;
      const lines = current.lines.map((line, position) =>
        position === index ? { ...line, ...patch } : line,
      );
      return { ...current, lines };
    });
  }, []);

  /**
   * Choosing a product fills the row from the catalog: its name, its unit and its price.
   *
   * The price is the product's **default** price, not the price list's, because this row is the
   * "what does this cost" question and the server re-resolves it from the selected list on save —
   * a client-side price list lookup would be a second implementation of a rule the module already
   * owns, and would be the one that drifts.
   */
  const pickProduct = useCallback(
    (index: number, productId: string) => {
      const product = products.find((entry) => entry.id === productId);
      if (!product) {
        setLine(index, { product_id: "" });
        return;
      }
      setLine(index, {
        product_id: product.id,
        description: product.name,
        unit: product.unit,
        unit_price: product.default_price,
        tax_percent: String(product.tax_percent),
      });
    },
    [products, setLine],
  );

  const move = useCallback(
    (index: number, direction: -1 | 1) => {
      setDraft((current) => {
        if (!current) return current;
        const target = index + direction;
        if (target < 0 || target >= current.lines.length) return current;
        const lines = [...current.lines];
        const [row] = lines.splice(index, 1);
        lines.splice(target, 0, row);
        return { ...current, lines };
      });
    },
    [],
  );

  const fieldError = (name: string) => (formField === name ? formError : null);

  const toApiLines = useCallback(
    (source: Draft) =>
      source.lines.map((line) => ({
        product_id: line.product_id === "" ? null : line.product_id,
        description: line.description,
        unit: line.unit,
        quantity: line.quantity,
        unit_price: line.unit_price,
        discount_percent: Number(line.discount_percent || "0"),
        tax_percent: Number(line.tax_percent || "0"),
      })),
    [],
  );

  const persist = useCallback(
    async (then: "stay" | "open" | "send") => {
      if (!draft) return;
      setSaving(true);
      setFormError(null);
      setFormField(null);
      setNotice(null);
      try {
        const lines = toApiLines(draft);
        let saved: SalesQuoteDetail;
        if (quoteId) {
          // The grid and the header are two writes on purpose: the grid is the whole document and
          // the header is the part a seller edits without touching the money.
          saved = await saveSalesQuoteLines(quoteId, lines, organizationId);
          if (
            draft.title !== saved.quote.title ||
            draft.valid_until !== saved.quote.valid_until ||
            draft.reference !== saved.reference ||
            draft.notes !== saved.notes
          ) {
            saved = await updateSalesQuoteHeader(
              quoteId,
              {
                title: draft.title,
                valid_until: draft.valid_until,
                reference: draft.reference,
                notes: draft.notes,
              },
              organizationId,
            );
          }
        } else {
          saved = await createSalesQuote(
            {
              customer_id: draft.customer_id,
              customer_type: draft.customer_type,
              customer_name: draft.customer_name,
              title: draft.title,
              currency: draft.currency,
              valid_until: draft.valid_until,
              payment_terms: draft.payment_terms,
              reference: draft.reference,
              notes: draft.notes,
              lines,
            },
            organizationId,
          );
        }
        setDetail(saved);
        setReloadToken((token) => token + 1);

        if (then === "send") {
          const sent = await sendSalesQuote(saved.quote.id, organizationId);
          setNotice(`${sent.quote.number} sent. Issue the customer a link from its page.`);
          router.push(`/sales/quotes/${sent.quote.id}`);
          return;
        }
        if (then === "open") {
          setNotice(`Saved as ${saved.quote.number}.`);
          router.push(`/sales/quotes/${saved.quote.id}`);
          return;
        }
        setNotice(`Saved as ${saved.quote.number}.`);
      } catch (problem) {
        setFormError(
          problem instanceof Error ? problem.message : "The quote could not be saved.",
        );
        setFormField(fieldOf(problem));
      } finally {
        setSaving(false);
      }
    },
    [draft, organizationId, quoteId, router, toApiLines],
  );

  if (error) {
    return (
      <ErrorState
        error={error}
        onRetry={() => setReloadToken((token) => token + 1)}
      />
    );
  }

  // The totals block reads the **saved** quote. Before the first save there is no server number to
  // show, and a locally computed one would be a guess printed as a total — so the footer says what
  // it is waiting for.
  const totals = detail?.quote.totals;
  const readOnly = detail ? !["draft", "pending_approval", "approved"].includes(detail.quote.status) : false;

  return (
    <form
      className="space-y-3"
      onSubmit={(event) => {
        event.preventDefault();
        void persist("stay");
      }}
    >
      <header className="flex flex-wrap items-end justify-between gap-2">
        <div>
          <h1 className="text-[15px] font-semibold">
            {quoteId ? `Edit ${detail?.quote.number ?? "quote"}` : "New quote"}
          </h1>
          <p className="text-[12.5px] text-muted">
            Prices and totals are the server&apos;s. What this screen shows after a save is what
            the customer will be charged.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={() => router.push("/sales/quotes")}
            data-qa-sales-builder-cancel
            className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            Back to the list
          </button>
          <button
            type="submit"
            disabled={saving || readOnly}
            data-qa-sales-builder-save
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] font-medium disabled:opacity-60"
          >
            {saving ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <Save className="h-3.5 w-3.5" aria-hidden />}
            Save draft
          </button>
          <button
            type="button"
            onClick={() => void persist("send")}
            disabled={saving || readOnly || needsApproval}
            title={
              needsApproval
                ? `A discount over ${threshold}% needs a manager before this can be sent`
                : undefined
            }
            data-qa-sales-builder-send
            className="inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] font-medium text-panel disabled:opacity-60"
          >
            <Send className="h-3.5 w-3.5" aria-hidden />
            Save and send
          </button>
        </div>
      </header>

      {readOnly ? (
        <p className="rounded-md border border-warn/40 bg-[color-mix(in_oklab,var(--warn)_10%,transparent)] px-3 py-2 text-[12.5px]">
          {detail?.quote.number} has been sent, so its lines are frozen. Duplicate it into a new
          draft to change anything — the version the customer read stays exactly as it was.
        </p>
      ) : null}

      {needsApproval && !readOnly ? (
        <p
          data-qa-sales-approval-banner
          className="flex items-start gap-2 rounded-md border border-warn/40 bg-[color-mix(in_oklab,var(--warn)_10%,transparent)] px-3 py-2 text-[12.5px]"
        >
          <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0 text-warn" aria-hidden />
          <span>
            A {maxDiscount}% discount is over this organization&apos;s {threshold}% threshold, so
            this quote needs a manager before it can be sent. Save it as a draft and the approval
            request is raised from the quote&apos;s page.
          </span>
        </p>
      ) : null}

      {notice ? (
        <p data-qa-sales-notice className="rounded-md border border-line bg-canvas px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}
      {formError && !formField ? (
        <p data-qa-sales-form-error className="rounded-md border border-negative/40 px-3 py-2 text-[12.5px] text-negative">
          {formError}
        </p>
      ) : null}

      <section className="grid gap-3 rounded-lg border border-line p-3 md:grid-cols-3">
        <Labelled label="Customer" name="customer_id" error={fieldError("customer_id")}>
          <input
            value={draft?.customer_name ?? ""}
            onChange={(event) => {
              const name = event.target.value;
              // The picker writes the name and the id together; a name typed by hand has no id
              // behind it, which is exactly what a free-text customer is, and the server refuses a
              // quote with no customer — so the field is honest about needing a real record.
              setDraft((current) =>
                current
                  ? {
                      ...current,
                      customer_name: name,
                      customer_id: current.customer_id === "" && name === "" ? "" : current.customer_id,
                    }
                  : current,
              );
            }}
            data-qa-sales-field="customer_id"
            aria-invalid={fieldError("customer_id") ? true : undefined}
            readOnly={readOnly}
            className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
          />
        </Labelled>
        <Labelled label="Title" name="title" error={fieldError("title")}>
          <input
            value={draft?.title ?? ""}
            onChange={(event) => setDraft((current) => (current ? { ...current, title: event.target.value } : current))}
            data-qa-sales-field="title"
            readOnly={readOnly}
            className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
          />
        </Labelled>
        <Labelled label="Valid until" name="valid_until" error={fieldError("valid_until")}>
          <input
            type="date"
            value={draft?.valid_until ?? ""}
            onChange={(event) => setDraft((current) => (current ? { ...current, valid_until: event.target.value } : current))}
            data-qa-sales-field="valid_until"
            readOnly={readOnly}
            className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
          />
        </Labelled>
        <Labelled label="Currency" name="currency" error={fieldError("currency")}>
          <input
            value={draft?.currency ?? ""}
            onChange={(event) =>
              setDraft((current) => (current ? { ...current, currency: event.target.value.toUpperCase() } : current))
            }
            maxLength={3}
            data-qa-sales-field="currency"
            readOnly={readOnly}
            className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] uppercase outline-none focus:border-ink-soft"
          />
        </Labelled>
        <Labelled label="Customer reference (their PO)" name="reference" error={fieldError("reference")}>
          <input
            value={draft?.reference ?? ""}
            onChange={(event) => setDraft((current) => (current ? { ...current, reference: event.target.value } : current))}
            data-qa-sales-field="reference"
            readOnly={readOnly}
            className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
          />
        </Labelled>
        <Labelled label="Notes the customer reads" name="notes" error={fieldError("notes")}>
          <input
            value={draft?.notes ?? ""}
            onChange={(event) => setDraft((current) => (current ? { ...current, notes: event.target.value } : current))}
            data-qa-sales-field="notes"
            readOnly={readOnly}
            className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
          />
        </Labelled>
      </section>

      <section
        ref={gridRef}
        data-qa-sales-lines
        className="space-y-2 rounded-lg border border-line p-3"
      >
        <div className="flex items-center justify-between">
          <h2 className="text-[13px] font-medium">Lines</h2>
          <button
            type="button"
            onClick={() =>
              setDraft((current) =>
                current ? { ...current, lines: [...current.lines, blankQuoteLine()] } : current,
              )
            }
            disabled={readOnly}
            data-qa-sales-line-add
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px] disabled:opacity-60"
          >
            <Plus className="h-3.5 w-3.5" aria-hidden />
            Add a line
          </button>
        </div>

        <div className="overflow-x-auto">
          <table className="w-full min-w-[54rem] border-collapse text-left text-[12.5px]">
            <thead>
              <tr className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
                <th scope="col" className="w-8 px-2 py-1.5">#</th>
                <th scope="col" className="px-2 py-1.5">Product</th>
                <th scope="col" className="px-2 py-1.5">Description</th>
                <th scope="col" className="w-20 px-2 py-1.5">Qty</th>
                <th scope="col" className="w-20 px-2 py-1.5">Unit</th>
                <th scope="col" className="w-28 px-2 py-1.5">Unit price</th>
                <th scope="col" className="w-20 px-2 py-1.5">Disc %</th>
                <th scope="col" className="w-16 px-2 py-1.5">Tax %</th>
                <th scope="col" className="w-28 px-2 py-1.5 text-right">Line total</th>
                <th scope="col" className="w-24 px-2 py-1.5" />
              </tr>
            </thead>
            <tbody>
              {(draft?.lines ?? []).map((line, index) => {
                // The per-row total is the one the **server** stored, matched by position. Before the
                // first save there is none, and a local sum would be a guess printed as a figure.
                const stored = detail?.lines?.[index];
                return (
                  <tr key={index} data-qa-sales-line={index + 1} className="border-b border-line last:border-b-0">
                    <td className="px-2 py-1.5 text-muted">{index + 1}</td>
                    <td className="px-2 py-1.5">
                      <select
                        value={line.product_id}
                        onChange={(event) => pickProduct(index, event.target.value)}
                        disabled={readOnly}
                        data-qa-sales-line-product
                        aria-label={`Product for line ${index + 1}`}
                        className="w-full rounded-md border border-line bg-canvas px-1.5 py-1 text-[12.5px] outline-none disabled:opacity-60"
                      >
                        <option value="">— free text —</option>
                        {products.map((product) => (
                          <option key={product.id} value={product.id}>
                            {product.sku} · {product.name}
                          </option>
                        ))}
                      </select>
                    </td>
                    <td className="px-2 py-1.5">
                      <input
                        value={line.description}
                        onChange={(event) => setLine(index, { description: event.target.value })}
                        disabled={readOnly}
                        data-qa-sales-line-description
                        aria-label={`Description for line ${index + 1}`}
                        className="w-full rounded-md border border-line bg-canvas px-1.5 py-1 text-[12.5px] outline-none disabled:opacity-60"
                      />
                    </td>
                    <td className="px-2 py-1.5">
                      <input
                        inputMode="decimal"
                        value={line.quantity}
                        onChange={(event) => setLine(index, { quantity: event.target.value })}
                        disabled={readOnly}
                        data-qa-sales-line-quantity
                        aria-label={`Quantity for line ${index + 1}`}
                        className="w-full rounded-md border border-line bg-canvas px-1.5 py-1 text-right tabular-nums outline-none disabled:opacity-60"
                      />
                    </td>
                    <td className="px-2 py-1.5">
                      <input
                        value={line.unit}
                        onChange={(event) => setLine(index, { unit: event.target.value })}
                        disabled={readOnly}
                        data-qa-sales-line-unit
                        aria-label={`Unit for line ${index + 1}`}
                        className="w-full rounded-md border border-line bg-canvas px-1.5 py-1 text-[12.5px] outline-none disabled:opacity-60"
                      />
                    </td>
                    <td className="px-2 py-1.5">
                      <input
                        inputMode="decimal"
                        value={line.unit_price}
                        onChange={(event) => setLine(index, { unit_price: event.target.value })}
                        disabled={readOnly}
                        data-qa-sales-line-price
                        aria-label={`Unit price for line ${index + 1}`}
                        className="w-full rounded-md border border-line bg-canvas px-1.5 py-1 text-right tabular-nums outline-none disabled:opacity-60"
                      />
                    </td>
                    <td className="px-2 py-1.5">
                      <input
                        inputMode="numeric"
                        value={line.discount_percent}
                        onChange={(event) => setLine(index, { discount_percent: event.target.value })}
                        disabled={readOnly}
                        data-qa-sales-line-discount
                        aria-label={`Discount percent for line ${index + 1}`}
                        className="w-full rounded-md border border-line bg-canvas px-1.5 py-1 text-right tabular-nums outline-none disabled:opacity-60"
                      />
                    </td>
                    <td className="px-2 py-1.5">
                      <input
                        inputMode="numeric"
                        value={line.tax_percent}
                        onChange={(event) => setLine(index, { tax_percent: event.target.value })}
                        disabled={readOnly}
                        data-qa-sales-line-tax
                        aria-label={`Tax percent for line ${index + 1}`}
                        className="w-full rounded-md border border-line bg-canvas px-1.5 py-1 text-right tabular-nums outline-none disabled:opacity-60"
                      />
                    </td>
                    <td className="px-2 py-1.5 text-right tabular-nums" data-qa-sales-line-total={index + 1}>
                      {stored
                        ? formatMoney(stored.line_total, detail?.quote.currency ?? "TRY")
                        : "—"}
                    </td>
                    <td className="px-2 py-1.5">
                      <span className="flex items-center gap-0.5">
                        <button
                          type="button"
                          onClick={() => move(index, -1)}
                          disabled={readOnly || index === 0}
                          title="Move up"
                          aria-label={`Move line ${index + 1} up`}
                          data-qa-sales-line-up
                          className="rounded border border-line p-1 disabled:opacity-40"
                        >
                          <ArrowUp className="h-3 w-3" aria-hidden />
                        </button>
                        <button
                          type="button"
                          onClick={() => move(index, 1)}
                          disabled={readOnly || index === (draft?.lines.length ?? 1) - 1}
                          title="Move down"
                          aria-label={`Move line ${index + 1} down`}
                          data-qa-sales-line-down
                          className="rounded border border-line p-1 disabled:opacity-40"
                        >
                          <ArrowDown className="h-3 w-3" aria-hidden />
                        </button>
                        <button
                          type="button"
                          onClick={() =>
                            setDraft((current) =>
                              current
                                ? {
                                    ...current,
                                    lines: current.lines.filter((_, position) => position !== index),
                                  }
                                : current,
                            )
                          }
                          disabled={readOnly || (draft?.lines.length ?? 0) <= 1}
                          title="Remove the line"
                          aria-label={`Remove line ${index + 1}`}
                          data-qa-sales-line-remove
                          className="rounded border border-line p-1 disabled:opacity-40"
                        >
                          <Trash2 className="h-3 w-3" aria-hidden />
                        </button>
                      </span>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>

        {fieldError("lines") ? (
          <p data-qa-sales-field-error="lines" className="text-[11.5px] text-negative">
            {fieldError("lines")}
          </p>
        ) : null}

        <div className="flex justify-end">
          <dl className="w-64 space-y-1 text-[12.5px]">
            <Row label="Subtotal" value={totals ? formatMoney(totals.subtotal, detail?.quote.currency ?? "TRY") : "—"} />
            <Row label="Discount" value={totals ? formatMoney(totals.discount_total, detail?.quote.currency ?? "TRY") : "—"} />
            <Row label="Tax" value={totals ? formatMoney(totals.tax_total, detail?.quote.currency ?? "TRY") : "—"} />
            <Row
              label="Total"
              value={totals ? formatMoney(totals.grand_total, detail?.quote.currency ?? "TRY") : "—"}
              strong
            />
            <p className="pt-1 text-[11.5px] text-muted">
              {totals
                ? "Computed by the server from the lines above."
                : "Save once and the server's totals appear here."}
            </p>
          </dl>
        </div>
      </section>
    </form>
  );
}

function Row({ label, value, strong }: { label: string; value: string; strong?: boolean }) {
  return (
    <div className="flex items-center justify-between">
      <dt className={strong ? "font-medium" : "text-muted"}>{label}</dt>
      <dd className={strong ? "font-semibold tabular-nums" : "tabular-nums"}>{value}</dd>
    </div>
  );
}

function Labelled({
  label,
  name,
  error,
  children,
}: {
  label: string;
  name: string;
  error: string | null;
  children: React.ReactNode;
}) {
  return (
    <label className="block text-[12.5px]">
      <span className="mb-1 block text-muted">{label}</span>
      {children}
      {error ? (
        <span data-qa-sales-field-error={name} className="mt-1 block text-[11.5px] text-negative">
          {error}
        </span>
      ) : null}
    </label>
  );
}
