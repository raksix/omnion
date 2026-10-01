/**
 * The AI data guard's client (docs/requests/REQ-105, slice 1).
 *
 * Kept in its own module rather than appended to `api.ts` for a reason that is not tidiness:
 * `api.ts` is 7800 lines and every guard call is used by exactly the four screens this slice
 * adds. Splitting them makes the "which client call does this screen make" question answerable
 * with one file listing, and it keeps the guard's response shapes — which are the *only* place
 * the guard's wire contract is written down on the admin side — legible next to the screens.
 *
 * **No payload is ever fetched, because there is none to fetch.** Every response below carries
 * short salted hashes and byte spans; the event rows carry no text. A future field that starts
 * carrying one would have to be added here deliberately, which is the point of a separate file:
 * the temptation to add "just the payload" is one grep away rather than zero.
 */

/** One label with the sentence the policy row and the About screen share. */
export type GuardLabel = {
  /** The wire name. */
  key: string;
  /** What the built-in pattern catches. */
  catches: string;
  /** What it deliberately does not catch — the half of a control people misread. */
  misses: string;
  /** The validator that runs on a match, or `null` when the label has none. */
  validator: string | null;
};

/** `GET /api/v1/ai/guard/policy`. */
export type GuardPolicy = {
  /** Default action per label, as wire names. */
  label_defaults: Record<string, string>;
  /** `numbered` or `deterministic`. */
  mask_style: string;
  /** Whether a user may weaken a label for their own calls. */
  allow_user_override: boolean;
  /** Every label this build knows. */
  labels: GuardLabel[];
  /** Per-label match counts over the window. */
  matches_by_label: Record<string, number>;
  /** Action totals over the window. */
  totals: Record<string, number>;
  /** Matches over the window, all labels. */
  matches: number;
  /** Enabled rules, and the ceiling. */
  enabled_rules: number;
  rule_budget: number;
  /** True when every label sits at `allow` — the warning banner's own predicate. */
  all_permissive: boolean;
  /** Exemptions in force right now. */
  active_exemptions: number;
  /** The window the stats cover, in days. */
  window_days: number;
  /** Whether a stored policy row exists. */
  has_policy_row: boolean;
  /**
   * Guard keys this viewer is missing — `["ai.guard.manage"]` for an auditor.
   *
   * Served by the API rather than guessed from a role name, so the panel's disabled controls name
   * the permission the API will refuse. `null` here would mean the policy could not be read at
   * all, which is a different screen state (an error), not a permission.
   */
  viewer_missing: string[];
};

/** `PUT /api/v1/ai/guard/policy` accepts this. */
export type GuardPolicyPatch = {
  /** The whole label → action map. Omit to leave the stored map alone. */
  label_defaults?: Record<string, string>;
  mask_style?: string;
  allow_user_override?: boolean;
};

/** One detector rule, as the rules table renders it. */
export type GuardRule = {
  id: string;
  key: string;
  /** The label this rule reports under; `custom:<name>` for a tenant's own. */
  label: string;
  /** `builtin` or `custom`. */
  kind: string;
  /** Whether it is a platform rule (`organization_id is null`). */
  platform: boolean;
  pattern: string;
  validator: string;
  action: string;
  severity: number;
  priority: number;
  /** Provider scope; empty means everywhere. */
  providers: string[];
  /** Feature scope; empty means everywhere. */
  features: string[];
  enabled: boolean;
  sample: string | null;
  updated_at: string;
};

/** `GET /api/v1/ai/guard/rules`. */
export type GuardRuleList = {
  rows: GuardRule[];
  enabled: number;
  budget: number;
  labels: string[];
  actions: string[];
  validators: string[];
  mask_styles: string[];
  label_notes: GuardLabel[];
  /** Guard keys this viewer is missing. See `GuardPolicy['viewer_missing']`. */
  viewer_missing: string[];
};

/** `POST /api/v1/ai/guard/rules` accepts this. */
export type GuardRuleBody = {
  key: string;
  label: string;
  custom_label?: string;
  pattern: string;
  validator?: string;
  action?: string;
  severity?: number;
  priority?: number;
  providers?: string[];
  features?: string[];
  enabled?: boolean;
  sample?: string;
};

/** One match, as the tester draws it. A hash — never the value. */
export type GuardMatch = {
  start: number;
  end: number;
  label: string;
  rule_key: string;
  value_hash: string;
};

