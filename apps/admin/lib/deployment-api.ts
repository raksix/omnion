/**
 * The typed client for the deployment centre's release surface (REQ-128, slice 4's screens).
 *
 * It is its own module rather than another 300 lines appended to `api.ts` for two reasons that are
 * both about legibility rather than size: `api.ts` is one flat list of every surface the panel
 * speaks, and a reviewer looking for "what does the upgrade screen know about a rollback" should
 * not have to page past nine hundred unrelated types; and this module's payloads are the ones with
 * a **contract about honesty** — the cached-manifest banner, the `missing_kinds` list, the
 * `unavailable` reason, the `renderable: false` that says *why* — and those read differently when
 * they are next to each other than when they are 4,000 lines apart.
 *
 * ## The shapes are the API's, verbatim
 *
 * `ReleaseArtifact`, `BundleFile`, `Destructiveness`, `Step`, `Checklist` and `Rollback` mirror the
 * serde output of `crates/deployment`. A screen that re-declared a field as optional because "the
 * API might not send it" would be a screen that renders `undefined` as an empty cell, which is the
 * blank the request explicitly forbids.
 *
 * ## Nothing here can carry a credential
 *
 * `BundleRequest` has no field a password can arrive in — the domain, registry, tag, TLS mode and
 * size preset are the whole vocabulary — so the type is the guarantee rather than a scan. The
 * generated files reference secrets **by name**, and the screen says so.
 */

import { request, ApiError } from "./api";

export { ApiError };

// -------------------------------------------------------------------------------------------
// Artifacts
// -------------------------------------------------------------------------------------------

/** One published artifact of one release. */
export type ReleaseArtifact = {
  id: number;
  version: string;
  /** `image`, `cli`, `chart`, `sbom` or `compose`. */
  kind: string;
  /** The image reference, the binary name or the chart name. */
  name: string;
  /** The image digest, or the file checksum. `null` when the publisher sent none. */
  digest: string | null;
  platforms: string[];
  size_bytes: number | null;
  download_url: string | null;
  published_at: string | null;
  manifest_version: string;
};

/** A cached release, with the coverage the panel computed from the rows that exist. */
export type CachedRelease = {
  version: string;
  channel: string;
  source_commit: string | null;
  core_min: string | null;
  fetched_at: string;
  migration_count: number;
  /** The publisher's own claim, cached verbatim and never treated as a verification. */
  migrations_destructive: boolean;
  published_kinds: string[];
  /** The kinds this version did NOT publish — an explicit list, not blank rows. */
  missing_kinds: string[];
};

/** `GET /api/v1/deployment/artifacts`. */
export type ArtifactsResponse = {
  artifacts: ReleaseArtifact[];
  releases: CachedRelease[];
  /** The complete vocabulary, so a client computes nothing and guesses nothing. */
  artifact_kinds: string[];
  total: number;
};

/** `GET /api/v1/deployment/artifacts` — every cached artifact, newest release first. */
export function fetchArtifacts(version?: string): Promise<ArtifactsResponse> {
  const query = version ? `?version=${encodeURIComponent(version)}` : "";
  return request<ArtifactsResponse>(`/api/v1/deployment/artifacts${query}`);
}

/** One release in full. */
export type ReleaseDetail = {
  version: string;
  channel: string;
  source_commit: string | null;
  core_min: string | null;
  migrations: string[];
  migrations_destructive: boolean;
  notes_md: string;
  upgrade_notes_url: string | null;
  fetched_at: string;
  raw: unknown;
  /** Answered against THIS build, so the screen can say what the comparison means. */
  core_minimum_satisfied: boolean;
};

/** `GET /api/v1/deployment/artifacts/{version}`. */
export function fetchRelease(version: string): Promise<{
  release: ReleaseDetail;
  artifacts: ReleaseArtifact[];
  published_kinds: string[];
  missing_kinds: string[];
}> {
  return request(`/api/v1/deployment/artifacts/${encodeURIComponent(version)}`);
}

// -------------------------------------------------------------------------------------------
// Bundles
// -------------------------------------------------------------------------------------------

