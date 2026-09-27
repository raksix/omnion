/**
 * The CRM client: the relationship layer the `/crm` screens read and write (REQ-051).
 *
 * Kept in its own file rather than appended to `api.ts` because it is the first **module** client
 * in the panel: the shape mirrors the module's own vocabulary (`crm.contacts.read`, the visibility
 * level, the shared list envelope) instead of the platform's, so a business feature can grow
 * without making the core's client a catalogue of every feature.
 *
 * The transport is the one the core already has: `api.ts` keeps its `request` private on purpose —
 * a second copy of the fetch/JSON/error handling would be a second thing that can drift from the
 * first — so what is shared here is a tiny wrapper over the platform's own error type, not a
 * reimplementation of the transport.
 */
import { ApiError, type ErrorBody } from "./api";

/** The JSON body an API error carries, as the core documents it. */
type ApiErrorBody = ErrorBody;

/** A `Response` that is not `ok`, turned into the platform's own error. */
async function readFailure(response: Response): Promise<ApiError> {
  const text = await response.text();
  let code = "unknown_error";
  let message = `The API answered with status ${response.status}.`;
  try {
    const body = JSON.parse(text) as ApiErrorBody;
    code = body.error?.code ?? code;
    message = body.error?.message ?? message;
  } catch {
    // A non-JSON body is still an error; the status stays in the message.
  }
  return new ApiError(response.status, code, message);
}

/** One JSON call, with the same session, accept header and error shape the panel uses. */
async function crmRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
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

/** A contact, as the list row and the detail header both read it. */
export type CrmContact = {
  id: string;
  organization_id: string;
  first_name: string;
  last_name: string;
  /** The name the list shows, joined by the API. */
  display_name: string;
  /** Avatar initials, joined by the API. */
  initials: string;
  email: string | null;
  phone: string | null;
  job_title: string | null;
  company_id: string | null;
  company_name: string | null;
  owner_user_id: string | null;
  owner_name: string | null;
  status: string;
  tags: string[];
  /** Custom values, with the flagged keys already removed for a role that may not read them. */
  custom: Record<string, unknown>;
  notes: string;
  last_activity_at: string | null;
  archived_at: string | null;
  created_at: string;
  updated_at: string;
};

/** A company, as the list row and the detail header read it. */
export type CrmCompany = {
  id: string;
  organization_id: string;
  name: string;
  initials: string;
  domain: string | null;
  industry: string | null;
  owner_user_id: string | null;
  owner_name: string | null;
  status: string;
  tags: string[];
  custom: Record<string, unknown>;
  notes: string;
  archived_at: string | null;
  created_at: string;
  updated_at: string;
};

/** A company plus the rollups its detail screen shows. */
export type CrmCompanyDetail = {
  company: CrmCompany;
  contact_count: number;
  open_deal_count: number;
  /** Text, so a money value never loses precision on the way through JSON. */
  pipeline_value: string;
  last_activity_at: string | null;
};

/** The envelope every CRM list answers with. */
export type CrmPage<T> = {
  items: T[];
  next_cursor: string | null;
  total_estimate: number;
};

/** The filters a CRM list sends. Every field is optional and they all combine. */
export type CrmListQuery = {
  search?: string;
  /** `me`, `unassigned` or an account id. */
  owner?: string;
  status?: string;
  tag?: string;
  company_id?: string;
  contact_id?: string;
  sort?: string;
  direction?: "asc" | "desc";
  limit?: number;
  cursor?: string;
  include_archived?: boolean;
  /** Contacts only: "no activity for N days". */
  inactive_days?: number;
  created_from?: string;
  created_to?: string;
};

/** A saved view: the filter, the column set and the sort a person keeps. */
export type CrmView = {
  id: string;
  organization_id: string;
  owner_user_id: string;
  entity: string;
  name: string;
  filters: Record<string, unknown>;
  columns: string[];
  sort: { key?: string; direction?: string };
  is_shared: boolean;
  created_at: string;
  updated_at: string;
};

