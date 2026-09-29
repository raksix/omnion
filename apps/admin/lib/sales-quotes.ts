/**
 * The sales client, part two: quotes (REQ-052, slice 2).
 *
 * It follows the same rules as `sales.ts` (slice 1) rather than inventing a second transport, and
 * two of those rules become sharper with a quote, so they are repeated here where they bite:
 *
 * * **Money is a string, end to end.** A quote line's price, the totals block, a frozen version's
 *   totals — all decimal text, all the way from `numeric(14,2)` to the input that edits it. A
 *   `Number()` in this file would make the printed total differ from the stored one, and on a
 *   quote with awkward fractions that is a cent the customer disputes.
 * * **A date is a `YYYY-MM-DD` string** in both directions, and the client's own type says so —
 *   a `Date` object here would re-introduce the timezone bug the module's `dates` module exists to
 *   prevent (a validity of "tomorrow" computed in the browser is tomorrow in the browser's zone).
 *
 * The client totals are **not computed here at all**. The builder displays what the server
 * echoed back, which is the only version of a total that can be printed on a document: a builder
 * that showed its own arithmetic would let a seller see 417.71 and send 407.76.
 */
import { ApiError, type ErrorBody } from "./api";

// ---------------------------------------------------------------------------------------------
// Transport (the same three helpers `sales.ts` uses, kept local rather than exported from there:
// a shared helper would make the catalog and the quotes a single file, and a merge in either
// module would then touch both.)
// ---------------------------------------------------------------------------------------------

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

async function quotesRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
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

/** `?a=1&b=2`, skipping absent values so a filter that is not set is not sent as empty. */
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

/** A customer, as the quote records it: a CRM reference plus the name captured on the quote. */
export type SalesCustomer = {
  kind: "company" | "contact";
  /** `null` once the CRM record is gone; the name is why the quote still reads. */
  id: string | null;
  name: string;
};

/** The four totals, as the API sends them — decimal text, never numbers. */
export type SalesQuoteTotals = {
  subtotal: string;
  discount_total: string;
  tax_total: string;
  grand_total: string;
};

/** A quote as the list screen sees it. */
export type SalesQuote = {
  id: string;
  organization_id: string;
  /** `Q-2026-0001`, assigned by the server and immutable. */
  number: string;
  title: string;
  status: SalesQuoteStatus;
  customer: SalesCustomer;
  owner: { id: string; name: string } | null;
  currency: string;
  price_list_id: string | null;
  /** `YYYY-MM-DD`. */
  valid_until: string;
  totals: SalesQuoteTotals;
  /** 0 until the quote has been sent, then the version the customer last read. */
  version: number;
  max_discount_percent: number;
  updated_at: string;
  created_at: string;
};

/** The module's statuses. A union, not a string, so a switch cannot miss a case silently. */
export type SalesQuoteStatus =
  | "draft"
  | "pending_approval"
  | "approved"
  | "sent"
  | "accepted"
  | "declined"
  | "expired"
  | "cancelled";

/** One line of the grid, as the builder and the detail read it. */
export type SalesQuoteLine = {
  id: string;
  position: number;
  product_id: string | null;
  product: { id: string; sku: string; name: string; unit: string } | null;
  description: string;
  unit: string;
  /** Decimal text with up to three places. */
  quantity: string;
  /** Decimal text with two places. */
  unit_price: string;
  discount_percent: number;
  tax_percent: number;
  /** Decimal text, computed by the server. */
  line_total: string;
};

/** One immutable snapshot — what the customer read at that version. */
export type SalesQuoteVersion = {
  version: number;
  currency: string;
  lines: unknown;
  totals: { subtotal?: string } & Record<string, string>;
  sent_at: string;
};

/** A quote with its lines, its versions and whether a public link exists. */
export type SalesQuoteDetail = {
  quote: SalesQuote;
  lines: SalesQuoteLine[];
  versions: SalesQuoteVersion[];
  has_public_link: boolean;
  public_link_expires_at: string | null;
  sent_at: string | null;
  decline_reason: string | null;
  cancel_reason: string | null;
  notes: string;
  reference: string;
  decided_at: string | null;
};

