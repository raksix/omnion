/**
 * The inventory client: the twenty-two routes slice 1 shipped and the eight slice 2 adds
 * (REQ-053).
 *
 * It follows the CRM and sales clients' rule rather than inventing a third one: the panel owns
 * the transport, and what lives here is the module's own vocabulary — the `inventory.*` keys, the
 * decimal **as a string**, and the two shapes the drawer's response can take.
 *
 * ## Three things about this client that are deliberate
 *
 * * **A quantity is a string, never a number.** The module sends decimal text on purpose (a
 *   binary float cannot hold `0.1`), so parsing one into a JS number here would reintroduce
 *   exactly the drift the module exists to prevent. The screens format for display and hand the
 *   text back untouched. `Quantity` is branded so a plain `number` cannot be passed where one is
 *   expected without a cast nobody will notice.
 * * **A record request has two outcomes and the server says which.** `POST /movements` answers
 *   `201` with a written movement, or `202` with a request that is waiting on somebody else's
 *   decision. The screen branches on `status` rather than deciding for itself — the threshold
 *   lives on the server and a second comparison in the browser is a second answer.
 * * **The CSV is fetched, not built.** A download built in the browser from the rows already on
 *   screen would export the *page* (one page of fifty), and the criterion says the file matches
 *   the table. Asking the server for it with the same filter gets the same rows the query
 *   returns, which is the honest version of "matches".
 */
import { ApiError, type ErrorBody } from "./api";

/** A decimal quantity as the module sends it: `"10.000"`, never `10`. */
export type Quantity = string & { readonly __quantity: unique symbol };

/** A `Response` that is not `ok`, turned into the platform's own error. */
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

/** One JSON call, with the same session, accept header and error shape the panel uses. */
async function inventoryRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
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

/** One call that downloads rather than parses — the CSV exports. */
async function download(path: string, fallbackName: string): Promise<void> {
  const response = await fetch(path, { credentials: "same-origin" });
  if (!response.ok) {
    throw await readFailure(response);
  }
  // The server sends the filename, so the file on disk is named by the same rule the export
  // used. A browser that ignores the header would otherwise call it `download` forever.
  const disposition = response.headers.get("content-disposition") ?? "";
  const match = /filename="([^"]+)"/.exec(disposition);
  const blob = await response.blob();
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = match?.[1] ?? fallbackName;
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  URL.revokeObjectURL(url);
}

// ---------------------------------------------------------------------------------------------
// Shapes
// ---------------------------------------------------------------------------------------------

/** The badge a stock row wears. One vocabulary, shared by the list, the header and the filter. */
export type StockStatus = "ok" | "low" | "below_reorder" | "negative";

/** An item as the list and the form read it. */
export type InventoryItem = {
  id: string;
  organization_id: string;
  sku: string;
  name: string;
  category: string | null;
  unit: string;
  barcode: string | null;
  min_threshold: Quantity;
  reorder_point: Quantity;
  reorder_qty: Quantity;
  cost: string | null;
  currency: string;
  product_id: string | null;
  notes: string;
  active: boolean;
  archived_at: string | null;
  created_at: string;
  updated_at: string;
};

/** One item × location row of the stock list. */
export type StockLevel = {
  id: string;
  item_id: string;
  sku: string;
  name: string;
  category: string | null;
  unit: string;
  location_id: string;
  location_code: string;
  location_name: string;
  warehouse_id: string;
  warehouse_code: string;
  on_hand: Quantity;
  reserved: Quantity;
  available: Quantity;
  min_threshold: Quantity;
  reorder_point: Quantity;
  status: StockStatus;
  last_movement_at: string | null;
};

/** A warehouse, with its totals. */
export type Warehouse = {
  id: string;
  code: string;
  name: string;
  active: boolean;
  locations: Location[];
  item_count: number;
  on_hand: Quantity;
};

/** A location inside a warehouse. */
export type Location = {
  id: string;
  warehouse_id: string;
  warehouse_code: string;
  code: string;
  name: string;
  kind: string;
  active: boolean;
  item_count: number;
  on_hand: Quantity;
};

/** The movement kinds the module writes. */
export type MovementKind =
  | "receipt"
  | "issue"
  | "transfer_out"
  | "transfer_in"
  | "adjustment"
  | "reserve"
  | "release";

