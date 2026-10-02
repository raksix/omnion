/**
 * The sales client, part three: orders (REQ-052, slice 4).
 *
 * The same two rules as `sales.ts` and `sales-quotes.ts`, unchanged and not re-argued here —
 * money is decimal **text** end to end, and a date is a `YYYY-MM-DD` string — plus the rule this
 * file has that the other two do not:
 *
 * * **A hold is a fact, not a hope.** `reservation_state` is reported verbatim and the detail
 *   reads `lines[].reservation` rather than assuming one. A screen that printed "reserved"
 *   because the order was confirmed would be claiming stock was held when the module only
 *   recorded the intent to hold it — and REQ-053, which owns the real ledger, is not installed.
 */
import { ApiError, type ErrorBody } from "./api";
import type { SalesCustomer, SalesQuoteTotals } from "./sales-quotes";

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

async function ordersRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, {
      ...init,
      credentials: "same-origin",
      headers: {
        accept: "application/json",
        ...(typeof init.body === "string" ? { "content-type": "application/json" } : {}),
        ...init.headers,
      },
    });
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

function withQuery(path: string, values: Record<string, string | number | undefined | null>): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(values)) {
    if (value === undefined || value === null || value === "") continue;
    search.set(key, String(value));
  }
  const query = search.toString();
  return query ? `${path}?${query}` : path;
}

// ---------------------------------------------------------------------------------------------
// Shapes
// ---------------------------------------------------------------------------------------------

/** The order lifecycle, in the order the status tabs print. */
export type SalesOrderStatus = "draft" | "confirmed" | "invoiced" | "delivered" | "cancelled";

/**
 * Whether stock is held, and whether all of it is.
 *
 * `none` is a **fact** — no holds at all — and not a synonym for "inventory is not installed".
 * REQ-053 owns the real stock ledger; until it is installed this module records the intent to
 * hold, and `total` means every line has one of those records.
 */
export type SalesReservationState = "none" | "partial" | "total" | "released";

/** Whether an invoice draft has been handed to accounting. */
export type SalesInvoiceState = "none" | "draft" | "issued";

/** The seller an order is attributed to. */
export type SalesOrderOwner = { id: string; name: string };

export type SalesOrder = {
  id: string;
  organization_id: string;
  number: string;
  status: SalesOrderStatus;
  customer: SalesCustomer;
  quote_id: string | null;
  /** The quote's number, so the list is readable without a fetch per row. */
  quote_number: string | null;
  currency: string;
  subtotal: string;
  discount_total: string;
  tax_total: string;
  grand_total: string;
  reservation_state: SalesReservationState;
  invoice_state: SalesInvoiceState;
  owner: SalesOrderOwner | null;
  created_at: string;
  updated_at: string;
};

/** A stock hold, as the order detail renders it. */
export type SalesReservation = {
  id: string;
  line_id: string;
  product_id: string | null;
  quantity: string;
  unit: string;
  state: "held" | "released";
  held_at: string;
  released_at: string | null;
  released_reason: string;
};

export type SalesOrderLine = {
  id: string;
  position: number;
  product_id: string | null;
  product: { id: string; sku: string; name: string; unit: string } | null;
  description: string;
  unit: string;
  quantity: string;
  unit_price: string;
  discount_percent: number;
  tax_percent: number;
  line_total: string;
  /** `null` on a draft: the hold is read, never assumed. */
  reservation: SalesReservation | null;
};

export type SalesOrderHistory = {
  id: string;
  from_status: string | null;
  to_status: string;
  note: string;
  actor: SalesOrderOwner | null;
  created_at: string;
};

export type SalesInvoiceHandoff = {
  id: string;
  order_id: string;
  state: "draft" | "issued" | "void";
  currency: string;
  subtotal: string;
  tax_total: string;
  grand_total: string;
  external_id: string | null;
  external_url: string | null;
  /** When accounting issued the document. `null` while the handoff is still a draft. */
  settled_at: string | null;
  raised_at: string;
};

export type SalesOrderDetail = {
  order: SalesOrder;
  lines: SalesOrderLine[];
  history: SalesOrderHistory[];
  invoice: SalesInvoiceHandoff | null;
  totals: SalesQuoteTotals;
};

export type SalesOrderPage = {
  items: SalesOrder[];
  next_cursor: string | null;
  total_estimate: number;
};

export type SalesOrderQuery = {
  search?: string;
  status?: string;
  owner_user_id?: string;
  from?: string;
  to?: string;
  active?: boolean;
  sort?: string;
  direction?: string;
  limit?: number;
  cursor?: string;
  organization_id?: string | null;
};

