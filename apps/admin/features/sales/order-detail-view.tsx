"use client";

/**
 * The order detail (REQ-052, slice 4): `/sales/orders/{id}`.
 *
 * A document in its own right, and the screen has to make three separate things legible at once:
 * the **lines** (a frozen copy of the quote, read-only the moment it is confirmed), the
 * **holds** (what this order has reserved, or gave back), and the **invoice draft** (what
 * accounting has been asked for). The decisions worth naming:
 *
 * * **The grid is read-only, and the screen says why.** A confirmed order's lines are frozen
 *   because the *warehouse* is holding stock against them — a different reason from a sent
 *   quote's, which is frozen because the *customer* read it. The banner under the header says
 *   which, because a greyed-out grid with no explanation is a screen somebody assumes is broken.
 * * **The holds are read, never assumed.** `lines[].reservation` is `null` on a draft and the
 *   screen says "nothing is held yet" rather than printing an empty cell or claiming stock.
 * * **A released hold keeps its reason on the screen.** Cancelling does not delete the holds, so
 *   the person who asks "when did this give that stock back?" reads it here rather than asking.
 * * **Confirming again is allowed and is a no-op**, so the button never greys out after a success.
 *   The server returns the same order; a button that looked broken after a slow response would
 *   make people press it harder.
 */
import { useCallback, useEffect, useState } from "react";
import { useParams, useRouter } from "next/navigation";

import {
  ArrowLeft,
  Ban,
  CheckCircle2,
  Download,
  FileText,
  Loader2,
  PackageCheck,
  Receipt,
} from "lucide-react";

import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";

import { formatTimestamp } from "@/lib/format";
import { formatMoney } from "@/lib/sales";
import { documentNotice, downloadOrderPdf } from "@/lib/sales-documents";
import {
  SALES_ORDER_STATUSES,
  cancelSalesOrder,
  confirmSalesOrder,
  fetchSalesOrder,
  raiseSalesInvoiceDraft,
  reservationNote,
  type SalesOrderDetail,
  type SalesReservationState,
} from "@/lib/sales-orders";

import { useSales } from "./sales-parts";

/** The badge class for a reservation state; the same rule the list uses, in one place. */
function reservationTone(state: SalesReservationState): string {
  switch (state) {
    case "total":
      return "border-positive/40 text-positive";
    case "partial":
      return "border-caution/40 text-caution";
    default:
      return "border-line text-muted";
  }
}

