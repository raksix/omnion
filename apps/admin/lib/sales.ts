/**
 * The sales client: the sellable catalog and the price lists the `/sales` screens read and write
 * (REQ-052, slice 1).
 *
 * It follows the CRM client's rule rather than inventing a second one: the panel owns the
 * transport (`api.ts` keeps its `request` private on purpose, so a second copy of the
 * fetch/JSON/error handling is a second thing that can drift), and what lives here is the module's
 * own vocabulary — `sales.products.manage`, the money as the API sends it, the validity window as
 * a `YYYY-MM-DD` string.
 *
 * Two things about the money are deliberate and are not the platform's defaults:
 *
 * * **A price is a string, never a number.** The module sends decimal text on purpose (a float
 *   cannot hold 0.1), so parsing one into a JS number here would reintroduce exactly the drift the
 *   module exists to prevent. The screens format it for display and hand the text back untouched.
 * * **A date is a `YYYY-MM-DD` string.** The panel's date inputs produce that and the API accepts
 *   it, so nothing in the UI has to guess a wire format.
 */
import { ApiError, type ErrorBody } from "./api";

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
    // `details` is read here, not only in the platform client: the forms attach
    // `error.details.field` to the input the refusal is about, so dropping the object turns every
    // field-level refusal into a banner with nothing under the field.
    details = body.error?.details ?? null;
    requestId = body.error?.request_id ?? requestId;
  } catch {
    // A non-JSON body is still an error; the status stays in the message.
  }
  return new ApiError(response.status, code, message, details, requestId);
}

/** One JSON call, with the same session, accept header and error shape the panel uses. */
async function salesRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
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

// ---------------------------------------------------------------------------------------------
// Shapes
// ---------------------------------------------------------------------------------------------

/** A product, as the catalog list and the editor both read it. */
export type SalesProduct = {
  id: string;
  organization_id: string;
  sku: string;
  name: string;
  description: string;
  category: string | null;
  unit: string;
  tax_percent: number;
  /** Decimal text, exactly as the module stores it. Never parsed into a number here. */
  default_price: string;
  currency: string;
  active: boolean;
  archived_at: string | null;
  created_at: string;
  updated_at: string;
};

/** One page of rows plus the cursor of the next one. */
export type SalesPage<T> = {
  items: T[];
  next_cursor: string | null;
  total_estimate: number;
};

/** A price list, as the list screen and the editor header read it. */
export type SalesPriceList = {
  id: string;
  organization_id: string;
  name: string;
  currency: string;
  active: boolean;
  /** `YYYY-MM-DD`, or null for an open edge. */
  valid_from: string | null;
  /** `YYYY-MM-DD`, or null for an open edge. */
  valid_until: string | null;
  item_count: number;
  archived_at: string | null;
  created_at: string;
};

/** A price list with its rows, which is what the editor needs. */
export type SalesPriceListDetail = {
  list: SalesPriceList;
  items: SalesPriceRow[];
};

/** One price row: a product, the price the list charges, and where that price starts. */
export type SalesPriceRow = {
  id: string;
  product_id: string;
  product_sku: string;
  product_name: string;
  /** Decimal text with the precision it was stored at. */
  min_quantity: string;
  price: string;
  unit: string;
};

/** The organizations' existing categories and the units the catalog offers. */
export type SalesVocabulary = {
  categories: string[];
  units: string[];
};

/** The organization's sales settings — what a new quote is built from. */
export type SalesSettings = {
  currency: string;
  discount_approval_threshold: number;
  quote_validity_days: number;
  quote_number_prefix: string;
  order_number_prefix: string;
};

/** The query of a catalog list. Every key is optional and every one lands on a real filter. */
export type SalesCatalogQuery = {
  search?: string;
  category?: string;
  active?: boolean;
  include_archived?: boolean;
  sort?: string;
  direction?: "asc" | "desc";
  limit?: number;
  cursor?: string;
  organization_id?: string;
};

/** The values a product create carries. */
export type SalesProductInput = {
  sku: string;
  name: string;
  description?: string;
  category?: string | null;
  unit?: string;
  tax_percent?: number;
  default_price?: string;
  currency?: string;
  active?: boolean;
};

/** A product edit. Only the keys present are changed. */
export type SalesProductPatch = Partial<SalesProductInput>;

/** The values a price-list create carries. */
export type SalesPriceListInput = {
  name: string;
  currency?: string;
  active?: boolean;
  /** `YYYY-MM-DD`; omitted and null are the same request, as a blank form field produces both. */
  valid_from?: string | null;
  valid_until?: string | null;
};

/** A price-list edit. Only the keys present are changed. */
export type SalesPriceListPatch = Partial<SalesPriceListInput>;

/** One row of the price editor's grid, as it is saved. */
export type SalesPriceRowInput = {
  product_id: string;
  price: string;
  min_quantity?: string;
};

