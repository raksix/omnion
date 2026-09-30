/**
 * The payment client of REQ-054 slice 3 — the API half of the payments screen.
 *
 * ## Why a payment is not an invoice with a different word on it
 *
 * An invoice is *sent*; a payment *arrives*. That single difference explains every field here that
 * has no counterpart on `lib/accounting-invoices.ts`:
 *
 * * `allocations` — one transfer settles three invoices. The invoice client is happy with a list of
 *   lines because the lines are one document's own children; the money here belongs to one receipt
 *   and is *applied* to documents that already exist, so the allocation is a reference plus an
 *   amount, never an owned row with a description;
 * * `auto_allocate` — the common case is "here is the money, work out what it settles", and the
 *   sweep (oldest due date first) is the server's rule, not a client-side `sort()`. A client that
 *   sorted the open invoices itself would disagree with the server the day the sweep's tiebreak
 *   changes, and the disagreement would be invisible until a receipt was applied to the wrong
 *   invoice;
 * * `allow_overpayment` — a business decision, therefore a **permission** on the server, therefore
 *   not something this file may set on the caller's behalf. It is offered here only so the screen
 *   can send it when the caller holds `accounting.payments.overpay`; the route refuses it
 *   otherwise, and the screen shows the refusal rather than hiding the button.
 *
 * ## Money crosses the wire as a string, always
 *
 * Same rule as the invoice client, and for the same reason: `numeric(14,2)` in PostgreSQL, and a
 * `number` here would be the first step of a rounding defect nobody notices until a cent is
 * missing from a receipt.
 */

import { ApiError } from "@/lib/api";

/** A `numeric(14,2)` amount, as the string the server sent. */
export type Money = string;

/** How the money arrived. The names are the schema's CHECK, verbatim. */
export const PAYMENT_METHODS = [
  { value: "bank_transfer", label: "Bank transfer" },
  { value: "card", label: "Card" },
  { value: "cash", label: "Cash" },
  { value: "other", label: "Other" },
] as const;

export type PaymentMethod = (typeof PAYMENT_METHODS)[number]["value"];

/** The human label of a method, falling back to the raw value for one the UI does not know. */
export function paymentMethodLabel(method: string): string {
  return PAYMENT_METHODS.find((entry) => entry.value === method)?.label ?? method;
}

/** Whether every cent of the payment has been applied. */
export const ALLOCATION_STATES = [
  { value: "unallocated", label: "Unapplied" },
  { value: "partial", label: "Partly applied" },
  { value: "applied", label: "Applied" },
] as const;

export type AllocationState = (typeof ALLOCATION_STATES)[number]["value"];

/** The human label of an allocation state, falling back to the raw value. */
export function allocationStateLabel(state: string): string {
  return ALLOCATION_STATES.find((entry) => entry.value === state)?.label ?? state;
}

/** A row in the payments list — no allocations, no journal lines. */
export type PaymentSummary = {
  id: string;
  /** The document number a person reads, e.g. `PAY-000007`. */
  number: string;
  customer_name: string;
  paid_on: string;
  method: PaymentMethod;
  amount: Money;
  currency: string;
  reference: string;
  /** How much of the payment is applied to invoices. */
  allocated: Money;
  /** `amount - allocated`: money the customer is owed back until it is applied. */
  unallocated: Money;
  allocation_state: AllocationState;
  journal_entry_id: string | null;
  reversed: boolean;
  recorded_by: string | null;
  created_at: string;
};

/** One allocation: the money this payment put on one invoice. */
export type Allocation = {
  id: string;
  invoice_id: string;
  /** The invoice's number, so the screen does not have to resolve it. */
  invoice_number: string;
  invoice_customer: string;
  amount: Money;
  invoice_total: Money;
  /** What the invoice still owed **before** this allocation — what makes a row self-checking. */
  invoice_outstanding_before: Money;
  created_at: string;
};

/** An invoice this payment moved, and where it left it. */
export type SettledInvoice = {
  invoice_id: string;
  invoice_number: string;
  status_before: string;
  status_after: string;
  outstanding: Money;
};

/** A payment with its allocations — the detail screen and the receipt print. */
export type Payment = PaymentSummary & {
  note: string;
  reversal_reason: string;
  reversed_at: string | null;
  reversal_entry_id: string | null;
  allocations: Allocation[];
  /** What this payment changed, so nobody has to open each invoice to learn one is now closed. */
  settled_invoices: SettledInvoice[];
};

