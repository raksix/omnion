/**
 * The accounting client: the chart of accounts, the tax rates and the journal (REQ-054, slice 1).
 *
 * It follows the inventory and sales clients' rule rather than inventing a fourth one: the panel
 * owns the transport, and what lives here is the module's own vocabulary — the `accounting.*`
 * keys, a money amount **as a string**, and the shape of an unbalanced entry.
 *
 * ## Why an amount is a string, again
 *
 * The module sends decimal text on purpose (a binary float cannot hold `0.1`, and a balance drawn
 * from one is a balance that is a cent out). Parsing it into a JS number here would reintroduce
 * exactly the drift the module exists to prevent — and in this module the consequence is worse
 * than a rounding artefact in a list, because **the sum of the debit column is the product**. So
 * `Money` is branded, the screens format it for display, and the text is handed back untouched.
 *
 * ## The one thing this client exists to get right
 *
 * [`postJournalEntry`] returns a **rejection with the numbers in it**. The server answers `422`
 * with `debit_total`, `credit_total` and a signed `difference`, and those three are what the
 * grid's footer prints. A client that collapsed the failure into "could not post" would throw
 * away the only part the operator can act on, and the entry they are looking at has to tell them
 * which way it is out by itself.
 */
import { ApiError, type ErrorBody } from "./api";

/** A money amount as the module sends it: `"1200.00"`, never `1200`. */
export type Money = string & { readonly __money: unique symbol };

/** The five top-level kinds, in the order the tree renders. */
export const ACCOUNT_KINDS = [
  { value: "asset", label: "Assets" },
  { value: "liability", label: "Liabilities" },
  { value: "equity", label: "Equity" },
  { value: "income", label: "Income" },
  { value: "expense", label: "Expenses" },
] as const;

/** Which side of the sale a rate applies to. */
export const TAX_RATE_KINDS = [
  { value: "sales", label: "Sales" },
  { value: "purchase", label: "Purchase" },
] as const;

/** What caused a journal entry. */
export const ENTRY_SOURCES = [
  { value: "manual", label: "Manual" },
  { value: "invoice", label: "Invoice" },
  { value: "payment", label: "Payment" },
  { value: "expense", label: "Expense" },
] as const;

/** An account as the tree editor reads it. */
export type Account = {
  id: string;
  organization_id: string;
  code: string;
  name: string;
  kind: string;
  parent_id: string | null;
  active: boolean;
  /** True for the eleven accounts the migration seeds. */
  system: boolean;
  /** How many journal lines name it — the "used by N lines" guard's number. */
  line_count: number;
  created_at: string;
};

/** A tax rate as the editor reads it. */
export type TaxRate = {
  id: string;
  organization_id: string;
  name: string;
  /** `"20.00"`. */
  percent: string;
  kind: string;
  is_default: boolean;
  active: boolean;
  created_at: string;
};

/** One line of an entry, with its account's code so the grid needs no second request. */
export type JournalLine = {
  id: string;
  entry_id: string;
  position: number;
  account_id: string;
  account_code: string;
  account_name: string;
  description: string;
  /** `"0.00"` or an amount — never `null`, so a cell is never blank. */
  debit: string;
  credit: string;
};

/** An entry with its lines. */
export type JournalEntry = {
  id: string;
  organization_id: string;
  entry_number: number;
  entry_date: string;
  memo: string;
  source_kind: string;
  source_id: string | null;
  debit_total: string;
  credit_total: string;
  /** Always `true` for a stored entry — that is the invariant, not a computed field. */
  balanced: boolean;
  posted_at: string | null;
  lines: JournalLine[];
  created_at: string;
};

/** An entry without its lines — what the list draws. */
export type JournalSummary = Omit<JournalEntry, "lines"> & { line_count: number };

/** One line of the posting form. */
export type JournalLineDraft = {
  account_id: string;
  description: string;
  debit: string;
  credit: string;
};

/**
 * What the server said when it refused an entry, with the three numbers kept.
 *
 * A class rather than a flag on the entry, because the screen has two genuinely different states
 * to render and one of them is an error: a form that has been refused is not a form with a
 * warning, and a screen that treats it as a warning buries the difference in a column the operator
 * has to notice themselves.
 */
export class UnbalancedEntryError extends ApiError {
  /** The sum of the debit column, as the server computed it. */
  readonly debitTotal: string;
  /** The sum of the credit column. */
  readonly creditTotal: string;
  /** `debitTotal - creditTotal`, signed, so the direction is readable. */
  readonly difference: string;