/** `POST /api/v1/ai/guard/test`. */
export type GuardTestResult = {
  /** `clear`, `allowed`, `masked` or `blocked`. */
  verdict: string;
  action: string;
  matches: GuardMatch[];
  /** The text the provider WOULD see — the payload, or the masked payload. */
  masked_text: string;
  would_block: boolean;
  blocked_label: string | null;
  blocked_rule: string | null;
  label_counts: Record<string, number>;
  rules_evaluated: number;
  fixture_id: string | null;
};

/** One event row. Carries no payload text — there is no column for it. */
export type GuardEvent = {
  id: number;
  created_at: string;
  action: string;
  labels: string[];
  match_count: number;
  feature: string | null;
  user_id: string | null;
  site_id: string | null;
  blocked: boolean;
  request_id: string;
  run_id: string | null;
  /** A short hash of one matched value — proves a value was seen, never shows it. */
  value_hash: string | null;
  error_code: string | null;
};

/** `GET /api/v1/ai/guard/events`. */
export type GuardEventPage = {
  rows: GuardEvent[];
  total: number;
  offset: number;
  limit: number;
  /** The literal sentence the drawer prints. */
  no_payload_note: string;
};

/** `GET /api/v1/ai/guard/events/{id}` — the detail drawer. */
export type GuardEventDetail = GuardEvent & {
  rule_keys: string[];
  label_counts: Record<string, number>;
  value_hashes: string[];
  no_payload_note: string;
};

/** One exemption. */
export type GuardExemption = {
  id: string;
  label: string;
  providers: string[];
  features: string[];
  reason: string;
  created_at: string;
  expires_at: string | null;
  /** Whether it is in force right now. */
  live: boolean;
  /** True when it narrows a `block`, which the detector refuses to apply. */
  ineffective_because_block: boolean;
};

/** `GET /api/v1/ai/guard/exemptions`. */
export type GuardExemptionList = {
  rows: GuardExemption[];
  active: number;
  note: string;
};

/** One seeded sample the tester offers. */
export type GuardFixture = {
  id: string;
  name: string;
  payload: string;
  context: Record<string, unknown>;
  expected: Record<string, unknown>;
  created_at: string;
};

/** The filters the events screen sends. Every field is optional. */
export type GuardEventFilters = {
  action?: string;
  label?: string;
  feature?: string;
  user_id?: string;
  blocked?: boolean;
  from?: string;
  to?: string;
  limit?: number;
  offset?: number;
};

// ---------------------------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------------------------

/**
 * `ApiError` and the shared `request` are module-private in `api.ts`, so this module reaches for
 * them through one re-export rather than duplicating the CSRF/fetch handling. Duplicating it
 * would produce a second fetch path that nobody remembers to update — and the CSRF header is
 * exactly the thing that has to be right for a `PUT` to be accepted at all.
 */
import { ApiError, request } from "./api";

export { ApiError };

/** The panel's data. */
export function fetchGuardPolicy(): Promise<GuardPolicy> {
  return request<GuardPolicy>("/api/v1/ai/guard/policy");
}

/** Save the label defaults, the mask style or the override switch. */
export function saveGuardPolicy(patch: GuardPolicyPatch): Promise<GuardPolicy> {
  return request<GuardPolicy>("/api/v1/ai/guard/policy", {
    method: "PUT",
    body: JSON.stringify(patch),
  });
}

/** Every rule, with the vocabularies the create form offers. */
export function fetchGuardRules(): Promise<GuardRuleList> {
  return request<GuardRuleList>("/api/v1/ai/guard/rules");
}