/** An allocation as the recorder's grid collects it. */
export type AllocationDraft = {
  invoice_id: string;
  amount: string;
};

/**
 * The body of `POST /accounting/payments`.
 *
 * There is no `allocated` field: the server computes it from `amount` and the allocations, and a
 * client that could post it would be a client that can claim to have applied money it did not.
 */
export type NewPaymentPayload = {
  customer_id?: string | null;
  customer_name?: string | null;
  company_id?: string | null;
  paid_on?: string | null;
  method?: string | null;
  amount?: string | null;
  currency?: string | null;
  reference?: string | null;
  note?: string | null;
  allocations?: AllocationDraft[];
  /** Apply the money to the organization's open invoices, oldest first, without naming them. */
  auto_allocate?: boolean;
  /** Only meaningful for a caller holding `accounting.payments.overpay`; the route enforces it. */
  allow_overpayment?: boolean;
};

/** The query the list takes. Every field maps to a named filter on the route. */
export type PaymentFilters = {
  method?: string;
  from?: string;
  to?: string;
  search?: string;
  /** The list's default view in practice: a reversed payment is history. */
  unreversed_only?: boolean;
  limit?: number;
};

/** The one-page envelope the API's `Page<T>` serialises to. */
export type Page<T> = {
  items: T[];
  /** Whether another page exists past `limit`. */
  has_more: boolean;
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

async function paymentRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
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

/** The payments, newest first. */
export async function fetchPayments(filters: PaymentFilters = {}): Promise<Page<PaymentSummary>> {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(filters)) {
    if (value !== undefined && value !== null && value !== "" && value !== false) {
      query.set(key, String(value));
    }
  }
  const suffix = query.toString();
  return paymentRequest<Page<PaymentSummary>>(
    `/api/v1/accounting/payments${suffix ? `?${suffix}` : ""}`,
  );
}

/** One payment with its allocations. */
export async function fetchPayment(id: string): Promise<Payment> {
  return paymentRequest<Payment>(`/api/v1/accounting/payments/${id}`);
}

/**
 * Record a payment and, optionally, apply it.
 *
 * The response is the server's own view: the allocations it wrote, in the order it walked them, and
 * the invoices each one left in a new state. The screen re-reads from this rather than from what
 * the form held, because the sweep may have applied the money to invoices the operator never named.
 */
export async function recordPayment(body: NewPaymentPayload): Promise<Payment> {
  return paymentRequest<Payment>("/api/v1/accounting/payments", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/**
 * Undo a payment. The row stays, the allocations are released and a counter entry is written.
 *
 * The reason is a parameter rather than an optional field: a reversal without one is an unexplained
 * hole in the books, and the server refuses it. A client that let the dialog be dismissed without a
 * reason would show the operator the refusal instead of asking the question.
 */
export async function reversePayment(id: string, reason: string): Promise<Payment> {
  return paymentRequest<Payment>(`/api/v1/accounting/payments/${id}/reverse`, {
    method: "POST",
    body: JSON.stringify({ reason }),
  });
}

/**
 * The sum of a grid's allocation amounts, as the recorder types.
 *
 * The screen shows this next to the payment's own amount because the server refuses a grid that
 * allocates **more than the payment is for** — not a business rule, arithmetic — and the operator
 * should see that constraint while typing rather than after a round trip. The check is a display
 * aid: `recordPayment` still sends whatever the grid holds and lets the module be the arbiter.
 */
export function sumAllocations(allocations: AllocationDraft[]): Money {
  const cents = allocations.reduce((acc, entry) => {
    const value = Number(entry.amount);
    return acc + (Number.isFinite(value) ? Math.round(value * 100) : 0);
  }, 0);
  return (cents / 100).toFixed(2);
}

/**
 * How much of `amount` the grid can still absorb.
 *
 * Returned as a **number of cents** rather than a string because the screen asks two questions of
 * it: "is the grid over the payment?" (a comparison) and "how much is left?" (a subtraction). A
 * string would force a parse at the point of the decision, which is where a rounding error belongs.
 */
export function remainingCents(amount: string, allocations: AllocationDraft[]): number {
  const total = Number(amount);
  const cents = Number.isFinite(total) ? Math.round(total * 100) : 0;
  return cents - Math.round(Number(sumAllocations(allocations)) * 100);
}
