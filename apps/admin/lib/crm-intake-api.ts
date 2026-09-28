/**
 * The CRM intake client's own section of the API surface (docs/requests/REQ-117, slice 1).
 *
 * It is a separate file rather than another thousand lines in `api.ts` for one reason that
 * matters more than the line count: `api.ts` is imported by every screen, and this surface has
 * exactly three consumers. The two shapes are here rather than in `lib/types.ts` because they
 * are the *answer* of one module's routes — the derivation the panels do (a countdown, a status
 * tone) lives in `lib/crm-intake.ts` next to them.
 *
 * Every function is organization-scoped on the server, so none of them take an organization:
 * a lead of another organization is a `404`, and a client that could ask about another tenant
 * would be the bug.
 */
import { request } from "./api";

/** The attribution split the lead detail's two panels read. */
export type LeadAttribution = {
  utm_source: string | null;
  utm_medium: string | null;
  utm_campaign: string | null;
  utm_term: string | null;
  utm_content: string | null;
  click_id: string | null;
  referrer_host: string | null;
  landing_path: string | null;
  source_path: string | null;
};

/** The counters beside the inbox, counted by the same filter list as the rows. */
export type LeadMetrics = {
  open: number;
  breached: number;
  unassigned: number;
  duplicates: number;
  discarded: number;
  converted: number;
};

/** One row of the inbox. */
export type Lead = {
  id: string;
  source_id: string | null;
  status: string;
  is_open: boolean;
  contact_id: string | null;
  deal_id: string | null;
  quote_id: string | null;
  owner_user_id: string | null;
  first_name: string | null;
  last_name: string | null;
  email: string | null;
  phone: string | null;
  company_name: string | null;
  product_interest: string | null;
  message: string | null;
  consent_text: string | null;
  consent_given: boolean;
  attribution: LeadAttribution;
  decision: string | null;
  dedupe_key: string | null;
  duplicate_of: string | null;
  rejection_reason: string | null;
  spam_score: number;
  first_response_due_at: string | null;
  first_response_at: string | null;
  escalated_at: string | null;
  received_at: string;
  converted_at: string | null;
  sla_running: boolean;
};

/** One line of a lead's trail. */
export type LeadEvent = {
  id: number;
  kind: string;
  actor_user_id: string | null;
  detail: Record<string, unknown>;
  created_at: string;
};

/** The lead detail: the row, the payload as submitted and the trail. */
export type LeadDetail = {
  lead: Lead;
  payload: Record<string, unknown>;
  payload_bytes: number;
  timeline: LeadEvent[];
};

/** One page of the inbox, with the counters from the same read. */
export type LeadInbox = {
  leads: Lead[];
  next_before: string | null;
  metrics: LeadMetrics;
};

/** The filters the inbox accepts; every field is shareable in the URL. */
export type LeadFilters = {
  source?: string;
  status?: string[];
  owner?: string;
  q?: string;
  product?: string;
  since?: string;
  until?: string;
  before?: string;
  limit?: number;
};

/** One line of a source's mapping, as the editor sends and the server answers it. */
export type MappingLine = {
  target: string;
  source: string;
  transforms: string[];
  required: boolean;
  fallback: string | null;
};

/** An intake source, with its health and — once — its key. */
export type IntakeSource = {
  id: string;
  site_id: string | null;
  name: string;
  kind: string;
  form_key: string | null;
  endpoint_key_hint: string | null;
  mapping: MappingLine[];
  required_targets: string[];
  consent_required: boolean;
  consent_text: string | null;
  dedupe_policy: string;
  pipeline_id: string | null;
  stage_id: string | null;
  auto_tags: string[];
  autoresponder: Record<string, unknown>;
  active: boolean;
  rate_limit_per_hour: number;
  last_received_at: string | null;
  last_error: string | null;
  broken_mappings: string[];
  binding_broken: boolean;
  created_at: string;
  updated_at: string;
  /** Present on the one answer that creates or rotates the key, and on no other. */
  endpoint_key?: string;
};

/** The fields a source create carries. */
export type NewIntakeSource = {
  name: string;
  kind?: string;
  form_key?: string | null;
  site_id?: string | null;
  mapping?: MappingLine[];
  required_targets?: string[];
  consent_required?: boolean;
  consent_text?: string | null;
  dedupe_policy?: string;
  pipeline_id?: string | null;
  stage_id?: string | null;
  auto_tags?: string[];
  autoresponder?: Record<string, unknown>;
  rate_limit_per_hour?: number;
  active?: boolean;
};

/** A source update: every field absent means "leave it alone". */
export type IntakeSourcePatch = Partial<Omit<NewIntakeSource, "kind" | "form_key" | "site_id">>;

/** The fields a lead edit may carry — the ones a human sees on the row, and nothing else. */
export type LeadPatch = {
  first_name?: string | null;
  last_name?: string | null;
  email?: string | null;
  phone?: string | null;
  company_name?: string | null;
  job_title?: string | null;
  product_interest?: string | null;
  message?: string | null;
  status?: string;
  contact_id?: string | null;
};