/** The customer document, as the public page reads it. */
export type SalesPublicQuote = {
  number: string;
  title: string;
  status: SalesQuoteStatus;
  customer_name: string;
  currency: string;
  valid_until: string;
  lines: {
    description: string;
    unit: string;
    quantity: string;
    unit_price: string;
    discount_percent: number;
    tax_percent: number;
    line_total: string;
  }[];
  totals: SalesQuoteTotals;
  notes: string;
  payment_terms: string;
  reference: string;
  decided: boolean;
};

/** One page of the list. */
export type SalesQuotePage = {
  items: SalesQuote[];
  next_cursor: string | null;
  total_estimate: number;
};

/** The status tab bar and the builder's defaults. */
export type SalesQuoteVocabulary = {
  statuses: { value: SalesQuoteStatus; label: string; open: boolean }[];
  default_currency: string;
  validity_days: number;
  discount_approval_threshold: number;
};

/** A line as the form holds it — every field a string, because every field arrives as one. */
export type SalesQuoteLineDraft = {
  product_id: string;
  description: string;
  unit: string;
  quantity: string;
  unit_price: string;
  discount_percent: string;
  tax_percent: string;
};

/** The quote create/replace body. */
export type SalesQuotePayload = {
  customer_id: string;
  customer_type: "company" | "contact";
  customer_name: string;
  title: string;
  currency: string;
  price_list_id?: string | null;
  valid_until: string;
  payment_terms: string;
  reference: string;
  notes: string;
  lines: {
    product_id?: string | null;
    description?: string;
    unit?: string;
    quantity?: string;
    unit_price?: string;
    discount_percent?: number;
    tax_percent?: number;
  }[];
};

/** The list's filters, as the toolbar sends them. */
export type SalesQuoteQuery = {
  search?: string;
  status?: string;
  owner_user_id?: string;
  expiring_in_days?: number;
  valid_from?: string;
  min_total?: string;
  max_total?: string;
  sort?: string;
  direction?: string;
  limit?: number;
  cursor?: string;
  include_archived?: boolean;
  organization_id?: string | null;
};

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

/** One page of the quote list. */
export function fetchSalesQuotes(
  query: SalesQuoteQuery = {},
): Promise<SalesQuotePage> {
  return quotesRequest<SalesQuotePage>(
    withQuery("/api/v1/sales/quotes", {
      search: query.search,
      status: query.status,
      owner_user_id: query.owner_user_id,
      expiring_in_days: query.expiring_in_days,
      valid_from: query.valid_from,
      min_total: query.min_total,
      max_total: query.max_total,
      sort: query.sort,
      direction: query.direction,
      limit: query.limit,
      cursor: query.cursor,
      include_archived: query.include_archived ? 1 : undefined,
      organization_id: query.organization_id ?? undefined,
    }),
  );
}

/** One quote with its lines and versions. */
export function fetchSalesQuote(id: string, organizationId?: string | null): Promise<SalesQuoteDetail> {
  return quotesRequest<SalesQuoteDetail>(
    withQuery(`/api/v1/sales/quotes/${id}`, { organization_id: organizationId ?? undefined }),
  );
}

/** The status tab bar and the builder's defaults. */
export function fetchSalesQuoteVocabulary(
  organizationId?: string | null,
): Promise<SalesQuoteVocabulary> {
  return quotesRequest<SalesQuoteVocabulary>(
    withQuery("/api/v1/sales/quotes/vocabulary", {
      organization_id: organizationId ?? undefined,
    }),
  );
}

/** The customer document, for the public preview the seller sees before sending the link. */
export function fetchPublicSalesQuote(token: string): Promise<SalesPublicQuote> {
  return quotesRequest<SalesPublicQuote>(`/api/v1/sales/public/quotes/${token}`);
}

// ---------------------------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------------------------

