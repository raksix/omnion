/**
 * The typed client for the deprecation surface (REQ-130, slice 4).
 *
 * ## Why the policy travels with the list
 *
 * `policy` is on `GET /api/v1/api/deprecations` rather than being a constant in this file or in
 * the screen. The windows are enforced by the server in `crates/graphql/src/deprecation.rs`, and
 * a form that stated its own copy would let an operator pick a date it called valid and be
 * refused — which teaches them the form is decorative. Reading the numbers means the hint beside
 * the date field and the refusal that follows it can never disagree.
 *
 * ## The amber flag is the server's, not a recomputation
 *
 * `amber` arrives on the row because the threshold is the server's (`AMBER_WITHIN_DAYS`) and a
 * client-side comparison would drift the day someone tuned it. Same for `countdown` and
 * `days_remaining`: the screen renders words, and the words come from the same decision the
 * middleware makes when it decides a surface is gone.
 *
 * ## `status` is the POLICY's status, not the column's
 *
 * A row whose sunset passed a minute ago and which no sweeper has touched still reads `removed`
 * here, because the server reads the dates rather than trusting the column. The screen therefore
 * agrees with the middleware instead of lagging it by however long the poller interval is.
 */

import { request, ApiError } from "./api";

export { ApiError };

/** One deprecation, as the list and the detail render it. */
export type DeprecationRow = {
  id: string;
  /** `null` on an installation-wide row, which the screen renders as such. */
  organization_id: string | null;
  route_pattern: string | null;
  method: string | null;
  field_path: string | null;
  /** `route_pattern`, `field_path`, or "the whole installation" — never empty. */
  surface: string;
  deprecated_in: string;
  sunset_at: string;
  replacement: string | null;
  note: string;
  /**
   * `announced` · `active` · `removed` · `withdrawn`, and already adjusted for the dates, so the
   * screen does not compute a status the server would disagree with.
   */
  status: string;
  notified_at: string | null;
  /** Negative once the sunset has passed — a countdown, not a stale positive number. */
  days_remaining: number;
  /** The server's amber threshold, applied server-side. */
  amber: boolean;
  /** The words the screen shows: "19 days left", "sunsets today", "sunset passed". */
  countdown: string;
  /** The minimum window that applied, so the screen can say which rule refused a date. */
  minimum_window_months: number;
  window_name: string;
};

/** The windows in force, read from the crate's constants by the server. */
export type DeprecationPolicy = {
  public_months: number;
  developer_months: number;
  amber_within_days: number;
};

export type DeprecationList = {
  deprecations: DeprecationRow[];
  total: number;
  removed: number;
  policy: DeprecationPolicy;
};

/** The body `POST /api/v1/api/deprecations` takes. */
export type NewDeprecation = {
  route_pattern: string | null;
  method: string | null;
  field_path: string | null;
  deprecated_in: string;
  /** RFC 3339. */
  sunset_at: string;
  replacement: string | null;
  note: string;
};

export function fetchDeprecations(): Promise<DeprecationList> {
  return request<DeprecationList>("/api/v1/api/deprecations");
}

export function fetchDeprecation(id: string): Promise<DeprecationRow> {
  return request<DeprecationRow>(`/api/v1/api/deprecations/${encodeURIComponent(id)}`);
}

export function announceDeprecation(body: NewDeprecation): Promise<DeprecationRow> {
  return request<DeprecationRow>("/api/v1/api/deprecations", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/**
 * Move a sunset later.
 *
 * `reason` is required by the server, not by this signature: the type makes forgetting it
 * awkward, and the refusal message that reaches the operator when they skip the field is what
 * actually explains the rule.
 */
export function extendDeprecation(
  id: string,
  sunsetAt: string,
  reason: string,
): Promise<DeprecationRow> {
  return request<DeprecationRow>(`/api/v1/api/deprecations/${encodeURIComponent(id)}/extend`, {
    method: "POST",
    body: JSON.stringify({ sunset_at: sunsetAt, reason }),
  });
}

export function withdrawDeprecation(id: string, reason: string): Promise<DeprecationRow> {
  return request<DeprecationRow>(`/api/v1/api/deprecations/${encodeURIComponent(id)}/withdraw`, {
    method: "POST",
    body: JSON.stringify({ reason }),
  });
}

export function markDeprecationNotified(id: string): Promise<DeprecationRow> {
  return request<DeprecationRow>(`/api/v1/api/deprecations/${encodeURIComponent(id)}/notified`, {
    method: "POST",
  });
}

/**
 * The CSV export, built HERE rather than fetched.
 *
 * The request names "a CSV export for integrator notifications", and the rows are already in
 * memory — a second endpoint would be a second code path through the same data with its own
 * escaping, and the one place that escapes a value into a CSV is the one place a reviewer can
 * check. Every field is quoted, and a value containing a quote, a comma or a newline is escaped by
 * doubling the quote, which is the RFC 4180 rule and the one a spreadsheet actually implements.
 */
export function exportDeprecationsCsv(rows: DeprecationRow[]): void {
  const header = [
    "surface",
    "method",
    "deprecated_in",
    "sunset_at",
    "days_remaining",
    "status",
    "replacement",
    "note",
    "notified_at",
  ];
  const lines = [header, ...rows.map((row) => [
    row.surface,
    row.method ?? "",
    row.deprecated_in,
    row.sunset_at,
    String(row.days_remaining),
    row.status,
    row.replacement ?? "",
    row.note,
    row.notified_at ?? "",
  ])];

  const csv = lines
    .map((line) => line.map((cell) => `"${cell.replaceAll('"', '""')}"`).join(","))
    // The BOM: without it Excel reads a UTF-8 CSV as the local code page, and a note written in
    // anything outside ASCII arrives as mojibake in exactly the file an operator sends to
    // integrators.
    .join("\r\n");

  const url = URL.createObjectURL(new Blob(["\uFEFF", csv], { type: "text/csv;charset=utf-8" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = `omnion-deprecations-${new Date().toISOString().slice(0, 10)}.csv`;
  document.body.appendChild(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(url);
}