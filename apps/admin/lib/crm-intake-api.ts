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

/**
 * One step of the documented flow, as the server computed it.
 *
 * The stepper does not decide these states in the browser: the server knows the row *and*
 * which modules this deployment has, and a client that worked it out itself would be the
 * second place the two can disagree.
 */
export type LeadStep = {
  key: "lead" | "opportunity" | "quotation" | "customer";
  state: "done" | "current" | "pending" | "blocked";
  note: string;
};

/** The lead detail: the row, the payload as submitted, the trail and the plan. */
export type LeadDetail = {
  lead: Lead;
  payload: Record<string, unknown>;
  payload_bytes: number;
  timeline: LeadEvent[];
  steps: LeadStep[];
};

/** What a conversion produced — and what it could not, which is a field not an error. */
export type LeadConversion = {
  lead: Lead;
  contact_id: string;
  contact_created: boolean;
  deal_id: string | null;
  deal_skipped: string | null;
};

/** Which of the documented flow's modules this deployment actually has. */
export type LeadFlow = {
  crm: boolean;
  sales: boolean;
  commerce: boolean;
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

/**
 * Hand a lead to a person, or put it back in the unassigned queue.
 *
 * `ownerUserId` is `null` for the queue and a user id for a person — the two are *sent*, never
 * omitted, because the API refuses a request that does not say. That refusal is deliberate
 * (an `Option<Option<Uuid>>` on the wire) and a client that dropped the key on a `null` would
 * turn "put it back" into a 400 rather than the act it asked for.
 *
 * `reason` is required by the server and the UI makes it required before the call, because a
 * hand-over with no explanation is the one thing the trail cannot be useful without.
 */
export function assignLead(id: string, ownerUserId: string | null, reason: string): Promise<Lead> {
  return request<Lead>(`/api/v1/crm/leads/${encodeURIComponent(id)}/assign`, {
    method: "POST",
    body: JSON.stringify({ owner_user_id: ownerUserId, reason }),
  });
}

/**
 * Turn a lead into a contact and an opportunity.
 *
 * Pressing it twice is safe by construction — the store reuses the contact the dedupe pass
 * already linked and never makes a second one — so this carries no idempotency key of its
 * own, for the same reason `markLeadResponded` does not.
 */
export function convertLead(id: string): Promise<LeadConversion> {
  return request<LeadConversion>(`/api/v1/crm/leads/${encodeURIComponent(id)}/convert`, {
    method: "POST",
    body: JSON.stringify({}),
  });
}

/** One person a lead can be handed to, with the load they already hold. */
export type LeadOwner = {
  id: string;
  label: string;
  email: string;
  open_leads: number;
  status: string;
};

/**
 * The hand-over roster.
 *
 * The hand-over screen used to take a uuid in a free-text box, which made the documented
 * "a person can decide whose work it is" something only somebody with the IAM screen open in
 * another tab could actually do. `open_leads` is on the same row for the same reason: handing
 * the tenth lead to somebody who already has nine is a decision somebody has to be able to
 * *see* they are making.
 */
export function fetchLeadOwners(): Promise<LeadOwner[]> {
  return request<LeadOwner[]>("/api/v1/crm/leads/owners");
}

/** What this deployment can do with the documented flow, so the stepper can say so. */
export function fetchLeadFlow(): Promise<LeadFlow> {
  return request<LeadFlow>("/api/v1/crm/leads/flow");
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

/** The autoresponder templates the server owns, with the placeholders the renderer knows. */
export type AutoresponderTemplates = {
  templates: { name: string; subject: string; body: string }[];
  placeholders: { token: string; renders: string }[];
  max_delay_minutes: number;
};

/** What a preview of the autoresponder answers: a verdict and, when there is one, the message. */
export type AutoresponderPreview = {
  verdict: string;
  reason: string;
  subject: string | null;
  body: string | null;
  delayed: boolean;
  due_at: string | null;
  unfilled: string[];
};

/**
 * The template list, from the server rather than from this file.
 *
 * `Autoresponder::from_json` reads the *template's* body out of the column, so a client that
 * offered its own list would let an operator pick a name the send path cannot render — the
 * message would go out as a bare subject line, and the operator would believe it was fine.
 */
export function fetchAutoresponderTemplates(): Promise<AutoresponderTemplates> {
  return request<AutoresponderTemplates>("/api/v1/crm/intake/autoresponder/templates");
}

/** Preview the autoresponder as the editor currently holds it. Writes nothing and claims nothing. */
export function previewAutoresponder(input: {
  autoresponder: Record<string, unknown>;
  first_name?: string;
  address?: string;
  product_interest?: string;
  source_name?: string;
}): Promise<AutoresponderPreview> {
  return request<AutoresponderPreview>("/api/v1/crm/intake/autoresponder/preview", {
    method: "POST",
    body: JSON.stringify(input),
  });
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

/** What one sweep of the retention window did, and the window it used. */
export type RetentionSweep = {
  archived: number;
  retention_days: number;
  dry_run: boolean;
};

/**
 * Archive the stored bodies of leads older than the window.
 *
 * The rows, their routing, their SLA facts and their timelines all stay — this erases the
 * submission body, not the history. Omitting the window uses the server's own default,
 * which is the one an organization that has never configured anything already lives with.
 *
 * `dryRun` asks what *would* be cleared and writes nothing. The action has no undo and no
 * second copy of what it erases, so the screen reads the number first and only then offers
 * the press — a control that erases on the first click is not a control, it is a trapdoor.
 */
export function sweepRetention(
  options: { retentionDays?: number; dryRun?: boolean } = {},
): Promise<RetentionSweep> {
  const body: Record<string, unknown> = {};
  if (options.retentionDays !== undefined) body.retention_days = options.retentionDays;
  if (options.dryRun) body.dry_run = true;
  return request<RetentionSweep>("/api/v1/crm/leads/retention/sweep", {
    method: "POST",
    body: JSON.stringify(body),
  });
}
