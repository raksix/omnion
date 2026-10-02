/**
 * The invoice client of REQ-054 slice 2 — the API half of the invoice screens.
 *
 * ## Why this is a separate file from `lib/accounting.ts`
 *
 * That file is the chart, the rates and the journal: three subjects that are all "a balance".
 * An invoice is a *document with a lifecycle* — draft, sent, partially paid, paid, overdue,
 * voided — and its client has to carry a state machine the other three have no opinion about.
 * One 400-line file where a reader has to work out which of two argument sets applies before
 * reading a line is the same mistake the Rust side made when invoices went to
 * `routes/accounting_invoices.rs`.
 *
 * ## What the server recomputes, and why this file has no total
 *
 * **`NewInvoice` has no `subtotal` and no `grand_total` field on the wire at all.** The server
 * prices every line and stores the totals; a client that could post a total would be a client that
 * can lie about money. So there is no "grand total" in `NewInvoicePayload` below, and the create
 * helpers physically cannot send one. The preview totals the form shows are computed in the
 * browser for feedback while typing and are never the numbers that get stored — the create call
 * returns the server's own figures, and the screen re-reads those.
 *
 * ## Money crosses the wire as a string, always
 *
 * `numeric` in PostgreSQL and a branded `Money` string in TypeScript. Parsing a total into a
 * `number` for display would be the first step of a rounding defect nobody notices until a
 * cent is missing from a legal document.
 */

import { ApiError } from "@/lib/api";

/** A `numeric(20,4)` amount, as the string the server sent. */
export type Money = string;

/** Where an invoice is in its life. The names are the schema's CHECK, verbatim. */
export const INVOICE_STATUSES = [
  { value: "draft", label: "Draft" },
  { value: "sent", label: "Sent" },
  { value: "partially_paid", label: "Partially paid" },
  { value: "paid", label: "Paid" },
  { value: "overdue", label: "Overdue" },
  { value: "void", label: "Void" },
] as const;

export type InvoiceStatus = (typeof INVOICE_STATUSES)[number]["value"];

/** The human label of a status, falling back to the raw value for one the UI does not know. */
export function invoiceStatusLabel(status: string): string {
  return INVOICE_STATUSES.find((entry) => entry.value === status)?.label ?? status;
}

/** A row in the invoice list — no lines, which is what makes a hundred-row list cheap. */
export type InvoiceSummary = {
  id: string;
  number: string;
  customer_name: string;
  status: InvoiceStatus;
  currency: string;
  issue_date: string;
  due_date: string | null;
  grand_total: Money;
  paid_total: Money;
  outstanding: Money;
  /** 0 when it is not past due; the list's red/amber hint reads it. */
  days_past_due: number;
  order_id: string | null;
  line_count: number;
  created_at: string;
  updated_at: string;
};

/** One line of an invoice, as stored. */
export type InvoiceLine = {
  id: string;
  invoice_id: string;
  position: number;
  product_id: string | null;
  description: string;
  qty: string;
  unit_price: Money;
  discount_percent: string;
  /** Copied at issue time — editing a tax rate must not rewrite an issued invoice. */
  tax_percent: string;
  line_total: Money;
  tax_amount: Money;
  net_amount: Money;
};

/** The whole document. */
export type Invoice = {
  id: string;
  organization_id: string;
  number: string;
  order_id: string | null;
  order_number: string | null;
  company_id: string | null;
  contact_id: string | null;
  customer_name: string;
  status: InvoiceStatus;
  currency: string;
  issue_date: string;
  due_date: string | null;
  payment_terms: string;
  reference: string;
  notes: string;
  subtotal: Money;
  discount_total: Money;
  tax_total: Money;
  grand_total: Money;
  paid_total: Money;
  /** `grand_total - paid_total`, computed by the server rather than stored. */
  outstanding: Money;
  days_past_due: number;
  sent_at: string | null;
  last_payment_at: string | null;
  paid_at: string | null;
  voided_at: string | null;
  /** Required by the route when voiding. */
  void_reason: string;
  overdue_at: string | null;
  lines: InvoiceLine[];
  created_at: string;
  updated_at: string;
};

/** A line as the form collects it. There is no total field, and there cannot be one. */
export type InvoiceLineDraft = {
  product_id?: string | null;
  description?: string | null;
  qty?: string | null;
  unit_price?: string | null;
  discount_percent?: string | null;
  tax_percent?: string | null;
};

/** The body of `POST /accounting/invoices`. */
export type NewInvoicePayload = {
  company_id?: string | null;
  contact_id?: string | null;
  /** When present the order's lines are copied and a second draft is refused. */
  order_id?: string | null;
  customer_name?: string | null;
  issue_date?: string | null;
  due_date?: string | null;
  currency?: string | null;
  payment_terms?: string | null;
  reference?: string | null;
  notes?: string | null;
  lines: InvoiceLineDraft[];
};

/** The query the list takes. Every field maps to a named filter on the route. */
export type InvoiceFilters = {
  status?: string;
  from?: string;
  to?: string;
  /** Only what is past its due date — the list's own definition, so the tab and the red date agree. */
  overdue_only?: boolean;
  search?: string;
  limit?: number;
};