/** A ledger row, as the ledger screen and the item history read it. */
export type Movement = {
  id: number;
  organization_id: string;
  item_id: string;
  sku: string;
  item_name: string;
  location_id: string;
  location_code: string;
  kind: MovementKind;
  quantity: Quantity;
  reason: string;
  note: string;
  source_kind: string | null;
  source_id: string | null;
  on_hand_after: Quantity;
  reserved_after: Quantity;
  actor_user_id: string | null;
  created_at: string;
};

/** An adjustment request waiting on — or carrying — a decision. */
export type AdjustmentApproval = {
  id: string;
  organization_id: string;
  item_id: string;
  sku: string;
  item_name: string;
  location_id: string;
  location_code: string;
  kind: MovementKind;
  mode: "delta" | "counted";
  quantity: Quantity;
  reason: string;
  note: string;
  amount: Quantity;
  threshold: Quantity;
  on_hand_at_request: Quantity;
  status: "pending" | "approved" | "rejected" | "cancelled";
  decision: string | null;
  decided_by: string | null;
  decided_at: string | null;
  comment: string | null;
  movement_id: number | null;
  requested_by: string;
  created_at: string;
};

/** The organization's thresholds. */
export type InventorySettings = {
  adjustment_approval_threshold: Quantity;
  default_adjustment_reason: string;
  alerts_on_read: boolean;
  default_unit: string;
};

/** One page of a list, with the cursor the next one starts from. */
export type Page<T> = {
  items: T[];
  next_cursor: string | null;
  total_estimate: number;
};

/** The overview's counters. */
export type InventoryOverview = {
  item_count: number;
  warehouse_count: number;
  below_threshold: number;
  negative: number;
  movements_today: number;
  pending_approvals: number;
  low_stock: { item_id: string; sku: string; name: string; available: Quantity; reorder_point: Quantity }[];
};

/** What a drawer preview says would happen. */
export type MovementPreview = {
  on_hand_before: Quantity;
  on_hand_after: Quantity;
  reserved_before: Quantity;
  reserved_after: Quantity;
  available_after: Quantity;
  kind: MovementKind;
  reason: string;
  status: StockStatus;
};

/**
 * What `POST /movements` produced.
 *
 * The discriminant is the server's answer, not the client's arithmetic: a screen that compared
 * the quantity to the threshold itself would have to hold the threshold, and the moment the two
 * disagreed the drawer would say "recorded" over a request nobody had approved.
 */
export type RecordOutcome =
  | { status: "recorded"; movement: Movement; position: StockLevel }
  | { status: "awaiting_approval"; approval: AdjustmentApproval };

/** The reason codes the drawer offers, with the sentence a person reads beside each. */
export const REASON_CODES: { value: string; label: string }[] = [
  { value: "purchase_receipt", label: "Purchase receipt" },
  { value: "sale_shipment", label: "Sale shipment" },
  { value: "customer_return", label: "Customer return" },
  { value: "supplier_return", label: "Return to supplier" },
  { value: "damage", label: "Damage" },
  { value: "loss", label: "Loss" },
  { value: "correction", label: "Correction" },
  { value: "internal_use", label: "Internal use" },
  { value: "stocktake_variance", label: "Stocktake variance" },
];

/** What the stock list and the ledger can be filtered by, as the API takes it. */
export type StockFilters = {
  search?: string;
  warehouse_id?: string;
  location_id?: string;
  status?: "ok" | "low" | "below_reorder" | "negative" | "below_threshold";
  category?: string;
  idle_days?: number;
  limit?: number;
  cursor?: string;
};

/** What the ledger can be filtered by. */
export type MovementFilters = {
  search?: string;
  item_id?: string;
  location_id?: string;
  kinds?: string[];
  reason?: string;
  actor_user_id?: string;
  source?: string;
  from?: string;
  to?: string;
  limit?: number;
  cursor?: string;
};

/** Turn a filter object into a query string, dropping the empty ones. */
function query(filters: Record<string, unknown> | undefined): string {
  const parts: string[] = [];
  for (const [key, value] of Object.entries(filters ?? {})) {
    if (value === undefined || value === null || value === "") {
      continue;
    }
    if (Array.isArray(value)) {
      // Repeated `kinds=` is what the API reads; a comma-joined string is not the same thing and
      // is silently dropped by the query builder.
      for (const item of value) {
        parts.push(`${encodeURIComponent(key)}=${encodeURIComponent(String(item))}`);
      }
      continue;
    }
    parts.push(`${encodeURIComponent(key)}=${encodeURIComponent(String(value))}`);
  }
  return parts.length ? `?${parts.join("&")}` : "";
}

