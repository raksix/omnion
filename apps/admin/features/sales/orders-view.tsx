"use client";

/**
 * The order list (REQ-052, slice 4): `/sales/orders`.
 *
 * A list, and the two transitions that belong on it: **confirm** an order (which holds its
 * stock) and **cancel** one (which gives the stock back). Four decisions worth naming, because
 * each of them is a way this screen could have lied to somebody:
 *
 * * **The reservation column says what was recorded, not what a warehouse did.** REQ-053 owns
 *   the real stock ledger and is not installed, so the module records the *intent* to hold, one
 *   row per line. The column therefore reads "held" / "partial" / "released" / "none" and the
 *   note under it says so, rather than claiming stock is reserved. A screen that overstates
 *   what the platform knows is worse than one that admits a gap.
 * * **Confirm is a one-click action with a spinner, and it is never disabled after a success.**
 *   The server makes a second confirm a no-op — the criteria ask for that in those words — so
 *   the button stays live and a person who pressed it after a slow response gets the same order
 *   rather than a greyed-out control that looks broken.
 * * **Cancelling asks for a reason and shows it.** The release is what somebody reads next
 *   month, when the person who pulled the stock is long gone, and the server refuses a blank one.
 * * **The totals are the server's.** Every amount here is the decimal text the API sent; this file
 *   never adds anything up, for the same reason the quote list does not.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";

import { Ban, CheckCircle2, Loader2, Package, RotateCcw } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import { formatMoney } from "@/lib/sales";
import {
  SALES_ORDER_STATUSES,
  cancelSalesOrder,
  confirmSalesOrder,
  fetchSalesOrders,
  reservationNote,
  type SalesOrder,
  type SalesReservationState,
} from "@/lib/sales-orders";

import { SalesShortcutSheet, SalesToolbar, salesRowCursor, useSales, useSalesKeyboard } from "./sales-parts";

/** The columns, in the order the table draws them. */
const COLUMNS = ["Number", "Customer", "From quote", "Amount", "Reservation", "Invoice", "Status", "Created", ""];

/** The status tabs, `All` first and then the pipeline in the order an order travels it. */
const TABS: { value: string; label: string }[] = [
  { value: "", label: "All" },
  ...SALES_ORDER_STATUSES.map((status) => ({ value: status.value, label: status.label })),
];

/** The badge class for a reservation state. Amber for partial, because a half-held order is a
 * question somebody must answer, and muted for the two states that need no attention. */
function reservationTone(state: SalesReservationState): string {
  switch (state) {
    case "total":
      return "border-positive/40 text-positive";
    case "partial":
      return "border-caution/40 text-caution";
    case "released":
      return "border-line text-muted";
    case "none":
    default:
      return "border-line text-muted";
  }
}

