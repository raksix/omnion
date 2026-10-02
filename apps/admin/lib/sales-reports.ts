/**
 * The sales client, part three: reports, the CSV export and the global search
 * (REQ-052, slice 4b).
 *
 * The transport is a fourth copy on purpose. `api.ts` keeps its `request` private so that one
 * file owns the fetch/JSON/error handling, and each sales client file re-declares the three
 * helpers rather than sharing them — a shared helper would merge the catalog, the quotes and the
 * reports into one file, and every merge in any of them would then touch all three. The cost is
 * about thirty lines per module; the benefit is that a transport change in `api.ts` cannot
 * silently change how a quote is read without a reviewer noticing the reports did not move.
 *
 * **The percentages arrive as hundredths of a percent and are only ever divided by 100 for
 * display — never re-derived.** The report's own numbers are computed in SQL over the same
 * classification its table is drawn from, and a screen that recomputed a conversion rate from the
 * rows it happens to be showing would produce a different figure from the one the CSV contains the
 * moment the table is capped. `formatPercent` therefore takes the server's integer and prints it.
 */
import { ApiError, type ErrorBody } from "./api";

// ---------------------------------------------------------------------------------------------
// Transport
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

async function reportsRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, {
      ...init,
      credentials: "same-origin",
      headers: { Accept: "application/json", ...(init.headers ?? {}) },
    });
  } catch {
    // A fetch that never reached the API is the network, not the server: naming which one keeps
    // the retry button honest about what it is retrying.
    throw new ApiError(0, "network_unreachable", "The server could not be reached.");
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

function withQuery(path: string, values: Record<string, string | number | boolean | undefined | null>): string {
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

/** Which bucket a quote fell into for the report. */
export type SalesOutcome = "won" | "lost" | "pending" | "cancelled";

/**
 * The five counts, which always add up to `quotesSeen`.
 *
 * `cancelled` is its own bucket rather than a loss: the organization withdrew the quote, which is
 * neither a win anybody can claim nor a loss the customer inflicted. Folding it into `lost` would
 * make a seller who tidied up their pipeline look like a seller who was beaten.
 */
export type SalesReportTotals = {
  won: number;
  lost: number;
  pending: number;
  cancelled: number;
  quotes_seen: number;
  accepted_with_order: number;
  accepted_without_order: number;
};

export type SalesReportRow = {
  quote_id: string;
  number: string;
  customer: string;
  owner_name: string;
  outcome: SalesOutcome;
  status: string;
  date: string;
  currency: string;
  grand_total: string;
  order_number: string | null;
};

export type SalesOwnerRow = {
  owner_user_id: string | null;
  owner_name: string;
  totals: SalesReportTotals;
  average_deal: string | null;
  won_value: string;
};

export type SalesReport = {
  from: string;
  to: string;
  /** The organization's default currency — not a claim that every row is in it. */
  currency: string;
  totals: SalesReportTotals;
  by_owner: SalesOwnerRow[];
  rows: SalesReportRow[];
  /** How many quotes the filter matched, before the table's cap. */
  rows_matched: number;
  /** Whether the table is showing only some of them. */
  truncated: boolean;
  /** Won ÷ (won + lost), in hundredths of a percent, or `null` when nothing was decided. */
  conversion_bps: number | null;
  /** Accepted quotes that became an order ÷ every accepted quote. */
  order_conversion_bps: number | null;
  average_deal: string | null;
  won_value: string;
  lost_value: string;
};

export type SalesReportQuery = {
  from?: string;
  to?: string;
  owner_user_id?: string;
  unassigned?: boolean;
  status?: string;
  limit?: number;
};

/** One document the global search found. */
export type SalesSearchHit = {
  kind: "quote" | "order";
  id: string;
  number: string;
  customer: string;
  title: string;
  status: string;
  currency: string;
  grand_total: string;
  updated_at: string;
  url: string;
  /** Whether the number or the customer is what matched. */
  matched_on: "number" | "customer";
};

export type SalesSearchResults = {
  hits: SalesSearchHit[];
  quotes: number;
  orders: number;
};

// ---------------------------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------------------------

export function fetchSalesReport(query: SalesReportQuery = {}): Promise<SalesReport> {
  return reportsRequest<SalesReport>(
    withQuery("/api/v1/sales/reports/summary", {
      from: query.from,
      to: query.to,
      owner_user_id: query.owner_user_id,
      unassigned: query.unassigned ? "true" : undefined,
      status: query.status,
      limit: query.limit,
    }),
  );
}

/**
 * The CSV, as a filename the browser will use.
 *
 * A `blob` URL rather than a plain `href`: the export is behind a permission, so a browser that
 * downloads it by navigating would have to be logged in twice, and — worse — the failing case
 * would render the API's JSON error *as the file*, so a seller whose session had expired would
 * find a file called `sales-report.csv` containing `{"error": ...}`. Fetching it puts the refusal
 * in the error path where `ErrorState` can show it, and only writes a file when the response is
 * a real CSV.
 */
export async function downloadSalesReportCsv(query: SalesReportQuery = {}): Promise<string> {
  const response = await fetch(
    withQuery("/api/v1/sales/reports/export", {
      from: query.from,
      to: query.to,
      owner_user_id: query.owner_user_id,
      unassigned: query.unassigned ? "true" : undefined,
      status: query.status,
      limit: query.limit,
    }),
    { credentials: "same-origin", headers: { Accept: "text/csv" } },
  );
  if (!response.ok) {
    throw await readFailure(response);
  }
  const blob = await response.blob();
  const filename = filenameFrom(response.headers.get("content-disposition")) ?? "sales-report.csv";
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  // Revoking on the next frame rather than immediately: a browser that has not started the
  // download yet cancels it, and the seller is left with no file and no error.
  window.setTimeout(() => URL.revokeObjectURL(url), 30_000);
  return filename;
}

/** The name the API asked for, read out of `Content-Disposition`. */
function filenameFrom(header: string | null): string | null {
  if (!header) return null;
  const match = /filename="([^"]+)"/.exec(header) ?? /filename=([^;]+)/.exec(header);
  if (!match) return null;
  const value = match[1].trim();
  // Only a plain, path-free name: a header is attacker-influenced in the general case, and a
  // `../../` in a download name is a thing no legitimate report produces.
  return /^[A-Za-z0-9._-]+$/.test(value) ? value : null;
}