// ---------------------------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------------------------

/** The overview's counters. */
export function fetchOverview(): Promise<InventoryOverview> {
  return inventoryRequest<InventoryOverview>("/api/v1/inventory");
}

/** The reason codes, the units and the categories, for the form's selects. */
export function fetchVocabulary(): Promise<{
  reasons: { value: string; label: string }[];
  units: string[];
  categories: string[];
}> {
  return inventoryRequest("/api/v1/inventory/vocabulary");
}

/** The organization's thresholds. */
export function fetchSettings(): Promise<InventorySettings> {
  return inventoryRequest<InventorySettings>("/api/v1/inventory/settings");
}

/** Write the thresholds. */
export function saveSettings(patch: Partial<InventorySettings>): Promise<InventorySettings> {
  return inventoryRequest<InventorySettings>("/api/v1/inventory/settings", {
    method: "PUT",
    body: JSON.stringify(patch),
  });
}

/** One page of items. */
export function fetchItems(
  filters: { search?: string; category?: string; active?: boolean; limit?: number; cursor?: string } = {},
): Promise<Page<InventoryItem>> {
  return inventoryRequest<Page<InventoryItem>>(`/api/v1/inventory/items${query(filters)}`);
}

/** One item, with its per-location stock and totals. */
export function fetchItem(id: string, historyLimit = 25): Promise<{
  item: InventoryItem;
  locations: StockLevel[];
  on_hand: Quantity;
  reserved: Quantity;
  available: Quantity;
  status: StockStatus;
  last_movement_at: string | null;
  history: Movement[];
}> {
  return inventoryRequest(`/api/v1/inventory/items/${encodeURIComponent(id)}?history_limit=${historyLimit}`);
}

/** Resolve a scanner's code to an item. A miss is a `404`, not an empty list. */
export function lookupItem(code: string): Promise<InventoryItem> {
  return inventoryRequest<InventoryItem>(
    `/api/v1/inventory/items/lookup${query({ code })}`,
  );
}