/** One file of a generated bundle. */
export type BundleFile = {
  name: string;
  size: number;
  sha256: string;
};

/** The bundle generator's vocabulary. Anything outside it is a 400 naming the field. */
export const BUNDLE_KINDS = ["compose-small", "compose-enterprise", "helm"] as const;
export type BundleKind = (typeof BUNDLE_KINDS)[number];

/** The TLS modes the chart and both stacks understand. */
export const TLS_MODES = ["existing-secret", "cert-manager", "none"] as const;

/** The size presets, with the numbers the request says must be shown rather than named. */
export const SIZE_PRESETS = [
  { key: "small", label: "Small", detail: "2 vCPU · 4 GiB · 1 replica" },
  { key: "medium", label: "Medium", detail: "4 vCPU · 8 GiB · 2 replicas" },
  { key: "large", label: "Large", detail: "8 vCPU · 16 GiB · 3 replicas" },
] as const;

/** The generator's request. **No field here can carry a secret value.** */
export type BundleRequest = {
  name: string;
  kind: BundleKind;
  version: string;
  domain: string;
  tls_mode: string;
  registry: string;
  tag: string;
  preset: string;
  observability: boolean;
};

/** A stored bundle, as the list and the detail both return it. */
export type EnvironmentBundle = {
  id: string;
  name: string;
  kind: string;
  version: string;
  /** The record the platform built, not the request the caller sent. */
  config: Record<string, unknown>;
  checksum: string;
  files: BundleFile[];
  commands: string[];
  generated_by?: string | null;
  generated_at?: string;
  download_count?: number;
  last_downloaded_at?: string | null;
};

/** `GET /api/v1/deployment/bundles`. */
export function fetchBundles(): Promise<{ bundles: EnvironmentBundle[]; total: number }> {
  return request("/api/v1/deployment/bundles");
}

/** `POST /api/v1/deployment/bundles` — answers `201` with the generated file list. */
export function createBundle(input: BundleRequest): Promise<
  EnvironmentBundle & { note: string }