/** One line of a hand-written order. */
export type SalesOrderLineDraft = {
  product_id?: string | null;
  description?: string;
  unit?: string;
  quantity: string;
  unit_price: string;
  discount_percent?: number;
  tax_percent?: number;
};

export type SalesOrderPayload = {
  quote_id?: string | null;
  customer_name?: string;
  customer_kind?: string;
  customer_id?: string | null;
  owner_user_id?: string | null;
  lines?: SalesOrderLineDraft[];
};

// ---------------------------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------------------------

export function fetchSalesOrders(query: SalesOrderQuery = {}): Promise<SalesOrderPage> {
  return ordersRequest<SalesOrderPage>(
    withQuery("/api/v1/sales/orders", {
      search: query.search,
      status: query.status,
      owner_user_id: query.owner_user_id,
      from: query.from,
      to: query.to,
      active: query.active === undefined ? undefined : query.active ? 1 : 0,
      sort: query.sort,
      direction: query.direction,
      limit: query.limit,
      cursor: query.cursor,
      organization_id: query.organization_id ?? undefined,
    }),
  );
}

export function fetchSalesOrder(
  id: string,
  organizationId?: string | null,
): Promise<SalesOrderDetail> {
  return ordersRequest<SalesOrderDetail>(
    withQuery(`/api/v1/sales/orders/${id}`, { organization_id: organizationId ?? undefined }),
  );
}

/** Convert an accepted quote into an order, or write one by hand. */
export function createSalesOrder(
  payload: SalesOrderPayload,
  organizationId?: string | null,
): Promise<SalesOrderDetail> {
  return ordersRequest<SalesOrderDetail>(
    withQuery("/api/v1/sales/orders", { organization_id: organizationId ?? undefined }),
    { method: "POST", body: JSON.stringify(payload) },
  );
}

/**
 * Promise the order and hold its stock.
 *
 * The server makes a second call a no-op, so this client does not have to guard the button — and
 * it must not, because a button that greys itself out after a slow response is a button that
 * looks broken.
 */
export function confirmSalesOrder(
  id: string,
  organizationId?: string | null,
): Promise<SalesOrderDetail> {
  return ordersRequest<SalesOrderDetail>(
    withQuery(`/api/v1/sales/orders/${id}/confirm`, { organization_id: organizationId ?? undefined }),
    { method: "POST" },
  );
}

/** Withdraw the order and release its stock. The reason is required by the server. */
export function cancelSalesOrder(
  id: string,
  reason: string,
  organizationId?: string | null,
): Promise<SalesOrderDetail> {
  return ordersRequest<SalesOrderDetail>(
    withQuery(`/api/v1/sales/orders/${id}/cancel`, { organization_id: organizationId ?? undefined }),
    { method: "POST", body: JSON.stringify({ reason }) },
  );
}

/**
 * Hand the delivery to accounting.
 *
 * A second call returns the draft that already exists, so this is safe to retry and needs no
 * client-side "have we already asked" flag.
 */
export function raiseSalesInvoiceDraft(
  id: string,
  organizationId?: string | null,
): Promise<SalesInvoiceHandoff> {
  return ordersRequest<SalesInvoiceHandoff>(
    withQuery(`/api/v1/sales/orders/${id}/invoice-draft`, {
      organization_id: organizationId ?? undefined,
    }),
    { method: "POST" },
  );
}

// ---------------------------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------------------------

/** The status tabs, in pipeline order. */
export const SALES_ORDER_STATUSES: { value: SalesOrderStatus; label: string }[] = [
  { value: "draft", label: "Draft" },
  { value: "confirmed", label: "Confirmed" },
  { value: "invoiced", label: "Invoiced" },
  { value: "delivered", label: "Delivered" },
  { value: "cancelled", label: "Cancelled" },
];

/**
 * The sentence the reservation column prints.
 *
 * It says what was **recorded**, not what a warehouse has done: REQ-053 owns the real ledger and
 * is not installed, so a screen claiming "stock reserved" would be claiming more than the
 * platform knows. The wording generalises to that module's answer without changing.
 */
export function reservationNote(state: SalesReservationState, currency?: string | null): string {
  switch (state) {
    case "total":
      return "Every line is held for this order.";
    case "partial":
      return "Some lines are held for this order.";
    case "released":
      return "The holds were released when the order was cancelled.";
    case "none":
    default:
      return currency
        ? "Nothing is held yet — confirm the order to hold its lines."
        : "Nothing is held yet — confirm the order to hold its lines.";
  }
}