export function searchSales(term: string, limit = 20): Promise<SalesSearchResults> {
  return reportsRequest<SalesSearchResults>(
    withQuery("/api/v1/sales/search", { q: term, limit }),
  );
}

// ---------------------------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------------------------

/**
 * A hundredths-of-a-percent figure, printed.
 *
 * `null` becomes an em dash rather than `0%`, because the difference between "nobody decided yet"
 * and "nobody won" is the whole reason the server sends `null`. A report that printed 0% for a
 * desk that has not quoted yet is a fact about a competitor that is not there.
 */
export function formatPercent(bps: number | null | undefined): string {
  if (bps === null || bps === undefined) return "—";
  const percent = bps / 100;
  const rounded = Math.round(percent * 100) / 100;
  return `${rounded.toLocaleString(undefined, { maximumFractionDigits: 2 })}%`;
}

/** The four buckets, in the order the stat cards print them. */
export const SALES_OUTCOMES: { value: SalesOutcome; label: string; tone: string }[] = [
  { value: "won", label: "Won", tone: "good" },
  { value: "lost", label: "Lost", tone: "bad" },
  { value: "pending", label: "Pending", tone: "neutral" },
  { value: "cancelled", label: "Cancelled", tone: "muted" },
];

/** The badge tone for one bucket, matching the stat cards. */
export function outcomeTone(outcome: string): "good" | "bad" | "neutral" | "muted" {
  switch (outcome) {
    case "won":
      return "good";
    case "lost":
      return "bad";
    case "cancelled":
      return "muted";
    default:
      return "neutral";
  }
}

/** The status filter's options — the quote lifecycle, as `sales_quotes.ts` names it. */
export const SALES_REPORT_STATUS_FILTERS: { value: string; label: string }[] = [
  { value: "all", label: "Every status" },
  { value: "draft", label: "Draft" },
  { value: "pending_approval", label: "Pending approval" },
  { value: "approved", label: "Approved" },
  { value: "sent", label: "Sent" },
  { value: "accepted", label: "Accepted" },
  { value: "declined", label: "Declined" },
  { value: "expired", label: "Expired" },
  { value: "cancelled", label: "Cancelled" },
];