/** Create a draft. */
export function createSalesQuote(
  payload: SalesQuotePayload,
  organizationId?: string | null,
): Promise<SalesQuoteDetail> {
  return quotesRequest<SalesQuoteDetail>(
    withQuery("/api/v1/sales/quotes", { organization_id: organizationId ?? undefined }),
    { method: "POST", body: JSON.stringify(payload) },
  );
}

/** Replace the whole line grid of a working quote. */
export function saveSalesQuoteLines(
  id: string,
  lines: SalesQuotePayload["lines"],
  organizationId?: string | null,
): Promise<SalesQuoteDetail> {
  return quotesRequest<SalesQuoteDetail>(
    withQuery(`/api/v1/sales/quotes/${id}/lines`, {
      organization_id: organizationId ?? undefined,
    }),
    { method: "PUT", body: JSON.stringify({ lines }) },
  );
}

/** Edit a quote's header — title, validity, terms, reference, notes. */
export function updateSalesQuoteHeader(
  id: string,
  patch: {
    title?: string;
    valid_until?: string;
    payment_terms?: string;
    reference?: string;
    notes?: string;
  },
  organizationId?: string | null,
): Promise<SalesQuoteDetail> {
  return quotesRequest<SalesQuoteDetail>(
    withQuery(`/api/v1/sales/quotes/${id}`, { organization_id: organizationId ?? undefined }),
    { method: "PATCH", body: JSON.stringify(patch) },
  );
}

/**
 * Send a quote: freeze the lines and snapshot the version the customer is about to read.
 *
 * Named rather than `post`, because every other write in this file says what it wrote and `send`
 * is a state change with a consequence (the document is no longer editable) rather than a save.
 */
export function sendSalesQuote(id: string, organizationId?: string | null): Promise<SalesQuoteDetail> {
  return quotesRequest<SalesQuoteDetail>(
    withQuery(`/api/v1/sales/quotes/${id}/send`, { organization_id: organizationId ?? undefined }),
    { method: "POST" },
  );
}

/** Withdraw a quote, with the reason the timeline shows. */
export function cancelSalesQuote(
  id: string,
  reason: string,
  organizationId?: string | null,
): Promise<SalesQuoteDetail> {
  return quotesRequest<SalesQuoteDetail>(
    withQuery(`/api/v1/sales/quotes/${id}/cancel`, {
      organization_id: organizationId ?? undefined,
    }),
    { method: "POST", body: JSON.stringify({ reason }) },
  );
}

/** Copy a quote into a new draft — the only way to change a document the customer has seen. */
export function duplicateSalesQuote(
  id: string,
  organizationId?: string | null,
): Promise<SalesQuoteDetail> {
  return quotesRequest<SalesQuoteDetail>(
    withQuery(`/api/v1/sales/quotes/${id}/duplicate`, {
      organization_id: organizationId ?? undefined,
    }),
    { method: "POST" },
  );
}

/**
 * Issue — or re-issue, which is how a link is revoked — the customer's link.
 *
 * The response carries the token in clear and it is the **only** response that does: the server
 * stores the hash. So it is not cached by anything here, and a re-render of the detail screen
 * asks for a fresh one rather than re-using a token that is already gone.
 */
export function issueSalesQuoteLink(
  id: string,
  organizationId?: string | null,
): Promise<{ url: string }> {
  return quotesRequest<{ url: string }>(
    withQuery(`/api/v1/sales/quotes/${id}/link`, { organization_id: organizationId ?? undefined }),
    { method: "POST" },
  );
}

// ---------------------------------------------------------------------------------------------
// Approvals (slice 3)
// ---------------------------------------------------------------------------------------------

/** Where a request sits. A union, so a switch cannot miss a case silently. */
export type SalesApprovalStatus = "pending" | "approved" | "rejected" | "cancelled";

/** Which list the inbox is showing. */
export type SalesApprovalScope = "pending" | "requested_by_me" | "decided" | "all";

/** The decision half of a request, or `null` while it is still open. */
export type SalesApprovalDecision = {
  /** `null` for a cancellation: withdrawn is not a verdict. */
  outcome: SalesApprovalStatus | null;
  decided_by: string | null;
  decider_name: string;
  comment: string;
  decided_at: string;
};

