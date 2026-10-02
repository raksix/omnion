/**
 * The eval-RUN client (docs/requests/REQ-107, slice 2).
 *
 * A module of its own because a run is a different fact from a suite: a suite is a thing you
 * author, a run is a thing that happened to it. `eval-api.ts` writes down the authoring wire
 * shapes; this one writes down the execution ones, and the two screens that read them are the
 * run list and the run detail.
 *
 * **The run's status is carried twice, and the client must read `state` and not `status`.**
 * The row's own `status` column is the stored value — `queued`, `running`, `passed`, `failed`,
 * `error`, `cancelled` — and the API adds a `state` beside it because two of those mean the
 * same thing to an operator and one of the pair means the opposite of "in progress":
 *
 *   - `queued` means **nothing has claimed it**. A runner that is not running, or a schedule
 *     that has not fired. It is not progress and colouring it as progress is a lie that hides
 *     a stuck suite.
 *   - `running` means a runner has it and is scoring.
 *
 * So a run list that renders both as "in progress" cannot answer the question an operator opens
 * the screen with, which is "is it stuck?". `state` is that answer, computed server-side where
 * the whole row is visible; the client never re-derives it from `status`, because the two rules
 * (merge the first pair, split the last) are exactly the sort of thing a second implementation
 * gets subtly wrong.
 *
 * **Nothing here reads a snapshot.** `RunRow.snapshot` is the reproduction data — the model
 * config, the property set, the judge prompt version — captured at claim time so a run can be
 * explained months later. The detail screen shows its *summary* fields (which model, which judge,
 * how many cases), not the blob, because a jsonb dump rendered raw is a wall of text an
 * operator cannot act on.
 */
import { request } from "./api";

/** One run, as the history list reads a row. */
export type EvalRun = {
  id: string;
  suite_id: string;
  /** Denormalised onto the row so the list needs no join to render. */
  suite_key: string;
  suite_name: string;
  organization_id: string;
  /** `manual`, `scheduled` or `gate`. */
  kind: string;
  /** The stored status — see the module header for why `state` is the one to read. */
  status: string;
  /** The reproduction data. Read its summary, not this. */
  snapshot: unknown;
  model_id: string | null;
  judge_model_id: string | null;
  total_cases: number;
  passed_cases: number;
  failed_cases: number;
  error_cases: number;
  /** The weighted pass share, 0–100, or `null` while the run has not settled. */
  pass_rate: number | null;
  /** The threshold this run was judged against, in percent. */
  threshold_percent: number;
  /** `none`, `pass` or `block`. */
  gate: string;
  /** The baseline this run was compared to, when it was a gate run. */
  base_run_id: string | null;
  /** Cost in micros — 1_000_000 micros is one currency unit. */
  cost_micros: number;
  duration_ms: number | null;
  triggered_by: string | null;
  /** Why it failed, when it failed. */
  error: string | null;
  started_at: string;
  finished_at: string | null;
  /**
   * The suite's regression tolerance at claim time.
   *
   * A snapshot of the suite's value, not a live read: a tolerance edited after the run must not
   * be able to restate that run's regression verdict. The diff screen therefore compares the two
   * runs' *own* tolerances rather than the suite's current one.
   */
  max_regression_points: number;
  /** The suite's blocking flag at claim time — whether a gate was asked for. */
  blocking: boolean;
};

/** One run plus the two fields only the API can decide. */
export type EvalRunSummary = {
  run: EvalRun;
  /** The status in one word: `queued`, `running`, `passed`, `failed`, `error`, `cancelled`. */
  state: string;
  /** The same fact as a sentence, for a screen that wants the reason and not the colour. */
  state_note: string;
};

/** One case's result inside a run. */
export type EvalCaseResult = {
  id: number;
  run_id: string;
  case_id: string | null;
  case_name: string;
  status: string;
  score: number | null;
  /** The property checks and what each concluded — the evidence behind `score`. */
  checks: unknown;
  /** The judge's reasoning, for a rubric case. Empty for a deterministic one. */
  judge_reason: string | null;
  output: string | null;
  latency_ms: number | null;
  prompt_tokens: number | null;
  completion_tokens: number | null;
  cost_micros: number;
};