/** Create an item. */
export function createItem(body: Record<string, unknown>): Promise<InventoryItem> {
  return inventoryRequest<InventoryItem>("/api/v1/inventory/items", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** Edit an item. */
export function updateItem(id: string, body: Record<string, unknown>): Promise<InventoryItem> {
  return inventoryRequest<InventoryItem>(`/api/v1/inventory/items/${encodeURIComponent(id)}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

/** Archive an item. Nothing is ever deleted — a ledger row still names it. */
export function archiveItem(id: string): Promise<InventoryItem> {
  return inventoryRequest<InventoryItem>(`/api/v1/inventory/items/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

/** One page of stock levels. */
export function fetchStock(filters: StockFilters = {}): Promise<Page<StockLevel>> {
  return inventoryRequest<Page<StockLevel>>(`/api/v1/inventory/stock${query(filters as Record<string, unknown>)}`);
}

/** The warehouse tree. */
export function fetchWarehouses(): Promise<Warehouse[]> {
  return inventoryRequest<Warehouse[]>("/api/v1/inventory/warehouses");
}

/** One page of the movement ledger. */
export function fetchMovements(filters: MovementFilters = {}): Promise<Page<Movement>> {
  return inventoryRequest<Page<Movement>>(`/api/v1/inventory/movements${query(filters as Record<string, unknown>)}`);
}

/**
 * Ask what a movement would do, without writing it.
 *
 * Called on every keystroke by the drawer. It answers **even when the write would be refused**,
 * because the refusal is the preview: "this would leave -3, and 6 are available" is the sentence
 * the drawer needs more than a `422` three fields later.
 */
export function previewMovement(body: {
  item_id: string;
  location_id: string;
  quantity: string;
  mode?: "delta" | "counted";
  kind?: string;
  reason?: string;
}): Promise<MovementPreview> {
  return inventoryRequest<MovementPreview>("/api/v1/inventory/movements/preview", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/**
 * Record a movement — or, when it is over the threshold, raise a request for it.
 *
 * The two outcomes share one call on purpose; see [`RecordOutcome`].
 */
export async function recordMovement(body: {
  item_id: string;
  location_id: string;
  quantity: string;
  mode?: "delta" | "counted";
  kind?: string;
  reason?: string;
  note?: string;
}): Promise<RecordOutcome> {
  return inventoryRequest<RecordOutcome>("/api/v1/inventory/movements", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** One page of the adjustment inbox. */
export function fetchApprovals(
  filters: { status?: string; item_id?: string; limit?: number; cursor?: string } = {},
): Promise<Page<AdjustmentApproval>> {
  return inventoryRequest<Page<AdjustmentApproval>>(`/api/v1/inventory/approvals${query(filters)}`);
}

/** How many requests are waiting — the nav badge. */
export function fetchPendingApprovalCount(): Promise<{ pending: number }> {
  return inventoryRequest<{ pending: number }>("/api/v1/inventory/approvals/pending-count");
}

/** Approve or reject. A rejection needs a comment; the server refuses one without. */
export function decideApproval(
  id: string,
  decision: "approve" | "reject",
  comment?: string,
): Promise<{ approval: AdjustmentApproval; movement: Movement | null }> {
  return inventoryRequest(`/api/v1/inventory/approvals/${encodeURIComponent(id)}/decision`, {
    method: "POST",
    body: JSON.stringify({ decision, comment }),
  });
}

/** Withdraw your own request while it is still waiting. */
export function cancelApproval(id: string): Promise<AdjustmentApproval> {
  return inventoryRequest<AdjustmentApproval>(
    `/api/v1/inventory/approvals/${encodeURIComponent(id)}/cancel`,
    { method: "POST" },
  );
}

/** Download the stock list as a CSV, with the same filters the table is showing. */
export function exportStockCsv(filters: StockFilters = {}): Promise<void> {
  return download(`/api/v1/inventory/stock/export${query(filters as Record<string, unknown>)}`, "stock.csv");
}

/** Download the ledger as a CSV, with the same filters the table is showing. */
export function exportMovementsCsv(filters: MovementFilters = {}): Promise<void> {
  return download(
    `/api/v1/inventory/movements/export${query(filters as Record<string, unknown>)}`,
    "movements.csv",
  );
}

/** Download the decision list as a CSV. */
export function exportApprovalsCsv(filters: { status?: string; item_id?: string } = {}): Promise<void> {
  return download(`/api/v1/inventory/approvals/export${query(filters)}`, "adjustments.csv");
}

// ---------------------------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------------------------

/**
 * A quantity for display, in the module's own three decimals.
 *
 * A passthrough rather than a formatter: the module prints `10.000` and the screen prints
 * `10.000`, because a ledger column that lines up on the server and not on the page is a column
 * nobody can scan. Trimming trailing zeros is a presentation decision this deliberately refuses
 * to make.
 */
export function formatQuantity(value: Quantity | null | undefined): string {
  return value ?? "—";
}

/** The sentence a badge prints beside its colour — never colour alone. */
export function statusLabel(status: StockStatus): string {
  switch (status) {
    case "ok":
      return "In stock";
    case "low":
      return "Below minimum";
    case "below_reorder":
      return "Below reorder point";
    case "negative":
      return "Negative";
    default:
      return status;
  }
}

/** The classes a badge wears. The text label above is what carries the meaning. */
export function statusTone(status: StockStatus): string {
  switch (status) {
    case "negative":
      return "border-red-300 bg-red-50 text-red-800";
    case "below_reorder":
      return "border-amber-300 bg-amber-50 text-amber-900";
    case "low":
      return "border-amber-200 bg-amber-50/70 text-amber-900";
    default:
      return "border-emerald-200 bg-emerald-50 text-emerald-900";
  }
}

/** A movement's signed quantity, coloured by direction — with the sign always in the text. */
export function signedText(movement: Movement): string {
  const raw = Number.parseFloat(movement.quantity);
  const sign = raw < 0 ? "−" : "+";
  return `${sign}${Math.abs(raw).toFixed(3)}`;
}

/** The direction's colour class. The sign in the text is what carries the meaning. */
export function movementTone(movement: Movement): string {
  return Number.parseFloat(movement.quantity) < 0 ? "text-red-700" : "text-emerald-800";
}