/** `/sales/orders`: the list, its filters, and the two transitions. */
export function OrdersView() {
  const router = useRouter();
  const params = useSearchParams();
  const { organizationId } = useSales();

  const [page, setPage] = useState<{ items: SalesOrder[]; total_estimate: number } | null>(null);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [reloadToken, setReloadToken] = useState(0);
  const [selected, setSelected] = useState(0);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [cancelling, setCancelling] = useState<SalesOrder | null>(null);
  const [cancelReason, setCancelReason] = useState("");
  const searchRef = useRef<HTMLInputElement | null>(null);

  const status = params.get("status") ?? "";
  const search = params.get("search") ?? "";
  const filtered = status !== "" || search !== "";

  const query = useCallback(
    () => ({
      search: search || undefined,
      status: status || undefined,
      organization_id: organizationId ?? undefined,
      limit: 100,
    }),
    [search, status, organizationId],
  );

  useEffect(() => {
    setError(null);
    fetchSalesOrders(query())
      .then((loaded) => {
        setPage(loaded);
        setSelected((current) => (current < loaded.items.length ? current : 0));
      })
      .catch((problem) => setError(toScreenError(problem, "The orders could not be loaded.")));
  }, [query, reloadToken]);

  const items = page?.items ?? [];

  const run = useCallback(
    async (order: SalesOrder, action: "confirm" | "cancel") => {
      setBusyId(order.id);
      setActionError(null);
      setNotice(null);
      try {
        if (action === "confirm") {
          const detail = await confirmSalesOrder(order.id, organizationId);
          setNotice(
            `Confirmed ${detail.order.number}. ${detail.order.reservation_state === "total" ? "Every line is held." : "Some lines are held."}`,
          );
        } else {
          await cancelSalesOrder(order.id, cancelReason.trim(), organizationId);
          setNotice(`Cancelled ${order.number} and released its holds.`);
        }
        setReloadToken((token) => token + 1);
      } catch (problem) {
        setActionError(
          problem instanceof Error ? problem.message : "That action could not be completed.",
        );
      } finally {
        setBusyId(null);
        setCancelling(null);
        setCancelReason("");
      }
    },
    [cancelReason, organizationId],
  );

  const { shortcutsOpen, setShortcutsOpen } = useSalesKeyboard(
    {
      count: items.length,
      selected,
      onSelect: setSelected,
      onOpen: (index) => {
        const order = items[index];
        if (order) router.push(`/sales/orders/${order.id}`);
      },
      onEdit: (index) => {
        const order = items[index];
        if (order) router.push(`/sales/orders/${order.id}`);
      },
      // There is no "new order" screen yet, and a `New` button leading nowhere is a dead button.
      // A hand-written order is created from an accepted quote's detail or the conversion action,
      // so the list's `n` is deliberately not bound rather than pointing at a page that does not
      // exist.
      onNew: () => router.push("/sales/quotes?status=accepted"),
    },
    searchRef,
  );

  // The tabs' counts, counted from the page for the same reason the quote list does it: a list of
  // 100 rows cannot count what it is not showing.
  const tabCounts = useMemo(() => {
    const counts: Record<string, number> = {};
    for (const order of items) {
      counts[order.status] = (counts[order.status] ?? 0) + 1;
    }
    return counts;
  }, [items]);

  return (
    <div className="space-y-3">
      <header className="flex flex-wrap items-end justify-between gap-2">
        <div>
          <h1 className="text-[15px] font-semibold">Orders</h1>
          <p className="text-[12.5px] text-muted">
            Confirming an order holds its lines; cancelling gives them back with a reason you can
            read next month.
          </p>
        </div>
        <button
          type="button"
          onClick={() => router.push("/sales/quotes?status=accepted")}
          data-qa-sales-new-order
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px] font-medium"
        >
          <Package className="h-3.5 w-3.5" aria-hidden />
          Turn an accepted quote into an order
        </button>
      </header>

      <SalesToolbar
        search={search}
        onSearchChange={(value) => {
          const next = new URLSearchParams(params.toString());
          if (value) next.set("search", value);
          else next.delete("search");
          const text = next.toString();
          router.replace(text ? `/sales/orders?${text}` : "/sales/orders", { scroll: false });
        }}
        searchRef={searchRef}
        count={items.length}
        filtered={filtered}
      >
        <div className="flex flex-wrap items-center gap-1" role="tablist" aria-label="Order status">
          {TABS.map((tab) => {
            const active = status === tab.value;
            const count = tab.value === "" ? items.length : (tabCounts[tab.value] ?? 0);
            return (
              <button
                key={tab.value || "all"}
                type="button"
                role="tab"
                aria-selected={active}
                onClick={() => {
                  const next = new URLSearchParams(params.toString());
                  if (tab.value) next.set("status", tab.value);
                  else next.delete("status");
                  const text = next.toString();
                  router.replace(text ? `/sales/orders?${text}` : "/sales/orders", { scroll: false });
                }}
                data-qa-sales-order-tab={tab.value || "all"}
                className={`rounded-md border px-2 py-1 text-[12px] ${
                  active ? "border-ink bg-ink text-panel" : "border-line text-muted"
                }`}
              >
                {tab.label}
                <span className="ml-1 opacity-70">{count}</span>
              </button>
            );
          })}
        </div>
      </SalesToolbar>

      {notice ? (
        <p data-qa-sales-order-notice className="rounded-md border border-line bg-canvas px-3 py-2 text-[12.5px]">
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

      {cancelling ? (
        <div data-qa-sales-order-cancel className="rounded-lg border border-line bg-panel p-3 text-[12.5px] shadow-sm">
          <p>
            Cancel <strong>{cancelling.number}</strong>? Its holds are given back and this cannot be
            undone.
          </p>
          <label className="mt-2 block">
            <span className="mb-1 block text-muted">Why (shown in the timeline and on the release)</span>
            <input
              value={cancelReason}
              onChange={(event) => setCancelReason(event.target.value)}
              data-qa-sales-order-cancel-reason
              placeholder="Customer bought elsewhere, duplicate order, …"
              className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
            />
          </label>
          <div className="mt-2 flex items-center gap-2">
            <button
              type="button"
              onClick={() => run(cancelling, "cancel")}
              disabled={busyId !== null || cancelReason.trim() === ""}
              data-qa-sales-order-cancel-yes
              className="inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] font-medium text-panel disabled:opacity-60"
            >
              {busyId ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
              Cancel the order
            </button>
            <button
              type="button"
              onClick={() => {
                setCancelling(null);
                setCancelReason("");
              }}
              data-qa-sales-order-cancel-no
              className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
            >
              Keep it
            </button>
          </div>
        </div>
      ) : null}

      {error ? (
        <ErrorState error={error} onRetry={() => setReloadToken((token) => token + 1)} />
      ) : !page ? (
        <LoadingTable columns={COLUMNS.length} />
      ) : items.length === 0 ? (
        <EmptyState
          title={filtered ? "Nothing matches that filter" : "No orders yet"}
          hint={
            filtered
              ? "There are orders, just not the ones this filter names."
              : "An order is what a quote becomes once the customer has said yes. Accept a quote and the order is one click away."
          }
          action={
            filtered ? (
              <button
                type="button"
                onClick={() => router.replace("/sales/orders")}
                data-qa-sales-order-empty-clear
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
              >
                <RotateCcw className="h-3.5 w-3.5" aria-hidden />
                Clear the filters
              </button>
            ) : (
              <button
                type="button"
                onClick={() => router.push("/sales/quotes?status=accepted")}
                data-qa-sales-order-empty-new
                className="inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] font-medium text-panel"
              >
                <Package className="h-3.5 w-3.5" aria-hidden />
                Find an accepted quote
              </button>
            )
          }
        />
      ) : (
        <div className="overflow-x-auto rounded-lg border border-line">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                {COLUMNS.map((column) => (
                  <th key={column} scope="col" className="px-3 py-2 font-medium">
                    {column}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {items.map((order, index) => {
                const cursor = salesRowCursor(index === selected);
                const canConfirm = order.status === "draft";
                const canCancel = order.status !== "cancelled" && order.status !== "delivered";
                return (
                  <tr
                    key={order.id}
                    data-qa-sales-order-row={order.number}
                    className={`border-b border-line last:border-b-0 ${cursor.className}`}
                    onClick={() => router.push(`/sales/orders/${order.id}`)}
                  >
                    <td className="px-3 py-2 font-medium">{order.number}</td>
                    <td className="px-3 py-2">{order.customer.name || "—"}</td>
                    <td className="px-3 py-2 text-muted">
                      {order.quote_number ? (
                        <button
                          type="button"
                          onClick={(event) => {
                            event.stopPropagation();
                            router.push(`/sales/quotes/${order.quote_id}`);
                          }}
                          data-qa-sales-order-quote-link
                          className="underline underline-offset-2 hover:text-ink"
                        >
                          {order.quote_number}
                        </button>
                      ) : (
                        "Written by hand"
                      )}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums">
                      {formatMoney(order.grand_total, order.currency)}
                    </td>
                    <td className="px-3 py-2">
                      <span
                        data-qa-sales-order-reservation={order.reservation_state}
                        className={`inline-block rounded-md border px-1.5 py-0.5 text-[11.5px] ${reservationTone(order.reservation_state)}`}
                      >
                        {order.reservation_state === "none"
                          ? "None"
                          : order.reservation_state === "total"
                            ? "Total"
                            : order.reservation_state === "partial"
                              ? "Partial"
                              : "Released"}
                      </span>
                    </td>
                    <td className="px-3 py-2 text-muted">
                      {order.invoice_state === "none" ? "—" : order.invoice_state === "draft" ? "Draft" : "Issued"}
                    </td>
                    <td className="px-3 py-2">
                      <span className="inline-block rounded-md border border-line px-1.5 py-0.5 text-[11.5px]">
                        {SALES_ORDER_STATUSES.find((entry) => entry.value === order.status)?.label ??
                          order.status}
                      </span>
                    </td>
                    <td className="px-3 py-2 text-muted">{order.created_at.slice(0, 10)}</td>
                    <td className="px-3 py-2 text-right">
                      <span className="inline-flex items-center gap-1">
                        {canConfirm ? (
                          <button
                            type="button"
                            onClick={(event) => {
                              event.stopPropagation();
                              void run(order, "confirm");
                            }}
                            disabled={busyId !== null}
                            data-qa-sales-order-confirm={order.number}
                            title={reservationNote(order.reservation_state, order.currency)}
                            className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] disabled:opacity-60"
                          >
                            {busyId === order.id ? (
                              <Loader2 className="h-3 w-3 animate-spin" aria-hidden />
                            ) : (
                              <CheckCircle2 className="h-3 w-3" aria-hidden />
                            )}
                            Confirm
                          </button>
                        ) : null}
                        {canCancel ? (
                          <button
                            type="button"
                            onClick={(event) => {
                              event.stopPropagation();
                              setCancelling(order);
                            }}
                            data-qa-sales-order-cancel={order.number}
                            className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px]"
                          >
                            <Ban className="h-3 w-3" aria-hidden />
                            Cancel
                          </button>
                        ) : null}
                      </span>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {shortcutsOpen ? <SalesShortcutSheet onClose={() => setShortcutsOpen(false)} /> : null}
    </div>
  );
}