/**
 * What one unit of a product costs, and where that answer came from.
 *
 * It is a **unit** price and not a line total: the quantity multiplies in the quote, so a resolver
 * that answered a total would make the builder multiply it a second time. The `source` is on the
 * answer rather than inferred by the caller, because "this price came from somewhere else" is a
 * question a person asks when a quote total is not what they expected — `default` is the one that
 * must never be a silent fallback.
 */
export type SalesResolvedPrice = {
  product_id: string;
  /** The quantity it was asked for, with the precision it was stored at. */
  quantity: string;
  unit: string;
  unit_price: string;
  currency: string;
  tax_percent: number;
  price_list_id: string | null;
  /** `resolved` when the named list had a row, `default` when the product's own price answered. */
  source: "resolved" | "default";
};

// ---------------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------------

/** Build a query string, dropping empty values so a blank filter is not sent as `?search=`. */
function queryString(params: Record<string, string | number | boolean | undefined | null>): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null || value === "") continue;
    search.set(key, String(value));
  }
  const text = search.toString();
  return text ? `?${text}` : "";
}

/** The catalog page. */
export function fetchSalesProducts(query: SalesCatalogQuery = {}): Promise<SalesPage<SalesProduct>> {
  return salesRequest<SalesPage<SalesProduct>>(`/api/v1/sales/products${queryString({ ...query })}`);
}

/** One product, readable even when archived — a past quote still names it. */
export function fetchSalesProduct(id: string, organizationId?: string | null): Promise<SalesProduct> {
  return salesRequest<SalesProduct>(
    `/api/v1/sales/products/${id}${queryString({ organization_id: organizationId ?? undefined })}`,
  );
}

/** The categories and units the catalog form offers. */
export function fetchSalesVocabulary(organizationId?: string | null): Promise<SalesVocabulary> {
  return salesRequest<SalesVocabulary>(
    `/api/v1/sales/products/vocabulary${queryString({ organization_id: organizationId ?? undefined })}`,
  );
}

/** What a line of `quantity` would cost, from the list or from the product's own default. */
export function fetchResolvedPrice(
  productId: string,
  quantity: string,
  priceListId?: string | null,
  organizationId?: string | null,
): Promise<SalesResolvedPrice> {
  return salesRequest<SalesResolvedPrice>(
    `/api/v1/sales/products/${productId}/price${queryString({
      quantity,
      price_list_id: priceListId ?? undefined,
      organization_id: organizationId ?? undefined,
    })}`,
  );
}

/** The price-list page. */
export function fetchSalesPriceLists(
  query: { search?: string; active?: boolean; limit?: number; organization_id?: string } = {},
): Promise<SalesPage<SalesPriceList>> {
  return salesRequest<SalesPage<SalesPriceList>>(`/api/v1/sales/pricelists${queryString({ ...query })}`);
}

/** One price list with its rows. */
export function fetchSalesPriceList(
  id: string,
  organizationId?: string | null,
): Promise<SalesPriceListDetail> {
  return salesRequest<SalesPriceListDetail>(
    `/api/v1/sales/pricelists/${id}${queryString({ organization_id: organizationId ?? undefined })}`,
  );
}

/** The settings every new quote is built from. */
export function fetchSalesSettings(organizationId?: string | null): Promise<SalesSettings> {
  return salesRequest<SalesSettings>(
    `/api/v1/sales/settings${queryString({ organization_id: organizationId ?? undefined })}`,
  );
}

// ---------------------------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------------------------