/** `/sales/orders/{id}`: the lines, the holds, the history and the invoice draft. */
export function OrderDetailView() {
  const router = useRouter();
  const params = useParams<{ id: string }>();
  const orderId = params?.id ?? "";
  const { organizationId } = useSales();

  const [detail, setDetail] = useState<SalesOrderDetail | null>(null);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [reloadToken, setReloadToken] = useState(0);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [confirmCancel, setConfirmCancel] = useState(false);
  const [reason, setReason] = useState("");

  const load = useCallback(() => {
    setError(null);
    fetchSalesOrder(orderId, organizationId)
      .then(setDetail)
      .catch((problem) => setError(toScreenError(problem, "That order could not be loaded.")));
  }, [orderId, organizationId]);

  useEffect(load, [load, reloadToken]);

  const run = useCallback(
    async (action: "confirm" | "cancel" | "invoice") => {
      setBusy(action);
      setActionError(null);
      setNotice(null);
      try {
        if (action === "confirm") {
          const next = await confirmSalesOrder(orderId, organizationId);
          setDetail(next);
          setNotice(
            next.order.reservation_state === "total"
              ? "Confirmed. Every line is held for this order."
              : "Confirmed. Some lines are held for this order.",
          );
        } else if (action === "cancel") {
          const next = await cancelSalesOrder(orderId, reason.trim(), organizationId);
          setDetail(next);
          setNotice("Cancelled. The holds were released and the reason is in the timeline.");
        } else {
          const handoff = await raiseSalesInvoiceDraft(orderId, organizationId);
          setNotice(
            handoff.state === "draft"
              ? "Invoice draft handed to accounting. It is a draft until that module issues it."
              : `Invoice ${handoff.state}.`,
          );
          setReloadToken((token) => token + 1);
        }
      } catch (problem) {
        setActionError(
          problem instanceof Error ? problem.message : "That action could not be completed.",
        );
      } finally {
        setBusy(null);
        setConfirmCancel(false);
        setReason("");
      }
    },
    [orderId, organizationId, reason],
  );

  /**
   * The PDF download, separate from `run` for the same reason the quote's is: `run` re-reads the
   * order after every action, and a download changes nothing — reloading would throw away the
   * scroll position of somebody reading the order they just printed.
   */
  const onDownload = useCallback(async () => {
    setBusy("pdf");
    setActionError(null);
    setNotice(null);
    try {
      const document = await downloadOrderPdf(orderId);
      setNotice(documentNotice(document) ?? `Downloaded ${document.filename}.`);
    } catch (problem) {
      setActionError(
        problem instanceof Error
          ? problem.message
          : "The PDF could not be prepared, so nothing was saved.",
      );
    } finally {
      setBusy(null);
    }
  }, [orderId]);

  if (error) {
    return <ErrorState error={error} onRetry={load} />;
  }

  if (!detail) {
    // A skeleton, not a spinner over an empty page: the header, the two columns and the totals
    // block are the shape that is about to arrive, so the page does not jump when it does.
    return (
      <div className="space-y-3" data-qa-sales-order-loading>
        <div className="h-4 w-40 animate-pulse rounded bg-line" />
        <div className="h-20 animate-pulse rounded-lg bg-line/60" />
        <div className="grid gap-3 md:grid-cols-[1fr_18rem]">
          <div className="h-56 animate-pulse rounded-lg bg-line/40" />
          <div className="h-56 animate-pulse rounded-lg bg-line/40" />
        </div>
      </div>
    );
  }

  const { order, lines, history, invoice, totals } = detail;
  const canConfirm = order.status === "draft";
  const canCancel = order.status !== "cancelled" && order.status !== "delivered";
  const canInvoice =
    order.status === "confirmed" || order.status === "invoiced" || order.status === "delivered";

  return (
    <div className="space-y-3">
      <button
        type="button"
        onClick={() => router.push("/sales/orders")}
        data-qa-sales-order-back
        className="inline-flex items-center gap-1.5 text-[12.5px] text-muted hover:text-ink"
      >
        <ArrowLeft className="h-3.5 w-3.5" aria-hidden />
        All orders
      </button>

      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="flex items-center gap-2 text-[15px] font-semibold">
            {order.number}
            <span
              data-qa-sales-order-status={order.status}
              className="rounded-md border border-line px-1.5 py-0.5 text-[11.5px] font-normal"
            >
              {SALES_ORDER_STATUSES.find((entry) => entry.value === order.status)?.label ??
                order.status}
            </span>
          </h1>
          <p className="text-[12.5px] text-muted">
            {order.customer.name || "No customer on file"}
            {order.quote_number ? (
              <>
                {" · from quote "}
                <button
                  type="button"
                  onClick={() => router.push(`/sales/quotes/${order.quote_id}`)}
                  data-qa-sales-order-quote-link
                  className="underline underline-offset-2 hover:text-ink"
                >
                  {order.quote_number}
                </button>
              </>
            ) : (
              " · written by hand"
            )}
            {order.owner ? ` · ${order.owner.name}` : ""}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            onClick={() => void onDownload()}
            disabled={busy !== null}
            data-qa-sales-order-pdf
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-60"
          >
            {busy === "pdf" ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
            ) : (
              <Download className="h-3.5 w-3.5" aria-hidden />
            )}
            PDF
          </button>
          {canConfirm ? (
            <button
              type="button"
              onClick={() => void run("confirm")}
              disabled={busy !== null}
              data-qa-sales-order-confirm
              className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12.5px] font-medium text-panel disabled:opacity-60"
            >
              {busy === "confirm" ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
              ) : (
                <CheckCircle2 className="h-3.5 w-3.5" aria-hidden />
              )}
              Confirm and hold the lines
            </button>
          ) : null}
          {canCancel ? (
            <button
              type="button"
              onClick={() => setConfirmCancel(true)}
              disabled={busy !== null}
              data-qa-sales-order-cancel
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-60"
            >
              <Ban className="h-3.5 w-3.5" aria-hidden />
              Cancel
            </button>
          ) : null}
          {canInvoice ? (
            <button
              type="button"
              onClick={() => void run("invoice")}
              disabled={busy !== null}
              data-qa-sales-order-invoice
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-60"
            >
              {busy === "invoice" ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
              ) : (
                <Receipt className="h-3.5 w-3.5" aria-hidden />
              )}
              {invoice && invoice.state !== "void" ? "Invoice draft" : "Create invoice draft"}
            </button>
          ) : null}
        </div>
      </header>

      {order.status !== "draft" ? (
        <p
          data-qa-sales-order-frozen
          className="rounded-md border border-line bg-canvas px-3 py-2 text-[12.5px] text-muted"
        >
          {order.status === "cancelled"
            ? "This order was cancelled, so its lines are read-only and its holds were released."
            : "These lines are frozen: the warehouse is holding stock against them, so changing the order means cancelling it and writing a new one."}
        </p>
      ) : null}

      {notice ? (
        <p
          data-qa-sales-order-notice
          className="rounded-md border border-line bg-canvas px-3 py-2 text-[12.5px]"
        >
          {notice}
        </p>
      ) : null}
      {actionError ? (
        <p
          data-qa-sales-order-action-error
          className="rounded-md border border-negative/40 px-3 py-2 text-[12.5px] text-negative"
        >
          {actionError}
        </p>
      ) : null}

      {confirmCancel ? (
        <div
          data-qa-sales-order-cancel-dialog
          className="rounded-lg border border-line bg-panel p-3 text-[12.5px] shadow-sm"
        >
          <p>
            Cancel <strong>{order.number}</strong>? Its holds are given back and this cannot be
            undone.
          </p>
          <label className="mt-2 block">
            <span className="mb-1 block text-muted">Why (shown on the release and in the timeline)</span>
            <input
              value={reason}
              onChange={(event) => setReason(event.target.value)}
              data-qa-sales-order-cancel-reason
              placeholder="Customer bought elsewhere, duplicate order, …"
              className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
            />
          </label>
          <div className="mt-2 flex items-center gap-2">
            <button
              type="button"
              onClick={() => void run("cancel")}
              disabled={busy !== null || reason.trim() === ""}
              data-qa-sales-order-cancel-yes
              className="inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] font-medium text-panel disabled:opacity-60"
            >
              {busy === "cancel" ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
              ) : null}
              Cancel the order
            </button>
            <button
              type="button"
              onClick={() => {
                setConfirmCancel(false);
                setReason("");
              }}
              data-qa-sales-order-cancel-no
              className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
            >
              Keep it
            </button>
          </div>
        </div>
      ) : null}

      <div className="grid gap-3 md:grid-cols-[1fr_20rem]">
        <section className="space-y-3">
          <div className="overflow-x-auto rounded-lg border border-line">
            <table className="w-full border-collapse text-left text-[13px]">
              <caption className="sr-only">The lines of {order.number}</caption>
              <thead>
                <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                  <th scope="col" className="px-3 py-2 font-medium">#</th>
                  <th scope="col" className="px-3 py-2 font-medium">Description</th>
                  <th scope="col" className="px-3 py-2 text-right font-medium">Qty</th>
                  <th scope="col" className="px-3 py-2 text-right font-medium">Unit price</th>
                  <th scope="col" className="px-3 py-2 text-right font-medium">Tax</th>
                  <th scope="col" className="px-3 py-2 text-right font-medium">Line total</th>
                  <th scope="col" className="px-3 py-2 font-medium">Hold</th>
                </tr>
              </thead>
              <tbody>
                {lines.map((line) => (
                  <tr key={line.id} data-qa-sales-order-line={line.position} className="border-b border-line last:border-b-0">
                    <td className="px-3 py-2 text-muted">{line.position}</td>
                    <td className="px-3 py-2">
                      <span className="inline-flex items-center gap-1.5">
                        <FileText className="h-3 w-3 text-muted" aria-hidden />
                        {line.description || line.product?.name || "—"}
                      </span>
                      {line.product?.sku ? (
                        <span className="ml-1.5 text-[11.5px] text-muted">{line.product.sku}</span>
                      ) : null}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums">
                      {line.quantity} {line.unit}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums">{formatMoney(line.unit_price, order.currency)}</td>
                    <td className="px-3 py-2 text-right tabular-nums">{line.tax_percent}%</td>
                    <td className="px-3 py-2 text-right tabular-nums">{formatMoney(line.line_total, order.currency)}</td>
                    <td className="px-3 py-2">
                      {line.reservation ? (
                        <span
                          data-qa-sales-order-hold={line.reservation.state}
                          title={
                            line.reservation.state === "held"
                              ? `Held since ${formatTimestamp(line.reservation.held_at)}`
                              : line.reservation.released_reason
                          }
                          className={`inline-block rounded-md border px-1.5 py-0.5 text-[11.5px] ${reservationTone(line.reservation.state === "held" ? "total" : "released")}`}
                        >
                          {line.reservation.state === "held"
                            ? `Held ${line.reservation.quantity}`
                            : "Released"}
                        </span>
                      ) : (
                        <span className="text-[11.5px] text-muted">—</span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <div className="rounded-lg border border-line p-3">
            <h2 className="mb-2 text-[12.5px] font-semibold">Timeline</h2>
            {history.length === 0 ? (
              <p className="text-[12.5px] text-muted">Nothing has happened to this order yet.</p>
            ) : (
              <ol className="space-y-2" data-qa-sales-order-history>
                {history.map((entry) => (
                  <li key={entry.id} className="flex flex-wrap items-baseline gap-x-2 text-[12.5px]">
                    <span className="text-muted">{formatTimestamp(entry.created_at)}</span>
                    <span className="font-medium">{entry.to_status}</span>
                    {entry.note ? <span className="text-muted">{entry.note}</span> : null}
                    {entry.actor ? <span className="text-muted">· {entry.actor.name}</span> : null}
                  </li>
                ))}
              </ol>
            )}
          </div>
        </section>

        <aside className="space-y-3">
          <div className="rounded-lg border border-line p-3">
            <h2 className="mb-2 text-[12.5px] font-semibold">Totals</h2>
            <dl className="space-y-1 text-[13px]">
              <div className="flex justify-between">
                <dt className="text-muted">Subtotal</dt>
                <dd className="tabular-nums" data-qa-sales-order-subtotal>
                  {formatMoney(totals.subtotal, order.currency)}
                </dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-muted">Discount</dt>
                <dd className="tabular-nums">−{formatMoney(totals.discount_total, order.currency)}</dd>
              </div>
              <div className="flex justify-between">
                <dt className="text-muted">Tax</dt>
                <dd className="tabular-nums" data-qa-sales-order-tax>
                  {formatMoney(totals.tax_total, order.currency)}
                </dd>
              </div>
              <div className="flex justify-between border-t border-line pt-1 font-semibold">
                <dt>Total</dt>
                <dd className="tabular-nums" data-qa-sales-order-total>
                  {formatMoney(totals.grand_total, order.currency)}
                </dd>
              </div>
            </dl>
          </div>

          <div className="rounded-lg border border-line p-3">
            <h2 className="mb-2 text-[12.5px] font-semibold">Reservation</h2>
            <span
              data-qa-sales-order-reservation={order.reservation_state}
              className={`inline-block rounded-md border px-1.5 py-0.5 text-[11.5px] ${reservationTone(order.reservation_state)}`}
            >
              {order.reservation_state}
            </span>
            <p className="mt-1.5 text-[12px] text-muted">
              {reservationNote(order.reservation_state, order.currency)}
            </p>
            <p className="mt-1.5 text-[12px] text-muted">
              <PackageCheck className="mr-1 inline h-3 w-3" aria-hidden />
              Inventory keeps the real stock ledger; until that module is installed this is what the
              order asked it to hold.
            </p>
          </div>

          <div className="rounded-lg border border-line p-3">
            <h2 className="mb-2 text-[12.5px] font-semibold">Invoice</h2>
            {invoice ? (
              <div className="space-y-1 text-[12.5px]" data-qa-sales-order-invoice-card>
                <p>
                  <span className="text-muted">State</span>{" "}
                  <span className="font-medium">{invoice.state}</span>
                </p>
                <p>
                  <span className="text-muted">Total</span>{" "}
                  <span className="tabular-nums" data-qa-sales-order-invoice-total>
                    {formatMoney(invoice.grand_total, invoice.currency)}
                  </span>
                </p>
                {invoice.external_url ? (
                  <a
                    href={invoice.external_url}
                    className="underline underline-offset-2"
                    data-qa-sales-order-invoice-link
                  >
                    Open the invoice
                  </a>
                ) : null}
              </div>
            ) : (
              <p className="text-[12.5px] text-muted">
                No invoice draft yet. Confirming the order is what makes one possible.
              </p>
            )}
          </div>
        </aside>
      </div>
    </div>
  );
}
