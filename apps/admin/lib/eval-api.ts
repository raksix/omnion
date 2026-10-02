/**
 * The eval-suite client (docs/requests/REQ-107, slice 1).
 *
 * A module of its own for the same reason `local-api.ts` is one: these are the only places the
 * eval wire shapes are written down on the admin side, and the two screens that read them sit
 * next to it.
 *
 * **The property list is never written down here.** `EvalSuiteDetail.properties` arrives from
 * the server, which derives it from `eval_case::PROPERTIES` — the same list the scorer reads. A
 * hard-coded copy on this side would be a second source of truth: a property added to the scorer
 * would appear in the editor as a checkbox that saves a case the runner then scores against a
 * rule nobody can see, and a property *removed* from the scorer would stay offerable as one that
 * does nothing. The editor renders what it is given.
 *
 * **A refusal names the field it is about.** `ApiError.details.field` is what the form marks, and
 * it is why the messages in this slice were written with the column name in backticks.
 */
import { request } from "./api";

/** One eval suite. */
export type EvalSuite = {
  id: string;
  organization_id: string;
  /** URL-safe key, unique per tenant; what a gate names and a delete confirms. */
  key: string;
  name: string;
  description: string;
  /** `agent`, `copilot`, `task` or `model`. */
  target: string;
  agent_id: string | null;
  copilot_key: string | null;
  task: string | null;
  /** The model under test. */
  model_id: string | null;
  temperature: number | null;
  tools: string[];
  collections: string[];
  /** The pass share below which a gate blocks, 1–100. */
  threshold_percent: number;
  /** How many points a run may drop against its baseline, 0–50. */
  max_regression_points: number;
  /** Whether this suite is the promotion gate. */
  blocking: boolean;
  /** A preset name or a five-field cron expression; `null` is manual-only. */
  schedule: string | null;
  /** The second model that judges rubric cases. It must differ from the one under test. */
  judge_model_id: string | null;
  judge_prompt: string | null;
  judge_prompt_version: number;
  enabled: boolean;
  created_by: string | null;
  created_at: string;
  updated_at: string;
  /** Cases, enabled or not. */
  case_count: number;
  /** How many a run would execute. */
  enabled_case_count: number;
  /** How many of those need the judge. */
  rubric_case_count: number;
  /**
   * The last run's pass rate — **always `null` in slice 1**.
   *
   * `ai_eval_runs` is slice 2's table and is deliberately not read yet, so a `null` here means
   * "no run has been recorded", not "the run failed". Slice 2 fills it, and the suite list starts
   * showing it without the screen changing shape.
   */
  last_pass_rate: number | null;
  /** When the last run finished; `null` for the same reason. */
  last_run_at: string | null;
  /** The last run's gate verdict; `null` for the same reason. */
  last_gate: string | null;
};

/** One case: a prompt and what the output must satisfy. */
export type EvalCase = {
  id: string;
  suite_id: string;
  organization_id: string;
  name: string;
  /** Always an object — a plain-text editor's string is normalized to `{prompt}` server-side. */
  input: Record<string, unknown>;
  /** The properties this case asserts. */
  expected: Record<string, unknown>;
  /** 0.1–10; how much this case counts for in a run's pass rate. */
  weight: number;
  tags: string[];
  enabled: boolean;
  /** `manual`, `import` or `run`. */
  source: string;
  /** The run a captured case came from. */
  source_run_id: string | null;
  created_at: string;
  updated_at: string;
  /** The most recent result, or `null` — slice 2 fills it. */
  last_status: string | null;
  /** When it last ran; `null` for the same reason. */
  last_run_at: string | null;
};

/** A suite row as the list screen reads it, with the badge it renders. */
export type EvalSuiteSummary = {
  /** The suite's own columns, flattened. */
  id: string;
  key: string;
  name: string;
  description: string;
  target: string;
  agent_id: string | null;
  copilot_key: string | null;
  task: string | null;
  model_id: string | null;
  temperature: number | null;
  tools: string[];
  collections: string[];
  threshold_percent: number;
  max_regression_points: number;
  blocking: boolean;
  schedule: string | null;
  judge_model_id: string | null;
  judge_prompt: string | null;
  judge_prompt_version: number;
  enabled: boolean;
  case_count: number;
  enabled_case_count: number;
  rubric_case_count: number;
  last_pass_rate: number | null;
  last_run_at: string | null;
  last_gate: string | null;
  /**
   * `ready`, `needs_judge`, `empty` or `disabled`.
   *
   * Four states rather than a boolean, because the request's own risk note is that "a suite of
   * ten easy cases passes everything": a green row next to a suite with no cases is worse than an
   * empty table. `empty` and `needs_judge` are both states a run cannot succeed from, and the
   * screen has to be able to say which one.
   */
  readiness: "ready" | "needs_judge" | "empty" | "disabled";
  /** Why the badge reads the way it does, in words the screen shows rather than re-derives. */
  readiness_note: string;
};