/** What a view may store when it is saved. */
export type CrmViewInput = {
  entity: string;
  name: string;
  filters?: Record<string, unknown>;
  columns?: string[];
  sort?: { key?: string; direction?: string };
  is_shared?: boolean;
};

/** One refused row of an import, with the line a person has to open. */
export type CrmImportError = {
  line: number;
  field: string | null;
  message: string;
};

/** The answer of a dry run. */
export type CrmImportPreview = {
  mode: "dry_run";
  mapping: {
    /** Which file column fed which field. */
    columns: { field: string; column: number }[];
    ignored: string[];
    duplicates: string[];
  };
  total_rows: number;
  valid_rows: number;
  errors: CrmImportError[];
  /** The first rows as they were read, so the screen can show what it parsed. */
  sample: Record<string, unknown>[];
  summary: string;
};

/** The answer of a commit. */
export type CrmImportCommit = {
  mode: "commit";
  created: number;
  refused: number;
  errors: CrmImportError[];
  contacts: CrmContact[];
};

/** The column catalogue the chooser reads (`GET /api/v1/crm/views/columns`). */
export type CrmColumnCatalogue = {
  entity: string;
  columns: string[];
  statuses: string[];
};

// ---------------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------------

/** Build a query string: only the parameters that were actually set are written. */
function listParams(query: CrmListQuery = {}): string {
  const params = new URLSearchParams();
  if (query.search) params.set("search", query.search);
  if (query.owner) params.set("owner", query.owner);
  if (query.status) params.set("status", query.status);
  if (query.tag) params.set("tag", query.tag);
  if (query.company_id) params.set("company_id", query.company_id);
  if (query.contact_id) params.set("contact_id", query.contact_id);
  if (query.sort) params.set("sort", query.sort);
  if (query.direction) params.set("direction", query.direction);
  if (query.limit) params.set("limit", String(query.limit));
  if (query.cursor) params.set("cursor", query.cursor);
  if (query.include_archived) params.set("include_archived", "true");
  if (query.inactive_days) params.set("inactive_days", String(query.inactive_days));
  if (query.created_from) params.set("created_from", query.created_from);
  if (query.created_to) params.set("created_to", query.created_to);
  return params.toString();
}

/** The list's own URL — the filters travel as the query the list already sends. */
function listUrl(entity: "contacts" | "companies", query: CrmListQuery = {}): string {
  const search = listParams(query);
  return `/api/v1/crm/${entity}${search ? `?${search}` : ""}`;
}

/**
 * The export's URL: the **same** filters as the list, on the export route.
 *
 * Sharing the builder is the point — a download that could carry a different filter set than the
 * screen would be a way to get a file the screen never showed.
 */
function exportUrl(entity: "contacts" | "companies", query: CrmListQuery = {}): string {
  const search = listParams(query);
  return `/api/v1/crm/${entity}/export${search ? `?${search}` : ""}`;
}

/** One page of contacts. */
export function fetchCrmContacts(query: CrmListQuery = {}): Promise<CrmPage<CrmContact>> {
  return crmRequest<CrmPage<CrmContact>>(listUrl("contacts", query));
}

/** One contact. */
export function fetchCrmContact(id: string): Promise<CrmContact> {
  return crmRequest<CrmContact>(`/api/v1/crm/contacts/${encodeURIComponent(id)}`);
}

/** One page of companies. */
export function fetchCrmCompanies(query: CrmListQuery = {}): Promise<CrmPage<CrmCompany>> {
  return crmRequest<CrmPage<CrmCompany>>(listUrl("companies", query));
}

/** One company with its rollups. */
export function fetchCrmCompany(id: string): Promise<CrmCompanyDetail> {
  return crmRequest<CrmCompanyDetail>(`/api/v1/crm/companies/${encodeURIComponent(id)}`);
}

// ---------------------------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------------------------

/** What a create form collects. Absent fields are left alone by the API. */
export type CrmContactInput = {
  first_name: string;
  last_name?: string;
  email?: string | null;
  phone?: string | null;
  job_title?: string | null;
  company_id?: string | null;
  owner_user_id?: string | null;
  status?: string;
  tags?: string[];
  custom?: Record<string, unknown>;
  notes?: string;
};