/** One request as the inbox and the quote detail read it. */
export type SalesApproval = {
  id: string;
  organization_id: string;
  quote_id: string;
  quote_number: string;
  quote_title: string;
  quote_status: SalesQuoteStatus;
  requested_by: string;
  requester_name: string;
  discount_percent: number;
  threshold_percent: number;
  currency: string;
  /** Decimal text. */
  grand_total: string;
  note: string;
  status: SalesApprovalStatus;
  decision: SalesApprovalDecision | null;
  /** The panel route for the quote under decision. */
  subject_url: string;
  created_at: string;
  updated_at: string;
};

/** One page of the inbox. */
export type SalesApprovalPage = {
  items: SalesApproval[];
  next_cursor: string | null;
  total_estimate: number;
};

/**
 * What the send button has to say about a quote.
 *
 * `null` means the quote is inside the limit and needs nothing — which is the **common** case and
 * the reason this is a nullable field rather than an object with a boolean: a screen that renders
 * `requirement.discount_percent` unconditionally would print "0%" above every ordinary quote.
 */
export type SalesApprovalRequirement = {
  quote_id: string;
  quote_number: string;
  discount_percent: number;
  threshold_percent: number;
  request: SalesApproval | null;
};

/** The query of the inbox. */
export type SalesApprovalQuery = {
  scope?: SalesApprovalScope;
  status?: SalesApprovalStatus;
  search?: string;
  limit?: number;
  organization_id?: string | null;
};

/** One page of the inbox, four ways. */
export function fetchSalesApprovals(
  query: SalesApprovalQuery = {},
): Promise<SalesApprovalPage> {
  return quotesRequest<SalesApprovalPage>(
    withQuery("/api/v1/sales/approvals", {
      scope: query.scope,
      status: query.status,
      search: query.search,
      limit: query.limit,
      organization_id: query.organization_id ?? undefined,
    }),
  );
}

/** One request with its decision. */
export function fetchSalesApproval(
  id: string,
  organizationId?: string | null,
): Promise<SalesApproval> {
  return quotesRequest<SalesApproval>(
    withQuery(`/api/v1/sales/approvals/${id}`, {
      organization_id: organizationId ?? undefined,
    }),
  );
}

/** A quote's whole approval history, newest first. */
export function fetchQuoteApprovals(
  quoteId: string,
  organizationId?: string | null,
): Promise<SalesApproval[]> {
  return quotesRequest<SalesApproval[]>(
    withQuery(`/api/v1/sales/quotes/${quoteId}/approvals`, {
      organization_id: organizationId ?? undefined,
    }),
  );
}

/**
 * What the builder needs to know about the gate.
 *
 * The builder calls this on load rather than reading `max_discount_percent` off the quote: the
 * threshold lives in the **settings**, so lowering it can make an already-open quote need a
 * manager, and only this endpoint compares the two.
 */
export function fetchApprovalRequirement(
  quoteId: string,
  organizationId?: string | null,
): Promise<SalesApprovalRequirement | null> {
  return quotesRequest<SalesApprovalRequirement | null>(
    withQuery(`/api/v1/sales/quotes/${quoteId}/approval-requirement`, {
      organization_id: organizationId ?? undefined,
    }),
  );
}

/** Ask a manager to look at a discount. */
export function requestSalesApproval(
  quoteId: string,
  note = "",
  organizationId?: string | null,
): Promise<SalesApproval> {
  return quotesRequest<SalesApproval>(
    withQuery(`/api/v1/sales/quotes/${quoteId}/approval-requests`, {
      organization_id: organizationId ?? undefined,
    }),
    { method: "POST", body: JSON.stringify({ note }) },
  );
}

/**
 * Approve or reject a request.
 *
 * The two verbs are named rather than a boolean, because a boolean cannot be misread: a
 * `decision: "true"` that reached the server as a string would be one careless change away from
 * approving a rejection.
 */