/** What a `Test mapping` answers: the fields a payload *would* produce, and nothing stored. */
export type MappingPreview = {
  values: Record<string, string>;
  missing_required: string[];
  contactable: boolean;
  consent_given: boolean;
  dedupe_key: string | null;
};

/** Build the inbox's query. `status` repeats rather than comma-joining, like the API takes it. */
function leadQuery(filters: LeadFilters): string {
  const params = new URLSearchParams();
  if (filters.source) params.set("source", filters.source);
  for (const status of filters.status ?? []) params.append("status", status);
  if (filters.owner) params.set("owner", filters.owner);
  if (filters.q) params.set("q", filters.q);
  if (filters.product) params.set("product", filters.product);
  if (filters.since) params.set("since", filters.since);
  if (filters.until) params.set("until", filters.until);
  if (filters.before) params.set("before", filters.before);
  if (filters.limit) params.set("limit", String(filters.limit));
  const query = params.toString();
  return query ? `?${query}` : "";
}

/** Read one page of the inbox, with the counters from the same read. */
export function fetchLeads(filters: LeadFilters = {}): Promise<LeadInbox> {
  return request<LeadInbox>(`/api/v1/crm/leads${leadQuery(filters)}`);
}

/** Read one lead with its payload and its trail. */
export function fetchLead(id: string): Promise<LeadDetail> {
  return request<LeadDetail>(`/api/v1/crm/leads/${encodeURIComponent(id)}`);
}

/** The duplicate queue: rows kept separate because something already matched them. */
export function fetchLeadDuplicates(): Promise<Lead[]> {
  return request<Lead[]>("/api/v1/crm/leads/duplicates");
}

/** Edit the fields a human sees on the row. */
export function patchLead(id: string, patch: LeadPatch): Promise<Lead> {
  return request<Lead>(`/api/v1/crm/leads/${encodeURIComponent(id)}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
}

/**
 * Record the first response.
 *
 * The store keeps the *first* instant, so a double click is not a second measurement — which
 * is why this needs no idempotency key of its own.
 */
export function markLeadResponded(id: string): Promise<Lead> {
  return request<Lead>(`/api/v1/crm/leads/${encodeURIComponent(id)}/respond`, {
    method: "POST",
    body: JSON.stringify({}),
  });
}

/** Refuse a lead, keeping the row and the reason the API demands. */
export function rejectLead(id: string, reason: string): Promise<Lead> {
  return request<Lead>(`/api/v1/crm/leads/${encodeURIComponent(id)}/reject`, {
    method: "POST",
    body: JSON.stringify({ reason }),
  });
}

/** Mark a lead as spam. The reason is optional: the score is already recorded. */
export function markLeadSpam(id: string, reason?: string): Promise<Lead> {
  return request<Lead>(`/api/v1/crm/leads/${encodeURIComponent(id)}/spam`, {
    method: "POST",
    body: JSON.stringify({ reason: reason ?? null }),
  });
}

/** Delete a lead's data. The row goes; the fact that somebody deleted it does not. */
export function deleteLead(id: string): Promise<null> {
  return request<null>(`/api/v1/crm/leads/${encodeURIComponent(id)}`, { method: "DELETE" });
}

/** Every intake source of the organization, with its health. */
export function fetchIntakeSources(): Promise<IntakeSource[]> {
  return request<IntakeSource[]>("/api/v1/crm/intake/sources");
}

/** One source. No key: it exists on exactly one answer in a source's lifetime. */
export function fetchIntakeSource(id: string): Promise<IntakeSource> {
  return request<IntakeSource>(`/api/v1/crm/intake/sources/${encodeURIComponent(id)}`);
}

/** Create a source. A keyed endpoint's key is on this one answer, and no other. */
export function createIntakeSource(input: NewIntakeSource): Promise<IntakeSource> {
  return request<IntakeSource>("/api/v1/crm/intake/sources", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Update a source. A mapping that would drop a required target is refused, naming the field. */
export function updateIntakeSource(id: string, patch: IntakeSourcePatch): Promise<IntakeSource> {
  return request<IntakeSource>(`/api/v1/crm/intake/sources/${encodeURIComponent(id)}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
}

/** Delete a source. Its leads survive with a null `source_id`. */
export function deleteIntakeSource(id: string): Promise<null> {
  return request<null>(`/api/v1/crm/intake/sources/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

/** Issue a fresh endpoint key. The clear key is on this one answer; the old one dies here. */
export function rotateIntakeKey(id: string): Promise<IntakeSource> {
  return request<IntakeSource>(`/api/v1/crm/intake/sources/${encodeURIComponent(id)}/rotate-key`, {
    method: "POST",
    body: JSON.stringify({}),
  });
}

/** Run a payload through the mapping. **Writes nothing** — this is a preview, not a capture. */
export function testIntakeMapping(id: string, payload: Record<string, unknown>): Promise<MappingPreview> {
  return request<MappingPreview>(`/api/v1/crm/intake/sources/${encodeURIComponent(id)}/test`, {
    method: "POST",
    body: JSON.stringify({ payload }),
  });
}

/** The public endpoint's URL for a keyed source, built from the browser's own origin. */
export function intakeEndpointUrl(sourceKey: string): string {
  return `${window.location.origin}/api/v1/crm/intake/${sourceKey}`;
}