  constructor(message: string, details: Record<string, unknown> | null, requestId: string | null) {
    super(422, "accounting_journal_unbalanced", message, details, requestId);
    const source = (details ?? {}) as Partial<{
      debit_total: string;
      credit_total: string;
      difference: string;
    }>;
    this.debitTotal = source.debit_total ?? "0.00";
    this.creditTotal = source.credit_total ?? "0.00";
    this.difference = source.difference ?? "0.00";
  }
}

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
  if (code === "accounting_journal_unbalanced") {
    return new UnbalancedEntryError(message, details, requestId);
  }
  return new ApiError(response.status, code, message, details, requestId);
}

/** One JSON call, with the same session, accept header and error shape the panel uses. */
async function accountingRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
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
// The chart of accounts
// ---------------------------------------------------------------------------------------------

/** The whole chart, one request. */
export async function fetchAccounts(includeInactive = true): Promise<Account[]> {
  const query = includeInactive ? "" : "?include_inactive=false";
  return accountingRequest<Account[]>(`/api/v1/accounting/accounts${query}`);
}

/** Add an account to the chart. */
export async function createAccount(body: {
  code: string;
  name: string;
  kind: string;
  parent_id?: string | null;
}): Promise<Account> {
  return accountingRequest<Account>("/api/v1/accounting/accounts", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** Rename, re-parent or (de)activate an account. The code is not patchable, by design. */
export async function updateAccount(
  id: string,
  body: { name?: string; active?: boolean; parent_id?: string | null },
): Promise<Account> {
  return accountingRequest<Account>(`/api/v1/accounting/accounts/${id}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

/**
 * Close an account — never delete it.
 *
 * A `POST` to `/deactivate` and not a `DELETE` on the account itself, because the server has no
 * delete route at all: a journal line references an account with `on delete restrict`. A client
 * that offered a delete would be promising a request the API does not have.
 */
export async function setAccountActive(id: string, active: boolean): Promise<Account> {
  return accountingRequest<Account>(`/api/v1/accounting/accounts/${id}/deactivate`, {
    method: "POST",
    body: JSON.stringify({ active }),
  });
}

// ---------------------------------------------------------------------------------------------
// Tax rates
// ---------------------------------------------------------------------------------------------

/** Every rate, for the editor and the line grid. */
export async function fetchTaxRates(includeInactive = true): Promise<TaxRate[]> {
  const query = includeInactive ? "" : "?include_inactive=false";
  return accountingRequest<TaxRate[]>(`/api/v1/accounting/tax-rates${query}`);
}

/** Add a rate, optionally taking the default for its kind. */
export async function createTaxRate(body: {
  name: string;
  percent: string;
  kind: string;
  is_default?: boolean;
}): Promise<TaxRate> {
  return accountingRequest<TaxRate>("/api/v1/accounting/tax-rates", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/**
 * Edit a rate.
 *
 * The percent is editable and **no issued document is rewritten** — an invoice line stores its own
 * `tax_percent`, which is why this is a normal `PATCH` rather than the frozen write a rate's
 * history might suggest. Correcting a rate entered as 15 when it meant 5 must not change an
 * invoice somebody already sent.
 */
export async function updateTaxRate(
  id: string,
  body: { name?: string; percent?: string; is_default?: boolean; active?: boolean },
): Promise<TaxRate> {
  return accountingRequest<TaxRate>(`/api/v1/accounting/tax-rates/${id}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

// ---------------------------------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------------------------------

/** The query a journal list takes. */
export type JournalFilters = {
  source?: string;
  from?: string;
  to?: string;
  search?: string;
  limit?: number;
};

/** The entries, newest first, without their lines. */
export async function fetchJournal(filters: JournalFilters = {}): Promise<JournalSummary[]> {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(filters)) {
    if (value !== undefined && value !== null && value !== "") {
      query.set(key, String(value));
    }
  }
  const suffix = query.toString();
  return accountingRequest<JournalSummary[]>(
    `/api/v1/accounting/journal${suffix ? `?${suffix}` : ""}`,
  );
}

/** One entry with its lines. */
export async function fetchJournalEntry(id: string): Promise<JournalEntry> {
  return accountingRequest<JournalEntry>(`/api/v1/accounting/journal/${id}`);
}

/**
 * Post a manual entry.
 *
 * Throws [`UnbalancedEntryError`] — which carries the three numbers — when the columns do not
 * agree. The `422` is not folded into a generic failure here on purpose: the numbers are the
 * answer, and a client that discards them sends the operator back to the grid to subtract two
 * columns by hand.
 */
export async function postJournalEntry(body: {
  entry_date?: string;
  memo?: string;
  lines: JournalLineDraft[];
}): Promise<JournalEntry> {
  return accountingRequest<JournalEntry>("/api/v1/accounting/journal", {
    method: "POST",
    body: JSON.stringify(body),
  });
}