export function decideSalesApproval(
  id: string,
  decision: "approve" | "reject",
  comment = "",
  organizationId?: string | null,
): Promise<SalesApproval> {
  return quotesRequest<SalesApproval>(
    withQuery(`/api/v1/sales/approvals/${id}/decision`, {
      organization_id: organizationId ?? undefined,
    }),
    { method: "POST", body: JSON.stringify({ decision, comment }) },
  );
}

/** Withdraw a request — the requester only; the server refuses anybody else. */
export function cancelSalesApproval(
  id: string,
  organizationId?: string | null,
): Promise<SalesApproval> {
  return quotesRequest<SalesApproval>(
    withQuery(`/api/v1/sales/approvals/${id}/cancel`, {
      organization_id: organizationId ?? undefined,
    }),
    { method: "POST" },
  );
}

/** The badge a request's status gets. */
export function approvalStatusTone(status: SalesApprovalStatus): string {
  switch (status) {
    case "approved":
      return "bg-[color-mix(in_oklab,var(--green)_14%,transparent)] text-green border-green/30";
    case "rejected":
      return "bg-[color-mix(in_oklab,var(--red)_12%,transparent)] text-red border-red/30";
    case "pending":
      return "bg-[color-mix(in_oklab,var(--warn)_14%,transparent)] text-warn border-warn/30";
    default:
      return "bg-canvas text-muted border-line";
  }
}

/** The empty-state sentence per tab: a box that says "no results" for all four is a dead end. */
export function approvalEmptyCopy(scope: SalesApprovalScope): string {
  switch (scope) {
    case "requested_by_me":
      return "You have not asked for a discount approval. A quote over the limit will need one.";
    case "decided":
      return "Nothing has been decided yet — the first decision shows up here with its comment.";
    case "all":
      return "No approval requests in this organization yet.";
    default:
      return "Nothing is waiting on you. Nice.";
  }
}

// ---------------------------------------------------------------------------------------------
// Presentation helpers
// ---------------------------------------------------------------------------------------------

/** The badge a status gets: the tailwind classes, so two screens cannot disagree about a colour. */
export function quoteStatusTone(status: SalesQuoteStatus): string {
  switch (status) {
    case "accepted":
      return "bg-[color-mix(in_oklab,var(--green)_14%,transparent)] text-green border-green/30";
    case "sent":
    case "approved":
      return "bg-[color-mix(in_oklab,var(--accent)_12%,transparent)] text-accent border-accent/30";
    case "pending_approval":
      return "bg-[color-mix(in_oklab,var(--warn)_14%,transparent)] text-warn border-warn/30";
    case "declined":
    case "cancelled":
    case "expired":
      return "bg-[color-mix(in_oklab,var(--red)_12%,transparent)] text-red border-red/30";
    default:
      return "bg-canvas text-muted border-line";
  }
}

/**
 * How urgent a validity is, in three steps rather than a boolean.
 *
 * `null` means there is no date to judge (never, for a quote — but a caller that has not loaded
 * the row yet still needs the type), and the caller renders nothing for it.
 */
export function quoteValidityTone(
  validUntil: string | null,
  today: Date = new Date(),
): "expired" | "soon" | "ok" | null {
  if (!validUntil) return null;
  const day = new Date(`${validUntil}T00:00:00Z`);
  if (Number.isNaN(day.getTime())) return null;
  // Compare at UTC midnight on both sides: the API's day is a `date`, not an instant, and
  // comparing it in the browser's zone is how a quote reads as expired a day early.
  const now = Date.UTC(today.getUTCFullYear(), today.getUTCMonth(), today.getUTCDate());
  const days = Math.round((day.getTime() - now) / 86_400_000);
  if (days < 0) return "expired";
  if (days <= 7) return "soon";
  return "ok";
}

/** A blank line for the builder: quantity 1, no discount, the tax the product carries. */
export function blankQuoteLine(taxPercent = "0"): SalesQuoteLineDraft {
  return {
    product_id: "",
    description: "",
    unit: "piece",
    quantity: "1",
    unit_price: "0.00",
    discount_percent: "0",
    tax_percent: taxPercent,
  };
}