> {
  return request("/api/v1/deployment/bundles", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** `GET /api/v1/deployment/bundles/{id}`. */
export function fetchBundle(id: string): Promise<EnvironmentBundle> {
  return request(`/api/v1/deployment/bundles/${encodeURIComponent(id)}`);
}

/** The render answer: what a real tool would read, and why this host cannot run it. */
export type BundleRender = {
  bundle_id: string;
  kind: string;
  /** The command whose output the operator wants: `helm template` or `docker compose config`. */
  tool: string;
  renderable: boolean;
  reason: string;
  files: BundleFile[];
  commands: string[];
  config: Record<string, unknown>;
};

/** `POST /api/v1/deployment/bundles/{id}/render`. */
export function renderBundle(id: string): Promise<BundleRender> {
  return request(`/api/v1/deployment/bundles/${encodeURIComponent(id)}/render`, {
    method: "POST",
  });
}

/**
 * `GET /api/v1/deployment/bundles/{id}/files/{name}` — the file's bytes.
 *
 * A `fetch` rather than a link, because the checksum travels in a response **header** and the
 * screen shows it beside the download: a plain anchor would download the file and discard the one
 * value that makes the download verifiable. The name is encoded per segment — a bundle's own file
 * list is the only set of names the endpoint serves, and a name with a slash in it must not become
 * a path.
 */
export async function downloadBundleFile(
  id: string,
  name: string,
): Promise<{ blob: Blob; checksum: string }> {
  const response = await fetch(
    `/api/v1/deployment/bundles/${encodeURIComponent(id)}/files/${encodeURIComponent(name)}`,
    { credentials: "same-origin", headers: { accept: "*/*" } },
  );
  if (!response.ok) {
    let message = `The file could not be downloaded (status ${response.status}).`;
    try {
      const body = (await response.json()) as { error?: { message?: string } };
      if (body.error?.message) message = body.error.message;
    } catch {
      /* a body that is not JSON is the status line above */
    }
    throw new ApiError(response.status, "bundle_file_unavailable", message);
  }
  return {
    blob: await response.blob(),
    checksum: response.headers.get("x-checksum-sha256") ?? "",
  };
}

// -------------------------------------------------------------------------------------------
// The upgrade plan
// -------------------------------------------------------------------------------------------

/** One ordered step. `command` is `null` for a step whose check is a path to open instead. */
export type PlanStep = {
  kind: string;
  text: string;
  command: string | null;
  destructive: boolean;
  point_of_no_return: boolean;
  migrations?: string[];
  image?: string;
  notes_url?: string;
  check?: { path: string; expect_status: number; how: string } | null;
};

/** What is known about whether the database can go back. */
export type Destructiveness = {
  /** `reversible`, `destructive` or **`unknown`**. */
  verdict: string;
  /** The sentence the screen shows, carrying the answer to "why". */
  reason: string;
  destructive_migrations: string[];
  /** `down-script`, `restore-from-backup` or `unknown`. */
  database_rollback: string;
  /** Which input decided it: `migration-marker`, `manifest` or `policy-absent`. */
  source: string;
};

export type ChecklistItem = {
  index: number;
  kind: string;
  text: string;
  destructive: boolean;
};

export type Rollback = {
  application: { available: boolean; command: string | null };
  database: {
    available: boolean;
    method: string;
    verdict: string;
    command?: string | null;
    reason?: string | null;
  };
};

export type UpgradePlan = {
  from_version: string;
  to_version: string;
  topology: string;
  bundle_kind: string | null;
  image: string | null;
  migrations_applied: string[];
  destructive: Destructiveness;
  /** Index of the point-of-no-return step. */
  point_of_no_return: number | null;
  steps: PlanStep[];
  rollback: Rollback;
  checklist: {
    items: ChecklistItem[];
    requires_acknowledgement: boolean;
    acknowledged: boolean;
    complete: boolean;
  };
  acknowledged_by?: string | null;
};

export type StoredPlan = {
  id: string;
  from_version: string;
  to_version: string;
  topology: string;
  destructive_verdict: string;
  destructive_acknowledged_by: string | null;
  destructive_acknowledged_at: string | null;
  created_by: string | null;
  created_at: string;
};

/** `GET /api/v1/deployment/upgrade-plan`. */
export function fetchUpgradePlan(params: {
  to?: string;
  channel?: string;
  topology?: string;
  bundle_kind?: string;
}): Promise<{
  summary: {
    current_version: string;
    target_version: string | null;
    plan: UpgradePlan | null;
    stored: StoredPlan | null;
    problems: string[];
    requires_acknowledgement: boolean;
    unavailable: string | null;
  };
  problems: string[];
  running_version: string;
  channel: string;
  topology: string;
}> {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value) query.set(key, value);
  }
  const suffix = query.toString();
  return request(`/api/v1/deployment/upgrade-plan${suffix ? `?${suffix}` : ""}`);
}

/**
 * `POST /api/v1/deployment/upgrade-plan/acknowledge`.
 *
 * The verdict is required, and the API parses `<plan id>@<verdict>` out of it — so the screen
 * always sends the plan's OWN verdict rather than a literal the operator picked from a list. An
 * acknowledgement that does not say what was accepted is a consent to an unknown.
 */
export function acknowledgeUpgradePlan(planId: string, verdict: string): Promise<{
  id: string;
  acknowledged_by: string | null;
  acknowledged_at: string | null;
  verdict: string;
  from_version: string;
  to_version: string;
  topology: string;
}> {
  return request("/api/v1/deployment/upgrade-plan/acknowledge", {
    method: "POST",
    body: JSON.stringify({ verdict: `${planId}@${verdict}` }),
  });
}

// -------------------------------------------------------------------------------------------
// The migration ledger (REQ-129, slice 1)
// -------------------------------------------------------------------------------------------

/** One migration the ledger has a row for. */
export type AppliedMigration = {
  state: "applied";
  version: string;
  name: string;
  checksum: string;
  applied_at: string;
  duration_ms: number;
  statement_count: number;
  actor: string;
  source: string;
  has_down: boolean;
  /** `null` means *never rehearsed* — which is not the same as "rehearsed and failed". */
  down_verified_at: string | null;
  down_verified_by: string | null;
  waiver_reason: string | null;
};

