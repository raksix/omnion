/**
 * The tool-telemetry client (docs/requests/REQ-107, slice 5).
 *
 * A module of its own for the reason `eval-api.ts` is one: `/ai/telemetry/tools` is read by
 * exactly one screen, and writing its wire shape next to the screen that renders it is what stops
 * the two from drifting apart silently — the client grows a field the screen ignores, or the
 * screen reads a field the route stopped sending, and neither breaks.
 *
 * **The window is echoed, never assumed.** `from`/`to` come back from the server because the
 * server *clamps* them (to `[today-365, today]`) and *swaps* an inverted pair rather than refusing
 * it. A screen that displayed the range it asked for would be a screen that says "last 90 days"
 * while rendering 30. So every label on this screen is computed from `data.from`/`data.to`.
 *
 * **`success_percent` is `null` for zero calls, and that is not the same as `0`.** A tool nobody
 * invoked has no success rate; a tool that was invoked and never worked has a rate of zero. The
 * screen renders the first as an em dash and the second as `0%`, and collapsing them would sort a
 * tool nobody called to the bottom of a table sorted by success — a false alarm invented by a
 * convenience.
 */
import { request } from "./api";

/** One tool's row: the store's aggregate, flattened onto the row. */
export type ToolTelemetryRow = {
  tool: string;
  calls: number;
  successes: number;
  failures: number;
  denials: number;
  /** `null` when no call in the window recorded a duration — "not measured", not "instant". */
  p50_ms: number | null;
  p95_ms: number | null;
  p99_ms: number | null;
  cost_micros: number;
  /** Failure codes ranked by the server, busiest first. Empty means no failure was attributed a code. */
  error_codes: Record<string, number>;
  /** How many days of the window this tool appears in, so "3 of 30" is computable. */
  days_seen: number;
  /** `null` for a tool with no calls at all; `0` for one that was called and never worked. */
  success_percent: number | null;
  denial_percent: number | null;
};

/** One day's costliest failing tool. */
export type CostliestFailing = {
  day: string;
  tool: string;
  failures: number;
  cost_micros: number;
};

/** One bar of the step histogram. */
export type StepBucket = {
  /** Steps taken, exactly — not a bucket edge. */
  steps: number;
  runs: number;
};

/** One point of the cost-per-solved scatter. */
export type SolvedDay = {
  day: string;
  runs: number;
  solved: number;
  cost_micros: number;
};

/** `GET /api/v1/ai/telemetry/tools`. */
export type ToolTelemetry = {
  /** The window **after** the server clamped and swapped it. Label from these, not from the input. */
  from: string;
  to: string;
  /** Days in the window, inclusive of both ends — a one-day window is 1. */
  days: number;
  /** Whether the tenant has nothing at all, as opposed to being filtered down to nothing. */
  is_empty: boolean;
  totals: {
    calls: number;
    successes: number;
    failures: number;
    denials: number;
    cost_micros: number;
  };
  tools: ToolTelemetryRow[];
  costliest_failing: CostliestFailing[];
  step_histogram: StepBucket[];
  cost_per_solved: SolvedDay[];
};

/** The filters the route accepts. All optional; an unset date means "let the server choose". */
export type ToolTelemetryFilter = {
  from?: string | null;
  to?: string | null;
  /** One tool's row, exactly as keyed. */
  tool?: string | null;
  /** Keep only the tools that failed at least once. */
  failing?: boolean | null;
};

/**
 * The window's tool numbers.
 *
 * The date parameters are sent as `YYYY-MM-DD` strings and deliberately not defaulted here: the
 * route picks a 30-day window when they are absent and clamps them when they are not, so sending
 * a client-side default would only ever fight the server's clamp. Echoing its answer is what
 * makes the screen's labels true.
 */
export function fetchToolTelemetry(
  filter: ToolTelemetryFilter = {},
): Promise<ToolTelemetry> {
  const params = new URLSearchParams();
  if (filter.from) params.set("from", filter.from);
  if (filter.to) params.set("to", filter.to);
  if (filter.tool) params.set("tool", filter.tool);
  if (filter.failing) params.set("failing", "true");
  const query = params.toString();
  return request<ToolTelemetry>(
    `/api/v1/ai/telemetry/tools${query ? `?${query}` : ""}`,
  );
}

/** The windows the range picker offers, in days. */
export const TELEMETRY_WINDOWS = [
  { days: 1, label: "Last 24 hours" },
  { days: 7, label: "Last 7 days" },
  { days: 30, label: "Last 30 days" },
  { days: 90, label: "Last 90 days" },
] as const;

/**
 * A micros amount in the unit the AI screens use.
 *
 * **Micros throughout, four decimal places.** The other AI panels divide by 1,000,000 and show
 * three; that is cents, and a tool costing 0.0004 of a cent per call then reads as `0.000` — a
 * row of zeros for a tool that is being called constantly. Four places puts the smallest figure
 * the roll-up can produce above the last digit it shows.
 */
export function micros(value: number): string {
  return (value / 1_000_000).toFixed(4);
}

/** A latency in milliseconds, or an em dash when it was never measured. */
export function latency(value: number | null): string {
  return value === null ? "—" : `${value.toLocaleString()} ms`;
}