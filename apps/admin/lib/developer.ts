/**
 * The developer portal's response shapes (REQ-022, slice 2).
 *
 * ## The types here mirror the API's views, and one of them mirrors an absence
 *
 * `DeveloperKey` has no `token` field, and that is not an oversight in this file — it mirrors
 * `omnion_developer::model::KeyView`, which has none either, because the row type that *does*
 * carry `key_hash` is never serialized. The only shape with a `token` is `IssuedDeveloperKey`,
 * which only `POST /api-keys` and `POST /api-keys/{id}/rotate` answer. So "reveal once" is
 * enforced at both ends: the server has one response that can hold a secret, and the panel has
 * one type that expects it.
 *
 * ## The calls live in `api.ts`, not here
 *
 * Every other feature in the panel reaches the platform through a named export in `lib/api.ts`,
 * because that module owns the session cookie, the CSRF header and the `ApiError` shape. A
 * second file that called `fetch` itself would be a second implementation of all three — the
 * first version of this was written with its own `request` import and did not compile, which is
 * the cheapest possible reminder of why the convention exists.
 *
 * ## Nothing here persists a key
 *
 * The REQ's risks section says the explorer "must never persist a key value in local storage",
 * and the portal honours that: the token lives in React state, is copied on request, and is gone
 * when the dialog closes. There is no `localStorage` in the portal and no reason to add one.
 */

/** One row of the key list, and the detail header (`omnion_developer::model::KeyView`). */
export type DeveloperKey = {
  id: string;
  name: string;
  environment: string;
  /** `omndev_live_7f3a9c1d2e` — the lookup namespace. Never the token. */
  key_prefix: string;
  scopes: string[];
  created_by_name: string;
  created_at: string;
  last_used_at: string | null;
  expires_at: string | null;
  revoked_at: string | null;
  /** The key this one replaced, when this key came from a rotation. */
  rotated_from: string | null;
  status: "active" | "expired" | "revoked";
};

/**
 * The only shape that carries a token.
 *
 * Named so that a reader scanning `api.ts` for anywhere a key could be persisted finds one
 * type, and that type is never stored by anything in `features/developer/`.
 */
export type IssuedDeveloperKey = {
  key: DeveloperKey;
  token: string;
};

/** `GET /api/v1/developer/overview` — the card row. */
export type DeveloperOverview = {
  keys: { active: number; expired: number; revoked: number };
  requests_today: number;
  errors_today: number;
  recent_failures: DeveloperFailureLine[];
  /** The window the log honours, printed rather than buried. */
  log_retention_days: number;
};

/** One line of the overview's "what went wrong" list. */
export type DeveloperFailureLine = {
  id: number;
  created_at: string;
  method: string;
  path: string;
  status: number;
  key_prefix: string | null;
  permission: string | null;
};

/** One request in the log (`omnion_developer::logs::LogRow`). */
export type DeveloperLogRow = {
  id: number;
  organization_id: string | null;
  api_key_id: string | null;
  api_key_prefix: string | null;
  actor_user_id: string | null;
  actor_name: string;
  method: string;
  /** Already stripped of its query string by the recorder. */
  path: string;
  status: number;
  duration_ms: number;
  permission: string | null;
  client_fingerprint: string | null;
  created_at: string;
};

/** `GET /api/v1/developer/logs` — a page plus its cursor. */
export type DeveloperLogPage = {
  rows: DeveloperLogRow[];
  next_before: number | null;
};

/** `GET /api/v1/developer/logs/{id}` — one request with its resolved scope. */
export type DeveloperLogDetail = {
  row: DeveloperLogRow;
  status_class: string;
  retention_days: number;
};

/** `GET /api/v1/developer/api-keys/{id}` — the detail screen's whole payload. */
export type DeveloperKeyDetail = {
  key: DeveloperKey;
  usage: DeveloperUsagePoint[];
  log_retention_days: number;
};

/** One day of a key's usage rollup. */
export type DeveloperUsagePoint = {
  day: string;
  requests: number;
  errors: number;
  avg_duration_ms: number;
};

/** `GET /api/v1/developer/scopes` — the picker's source, grouped by category. */
export type DeveloperScopeCatalogue = {
  categories: {
    key: string;
    scopes: { key: string; description: string; grantable: boolean }[];
  }[];
  environments: string[];
};

/** What the create form submits. */
export type CreateDeveloperKeyInput = {
  name: string;
  scopes: string[];
  environment: string;
  /** ISO 8601, or `null` for "never". */
  expires_at: string | null;
};

/** The log screen's filters, exactly the ones the API accepts. */
export type DeveloperLogFilters = {
  api_key_id?: string | null;
  method?: string | null;
  path_prefix?: string | null;
  status_class?: string | null;
  window_days?: number | null;
};

/** The four classes the log screen filters by, in the order the toolbar shows them. */
export const DEVELOPER_STATUS_CLASSES = ["2xx", "3xx", "4xx", "5xx"] as const;

/** The methods the log screen offers. `""` is "every method". */
export const DEVELOPER_METHODS = ["", "GET", "POST", "PUT", "PATCH", "DELETE"] as const;

/** The windows the log screen offers, in days. `7` matches the API's own default. */
export const DEVELOPER_WINDOWS = [1, 7, 30, 90] as const;

/**
 * The CSV export of whatever the log screen is currently showing.
 *
 * Assembled in the browser from the rows already loaded, deliberately: the platform has no bulk
 * export endpoint for this table, because a debugging surface that can dump its whole retention
 * window in one call is a surface that can leak it. Every cell is quoted and internal quotes are
 * doubled — a path or an actor name may legitimately contain a comma, and a hand-rolled CSV that
 * breaks on one shifts every column after it, silently.
 */
export function developerLogsCsv(rows: DeveloperLogRow[]): string {
  const header = [
    "id",
    "time",
    "method",
    "path",
    "status",
    "duration_ms",
    "key",
    "actor",
    "permission",
    "client_fingerprint",
  ];
  const quote = (cell: string) => `"${cell.replace(/"/g, '""')}"`;
  const lines = rows.map((row) =>
    [
      String(row.id),
      row.created_at,
      row.method,
      row.path,
      String(row.status),
      String(row.duration_ms),
      row.api_key_prefix ?? "",
      row.actor_name,
      row.permission ?? "",
      row.client_fingerprint ?? "",
    ]
      .map(quote)
      .join(","),
  );
  return [header.map(quote).join(","), ...lines].join("\n");
}

/**
 * The field a create/rotate refusal points at, so the form can place its message.
 *
 * `null` when the API named no field — the caller then shows the message at the form's foot,
 * which is where a refusal that is not about one input belongs.
 */
export function developerFieldOf(details: unknown): string | null {
  if (details === null || typeof details !== "object") return null;
  const field = (details as { field?: unknown }).field;
  return typeof field === "string" ? field : null;
}