/** One migration this binary ships that the database has not applied. */
export type PendingMigration = {
  state: "pending";
  version: string;
  name: string;
  filename: string;
  checksum: string;
  statement_count: number;
  has_down: boolean;
  declared_no_down: boolean;
  /** `none`, `brief` or `rewrites_table`. */
  lock_risk: string;
};

/** A file whose checksum disagrees with what the ledger recorded. */
export type MigrationDrift = {
  version: string;
  name: string;
  /** What the ledger recorded when the file was applied. */
  recorded: string;
  /** What the file hashes to now. */
  current: string;
};

/** A banned-shape finding over a shipped migration file. */
export type MigrationViolation = {
  version: string;
  pattern: string;
  line: number;
  excerpt: string;
  /** `true` when the finding fails the gate. */
  blocking?: boolean;
  commented?: boolean;
};

/** The current migration lock, with the queries waiting behind it. */
export type MigrationLock = {
  held: boolean;
  version: string | null;
  direction: string | null;
  actor: string | null;
  source: string | null;
  started_at: string | null;
  age_seconds: number | null;
  /** Rendered as `<number> (omnionmg)` so it can be found in `pg_locks`. */
  lock_key: string;
  blocked: { pid: number; age_seconds: number; query: string; application: string | null }[];
};

/** The policy a run would execute under. */
export type MigrationPolicy = {
  require_down_scripts: boolean;
  lock_timeout_ms: number;
  statement_timeout_ms: number;
  banned_patterns: Record<string, boolean>;
  backfill_batch_size: number;
  backfill_rate_per_second: number;
  require_approval_for_destructive: boolean;
  updated_by: string | null;
  updated_at: string | null;
};

/** `GET /api/v1/deployment/migrations`. */
export type MigrationLedger = {
  applied: AppliedMigration[];
  pending: PendingMigration[];
  drift: MigrationDrift[];
  violations: MigrationViolation[];
  missing_down: PendingMigration[];
  policy: MigrationPolicy;
  lock: MigrationLock;
  gate_fails: boolean;
  summary: string;
  total: number;
};

/** `GET /api/v1/deployment/migrations` — applied, pending and drift in one answer. */
export function fetchMigrationLedger(version?: string): Promise<MigrationLedger> {
  const query = version ? `?version=${encodeURIComponent(version)}` : "";
  return request(`/api/v1/deployment/migrations${query}`);
}

/** One run of one migration, as the journal recorded it. */
export type MigrationRun = {
  id: number;
  direction: string;
  status: "running" | "succeeded" | "failed" | "aborted";
  actor: string;
  plan: unknown;
  started_at: string;
};

/** `GET /api/v1/deployment/migrations/{version}` — one migration in full. */
export type MigrationDetail = {
  version: string;
  state: "applied" | "pending";
  name: string | null;
  filename: string | null;
  checksum: string | null;
  sql: string | null;
  statements: string[];
  statement_count: number | null;
  down_statements: string[];
  has_down: boolean;
  declared_no_down: boolean;
  lock_risk: string | null;
  ledger: AppliedMigration | null;
  runs: MigrationRun[];
};

export function fetchMigration(version: string): Promise<MigrationDetail> {
  return request(`/api/v1/deployment/migrations/${encodeURIComponent(version)}`);
}

/** The plan preview. The API answers this without writing anything. */
export type MigrationPlan = {
  pending: PendingMigration[];
  violations: MigrationViolation[];
  missing_down: PendingMigration[];
  policy: MigrationPolicy;
  gate_fails: boolean;
  summary: string;
};

/** `POST /api/v1/deployment/migrations/plan` — a dry run over the whole pending set. */
export function previewMigrationPlan(disabledPatterns: string[] = []): Promise<MigrationPlan> {
  return request("/api/v1/deployment/migrations/plan", {
    method: "POST",
    body: JSON.stringify({ disabled_patterns: disabledPatterns }),
  });
}