/** Create a product. */
export function createSalesProduct(
  body: SalesProductInput,
  organizationId?: string | null,
): Promise<SalesProduct> {
  return salesRequest<SalesProduct>(`/api/v1/sales/products${queryString({ organization_id: organizationId ?? undefined })}`, {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** Edit a product. */
export function updateSalesProduct(
  id: string,
  body: SalesProductPatch,
  organizationId?: string | null,
): Promise<SalesProduct> {
  return salesRequest<SalesProduct>(`/api/v1/sales/products/${id}${queryString({ organization_id: organizationId ?? undefined })}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

/**
 * Archive a product.
 *
 * Named `archive` and not `delete` because that is what it does: the row stays readable for the
 * quote lines that named it, and only the list stops offering it.
 */
export function archiveSalesProduct(id: string, organizationId?: string | null): Promise<SalesProduct> {
  return salesRequest<SalesProduct>(`/api/v1/sales/products/${id}${queryString({ organization_id: organizationId ?? undefined })}`, {
    method: "DELETE",
  });
}

/** Create a price list. */
export function createSalesPriceList(
  body: SalesPriceListInput,
  organizationId?: string | null,
): Promise<SalesPriceList> {
  return salesRequest<SalesPriceList>(`/api/v1/sales/pricelists${queryString({ organization_id: organizationId ?? undefined })}`, {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** Edit a price list. */
export function updateSalesPriceList(
  id: string,
  body: SalesPriceListPatch,
  organizationId?: string | null,
): Promise<SalesPriceList> {
  return salesRequest<SalesPriceList>(`/api/v1/sales/pricelists/${id}${queryString({ organization_id: organizationId ?? undefined })}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

/** Archive a price list, which also frees its name. */
export function archiveSalesPriceList(
  id: string,
  organizationId?: string | null,
): Promise<SalesPriceList> {
  return salesRequest<SalesPriceList>(`/api/v1/sales/pricelists/${id}${queryString({ organization_id: organizationId ?? undefined })}`, {
    method: "DELETE",
  });
}

/**
 * Replace a price list's rows in one call.
 *
 * `PUT` and not `PATCH`, because it is a replacement: a row missing from the request is a row the
 * list no longer has, and doing it in a single statement is what stops a save that deleted the
 * rows and then failed on the third insert from leaving a list that prices nothing.
 */
export function replaceSalesPriceRows(
  id: string,
  items: SalesPriceRowInput[],
  organizationId?: string | null,
): Promise<SalesPriceListDetail> {
  return salesRequest<SalesPriceListDetail>(`/api/v1/sales/pricelists/${id}/items${queryString({ organization_id: organizationId ?? undefined })}`, {
    method: "PUT",
    body: JSON.stringify({ items }),
  });
}

/** Save the settings. */
export function saveSalesSettings(
  body: Partial<SalesSettings>,
  organizationId?: string | null,
): Promise<SalesSettings> {
  return salesRequest<SalesSettings>(`/api/v1/sales/settings${queryString({ organization_id: organizationId ?? undefined })}`, {
    method: "PUT",
    body: JSON.stringify(body),
  });
}

// ---------------------------------------------------------------------------------------------
// Presentation
// ---------------------------------------------------------------------------------------------

/** The units a product may be sold in, and the label each one carries. */
export const SALES_UNITS: { value: string; label: string }[] = [
  { value: "piece", label: "Piece" },
  { value: "hour", label: "Hour" },
  { value: "day", label: "Day" },
  { value: "kg", label: "Kilogram" },
  { value: "m", label: "Metre" },
  { value: "month", label: "Month" },
];

/** A unit as a readable word, for a cell that shows it beside a quantity. */
export function unitLabel(unit: string): string {
  return SALES_UNITS.find((entry) => entry.value === unit)?.label ?? unit;
}

/**
 * Money for display, from the decimal text the API sends.
 *
 * The text is split by hand rather than through `Number`, because a float cannot hold 0.1 and this
 * screen's job is to show what was stored. Grouping is added at the thousand, and the decimals are
 * whatever precision the amount carries — a price list may quote 0.125 of a currency unit per
 * kilogram, and rounding that to two places on the way to the screen would be a lie.
 */
export function formatMoney(amount: string, currency?: string | null): string {
  const text = (amount ?? "").trim();
  const match = /^(-?)(\d+)(?:\.(\d+))?$/.exec(text);
  if (!match) {
    // Never show `NaN`: an amount the module could not read is shown as the text that arrived, so
    // the reader sees the problem rather than a formatted lie.
    return currency ? `${text} ${currency}` : text;
  }
  const [, sign, whole, decimals] = match;
  const grouped = whole.replace(/\B(?=(\d{3})+(?!\d))/g, " ");
  const body = decimals ? `${grouped}.${decimals}` : grouped;
  const value = `${sign}${body}`;
  return currency ? `${value} ${currency}` : value;
}

/**
 * The today/tomorrow comparison a validity window is judged by.
 *
 * The day is computed from the browser's own clock rather than UTC: a list that ends "today" must
 * stay usable for the person in it, and `new Date().toISOString()` is tomorrow for anybody east of
 * Greenwich after four in the afternoon.
 */
export function todayIso(): string {
  const now = new Date();
  const local = new Date(now.getTime() - now.getTimezoneOffset() * 60_000);
  return local.toISOString().slice(0, 10);
}

/** How a validity window reads on a list row: the days left, or what it says instead. */
export function validityHint(
  validUntil: string | null,
  today: string = todayIso(),
): { tone: "ok" | "warn" | "expired"; text: string } {
  if (!validUntil) {
    return { tone: "ok", text: "No end date" };
  }
  if (validUntil < today) {
    return { tone: "expired", text: `Ended ${validUntil}` };
  }
  const days = Math.round(
    (Date.parse(`${validUntil}T00:00:00Z`) - Date.parse(`${today}T00:00:00Z`)) / 86_400_000,
  );
  if (days === 0) return { tone: "warn", text: "Ends today" };
  if (days === 1) return { tone: "warn", text: "Ends tomorrow" };
  if (days <= 7) return { tone: "warn", text: `Ends in ${days} days` };
  return { tone: "ok", text: `Ends ${validUntil}` };
}

/** The field a refusal is about, so a form can put the message under the input. */
export function fieldOf(problem: unknown): string | null {
  if (problem instanceof ApiError) {
    const field = problem.details?.field;
    return typeof field === "string" ? field : null;
  }
  return null;
}
