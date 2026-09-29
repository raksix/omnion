/**
 * The two formatters every AI screen needs, in one place.
 *
 * They were copy-pasted into `ai-run-detail.tsx` and then into `ai-agents-list.tsx`, and a
 * duplicated money formatter is not a style question: a run detail rounding `$0.004` to
 * `$0.00` while the agents table rounds it to `$0.0040` puts two different numbers for the
 * same run on two screens, and the reader has to work out which one to believe. The unit is
 * millionths of a dollar on the wire, and every screen that shows cost converts it here.
 */

/**
 * Cost in millionths as currency.
 *
 * Four decimals below a cent, because the numbers that matter at this scale are the small
 * ones: an agent that costs `$0.004` per run is not a rounding detail, it is the difference
 * between a habit and a bill. A leading em dash for zero is deliberate — "no cost recorded"
 * and "free" are different facts, and a run that never called a provider has neither.
 */
export function cost(micros: number): string {
  if (!micros) return "—";
  const dollars = micros / 1_000_000;
  return dollars < 0.01 ? `$${dollars.toFixed(4)}` : `$${dollars.toFixed(2)}`;
}

/**
 * A token count, abbreviated above a thousand.
 *
 * Abbreviated rather than grouped because a table cell has to stay one line, and `1.2M` is
 * what an operator scans for. Below a thousand the exact number is shown, because that is the
 * range where the exact number is the point.
 */
export function tokens(count: number): string {
  if (count < 1000) return String(count);
  if (count < 1_000_000) return `${Math.round(count / 1000)}k`;
  return `${(count / 1_000_000).toFixed(1)}M`;
}

/**
 * A duration in milliseconds as a short phrase.
 *
 * Two units and no more: a run is either under a minute or it is not, and a panel that says
 * "2m 14.3s" has spent a column on precision nobody acts on.
 */
export function duration(milliseconds: number): string {
  if (!milliseconds || milliseconds < 0) return "—";
  if (milliseconds < 1000) return `${Math.round(milliseconds)}ms`;
  const seconds = milliseconds / 1000;
  if (seconds < 60) return `${seconds.toFixed(seconds < 10 ? 1 : 0)}s`;
  return `${Math.floor(seconds / 60)}m ${Math.round(seconds % 60)}s`;
}

/**
 * How a stop reason reads in the panel.
 *
 * One table for every screen that shows the column, because the reason vocabulary grows and a
 * label map that lives in three files is three chances to forget the new one — and the failure
 * is silent: `REASON_LABEL[reason] ?? reason` falls back to the raw wire name, so a missing
 * entry produces a screen reading "output_schema" instead of a sentence, and nothing in the
 * UI looks broken.
 *
 * `output_schema` is the reason that most needs a sentence. It is not an error in the sense
 * "something went wrong": nothing broke, the model simply produced an answer that did not
 * match the shape the caller asked for, and the reader's next move is to look at the rule
 * rather than at the provider.
 */
export const REASON_LABEL: Record<string, string> = {
  final_answer: "Final answer",
  max_steps: "Max steps",
  deadline: "Deadline",
  token_budget: "Token budget",
  cancelled: "Cancelled",
  loop_detected: "Loop detected",
  error: "Error",
  output_schema: "Answer did not match the required shape",
};

/** A stop reason in words, falling back to the wire name only when it is genuinely unknown. */
export function reasonLabel(reason: string | null | undefined): string {
  if (!reason) return "—";
  return REASON_LABEL[reason] ?? reason;
}