/** `POST /api/v1/deployment/migrations` — apply the pending set. Refused on production. */
export function applyMigrations(): Promise<{ applied: string[]; summary: string }> {
  return request("/api/v1/deployment/migrations", { method: "POST" });
}

/** `GET /api/v1/deployment/migrations/lock`. */
export function fetchMigrationLock(): Promise<{ lock: MigrationLock }> {
  return request("/api/v1/deployment/migrations/lock");
}

/** One banned-shape rule, with the plain-language reason the policy screen shows. */
export type LintPattern = {
  key: string;
  why: string;
  blocking: boolean;
  enabled?: boolean;
};

/** `GET /api/v1/deployment/migrations/violations` — findings and the closed rule vocabulary. */
export type ViolationsResponse = {
  findings: (MigrationViolation & {
    id: number | null;
    severity: "error" | "warning";
    waived_by: string | null;
    waived_at: string | null;
    waiver_reason: string | null;
  })[];
  patterns: LintPattern[];
  total: number;
};

export function fetchViolations(): Promise<ViolationsResponse> {
  return request("/api/v1/deployment/migrations/violations");
}

/**
 * `POST /api/v1/deployment/migrations/violations/{id}/waive`.
 *
 * The `line` is part of the call because the waiver's key is the finding's own identity
 * `(version, pattern, line)` — a waiver keyed on the excerpt would expire the moment somebody
 * improved a comment, and a waiver that silently expires is a gate firing on a change nobody made.
 */