/** The suite's baseline, when it has one. */
export type EvalBaseline = {
  suite_id: string;
  run_id: string;
  pass_rate: number;
  set_by: string | null;
  set_at: string;
};

/** A run in the diff picker. */
export type EvalRunOption = {
  id: string;
  started_at: string;
  pass_rate: number | null;
  label: string;
};

/** The stat tiles' window, and what fell inside it. */
export type EvalRunStats = {
  suites: number;
  runs: number;
  average_pass_rate: number | null;
  cost_micros: number;
  days: number;
};

/** The run history. */
export type EvalRunList = {
  total: number;
  is_empty: boolean;
  stats: EvalRunStats;
  runs: EvalRunSummary[];
};

/** How the history is filtered. */
export type EvalRunFilter = {
  /** A suite key, not an id — the run list names the suite the way a person types it. */
  suite?: string | null;
  /**
   * `queued` / `running` / `passed` / `failed` / `error` / `cancelled`, or `incomplete` for the
   * first two. `incomplete` is a server-side union and deliberately not something the client
   * filters after fetching: it would need every run to be correct about a second time.
   */
  status?: string | null;
  kind?: string | null;
  /** `none` / `pass` / `block`. */
  gate?: string | null;
  user?: string | null;
  /** `oldest` reverses the newest-first default. */
  order?: string | null;
  limit?: number | null;
  offset?: number | null;
  /** The stat tiles' window in days. Separate from the filters on purpose — a window is not a filter. */
  days?: number | null;
};

/** The run detail payload. */
export type EvalRunDetail = {
  run: EvalRun;
  state: string;
  state_note: string;
  results: EvalCaseResult[];
  baseline: EvalBaseline | null;
  /**
   * Settled runs of the same suite the diff picker offers.
   *
   * Only settled ones, and that is the point: a queued or errored run has no rate, so offering
   * it would put a dead choice in a picker — the kind that fails at the moment the operator
   * needs it.
   */
  diff_candidates: EvalRunOption[];
  /** `true` when the run produced no rows at all — queued, or errored before the first case. */
  is_empty: boolean;
};

/** One case in a diff, as both runs saw it. */
export type EvalDiffRow = {
  case_id: string | null;
  case_name: string;
  /** `null` when the case is not in the baseline. */
  base_status: string | null;
  base_score: number | null;
  head_status: string;
  head_score: number | null;
  /** `improved`, `regressed`, `unchanged`, `added` or `removed`. */
  movement: string;
};

/** What the comparison concludes. */
export type EvalGateVerdict = {
  /** `pass` or `block`. */
  gate: string;
  /** Whether the threshold itself was met. */
  held_threshold: boolean;
  baseline_pass_rate: number | null;
  /** How far below the baseline the run landed, in points. */
  drop_points: number | null;
  /** Whether that drop is a regression — past the tolerance, not merely down. */
  regressed: boolean;
};

/** One run against a baseline, case by case. */
export type EvalRunDiff = {
  run: EvalRun;
  base: EvalRun;
  diff: {
    rows: EvalDiffRow[];
    improved: number;
    regressed: number;
    unchanged: number;
    /** Cases the baseline did not have. */
    added: number;
    /** Cases the baseline had and this run dropped. */
    removed: number;
  };
  gate: EvalGateVerdict;
  /** The sentence above the table. */
  summary: string;
};

/** The run history, filtered. */
export function fetchEvalRuns(filter: EvalRunFilter = {}): Promise<EvalRunList> {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(filter)) {
    if (value === null || value === undefined || value === "") continue;
    params.set(key, String(value));
  }
  const query = params.toString();
  return request<EvalRunList>(`/api/v1/ai/evals/runs${query ? `?${query}` : ""}`);
}

/** One run, with its results, its baseline and the diff picker. */
export function fetchEvalRun(id: string): Promise<EvalRunDetail> {
  return request<EvalRunDetail>(`/api/v1/ai/evals/runs/${encodeURIComponent(id)}`);
}