/** `GET /api/v1/ai/evals/suites`. */
export type EvalSuiteList = {
  suites: EvalSuiteSummary[];
  /** How many the tenant owns in total, so a filtered list can say "3 of 11". */
  total: number;
  blocking_count: number;
  scheduled_count: number;
  /** Cases across all suites — the coverage stat tile. */
  case_count: number;
  /** Tag → case count. The request's own warning is that coverage matters. */
  coverage: { tag: string; cases: number }[];
  /**
   * `true` when the tenant has no suite at all.
   *
   * Distinct from "loaded and found none in the current filter": an installation that has never
   * made a suite gets the empty state, while a filtered list gets "no match".
   */
  is_empty: boolean;
  /**
   * The write keys this viewer is missing, so `Run now` can be disabled **with the reason
   * attached** rather than present-and-403.
   *
   * The list is the server's answer, recomputed from the caller's effective permissions. The
   * panel never derives it from the role's name: two people with the same role can differ, and a
   * guess either hides a button the caller may use or offers one they cannot.
   */
  viewer_missing: string[];
};

/** A picker option: a row id plus the label a human reads. */
export type EvalOption = { id: string; label: string; detail?: string };

/** A cron preset and what it expands to. */
export type EvalSchedulePreset = {
  id: string;
  label: string;
  /** `null` for a manual-only suite and for `custom`. */
  cron: string | null;
};

/** One assertable property and the editor field it reveals. */
export type EvalProperty = {
  /** The key, exactly as it appears in `expected`. */
  key: string;
  label: string;
  /** The field the editor reveals, so the form is not hard-coded against this list. */
  field: string;
  /** Whether a second model has to judge it. */
  needs_judge: boolean;
};

/** `GET /api/v1/ai/evals/suites/{key}` — the suite, its cases and what the form may choose from. */
export type EvalSuiteDetail = EvalSuite & {
  cases: EvalCase[];
  readiness: EvalSuiteSummary["readiness"];
  readiness_note: string;
  model_options: EvalOption[];
  agent_options: EvalOption[];
  schedule_presets: EvalSchedulePreset[];
  properties: EvalProperty[];
};

/** `GET /api/v1/ai/evals/suites/{key}/cases`. */
export type EvalCaseList = {
  cases: EvalCase[];
  /** Enabled cases' weight total — the denominator of a run's pass rate. */
  enabled_weight: number;
  rubric_count: number;
};

/** The filters `GET /ai/evals/suites` accepts; all optional. */
export type EvalSuiteFilter = {
  target?: string | null;
  enabled?: boolean | null;
  blocking?: boolean | null;
  q?: string | null;
  tag?: string | null;
};

/** The suites, filtered. */
export function fetchEvalSuites(filter: EvalSuiteFilter = {}): Promise<EvalSuiteList> {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(filter)) {
    if (value === null || value === undefined || value === "") continue;
    params.set(key, String(value));
  }
  const query = params.toString();
  return request<EvalSuiteList>(`/api/v1/ai/evals/suites${query ? `?${query}` : ""}`);
}

/**
 * Create a suite.
 *
 * The defaults match the API's, so a form posting only the fields it shows creates a valid row.
 * `tools` and `collections` are arrays of names rather than jsonb strings: the wire type is a
 * jsonb array, and a client that had to stringify it would own a second encoder.
 */