/** Register a tenant rule. */
export function createGuardRule(body: GuardRuleBody): Promise<GuardRule> {
  return request<GuardRule>("/api/v1/ai/guard/rules", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/**
 * Change a rule. Every field is optional and `null` means "leave it alone", so the client sends
 * only what the form actually changed — sending the whole draft would clear a scope the operator
 * never touched.
 */
export function updateGuardRule(
  id: string,
  patch: Partial<GuardRuleBody>,
): Promise<GuardRule> {
  return request<GuardRule>(`/api/v1/ai/guard/rules/${encodeURIComponent(id)}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
}

/** Remove a tenant rule. */
export function deleteGuardRule(id: string): Promise<void> {
  return request<void>(`/api/v1/ai/guard/rules/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

/**
 * Dry-run a payload. **This does not reach a provider and has no code path that could.** The
 * payload never leaves the process, which is why the returned `value_hash` is salted with an
 * empty salt on the server: hashing the value an operator is looking at their own screen would
 * change nothing and obscure nothing either.
 */
export function runGuardTest(input: {
  payload: string;
  provider?: string;
  feature?: string;
  fixture_id?: string;
}): Promise<GuardTestResult> {
  return request<GuardTestResult>("/api/v1/ai/guard/test", {
    method: "POST",
    body: JSON.stringify({
      payload: input.payload,
      provider: input.provider ?? null,
      feature: input.feature ?? null,
      fixture_id: input.fixture_id ?? null,
    }),
  });
}

/** The seeded samples. */
export function fetchGuardFixtures(): Promise<GuardFixture[]> {
  return request<GuardFixture[]>("/api/v1/ai/guard/fixtures");
}

/** Store a sample so a known payload is one click away. */
export function createGuardFixture(input: {
  name: string;
  payload: string;
  context?: Record<string, unknown>;
  expected?: Record<string, unknown>;
}): Promise<GuardFixture> {
  return request<GuardFixture>("/api/v1/ai/guard/fixtures", {
    method: "POST",
    body: JSON.stringify({
      name: input.name,
      payload: input.payload,
      context: input.context ?? {},
      expected: input.expected ?? {},
    }),
  });
}

/** One page of the event log. */
export function fetchGuardEvents(
  filters: GuardEventFilters = {},
): Promise<GuardEventPage> {
  const params = new URLSearchParams();
  if (filters.action) params.set("action", filters.action);
  if (filters.label) params.set("label", filters.label);
  if (filters.feature) params.set("feature", filters.feature);
  if (filters.user_id) params.set("user_id", filters.user_id);
  if (filters.blocked) params.set("blocked", "true");
  if (filters.from) params.set("from", filters.from);
  if (filters.to) params.set("to", filters.to);
  if (filters.limit) params.set("limit", String(filters.limit));
  // `offset` is sent whenever it is a number, and zero is a real offset (the first page), so the
  // guard is `!== undefined` rather than truthiness.
  if (filters.offset !== undefined) params.set("offset", String(filters.offset));
  const query = params.toString();
  return request<GuardEventPage>(
    `/api/v1/ai/guard/events${query ? `?${query}` : ""}`,
  );
}

/** One event with the rule keys that fired. */
export function fetchGuardEvent(id: number): Promise<GuardEventDetail> {
  return request<GuardEventDetail>(`/api/v1/ai/guard/events/${id}`);
}

/** Every exemption, live and lapsed. */
export function fetchGuardExemptions(): Promise<GuardExemptionList> {
  return request<GuardExemptionList>("/api/v1/ai/guard/exemptions");
}

/** Narrow one label, in the operator's words. The reason is required by the server. */
export function createGuardExemption(input: {
  label: string;
  providers?: string[];
  features?: string[];
  reason: string;
  expires_at?: string;
}): Promise<GuardExemption> {
  return request<GuardExemption>("/api/v1/ai/guard/exemptions", {
    method: "POST",
    body: JSON.stringify({
      label: input.label,
      providers: input.providers ?? [],
      features: input.features ?? [],
      reason: input.reason,
      expires_at: input.expires_at ?? null,
    }),
  });
}

/** Withdraw an exemption. */
export function deleteGuardExemption(id: string): Promise<void> {
  return request<void>(`/api/v1/ai/guard/exemptions/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

/**
 * `GET /api/v1/ai/guard/about` — the residual-risk statement.
 *
 * Every label with what it misses, plus the measured counts. The labels are the SAME constant the
 * detector is built from (served by the API, not restated here), so this screen cannot drift away
 * from the rules it describes — a hand-typed list of "what we don't catch" is the kind of page
 * that is wrong within one release.
 */
export type GuardAbout = {
  /** Every label, with `catches` and `misses`. */
  labels: GuardLabel[];
  /** Labels whose rule is switched off, so they catch nothing at all right now. */
  labels_disabled: number;
  /** Enabled rules in force for this tenant. */
  enabled_rules: number;
  /** The ceiling; above it the guard refuses to start. */
  rule_budget: number;
  /** True when every label sits at `allow`. */
  all_permissive: boolean;
};

/**
 * Read the residual-risk statement.
 *
 * Gated on `ai.guard.read`, not `manage`, on purpose: an operator who may look but not configure
 * is exactly the person who needs to know what the guard misses, so the disclosure must not sit
 * behind the permission to change the control.
 */
export function fetchGuardAbout(): Promise<GuardAbout> {
  return request<GuardAbout>("/api/v1/ai/guard/about");
}