/** A partial update — what is present is replaced, what is absent is kept. */
export type CrmContactPatch = Partial<CrmContactInput>;

/** Create a contact. */
export function createCrmContact(input: CrmContactInput): Promise<CrmContact> {
  return crmRequest<CrmContact>("/api/v1/crm/contacts", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Edit a contact. The list's inline edit is this call with one field. */
export function updateCrmContact(id: string, changes: CrmContactPatch): Promise<CrmContact> {
  return crmRequest<CrmContact>(`/api/v1/crm/contacts/${encodeURIComponent(id)}`, {
    method: "PATCH",
    body: JSON.stringify(changes),
  });
}

/** Archive a contact — the API answers with the archived row, not a `204`. */
export function archiveCrmContact(id: string): Promise<CrmContact> {
  return crmRequest<CrmContact>(`/api/v1/crm/contacts/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

/** Merge two contacts: `survivor` keeps its identity, `loser` is folded into it and archived. */
export function mergeCrmContacts(survivor: string, loser: string): Promise<CrmContact> {
  return crmRequest<CrmContact>("/api/v1/crm/contacts/merge", {
    method: "POST",
    body: JSON.stringify({ survivor, loser }),
  });
}

/** What a company form collects. */
export type CrmCompanyInput = {
  name: string;
  domain?: string | null;
  industry?: string | null;
  owner_user_id?: string | null;
  status?: string;
  tags?: string[];
  custom?: Record<string, unknown>;
  notes?: string;
};

/** Create a company. */
export function createCrmCompany(input: CrmCompanyInput): Promise<CrmCompany> {
  return crmRequest<CrmCompany>("/api/v1/crm/companies", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Edit a company. */
export function updateCrmCompany(
  id: string,
  changes: Partial<CrmCompanyInput>,
): Promise<CrmCompany> {
  return crmRequest<CrmCompany>(`/api/v1/crm/companies/${encodeURIComponent(id)}`, {
    method: "PATCH",
    body: JSON.stringify(changes),
  });
}

/** Archive a company. */
export function archiveCrmCompany(id: string): Promise<CrmCompany> {
  return crmRequest<CrmCompany>(`/api/v1/crm/companies/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

// ---------------------------------------------------------------------------------------------
// Saved views
// ---------------------------------------------------------------------------------------------

/** The caller's views and the organization's shared ones. */
export async function fetchCrmViews(entity?: string): Promise<CrmView[]> {
  const query = entity ? `?entity=${encodeURIComponent(entity)}` : "";
  const body = await crmRequest<{ views: CrmView[] }>(`/api/v1/crm/views${query}`);
  return body.views;
}

/** Save a view. */
export function createCrmView(input: CrmViewInput): Promise<CrmView> {
  return crmRequest<CrmView>("/api/v1/crm/views", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Forget a view. A view is a lens — the records it pointed at are untouched. */
export function deleteCrmView(id: string): Promise<CrmView> {
  return crmRequest<CrmView>(`/api/v1/crm/views/${encodeURIComponent(id)}`, { method: "DELETE" });
}

/** The columns the chooser may offer, and the statuses the status filter may hold. */
export async function fetchCrmColumnCatalogue(entity: string): Promise<CrmColumnCatalogue> {
  const body = await crmRequest<{ entity: string; columns: string[]; statuses: string[] }>(
    `/api/v1/crm/views/columns?entity=${encodeURIComponent(entity)}`,
  );
  return body;
}

// ---------------------------------------------------------------------------------------------
// Import and export
// ---------------------------------------------------------------------------------------------

/** Read a file without writing anything. */
export function dryRunCrmImport(
  csv: string,
  entity = "contacts",
): Promise<CrmImportPreview> {
  return crmRequest<CrmImportPreview>("/api/v1/crm/contacts/import", {
    method: "POST",
    body: JSON.stringify({ csv, mode: "dry_run", entity }),
  });
}

/** Write the rows a dry run accepted. The answer names what it wrote and what it refused. */
export function commitCrmImport(csv: string, entity = "contacts"): Promise<CrmImportCommit> {
  return crmRequest<CrmImportCommit>("/api/v1/crm/contacts/import", {
    method: "POST",
    body: JSON.stringify({ csv, mode: "commit", entity }),
  });
}

/**
 * Download a list as a CSV.
 *
 * The file comes from the API with the same filters the screen shows, so its rows and the
 * table's rows are the same answer; `rows` is the count the server reported in its own header.
 */
export async function downloadCrmExport(
  entity: "contacts" | "companies",
  query: CrmListQuery = {},
): Promise<{ rows: number; truncated: boolean; blob: Blob; filename: string }> {
  const response = await fetch(exportUrl(entity, query), {
    credentials: "same-origin",
    headers: { accept: "text/csv" },
  });
  if (!response.ok) {
    const text = await response.text();
    let code = "export_failed";
    let message = `The export answered with status ${response.status}.`;
    try {
      const body = JSON.parse(text) as { error?: { code?: string; message?: string } };
      code = body.error?.code ?? code;
      message = body.error?.message ?? message;
    } catch {
      // A non-JSON body is still an error; the status stays in the message.
    }
    throw new ApiError(response.status, code, message);
  }
  const rows = Number(response.headers.get("x-export-rows") ?? "0");
  const truncated = response.headers.get("x-export-truncated") === "true";
  const disposition = response.headers.get("content-disposition") ?? "";
  const match = /filename="?([^";]+)"?/.exec(disposition);
  return {
    rows,
    truncated,
    blob: await response.blob(),
    filename: match?.[1] ?? `omnion-${entity}.csv`,
  };
}

/** Save a download the browser asked for, and report the file name. */
export function saveDownload(blob: Blob, filename: string): void {
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  URL.revokeObjectURL(url);
}

// ---------------------------------------------------------------------------------------------
// Deals, the board and the pipeline editor (slice 3)
// ---------------------------------------------------------------------------------------------

/** One column of the board: the stage plus its count, sum and weighted sum. */
export type CrmStageTotals = {
  stage_id: string;
  name: string;
  /** `open`, `won` or `lost`. */
  kind: "open" | "won" | "lost";
  position: number;
  /** The stage's default probability for a deal entering it. */
  probability: number;
  deal_count: number;
  /** Text, so a money value never loses precision on the way through JSON. */
  total: string;
  /** `amount × probability / 100`, in the same shape. */
  weighted_total: string;
};

/** One stage of a pipeline, as the editor and the board both read it. */
export type CrmPipelineStage = {
  id: string;
  organization_id: string;
  pipeline_id: string;
  name: string;
  kind: "open" | "won" | "lost";
  position: number;
  probability: number;
};

/** A pipeline with its stages in board order. */
export type CrmPipeline = {
  id: string;
  organization_id: string;
  name: string;
  is_default: boolean;
  stages: CrmPipelineStage[];
  created_at: string;
};

/** A deal, as a board card, a list row and a detail header. */
export type CrmDeal = {
  id: string;
  organization_id: string;
  pipeline_id: string;
  stage_id: string;
  stage_name: string;
  stage_kind: "open" | "won" | "lost";
  title: string;
  company_id: string | null;
  company_name: string | null;
  contact_id: string | null;
  contact_name: string | null;
  owner_user_id: string | null;
  owner_name: string | null;
  amount: string;
  currency: string;
  probability: number | null;
  expected_close_on: string | null;
  source: string | null;
  lost_reason: string | null;
  stage_changed_at: string;
  /** Whole days in the current stage, joined by the API. */
  days_in_stage: number;
  /** `true` past 30 days in one stage. */
  stale: boolean;
  archived_at: string | null;
  created_at: string;
  updated_at: string;
};

/** The board in one request. */
export type CrmBoard = {
  pipeline: CrmPipeline;
  /** Every stage, including the empty ones. */
  columns: CrmStageTotals[];
  deals: CrmDeal[];
  /** The sum of the open columns. */
  open_total: string;
  /** The sum of the open columns, weighted by probability. */
  weighted_forecast: string;
};

/** One row of the pipeline editor, as the form holds it while the set is being edited. */
export type CrmStageInput = {
  name: string;
  kind: "open" | "won" | "lost";
  probability: number;
};

/** The currencies the deal form offers — the API refuses anything else. */
export const CRM_CURRENCIES = ["USD", "EUR", "GBP", "TRY", "CHF", "CAD", "AUD", "JPY"] as const;

/**
 * The board, or a page of deals.
 *
 * The default is the board because `/crm/deals` is the screen a person opens to see where the
 * pipeline stands; the list is `?view=list` and is the one a saved view is stored against.
 */
export function fetchCrmDealsBoard(pipelineId?: string): Promise<{ view: string; board: CrmBoard }> {
  const query = new URLSearchParams({ view: "board" });
  if (pipelineId) query.set("pipeline_id", pipelineId);
  return crmRequest(`/api/v1/crm/deals?${query.toString()}`);
}

/** A page of deals, with the same list contract the contacts list sends. */
export function fetchCrmDeals(query: CrmListQuery = {}): Promise<{ view: string; page: CrmPage<CrmDeal> }> {
  const search = new URLSearchParams();
  search.set("view", "list");
  for (const [key, value] of Object.entries(query)) {
    if (value !== undefined && value !== null && value !== "") search.set(key, String(value));
  }
  return crmRequest(`/api/v1/crm/deals?${search.toString()}`);
}

/** Every pipeline with its stages — the editor's and the board selector's one call. */
export function fetchCrmPipelines(): Promise<CrmPipeline[]> {
  return crmRequest("/api/v1/crm/pipelines");
}

/** What the create form sends. `amount` is text so the form sends what it displays. */
export type CrmDealInput = {
  title: string;
  pipeline_id?: string;
  stage_id?: string;
  company_id?: string;
  contact_id?: string;
  amount?: string;
  currency?: string;
  probability?: number;
  expected_close_on?: string;
  source?: string;
  lost_reason?: string;
};

/** A partial update; the stage moves through {@link moveCrmDealStage}, not here. */
export type CrmDealPatch = Partial<Omit<CrmDealInput, "pipeline_id" | "stage_id">>;

export function createCrmDeal(input: CrmDealInput): Promise<CrmDeal> {
  return crmRequest("/api/v1/crm/deals", { method: "POST", body: JSON.stringify(input) });
}

export function updateCrmDeal(id: string, changes: CrmDealPatch): Promise<CrmDeal> {
  return crmRequest(`/api/v1/crm/deals/${id}`, {
    method: "PATCH",
    body: JSON.stringify(changes),
  });
}

export function archiveCrmDeal(id: string): Promise<CrmDeal> {
  return crmRequest(`/api/v1/crm/deals/${id}`, { method: "DELETE" });
}

/**
 * Move a deal to another stage — the drag and the keyboard's `ctrl + ←/→` are the same call.
 *
 * `lostReason` is required by the API when the target stage is a lost one, and `closeOn` is
 * the confirmation for a won one; both are the dialogs, not a default this function could
 * invent, which is why they are explicit rather than defaulted here.
 */
export function moveCrmDealStage(
  id: string,
  stageId: string,
  extra: { lostReason?: string; closeOn?: string } = {},
): Promise<CrmDeal> {
  return crmRequest(`/api/v1/crm/deals/${id}/stage`, {
    method: "POST",
    body: JSON.stringify({ stage_id: stageId, ...extra }),
  });
}

/**
 * Save the whole ordered stage set.
 *
 * A `PUT` and not a `PATCH`: a drag reorder is the *set*, and a partial update of a set has no
 * meaning — the position of a column is its index, so moving one is moving all of them.
 */
export function saveCrmPipelineStages(pipelineId: string, stages: CrmStageInput[]): Promise<CrmPipeline> {
  return crmRequest(`/api/v1/crm/pipelines/${pipelineId}/stages`, {
    method: "PUT",
    body: JSON.stringify({ stages }),
  });
}