/**
 * One run against a baseline.
 *
 * **The baseline is always explicit.** There is no "compare against the previous run" shortcut,
 * and the omission is not a UI nicety: a diff whose baseline is implicit means something
 * different on every call depending on what else has run since, and a regression report nobody
 * can reproduce is a regression report nobody acts on. `GET .../diff` with no `base` therefore
 * refuses, and the screen ships a picker rather than a button.
 */
export function fetchEvalRunDiff(id: string, base: string): Promise<EvalRunDiff> {
  return request<EvalRunDiff>(
    `/api/v1/ai/evals/runs/${encodeURIComponent(id)}/diff?base=${encodeURIComponent(base)}`
  );
}

/**
 * Start a run of a suite.
 *
 * The response is the queued row, not a finished run: the route **enqueues** and the runner
 * claims and scores, because a route that scored inline would hold an HTTP request open for the
 * length of a suite — forty judge calls is minutes. So the screen must not wait for this to mean
 * "done"; it re-reads the run list.
 *
 * `kind` is `manual` or `gate` only. A caller cannot ask for `scheduled`: that kind belongs to
 * the scheduler, and a hand-posed scheduled run would put a row in the history that no schedule
 * produced. `base_run_id` is required for a `gate` — a gate with no baseline is refused, since
 * there is nothing to hold the threshold against.
 */
export function startEvalRun(
  key: string,
  body: { kind?: "manual" | "gate"; base_run_id?: string | null } = {}
): Promise<EvalRunSummary> {
  return request<EvalRunSummary>(`/api/v1/ai/evals/suites/${encodeURIComponent(key)}/run`, {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/**
 * Stop a run, keeping whatever it produced.
 *
 * A run that has already settled answers `409` rather than a cheerful success — "cancel" on a
 * finished run reads as done in a panel, and the operator then waits for a stop that already
 * happened. The screen must show that refusal rather than closing the sheet.
 */
export function cancelEvalRun(id: string): Promise<EvalRunSummary> {
  return request<EvalRunSummary>(`/api/v1/ai/evals/runs/${encodeURIComponent(id)}/cancel`, {
    method: "POST",
  });
}

/**
 * Make a settled run the suite's baseline.
 *
 * This is a write, not a reading: the baseline is the yardstick every later regression is
 * measured against, so it sits behind `ai.evals.manage` alongside deleting a case — the person
 * who may weaken the ruler must not be the person who presses it. Only a settled run of its own
 * suite qualifies; the store refuses anything else.
 */
export function setEvalBaseline(key: string, runId: string): Promise<EvalBaseline> {
  return request<EvalBaseline>(`/api/v1/ai/evals/suites/${encodeURIComponent(key)}/baseline`, {
    method: "POST",
    body: JSON.stringify({ run_id: runId }),
  });
}

/**
 * The statuses the history filter offers, in the order an operator reads them.
 *
 * `incomplete` leads because it is the question the screen is opened with — "is anything stuck?"
 * — and `queued` and `running` mean different things underneath it, which is exactly why the
 * server unions them rather than the client filtering twice.
 */
export const RUN_STATUS_FILTERS = [
  { value: "", label: "All statuses" },
  { value: "incomplete", label: "In progress (queued or running)" },
  { value: "queued", label: "Queued — nothing has claimed it" },
  { value: "running", label: "Running — a runner is scoring" },
  { value: "passed", label: "Passed" },
  { value: "failed", label: "Failed" },
  { value: "error", label: "Errored" },
  { value: "cancelled", label: "Cancelled" },
] as const;

/** The kinds the filter offers. */
export const RUN_KIND_FILTERS = [
  { value: "", label: "All kinds" },
  { value: "manual", label: "Manual" },
  { value: "scheduled", label: "Scheduled" },
  { value: "gate", label: "Gate" },
] as const;

/** The gate verdicts the filter offers. */
export const RUN_GATE_FILTERS = [
  { value: "", label: "Any gate" },
  { value: "block", label: "Blocked" },
  { value: "pass", label: "Gate passed" },
  { value: "none", label: "No gate" },
] as const;