export function createEvalSuite(body: {
  key: string;
  name: string;
  description?: string;
  target: string;
  agent_id?: string | null;
  copilot_key?: string | null;
  task?: string | null;
  model_id?: string | null;
  temperature?: number | null;
  tools?: string[];
  collections?: string[];
  threshold_percent?: number;
  max_regression_points?: number;
  blocking?: boolean;
  schedule?: string | null;
  judge_model_id?: string | null;
  judge_prompt?: string | null;
  enabled?: boolean;
}): Promise<{ suite: EvalSuite }> {
  return request<{ suite: EvalSuite }>("/api/v1/ai/evals/suites", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** One suite with its cases and the pickers the form needs. */
export function fetchEvalSuite(key: string): Promise<EvalSuiteDetail> {
  return request<EvalSuiteDetail>(`/api/v1/ai/evals/suites/${encodeURIComponent(key)}`);
}

/**
 * Edit a suite.
 *
 * A field the body omits keeps its value; a field set to `null` is cleared. That is the store's
 * `Option<Option<_>>` shape and the form has to mirror it exactly — sending `temperature: null`
 * for an untouched field would silently clear the suite's temperature on every save.
 */
export function updateEvalSuite(
  key: string,
  body: {
    name?: string;
    description?: string;
    temperature?: number | null;
    tools?: string[];
    collections?: string[];
    threshold_percent?: number;
    max_regression_points?: number;
    blocking?: boolean;
    schedule?: string | null;
    judge_model_id?: string | null;
    judge_prompt?: string | null;
    enabled?: boolean;
  },
): Promise<{ suite: EvalSuite }> {
  return request<{ suite: EvalSuite }>(`/api/v1/ai/evals/suites/${encodeURIComponent(key)}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

/**
 * Delete a suite. The key must be sent back as `confirm`.
 *
 * A suite is what a promotion pipeline and a schedule are named after, so the confirm is the key
 * itself rather than a yes/no: a mistyped id should not be able to remove the ruler a release
 * depends on.
 */
export function deleteEvalSuite(key: string): Promise<void> {
  return request<void>(
    `/api/v1/ai/evals/suites/${encodeURIComponent(key)}?confirm=${encodeURIComponent(key)}`,
    { method: "DELETE" },
  );
}

/** The cases of one suite. */
export function fetchEvalCases(key: string): Promise<EvalCaseList> {
  return request<EvalCaseList>(`/api/v1/ai/evals/suites/${encodeURIComponent(key)}/cases`);
}

/** Add a case. */
export function createEvalCase(
  key: string,
  body: {
    name: string;
    /** A string is accepted and normalized to `{prompt}` server-side. */
    input: string | Record<string, unknown>;
    expected: Record<string, unknown>;
    weight?: number;
    tags?: string[];
    enabled?: boolean;
    source?: string;
    source_run_id?: string | null;
  },
): Promise<{ case: EvalCase }> {
  return request<{ case: EvalCase }>(
    `/api/v1/ai/evals/suites/${encodeURIComponent(key)}/cases`,
    { method: "POST", body: JSON.stringify(body) },
  );
}

/** Edit a case; a field the body omits keeps its value. */
export function updateEvalCase(
  id: string,
  body: {
    name?: string;
    input?: string | Record<string, unknown>;
    expected?: Record<string, unknown>;
    weight?: number;
    tags?: string[];
    enabled?: boolean;
  },
): Promise<{ case: EvalCase }> {
  return request<{ case: EvalCase }>(`/api/v1/ai/evals/cases/${id}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

/** Remove a case. */
export function deleteEvalCase(id: string): Promise<void> {
  return request<void>(`/api/v1/ai/evals/cases/${id}`, { method: "DELETE" });
}

/** One row the importer refused, with the line the editor can point at. */
export type EvalImportProblem = { line: number; message: string };

/** What an import wrote and what it refused. */
export type EvalImportReport = {
  imported: EvalCase[];
  problems: EvalImportProblem[];
  /** `true` when nothing was refused. */
  clean: boolean;
};

/**
 * Import cases from CSV.
 *
 * The import is **partial by design**: the good rows are written and the bad ones come back in
 * `problems` with their line numbers, rather than the whole file being refused over one typo. The
 * screen must render both halves — a report that only said "3 imported" would hide which of the
 * remaining lines still needs fixing.
 */
export function importEvalCases(key: string, csv: string): Promise<EvalImportReport> {
  return request<EvalImportReport>(`/api/v1/ai/evals/suites/${encodeURIComponent(key)}/import`, {
    method: "POST",
    body: JSON.stringify({ csv }),
  });
}

/** The columns the importer reads, for the helper text under the paste box. */
export const IMPORT_COLUMNS =
  "name, input, expected (a JSON object), weight, tags, enabled — plus one column per property (exact, contains, regex, rubric, …)";