export function waiveViolation(
  id: number,
  input: { line: number; reason: string; pattern?: string },
): Promise<{ id: number; version: string; pattern: string; line: number }> {
  return request(`/api/v1/deployment/migrations/violations/${id}/waive`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** The bounds the policy may not leave, so the form can refuse before the round trip. */
export type PolicyBounds = {
  min_lock_timeout_ms: number;
  max_lock_timeout_ms: number;
  min_statement_timeout_ms: number;
  max_statement_timeout_ms: number;
  min_backfill_batch: number;
  max_backfill_batch: number;
};

export type PolicyWaiver = {
  version: string;
  pattern: string;
  line: number;
  excerpt: string;
  waived_by: string;
  waiver_reason: string;
};

/** `GET /api/v1/deployment/migrations/policy`. */
export type PolicyResponse = {
  policy: MigrationPolicy;
  bounds: PolicyBounds;
  patterns: LintPattern[];
  waivers: PolicyWaiver[];
};

export function fetchMigrationPolicy(): Promise<PolicyResponse> {
  return request("/api/v1/deployment/migrations/policy");
}

/** `PUT /api/v1/deployment/migrations/policy` — the bounds are enforced before the write. */
export function saveMigrationPolicy(policy: MigrationPolicy): Promise<{ policy: MigrationPolicy }> {
  return request("/api/v1/deployment/migrations/policy", {
    method: "PUT",
    body: JSON.stringify(policy),
  });
}

/** What a reversal rehearsal proved, against a throwaway copy of the schema. */
export type VerifyReport = {
  version: string;
  filename: string;
  statements: number;
  duration_ms: number;
  tables_before: string[];
  tables_after: string[];
  /** `true` only when the structure came back exactly. */
  restored: boolean;
};

/**
 * `POST /api/v1/deployment/migrations/{version}/verify-down`.
 *
 * `scratch` is a database NAME on this server, not a URL. The API derives the connection string
 * from this installation's own configuration, so a rehearsal can never be aimed at another server
 * — and the API refuses a name that is the live database.
 */
export function rehearseReversal(
  version: string,
  scratch: string,
): Promise<{ report: VerifyReport; scratch: string }> {
  return request(`/api/v1/deployment/migrations/${encodeURIComponent(version)}/verify-down`, {
    method: "POST",
    body: JSON.stringify({ scratch }),
  });
}

// -------------------------------------------------------------------------------------------
// Backfills (REQ-129 slice 3)
// -------------------------------------------------------------------------------------------

/** `pending` | `running` | `paused` | `completed` | `failed`. */
export type BackfillState = "pending" | "running" | "paused" | "completed" | "failed";

/**
 * One backfill job.
 *
 * `cursor_display` exists beside `resume_key` on purpose: a job that has never run has a NULL
 * cursor, and a cell rendering `null` as an empty string reads as "a key of nothing" rather than
 * "has not started". The API sends the em-dash form so the screen cannot get it wrong.
 *
 * `rows_done` is a self-report and the screen treats it as one — the progress bar is labelled with
 * it, but nothing is presented as "verified" on the strength of this number alone.
 */
export type BackfillJob = {
  id: string;
  name: string;
  table_name: string;
  column_name: string;
  key_column: string;
  batch_size: number;
  rate_limit_per_second: number;
  /** `null` until the first batch commits. Never a sentinel — see `INITIAL_CURSOR`. */
  resume_key: string | null;
  rows_done: number;
  state: BackfillState;
  /** The database's own message, set only while `state = 'failed'`. */
  last_error: string | null;
  paused_at: string | null;
  started_at: string | null;
  completed_at: string | null;
  created_at: string;
  /** `true` for every state that is neither completed nor failed. */
  open: boolean;
  /** Only a `running` job can be paused — a pending one has no batch in flight. */
  can_pause: boolean;
  /** A paused, failed or pending job can be resumed. A completed one cannot. */
  can_resume: boolean;
  /** `resume_key`, or an em-dash when there is none. */
  cursor_display: string;
};

/**
 * A descriptor with no job yet.
 *
 * Its own type rather than a `BackfillJob` full of nulls: a migration that registered a backfill is
 * a backfill the operator has to run, and on a fresh installation that is the ONLY row the screen
 * has. Rendering it through the job type would force `state`/`resume_key`/`rows_done` placeholders
 * into a card, and placeholders in a progress UI are how a "0 of 0" bar gets read as "nothing to
 * do".
 */
export type PendingBackfill = {
  version: string;
  name: string;
  table_name: string;
  column_name: string;
  key_column: string;
  batch_size: number;
  rate_limit_per_second: number;
};

/** The ceilings the client must know without trying them. */
export type BackfillBounds = {
  max_batches_per_request: number;
  min_batch_size: number;
  max_batch_size: number;
};

export type BackfillList = {
  jobs: BackfillJob[];
  states: BackfillState[];
  pending_descriptors: PendingBackfill[];
  bounds: BackfillBounds;
};

/** The descriptor WITH its statement — the "before I resume, show me what will run" payload. */
export type BackfillDescriptor = {
  version: string;
  name: string;
  /** Interpolated SQL from a migration file. It is a query, never a credential. */
  statement: string;
  batch_size: number;
  rate_limit_per_second: number;
};

export type BackfillDetail = {
  job: BackfillJob;
  /** `null` when the descriptor row is gone — a job can outlive its registration. */
  descriptor: BackfillDescriptor | null;
  states: BackfillState[];
};

/**
 * What one batch request actually did.
 *
 * `rows` is THIS request's rows and `job.rows_done` is where the job stands afterwards. Keeping them
 * apart is the point: a paused job refuses to run, and a refusal that answered only `rows_done: 0`
 * reads exactly like an empty table.
 *
 * `requested_batches` versus `batches` is the other one. The API clamps rather than refuses, so an
 * operator who typed 50 needs to see that 10 ran — a silent clamp is what a platform gets reported
 * as ignoring the operator.
 */
export type BackfillRun = {
  job: BackfillJob;
  ran: boolean;
  rows: number;
  /** Present on the resume route only. */
  batches?: number;
  requested_batches?: number;
  finished: boolean;
};

export function listBackfills(params?: {
  state?: BackfillState;
  open?: boolean;
}): Promise<BackfillList> {
  const query = new URLSearchParams();
  if (params?.state) query.set("state", params.state);
  if (params?.open !== undefined) query.set("open", String(params.open));
  const suffix = query.toString() ? `?${query.toString()}` : "";
  return request(`/api/v1/deployment/backfills${suffix}`);
}

export function readBackfill(id: string): Promise<BackfillDetail> {
  return request(`/api/v1/deployment/backfills/${encodeURIComponent(id)}`);
}

/** Run exactly one batch. Never drains — see `MAX_BATCHES_PER_REQUEST`. */
export function runBackfillBatch(id: string): Promise<BackfillRun> {
  return request(`/api/v1/deployment/backfills/${encodeURIComponent(id)}/run`, {
    method: "POST",
    body: JSON.stringify({}),
  });
}

export function pauseBackfill(id: string, reason?: string): Promise<{ job: BackfillJob }> {
  return request(`/api/v1/deployment/backfills/${encodeURIComponent(id)}/pause`, {
    method: "POST",
    body: JSON.stringify({ reason: reason ?? null }),
  });
}

/** `batches` is clamped by the API; the response reports both what was asked for and what ran. */
export function resumeBackfill(id: string, batches: number): Promise<BackfillRun> {
  return request(`/api/v1/deployment/backfills/${encodeURIComponent(id)}/resume`, {
    method: "POST",
    body: JSON.stringify({ batches }),
  });
}

// -------------------------------------------------------------------------------------------
// Seeds
// -------------------------------------------------------------------------------------------

/** One declared dataset, and whether its manifest files actually exist. */
export type SeedDataset = {
  name: string;
  description: string;
  row_estimate: number;
  compatible_from: string;
  compatible_to: string | null;
  /** A seeded row carries `declared-<name>`; a discovered manifest carries a real SHA-256. */
  manifest_checksum: string;
  /** Derived from `manifest_checksum`, sent so the caller does not have to infer it. */
  files_present: boolean;
};

export type SeedLoad = {
  id: string;
  dataset: string;
  installation_kind: string;
  loaded_by: string;
  rows_loaded: number;
  loaded_at: string;
};

export type SeedList = {
  datasets: SeedDataset[];
  loads: SeedLoad[];
  installation_kind: string;
  /**
   * The refusal the API would give, or `null` when a load is allowed.
   *
   * Sent rather than inferred so the screen renders the refusal INSTEAD OF a working button. A load
   * button that 409s when pressed is a dead button, and the request forbids those.
   */
  load_refused: string | null;
};

export function listSeeds(): Promise<SeedList> {
  return request("/api/v1/deployment/seeds");
}

/**
 * Load a dataset. `confirm` must equal the dataset's own name.
 *
 * The typed name is checked before the installation kind on purpose: a typo is a caller mistake
 * with no consequence, so the answer can be about the mistake. An order that checked the
 * environment first would answer a typo with a sentence about production.
 */
export function loadSeed(
  name: string,
  confirm: string,
): Promise<{ dataset: string; rows_loaded: number }> {
  return request(`/api/v1/deployment/seeds/${encodeURIComponent(name)}/load`, {
    method: "POST",
    body: JSON.stringify({ confirm }),
  });
}

// -------------------------------------------------------------------------------------------
// Anonymised exports (REQ-129, slice 4)
//
// The three states the API can refuse a download with are the reason this section exists as
// types rather than as `any`: `export_revoked`, `export_expired` and `export_already_downloaded`
// are three different follow-ups for the same HTTP 410, and a screen that only knows "it failed"
// turns the single-use guarantee into a support ticket. `download_count: number` is the field
// that lets the panel say which one happened BEFORE anyone presses the button.
// -------------------------------------------------------------------------------------------

/** One export, as `export_json` writes it. */
export type SupportExport = {
  id: string;
  reason: string;
  tables: string[];
  /** Per-column decisions, as `ColumnPlan[]` — the plan the row was created from. */
  column_actions: { table: string; column: string; class: string; action: string; overridden: boolean }[];
  row_limit: number | null;
  window_start: string | null;
  window_end: string | null;
  status: string;
  file_key: string | null;
  file_size: number | null;
  checksum: string | null;
  /**
   * The salt's fingerprint, never the salt itself.
   *
   * Two exports of the same rows do not hash the same value to the same digest, and that is the
   * point: a per-export salt is what stops a vendored file from being a cross-customer lookup
   * table. The fingerprint is stored so an operator can tell two exports apart without the salt
   * ever being derivable.
   */
  salt_fingerprint: string | null;
  watermark: string;
  expires_at: string;
  download_count: number;
  last_downloaded_at: string | null;
  revoked_at: string | null;
  requested_by: string | null;
  requested_by_name: string;
  error: string | null;
  created_at: string;
};

export type ExportList = {
  exports: SupportExport[];
  /** How many rows the expiry sweep flipped on this very request. */
  expired_by_this_request: number;
  limits: { max_rows: number; ttl_hours: number };
};

/** One reviewed column classification. */
export type ColumnClassification = {
  table: string;
  column: string;
  class: string;
  default_action: string;
  notes: string;
  reviewed_by: string | null;
  reviewed_at: string | null;
};

export type ClassificationList = {
  classifications: ColumnClassification[];
  /**
   * `table.column` for every public column with no classification row.
   *
   * This list is the remaining work on the whole feature: the builder fails closed on a
   * classified-nothing column, so while it is non-empty no export can be produced at all.
   */
  unclassified: string[];
  classes: string[];
  actions: string[];
};

export function listExports(status?: string): Promise<ExportList> {
  const query = status && status.trim() ? `?status=${encodeURIComponent(status.trim())}` : "";
  return request(`/api/v1/deployment/exports${query}`);
}

export function fetchExport(id: string): Promise<SupportExport> {
  return request(`/api/v1/deployment/exports/${encodeURIComponent(id)}`);
}

export function listClassifications(): Promise<ClassificationList> {
  return request("/api/v1/deployment/exports/classifications");
}

/**
 * Classify or re-classify one column.
 *
 * `notes` is required by the API, not by the type, because the note is what the next reviewer
 * reads; the screen asks for it rather than letting the server be the only place that refuses.
 */
export function classifyColumn(input: {
  table_name: string;
  column_name: string;
  class: string;
  default_action: string;
  notes: string;
}): Promise<ColumnClassification> {
  return request("/api/v1/deployment/exports/classifications", {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

export type ExportPlanColumn = {
  table: string;
  column: string;
  class: string;
  action: string;
  overridden: boolean;
};

/**
 * `POST /deployment/exports` — plan and record. Does NOT produce the file.
 *
 * Producing is `runExport`; keeping them apart is what lets the refusal for an unclassified
 * column land in front of the operator instead of after a ten-minute export.
 */
export function createExport(input: {
  reason: string;
  tables: string[];
  column_actions?: Record<string, string>;
  row_limit?: number | null;
  window_start?: string | null;
  window_end?: string | null;
}): Promise<{
  id: string;
  status: string;
  watermark: string;
  salt_fingerprint: string;
  expires_at: string;
  plan: {
    tables: string[];
    columns: ExportPlanColumn[];
    removed: number;
    hashed: number;
    synthetic: number;
    kept: number;
  };
}> {
  return request("/api/v1/deployment/exports", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** `POST /deployment/exports/{id}/run` — produce the planned file. */
export function runExport(id: string): Promise<{
  id: string;
  status: string;
  file_key: string;
  file_size: number;
  checksum: string;
  download_url: string;
}> {
  return request(`/api/v1/deployment/exports/${encodeURIComponent(id)}/run`, { method: "POST" });
}

/** `DELETE /deployment/exports/{id}` — revoke. The audit row stays; the link dies. */
export function revokeExport(id: string): Promise<{ id: string; status: string; download_count: number }> {
  return request(`/api/v1/deployment/exports/${encodeURIComponent(id)}`, { method: "DELETE" });
}

/**
 * The download link, as a URL for an `<a download>`.
 *
 * A plain URL rather than `request()`, because `request()` parses JSON and this endpoint answers
 * with a file. The browser carries the session cookie on a same-origin navigation, and the API
 * claims the single use itself — so the panel's job is to say clearly that pressing this spends
 * the download, not to guard it.
 */
export function exportDownloadUrl(id: string): string {
  return `/api/v1/deployment/exports/${encodeURIComponent(id)}/download`;
}