async function readFailure(response: Response): Promise<ApiError> {
  const text = await response.text();
  let code = "unknown_error";
  let message = `The API answered with status ${response.status}.`;
  let details: Record<string, unknown> | null = null;
  let requestId: string | null = response.headers.get("x-request-id");
  try {
    const body = JSON.parse(text) as {
      error?: { code?: string; message?: string; details?: Record<string, unknown> | null; request_id?: string };
    };
    code = body.error?.code ?? code;
    message = body.error?.message ?? message;
    details = body.error?.details ?? null;
    requestId = body.error?.request_id ?? requestId;
  } catch {
    // A non-JSON body is still an error; the status stays in the message.
  }
  return new ApiError(response.status, code, message, details, requestId);
}

async function invoiceRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
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

/** The invoices, newest first. */
export async function fetchInvoices(filters: InvoiceFilters = {}): Promise<InvoiceSummary[]> {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(filters)) {
    if (value !== undefined && value !== null && value !== "" && value !== false) {
      query.set(key, String(value));
    }
  }
  const suffix = query.toString();
  return invoiceRequest<InvoiceSummary[]>(`/api/v1/accounting/invoices${suffix ? `?${suffix}` : ""}`);
}

/** One invoice with its lines. */
export async function fetchInvoice(id: string): Promise<Invoice> {
  return invoiceRequest<Invoice>(`/api/v1/accounting/invoices/${id}`);
}

/**
 * Create an invoice, manually or converted from a sales order.
 *
 * The two are one call because they are one document: an order's lines are copied, not
 * re-implemented, and a second code path is a second set of bugs.
 */
export async function createInvoice(body: NewInvoicePayload): Promise<Invoice> {
  return invoiceRequest<Invoice>("/api/v1/accounting/invoices", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/**
 * Issue the document to the customer.
 *
 * Only a draft may be sent. The server refuses a second send by name, and a client that hides
 * the button anyway would leave the operator reading "already Sent" instead of the rule.
 */
export async function sendInvoice(id: string): Promise<Invoice> {
  return invoiceRequest<Invoice>(`/api/v1/accounting/invoices/${id}/send`, { method: "POST" });
}

/**
 * Withdraw the document, keeping its number.
 *
 * The reason is required — a void without one is an unexplained hole in the numbering — so it is
 * a parameter here rather than an optional field the screen may forget to fill.
 */
export async function voidInvoice(id: string, reason: string): Promise<Invoice> {
  return invoiceRequest<Invoice>(`/api/v1/accounting/invoices/${id}/void`, {
    method: "POST",
    body: JSON.stringify({ reason }),
  });
}

/** What one overdue sweep changed. */
export type OverdueSweep = {
  flipped: number;
  /** The same set the server announced as `accounting.invoice.overdue`. */
  invoice_ids: string[];
};

/**
 * Run the overdue sweep now.
 *
 * A button, not a mystery: the sweep is normally the automation's job (wave 3 owns the trigger),
 * and an operator who suspects a late invoice has not turned red needs a way to ask. It is
 * idempotent, so pressing it twice is safe — the second call flips nothing.
 */
export async function sweepOverdue(): Promise<OverdueSweep> {
  return invoiceRequest<OverdueSweep>("/api/v1/accounting/invoices/sweep-overdue", {
    method: "POST",
  });
}

/**
 * A line priced in the browser, for the form's live total.
 *
 * ## This is feedback, not the truth
 *
 * It rounds once, half away from zero, in the same order the server uses (discount before tax) —
 * which is why the number the operator sees before saving is the number they get after. It is
 * still only a preview: `createInvoice` sends no total at all, and the screen re-reads the
 * server's own figures after saving. A client that trusted this and posted it would be a client
 * that can lie about money, so nothing here is ever sent.
 */
export function previewLine(
  line: InvoiceLineDraft,
): { face: Money; net: Money; tax: Money; gross: Money } {
  const qty = Number(line.qty ?? "1");
  const unit = Number(line.unit_price ?? "0");
  const discount = Number(line.discount_percent ?? "0");
  const tax = Number(line.tax_percent ?? "0");
  const round = (value: number) =>
    (Math.round((value + (value >= 0 ? 0.5 : -0.5)) * 10000) / 10000).toFixed(2);
  // `face` is the line's pre-discount amount — what the server's `subtotal` sums, and the base
  // the discount is measured from.
  const face = round(qty * unit);
  const net = round(Number(face) * (1 - discount / 100));
  const taxAmount = round(Number(net) * (tax / 100));
  return { face, net, tax: taxAmount, gross: round(Number(net) + Number(taxAmount)) };
}

/**
 * The form's running totals, summed from the lines the operator has typed.
 *
 * `subtotal` is the sum of the lines' **pre-discount** amounts and `discountTotal` is the
 * difference between that and the net — the same two figures the server stores, so the block the
 * operator watches while typing has the same shape as the one they get after saving. Summing
 * rounded lines rather than rounding the sum is the rule the whole module inherits: a hundred
 * lines each rounded once must add up to the rounded total, not to a figure a cent away from it.
 */
export function previewTotals(lines: InvoiceLineDraft[]): {
  subtotal: Money;
  discountTotal: Money;
  taxTotal: Money;
  grandTotal: Money;
} {
  const sum = (values: Money[]) =>
    (values.reduce((acc, value) => acc + Math.round(Number(value) * 100), 0) / 100).toFixed(2);
  const priced = lines.map((line) => previewLine(line));
  const subtotal = sum(priced.map((line) => line.face));
  const taxTotal = sum(priced.map((line) => line.tax));
  const grandTotal = sum(priced.map((line) => line.gross));
  const netTotal = sum(priced.map((line) => line.net));
  return {
    subtotal,
    taxTotal,
    grandTotal,
    discountTotal: (Math.round((Number(subtotal) - Number(netTotal)) * 100) / 100).toFixed(2),
  };
}
