/**
 * Typed client for the Omnion API.
 *
 * The browser always calls the API on the admin panel's own origin: `next.config.ts` forwards
 * `/api/*` to the API origin, so the HttpOnly session cookie is first-party everywhere.
 */
import type {
  SecurityBulkResult,
  SecurityFinding,
  SecurityFindingFilter,
  SecurityFindingPage,
  SecurityFindingStatus,
  SecurityImportReport,
  SecurityOverview,
  HealthOverview,
  HealthServiceDetail,
  HealthSamplePoint,
  HealthSummary,
  HealthPruneResult,
  HealthMetricRow,
  HealthMetricsReport,
  HealthRangeKey,
  HealthIncident,
  HealthIncidentPage,
  HealthIncidentAction,
  HealthMaintenanceWindow,
  HealthSettings,
  HealthThreshold,
  HeaderPolicyDocument,
  HeaderPolicySave,
  HeaderPolicySaved,
  CreateIpRuleInput,
  CreateIpRuleResult,
  IpRulesPage,
  SecretInventory,
  SecurityEventsFilter,
  SecurityEventsPage,
  IpTestResult,
  LockedAccountsPage,
  LockoutPolicy,
  RateLimitScope,
  RateLimitsDocument,
  RateLimitsSave,
  RateLimitsSaved,
  RateLimitTestRequest,
  RateLimitTestResponse,
  SignInProtectionDocument,
  SignInProtectionSave,
  SignInProtectionSaved,
  WebhookDeliveryFilters,
  WebhookDeliveryPage,
  WebhookEndpoint,
  WebhookList,
  WebhookRedeliverBatch,
  WebhookRotation,
  WebhookStats,
  WebhookTestReport,
  CreatedMediaShare,
  EventCatalogue,
  EventFilters,
  EventPage,
  RetentionStatus,
  SweepResult,
  NewMediaGrant,
  Media,
  MediaBulkResult,
  MediaCrossSiteReport,
  MediaDuplicateReport,
  MediaFile,
  MediaFilePage,
  MediaFilters,
  MediaFolder,
  MediaUploader,
  MediaFolderTree,
  MediaMergeResult,
  MediaPreset,
  MediaGrant,
  MediaGrantSubject,
  MediaGrantsResponse,
  MediaShare,
  MediaActivity,
  MediaUsage,
  MediaStorageProbe,
  MediaStorageSettings,
  MediaQuarantineList,
  MediaScanProbe,
  MediaScanRunList,
  MediaScanSettings,
  MediaScanSettingsInput,
  MediaSweepResult,
  MediaStorageSettingsInput,
  MediaReplaceResult,
  MediaRetentionList,
  MediaRetentionPolicy,
  MediaRetentionPolicyInput,
  MediaRetentionRepair,
  BackupCreateResult,
  BackupDetail,
  BackupList,
  BackupSchedule,
  BackupSettings,
  BackupStatus,
  BackupPurge,
  BackupPurgeFailure,
  BackupSweepReport,
  RestoreJob,
  RestoreOutcome,
  RestorePreview,
  BackupVerification,
  MediaRetentionRunList,
  MediaRetentionRunResult,
  MediaTrash,
  MediaVersionList,
  OnboardingStatus,
  Organization,
  OwnerSetupResult,
  Page,
  NotificationBulkResult,
  NotificationChannelReadiness,
  NotificationDevice,
  NotificationFilters,
  NotificationOutbox,
  NotificationPage,
  NotificationPreferences,
  NotificationPreferencesSaved,
  NotificationPushKey,
  NotificationPushOutcome,
  NotificationRouteReport,
  NotificationRouteRule,
  NotificationRow,
  NotificationSettingsRow,
  NotificationSummary,
  Site,
  User,
  AppBuilderArtifact,
  AppBuilderBlocker,
  AppBuilderBulkDelete,
  AppBuilderCounts,
  AppBuilderDecision,
  AppBuilderExample,
  AppBuilderFinding,
  AppBuilderPlan,
  AppBuilderPlanDecision,
  AppBuilderPlanDetail,
  AppBuilderPlanList,
  AppBuilderVocabulary,
} from "./types";

// The portal's own shapes live in their own module rather than in `types.ts`, because they come
// with a *rule* attached (only `IssuedDeveloperKey` may hold a token) that is worth reading
// next to the type itself rather than one of two hundred lines of a shared list.
import type {
  CreateDeveloperKeyInput,
  DeveloperKey,
  DeveloperKeyDetail,
  DeveloperLogDetail,
  DeveloperLogFilters,
  DeveloperLogPage,
  DeveloperOverview,
  DeveloperScopeCatalogue,
  IssuedDeveloperKey,
} from "./developer";

/** An error answered by the API, or raised before the request could leave the browser. */
export class ApiError extends Error {
  /** HTTP status; `0` when the API could not be reached at all. */
  readonly status: number;
  /** Stable machine-readable code from the API error body. */
  readonly code: string;
  /**
   * Structured detail the API attached to the refusal.
   *
   * A security-policy refusal names the `field` it refused, a step-up refusal names the
   * `action`; without this the panel could only print the sentence.
   */
  readonly details: Record<string, unknown> | null;
  /**
   * Seconds the API asked the caller to wait, from `Retry-After`.
   *
   * `null` on everything that is not a refusal with a window behind it, and that distinction is
   * the point: a screen that retried blindly would spin against the very limiter it exists to
   * diagnose, and an operator watching a page refresh into `429` learns less from the error than
   * from the number of seconds the platform is willing to wait.
   */
  readonly retryAfterSeconds: number | null;

  constructor(
    status: number,
    code: string,
    message: string,
    details: Record<string, unknown> | null = null,
    retryAfterSeconds: number | null = null,
  ) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
    this.details = details;
    this.retryAfterSeconds = retryAfterSeconds;
  }

  /** `true` when the session is missing or expired. */
  get isUnauthenticated(): boolean {
    return this.status === 401;
  }

  /**
   * `true` when the platform refused the request because a scope's budget is spent.
   *
   * A screen handles this differently from every other error: it says what was refused, by which
   * scope and when to try again — instead of a "something went wrong" banner that implies the
   * panel is broken when it is in fact doing exactly what it was configured to do.
   */
  get isRateLimited(): boolean {
    return this.status === 429;
  }
}

/**
 * Read the wait the API attached, or `null` when there is none.
 *
 * Parsed rather than trusted: a header the platform sent is still a string that came off a wire,
 * and `Number("soon")` is a number.
 */
function retryAfterOf(response: Response): number | null {
  const raw = response.headers.get("retry-after");
  if (!raw) return null;
  const seconds = Number(raw);
  return Number.isFinite(seconds) && seconds > 0 ? seconds : null;
}

type ErrorBody = {
  error?: {
    code?: string;
    message?: string;
    details?: Record<string, unknown>;
  };
};

/** The header the API reads the CSRF token back in, matching `CSRF_HEADER` on the server. */
const CSRF_COOKIE = "omnion_csrf";
const CSRF_HEADER = "x-omnion-csrf";
const SAFE_METHODS = new Set(["GET", "HEAD", "OPTIONS"]);

/** Read one cookie, tolerating the several-cookie headers and whitespace a browser may send. */
function readCookie(name: string): string | null {
  if (typeof document === "undefined") return null;
  for (const part of document.cookie.split(";")) {
    const separator = part.indexOf("=");
    if (separator === -1) continue;
    if (part.slice(0, separator).trim() !== name) continue;
    return decodeURIComponent(part.slice(separator + 1).trim());
  }
  return null;
}

/**
 * The `x-omnion-csrf` header a state-changing request needs, or nothing.
 *
 * The API refuses a cookie-authenticated mutation that carries no token (REQ-012, slice 2), and
 * the token arrives as a readable cookie at sign-in. So every mutating call has to echo it here —
 * **one** place, or a second screen that forgot would answer `403 csrf_failed` on a save that
 * works everywhere else, which is the hardest kind of bug to find from a user's report.
 *
 * Read-safe methods send nothing: the server never asks for one, and sending it anyway would put
 * a token in a request that has no need for it. When the cookie is absent (a server-rendered
 * first paint, a test double) the header is simply omitted and the server's own refusal — which
 * names the missing secret, or the missing token — is what the operator sees, rather than a
 * client-side guess about which of the two it is.
 */
function csrfHeader(init: RequestInit): Record<string, string> {
  const method = (init.method ?? "GET").toUpperCase();
  if (SAFE_METHODS.has(method)) return {};
  const token = readCookie(CSRF_COOKIE);
  return token ? { [CSRF_HEADER]: token } : {};
}

async function readJson(response: Response): Promise<unknown> {
  const text = await response.text();
  if (!text) {
    return null;
  }
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return null;
  }
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, {
      ...init,
      credentials: "same-origin",
      headers: {
        accept: "application/json",
        // Only a JSON body gets the JSON content type: an upload sends `FormData`, and the
        // browser has to set that content type itself — including its multipart boundary.
        ...(typeof init.body === "string" ? { "content-type": "application/json" } : {}),
        ...csrfHeader(init),
        ...init.headers,
      },
    });
  } catch {
    throw new ApiError(0, "network_error", "The Omnion API could not be reached.");
  }

  const payload = await readJson(response);

  if (!response.ok) {
    const body = (payload ?? {}) as ErrorBody;
    throw new ApiError(
      response.status,
      body.error?.code ?? "unknown_error",
      body.error?.message ?? `The API answered with status ${response.status}.`,
      body.error?.details ?? null,
      retryAfterOf(response),
    );
  }

  return payload as T;
}

/** What a password check answered: a session, or the second factor it still needs. */
export type LoginOutcome =
  | { status: "signed-in"; user: User }
  | { status: "mfa-required"; challenge: string; expiresInMinutes: number };

/** Sign in with email and password; an account with a confirmed factor answers a challenge. */
export async function login(email: string, password: string): Promise<LoginOutcome> {
  const body = await request<{
    user?: User;
    mfa_required?: boolean;
    challenge?: string;
    expires_in_minutes?: number;
  }>("/api/v1/auth/login", {
    method: "POST",
    body: JSON.stringify({ email, password }),
  });

  if (body.mfa_required && body.challenge) {
    return {
      status: "mfa-required",
      challenge: body.challenge,
      expiresInMinutes: body.expires_in_minutes ?? 5,
    };
  }
  if (!body.user) {
    throw new ApiError(502, "unexpected_response", "The API answered without an account.");
  }
  return { status: "signed-in", user: body.user };
}

/** End the session. Safe to call without one. */
export function logout(): Promise<null> {
  return request<null>("/api/v1/auth/logout", { method: "POST" });
}

/** The account behind the current session. */
export async function fetchMe(): Promise<User> {
  const body = await request<{ user: User }>("/api/v1/me");
  return body.user;
}

// ---------------------------------------------------------------------------------------------
// Onboarding (the first run, REQ-050)
// ---------------------------------------------------------------------------------------------

/** How far the first run of this installation has come. Open: it also answers before sign-in. */
export function fetchOnboarding(): Promise<OnboardingStatus> {
  return request<OnboardingStatus>("/api/v1/onboarding");
}

/** Create the owner account; the API answers with the account and signs it in. */
export async function createOwnerAccount(input: {
  displayName: string;
  email: string;
  password: string;
}): Promise<OwnerSetupResult> {
  return request<OwnerSetupResult>("/api/v1/onboarding/owner", {
    method: "POST",
    body: JSON.stringify({
      display_name: input.displayName,
      email: input.email,
      password: input.password,
    }),
  });
}

/** Create the first organization of the first run. */
export function createOnboardingOrganization(
  name: string,
  slug?: string,
): Promise<OnboardingStatus> {
  return request<OnboardingStatus>("/api/v1/onboarding/organization", {
    method: "POST",
    body: JSON.stringify({ name, slug: slug?.trim() ? slug.trim() : null }),
  });
}

/** Create the first site (and its domain, when one is given). */
export function createOnboardingSite(
  name: string,
  key?: string,
  domain?: string,
): Promise<OnboardingStatus> {
  return request<OnboardingStatus>("/api/v1/onboarding/site", {
    method: "POST",
    body: JSON.stringify({
      name,
      key: key?.trim() ? key.trim() : null,
      domain: domain?.trim() ? domain.trim() : null,
    }),
  });
}

/** Choose the theme the first site renders with. */
export function setOnboardingTheme(theme: string): Promise<OnboardingStatus> {
  return request<OnboardingStatus>("/api/v1/onboarding/theme", {
    method: "POST",
    body: JSON.stringify({ theme }),
  });
}

/** Record the AI step as skipped (provider connections arrive with the AI Hub). */
export function skipAiProvider(): Promise<OnboardingStatus> {
  return request<OnboardingStatus>("/api/v1/onboarding/ai-provider", {
    method: "POST",
    body: JSON.stringify({ provider: null }),
  });
}

/** Close the first run. */
export function completeOnboarding(): Promise<OnboardingStatus> {
  return request<OnboardingStatus>("/api/v1/onboarding/complete", {
    method: "POST",
    body: JSON.stringify({}),
  });
}

/** Change a site (name, status, theme) — `PATCH /api/v1/sites/{id}`. */
export function updateSite(
  siteId: string,
  changes: { name?: string; status?: string; theme?: string },
): Promise<Site> {
  return request<Site>(`/api/v1/sites/${encodeURIComponent(siteId)}`, {
    method: "PATCH",
    body: JSON.stringify(changes),
  });
}

/** The tenants the account may see. */
export async function fetchOrganizations(): Promise<Organization[]> {
  const body = await request<{ organizations: Organization[] }>("/api/v1/organizations");
  return body.organizations;
}

/** The sites the account may see, optionally narrowed to one tenant. */
export async function fetchSites(organizationId?: string): Promise<Site[]> {
  const query = organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
  const body = await request<{ sites: Site[] }>(`/api/v1/sites${query}`);
  return body.sites;
}

/** The pages of one site, in the API's own order, optionally narrowed to one lifecycle state. */
export async function fetchPages(siteId: string, status?: string): Promise<Page[]> {
  const query = new URLSearchParams({ site_id: siteId });
  if (status) {
    query.set("status", status);
  }
  const body = await request<{ pages: Page[] }>(`/api/v1/pages?${query.toString()}`);
  return body.pages;
}

/** Create a page together with its first, draft revision. */
export function createPage(input: {
  siteId: string;
  slug: string;
  title: string;
  body?: string;
  summary?: string;
}): Promise<Page> {
  return request<Page>("/api/v1/pages", {
    method: "POST",
    body: JSON.stringify({
      site_id: input.siteId,
      slug: input.slug,
      title: input.title,
      body: input.body ?? null,
      summary: input.summary ?? null,
    }),
  });
}

/** Edit a page: a content change appends the next draft revision, a slug rename does not. */
export function updatePage(
  pageId: string,
  changes: { slug?: string; title?: string; body?: string; summary?: string },
): Promise<Page> {
  const body: Record<string, unknown> = {};
  if (changes.slug !== undefined) body.slug = changes.slug;
  if (changes.title !== undefined) body.title = changes.title;
  if (changes.body !== undefined) body.body = changes.body;
  if (changes.summary !== undefined) body.summary = changes.summary;
  return request<Page>(`/api/v1/pages/${encodeURIComponent(pageId)}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

/** Publish the page's working draft — the revision visitors then see. */
export function publishPage(pageId: string): Promise<Page> {
  return request<Page>(`/api/v1/pages/${encodeURIComponent(pageId)}/publish`, {
    method: "POST",
  });
}

/** The media library of one site, newest first. */
export async function fetchMedia(siteId: string): Promise<Media[]> {
  const body = await request<{ media: Media[] }>(
    `/api/v1/media?site_id=${encodeURIComponent(siteId)}`,
  );
  return body.media;
}

/** Upload one file into a site's library. */
export function uploadMedia(siteId: string, file: File): Promise<Media> {
  const form = new FormData();
  form.append("file", file, file.name);
  return request<Media>(`/api/v1/media?site_id=${encodeURIComponent(siteId)}`, {
    method: "POST",
    body: form,
  });
}

/** Remove one file — its object and its row. */
export function deleteMedia(mediaId: string): Promise<null> {
  return request<null>(`/api/v1/media/${encodeURIComponent(mediaId)}`, { method: "DELETE" });
}

/**
 * Browser URL of one file's bytes, read with the session cookie.
 *
 * `preset` names a transformation (REQ-010 slice 3). Without it the URL serves the original
 * bytes exactly as before, so every existing caller keeps working; with it the answer is a
 * generated derivative, addressed by a hash of its inputs.
 */
export function mediaRawUrl(mediaId: string, preset?: string): string {
  const base = `/api/v1/media/${encodeURIComponent(mediaId)}/raw`;
  return preset ? `${base}?preset=${encodeURIComponent(preset)}` : base;
}

// ---------------------------------------------------------------------------------------------
// File manager (docs/requests/REQ-010, slice 1)
// ---------------------------------------------------------------------------------------------

/** Build a query string from the filters that are actually set. */
function mediaQuery(siteId: string, filters: MediaFilters = {}): string {
  const params = new URLSearchParams({ site_id: siteId });
  for (const [key, value] of Object.entries(filters)) {
    if (value === undefined || value === null || value === "" || value === false) {
      continue;
    }
    if (key === "created_after" || key === "created_before") {
      // The store's window is two instants, the toolbar's is two days, and the expansion happens
      // here and NOT in the API for one reason: `new Date("2026-10-03")` on a server is midnight
      // **UTC**, so a server-side expansion shifts the window by the server's offset from the
      // operator's. A file uploaded at 23:00 on the 3rd in Istanbul is outside a UTC-midnight
      // window, and the panel would report "nothing matches" about a file the operator can see.
      // The browser knows the operator's own day, so the browser draws the line.
      //
      // `created_before` is the start of the day AFTER the one picked, because the store's
      // clause is `created_at <` — sending the start of the picked day would exclude every file
      // uploaded after midnight on the day the operator chose, and the window's last day would
      // be silently empty.
      const instant =
        key === "created_after"
          ? startOfLocalDay(value as string)
          : startOfLocalDay(nextLocalDay(value as string));
      if (instant) {
        params.set(key, instant);
      }
      continue;
    }
    params.set(key, value === true ? "true" : String(value));
  }
  return params.toString();
}

/**
 * Split a `YYYY-MM-DD` string into numbers, or `null` when it is not a plain calendar day.
 *
 * The check exists so a malformed value cannot reach the query string as `Invalid Date` and come
 * back from the store as a `400` nobody can act on. A date input hands over nothing else, so
 * this is a guard against a hand-edited URL, not against the control.
 */
function calendarDay(day: string): [number, number, number] | null {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(day);
  if (!match) {
    return null;
  }
  return [Number(match[1]), Number(match[2]), Number(match[3])];
}

/**
 * The first instant of a `YYYY-MM-DD` day **in this browser's timezone**, as RFC 3339.
 *
 * Built from the parts rather than from `new Date("2026-10-03")`, because that form is specified
 * as UTC: an operator picking "3 October" means their own 3 October, and a UTC reading of the
 * same string is a different day for most of the planet.
 *
 * `null` for a value that is not a plain calendar day.
 */
function startOfLocalDay(day: string): string | null {
  const parts = calendarDay(day);
  if (!parts) {
    return null;
  }
  const [year, month, date] = parts;
  const moment = new Date(year, month - 1, date, 0, 0, 0, 0);
  return Number.isNaN(moment.getTime()) ? null : moment.toISOString();
}

/**
 * The `YYYY-MM-DD` day after the one given, in this browser's timezone.
 *
 * Round-tripped through the offset rather than built by hand from the parts: adding a day to
 * `31 December` has to be `1 January`, and `new Date(year, month - 1, 32)` does that while
 * hand-rolled arithmetic on the month does not.
 */
function nextLocalDay(day: string): string {
  const parts = calendarDay(day);
  if (!parts) {
    return day;
  }
  const [year, month, date] = parts;
  const moment = new Date(year, month - 1, date + 1, 0, 0, 0, 0);
  const shifted = new Date(moment.getTime() - moment.getTimezoneOffset() * 60_000);
  return shifted.toISOString().slice(0, 10);
}

/** The folder tree of a site, with the file count of every folder. */
export function fetchMediaFolders(siteId: string): Promise<MediaFolderTree> {
  return request<MediaFolderTree>(`/api/v1/media/folders?${mediaQuery(siteId)}`);
}

/** Create one folder under a parent (the root when `parentId` is omitted). */
export function createMediaFolder(
  siteId: string,
  name: string,
  parentId?: string,
): Promise<MediaFolder> {
  return request<MediaFolder>(`/api/v1/media/folders`, {
    method: "POST",
    body: JSON.stringify({ site_id: siteId, name, parent_id: parentId ?? null }),
  });
}

/** Rename, move, or both — omit a field to leave it as it is. */
export function moveMediaFolder(
  folderId: string,
  change: { name?: string; parent_id?: string | null },
): Promise<MediaFolder> {
  return request<MediaFolder>(`/api/v1/media/folders/${encodeURIComponent(folderId)}`, {
    method: "PATCH",
    body: JSON.stringify(change),
  });
}

/** Delete one empty folder; a folder that still holds something is refused. */
export function deleteMediaFolder(folderId: string): Promise<null> {
  return request<null>(`/api/v1/media/folders/${encodeURIComponent(folderId)}`, {
    method: "DELETE",
  });
}

/** One page of the browser listing. */
export function fetchMediaFiles(
  siteId: string,
  filters: MediaFilters = {},
): Promise<MediaFilePage> {
  return request<MediaFilePage>(`/api/v1/media/files?${mediaQuery(siteId, filters)}`);
}

/**
 * The accounts the uploader filter offers, most prolific first.
 *
 * Its own route rather than a field on the listing: a dropdown of candidates is a different
 * question from the rows that question selects, and a list response carrying both would make
 * every page of the library pay for a filter nobody touched. `media.read` is enough — it reads
 * the `created_by` of rows the caller may already list.
 */
export function fetchMediaUploaders(siteId: string): Promise<MediaUploader[]> {
  return request<{ site_id: string; uploaders: MediaUploader[] }>(
    `/api/v1/media/uploaders?site_id=${encodeURIComponent(siteId)}`,
  ).then((body) => body.uploaders);
}

/** Rename, move or edit the metadata of one file. */
export function updateMediaFile(
  fileId: string,
  patch: {
    filename?: string;
    alt_text?: string;
    caption?: string;
    description?: string;
    tags?: string[];
    /**
     * The whole custom pair set, sent as the editor holds it.
     *
     * `{}` clears the pairs; omitting the field leaves them alone. A *merge* would be the
     * friendlier shape and the wrong one here — a caller that does not know the current set
     * would drop every pair it did not send, which is how a licence number disappears during a
     * caption edit.
     */
    metadata?: Record<string, string>;
    folder_id?: string | null;
  },
): Promise<MediaFile> {
  return request<MediaFile>(`/api/v1/media/files/${encodeURIComponent(fileId)}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
}

/** Move one file to the trash. The bytes stay until it is purged. */
export function trashMediaFile(fileId: string): Promise<MediaFile> {
  return request<MediaFile>(`/api/v1/media/files/${encodeURIComponent(fileId)}`, {
    method: "DELETE",
  });
}

/** Bring one file back from the trash. */
export function restoreMediaFile(fileId: string): Promise<MediaFile> {
  return request<MediaFile>(`/api/v1/media/files/${encodeURIComponent(fileId)}/restore`, {
    method: "POST",
  });
}

/** Purge one file for good: the bytes and the row. */
export function purgeMediaFile(fileId: string): Promise<null> {
  return request<null>(`/api/v1/media/files/${encodeURIComponent(fileId)}/purge`, {
    method: "POST",
  });
}

/** The trash of a site, with the countdown on every row. */
export function fetchMediaTrash(siteId: string): Promise<MediaTrash> {
  return request<MediaTrash>(`/api/v1/media/trash?${mediaQuery(siteId)}`);
}

/** Purge every trashed file of a site. */
export function emptyMediaTrash(siteId: string): Promise<MediaBulkResult> {
  return request<MediaBulkResult>(`/api/v1/media/trash/empty?${mediaQuery(siteId)}`, {
    method: "POST",
  });
}

/** One bulk action over a selection. */
export function mediaBulkAction(
  siteId: string,
  action: "move" | "tag" | "delete" | "restore" | "purge",
  ids: string[],
  options: { folder_id?: string | null; tags?: string[] } = {},
): Promise<MediaBulkResult> {
  return request<MediaBulkResult>(`/api/v1/media/bulk`, {
    method: "POST",
    body: JSON.stringify({ site_id: siteId, action, ids, ...options }),
  });
}

/** One file of the library, with its folder and editorial fields. */
export function fetchMediaFile(mediaId: string): Promise<MediaFile> {
  return request<MediaFile>(`/api/v1/media/files/${encodeURIComponent(mediaId)}`);
}

/** The version history of one file. */
export function fetchMediaVersions(mediaId: string): Promise<MediaVersionList> {
  return request<MediaVersionList>(`/api/v1/media/${encodeURIComponent(mediaId)}/versions`);
}

/**
 * Replace the bytes of a file, keeping the old ones.
 *
 * The note rides as its own multipart part rather than in the query, so a note with a newline in
 * it cannot corrupt the URL — and the file name is deliberately *not* sent: a replace keeps the
 * name the library already shows, and renaming is a separate, auditable action.
 */
export function createMediaVersion(
  mediaId: string,
  file: File,
  note = "",
): Promise<MediaReplaceResult> {
  const form = new FormData();
  form.append("file", file);
  form.append("note", note);
  return request<MediaReplaceResult>(`/api/v1/media/${encodeURIComponent(mediaId)}/versions`, {
    method: "POST",
    body: form,
  });
}

/** Bring an old version back as the newest one. */
export function restoreMediaVersion(
  mediaId: string,
  version: number,
): Promise<MediaReplaceResult> {
  return request<MediaReplaceResult>(
    `/api/v1/media/${encodeURIComponent(mediaId)}/versions/${version}/restore`,
    { method: "POST" },
  );
}

/** Panel read path of one version's bytes. */
export function mediaVersionRawUrl(mediaId: string, version: number): string {
  return `/api/v1/media/${encodeURIComponent(mediaId)}/versions/${version}/raw`;
}

// ---------------------------------------------------------------------------------------------
// Usage and activity (docs/requests/REQ-010, slice 4)
// ---------------------------------------------------------------------------------------------

/**
 * Where a file is used.
 *
 * Both of these read a file that may already be in the trash: "what happened to this" is asked
 * precisely after the deletion, so the panel must be able to open the trail from the trash
 * screen and not answer a blank page to the one person who needs it.
 */
export function fetchMediaUsage(mediaId: string): Promise<MediaUsage> {
  return request<MediaUsage>(`/api/v1/media/${encodeURIComponent(mediaId)}/references`);
}

/** What has happened to a file, newest first. */
export function fetchMediaActivity(mediaId: string): Promise<MediaActivity> {
  return request<MediaActivity>(`/api/v1/media/${encodeURIComponent(mediaId)}/activity`);
}

// ---------------------------------------------------------------------------------------------
// Share links (docs/requests/REQ-010, slice 3)
// ---------------------------------------------------------------------------------------------

/** Every share over one file, newest first, including revoked ones. */
export function fetchMediaShares(mediaId: string): Promise<MediaShare[]> {
  return request<MediaShare[]>(`/api/v1/media/${encodeURIComponent(mediaId)}/shares`);
}

/**
 * Create a share link.
 *
 * `expiresInDays` omitted means "until revoked" and sends **no body at all** — the API accepts
 * an empty POST, and sending `{}` would only be a workaround for a rule that does not exist.
 */
export function createMediaShare(
  mediaId: string,
  options: { expiresInDays?: number; password?: string } = {},
): Promise<CreatedMediaShare> {
  const hasChoices = options.expiresInDays !== undefined || options.password !== undefined;
  return request<CreatedMediaShare>(`/api/v1/media/${encodeURIComponent(mediaId)}/shares`, {
    method: "POST",
    ...(hasChoices
      ? {
          body: JSON.stringify({
            ...(options.expiresInDays !== undefined
              ? { expires_in_days: options.expiresInDays }
              : {}),
            ...(options.password !== undefined ? { password: options.password } : {}),
          }),
        }
      : {}),
  });
}

/** Revoke one link. Immediate: the next request against the token is refused. */
export function revokeMediaShare(mediaId: string, shareId: string, reason = ""): Promise<void> {
  return request<void>(
    `/api/v1/media/${encodeURIComponent(mediaId)}/shares/${encodeURIComponent(shareId)}`,
    { method: "DELETE", ...(reason ? { body: JSON.stringify({ reason }) } : {}) },
  );
}

/** Revoke every live link over a file — for when the file itself stops being servable. */
export function revokeAllMediaShares(mediaId: string): Promise<{ revoked: number }> {
  return request<{ revoked: number }>(
    `/api/v1/media/${encodeURIComponent(mediaId)}/shares/revoke-all`,
    { method: "POST" },
  );
}

// --------------------------------------------------------------------------------------------
// Folder and file grants (docs/requests/REQ-010, slice 4)
// --------------------------------------------------------------------------------------------

/** The grants on one node — a folder or a file — with the chain a file inherits from. */
export function fetchMediaGrants(
  targetKind: "file" | "folder",
  targetId: string,
): Promise<MediaGrantsResponse> {
  const path = targetKind === "folder" ? "folders" : "media";
  return request<MediaGrantsResponse>(
    `/api/v1/${path}/${encodeURIComponent(targetId)}/grants`,
  );
}

/** What a grant is written with. The four bits are separate, so a caller that sends only
 * `can_read` gets exactly `can_read`. */
export function createMediaGrant(
  targetKind: "file" | "folder",
  targetId: string,
  grant: NewMediaGrant,
): Promise<MediaGrant> {
  const path = targetKind === "folder" ? "folders" : "media";
  return request<MediaGrant>(`/api/v1/${path}/${encodeURIComponent(targetId)}/grants`, {
    method: "PUT",
    body: JSON.stringify(grant),
  });
}

/**
 * Remove one grant, by its own id.
 *
 * The node is deliberately **not** in the URL: the row already knows what it was written on,
 * and a path with two parameters and a one-parameter handler is the shape axum rejects with a
 * bare `500` and no body. The server answers `404` for a grant of another organization, so an
 * id that is not yours is indistinguishable from one that does not exist.
 */
export function removeMediaGrant(grantId: string): Promise<void> {
  return request<void>(`/api/v1/media/grants/${encodeURIComponent(grantId)}`, {
    method: "DELETE",
  });
}

/** The subjects this site's organization can name: its own users, groups and roles. */
export function fetchGrantSubjects(siteId: string, search = ""): Promise<MediaGrantSubject[]> {
  const query = new URLSearchParams({ site_id: siteId });
  if (search) {
    query.set("search", search);
  }
  return request<MediaGrantSubject[]>(`/api/v1/media/grant-subjects?${query.toString()}`);
}

// --------------------------------------------------------------------------------------------
// Duplicate detection and merge (docs/requests/REQ-010, slice 3)
// --------------------------------------------------------------------------------------------

/**
 * The duplicate report of one site.
 *
 * `expand` asks for each group's members. It is off by default because a library with four
 * hundred duplicate pairs would otherwise answer a report nobody scrolls with four thousand rows.
 */
export function fetchMediaDuplicates(
  siteId: string,
  options: { expand?: boolean } = {},
): Promise<MediaDuplicateReport> {
  const params = new URLSearchParams({ site_id: siteId });
  if (options.expand) {
    params.set("expand", "1");
  }
  return request<MediaDuplicateReport>(`/api/v1/media/duplicates?${params}`);
}

/**
 * The installation-wide report, for a platform account.
 *
 * A *separate* function rather than an option on the above: the two reports have different
 * shapes and different affordances, and a caller that got one when it asked for the other would
 * find a `Merge group` button on a row that cannot be merged.
 */
export function fetchCrossSiteDuplicates(siteIds: string[]): Promise<MediaCrossSiteReport> {
  const params = new URLSearchParams({ sites: siteIds.join(",") });
  return request<MediaCrossSiteReport>(`/api/v1/media/duplicates?${params}`);
}

/**
 * Merge a duplicate group down to one file.
 *
 * `keep` is required and comes from the operator's own choice. There is no default and no
 * "suggested" value, because a merge that picked for itself breaks a live page and the operator
 * finds out from a 404 rather than from this report.
 */
export function mergeMediaDuplicates(input: {
  siteId: string;
  /** The **full** checksum from the report. */
  checksum: string;
  keep: string;
}): Promise<MediaMergeResult> {
  return request<MediaMergeResult>("/api/v1/media/duplicates/merge", {
    method: "POST",
    body: JSON.stringify({
      site_id: input.siteId,
      checksum: input.checksum,
      keep: input.keep,
    }),
  });
}

// ---------------------------------------------------------------------------------------------
// Search (docs/requests/REQ-002)
// ---------------------------------------------------------------------------------------------

/** One hit of a search answer (`GET /api/v1/search`). */
export type SearchHit = {
  /** Provider key (`pages`). */
  provider: string;
  /** Document type (`page`). */
  entity_type: string;
  /** Entity id inside its own domain. */
  entity_id: string;
  /** Title. */
  title: string;
  /** Supporting line — for pages and media, the site and address of the hit. */
  subtitle: string;
  /** Panel route a click opens. */
  url: string;
  /** Display name of the account the entity belongs to, when it has one. */
  owner: string | null;
  /** Tags stored with the document (`draft`, `published`, …). */
  tags: string[];
  /** When the entity last changed, RFC 3339. */
  updated_at: string | null;
  /** Rank inside this answer. */
  score: number;
};

/** One provider's share of a result set — the palette's section counts. */
export type SearchCount = {
  provider: string;
  title: string;
  route: string;
  count: number;
};

/** One value of the facet rail. */
export type SearchFacetValue = {
  value: string;
  label: string;
  count: number;
};

/** One group of the facet rail. */
export type SearchFacetGroup = {
  key: string;
  title: string;
  values: SearchFacetValue[];
  more: number;
};

/** The filters the results screen applies through the URL. */
export type SearchFilterInput = {
  /** Comma-separated provider keys. */
  type?: string;
  /** A site id, key or domain host. */
  site?: string;
  /** `me` or an account id. */
  owner?: string;
  /** A language code. */
  language?: string;
  /** One status tag. */
  status?: string;
  /** `today`, `week`, `month`, `older` or `never`. */
  updated?: string;
  /** Exclusive upper bound (`YYYY-MM-DD`). */
  before?: string;
  /** Inclusive lower bound (`YYYY-MM-DD`). */
  after?: string;
};

/** The whole answer of one search. */
export type SearchResult = {
  /** The query as the API understood it (trimmed and capped). */
  query: string;
  /** The text terms it parsed. */
  terms: string[];
  /** The `type:` filters it parsed. */
  types: string[];
  /** The `is:` flags it parsed. */
  flags: string[];
  /** Everything the parser refused, in caller-facing language. */
  hints: string[];
  /** This page's hits, best first. */
  hits: SearchHit[];
  /** Total hits the query matches. */
  total: number;
  /** One-based page number. */
  page: number;
  /** Hits per page. */
  per_page: number;
  /** Which provider contributed how many, ordered by count. */
  counts: SearchCount[];
  /**
   * How many documents the query matches outside this account's own read scope.
   *
   * A count and nothing else: it is how "nothing matched" and "nothing you may read matched"
   * are told apart. `0` when the account covers every provider.
   */
  hidden_total: number;
  /** The facet rail's counts; present when `facets=true` was asked for. */
  facets?: SearchFacetGroup[];
  /** How long the search took, in milliseconds. */
  took_ms: number;
};

/** Sort orders the search endpoint accepts. */
export type SearchSort = "relevance" | "newest" | "title";

/** One title-prefix suggestion (`GET /api/v1/search/suggest`). */
export type SearchSuggestion = {
  title: string;
  url: string;
  provider: string;
};

/** The search parameters a request or an export URL carries. */
type SearchQueryInput = {
  q: string;
  page?: number;
  per_page?: number;
  sort?: SearchSort;
  /** Ask the answer for the facet rail's counts. */
  facets?: boolean;
  /** The filters the rail applies. */
  filters?: SearchFilterInput;
  /**
   * `false` keeps the call out of the account's own search history.
   *
   * The palette asks once per group per keystroke; only a search someone committed to — opened a
   * hit, ran it on the results screen — belongs in `search_recent`.
   */
  history?: boolean;
};

/** Build the query string of a search or export call. */
function searchParams(input: SearchQueryInput): URLSearchParams {
  const params = new URLSearchParams({ q: input.q });
  if (input.page) params.set("page", String(input.page));
  if (input.per_page) params.set("per_page", String(input.per_page));
  if (input.sort) params.set("sort", input.sort);
  if (input.facets) params.set("facets", "true");
  if (input.history === false) params.set("history", "false");
  const filters = input.filters ?? {};
  if (filters.type) params.set("types", filters.type);
  if (filters.site) params.set("site_id", filters.site);
  if (filters.owner) params.set("owner", filters.owner);
  if (filters.language) params.set("language", filters.language);
  if (filters.status) params.set("status", filters.status);
  if (filters.updated) params.set("updated", filters.updated);
  if (filters.before) params.set("before", filters.before);
  if (filters.after) params.set("after", filters.after);
  return params;
}

/**
 * Search every provider the account's own read permissions cover.
 *
 * The scoped syntax travels inside `q` (`type:page site:acme is:draft`), exactly as the box
 * accepts it — the results screen passes what the account typed instead of re-encoding it. The
 * rail's own filters travel as their own parameters, so a result set stays a URL.
 */
export function searchAll(input: SearchQueryInput): Promise<SearchResult> {
  return request<SearchResult>(`/api/v1/search?${searchParams(input).toString()}`);
}

/** One provider's line on the search settings screen. */
export type SearchProviderStatus = {
  provider: string;
  title: string;
  documents: number;
  last_indexed_at: string | null;
  /** `indexing`, `failed`, `stale`, `ready` or `empty`. */
  state: string;
  last_run: {
    indexed: number | null;
    pruned: number | null;
    duration_ms: number | null;
    started_at: string;
    finished_at: string | null;
    error: string | null;
  } | null;
};

/** The index's per-provider health. */
export function fetchSearchStatus(): Promise<{
  providers: SearchProviderStatus[];
  documents: number;
}> {
  return request<{ providers: SearchProviderStatus[]; documents: number }>(
    "/api/v1/search/status",
  );
}

/** Rebuild one provider's index, or every one of them. */
export function reindexSearch(provider?: string): Promise<{
  providers: { provider: string; indexed: number; pruned: number; duration_ms: number }[];
}> {
  return request(`/api/v1/search/reindex`, {
    method: "POST",
    body: JSON.stringify(provider ? { provider } : {}),
  });
}

/** The ranking weights, as the settings form writes them. */
export type SearchWeights = {
  title: number;
  tags: number;
  subtitle: number;
  body: number;
};

/** The installation's search settings. */
export type SearchSettings = {
  weights: SearchWeights;
  /** The weights an installation starts with — the server's own answer to "restore defaults". */
  defaults: SearchWeights;
  enabled_providers: string[];
  available_providers: string[];
  updated_at: string | null;
};

/** Read the search settings. */
export function fetchSearchSettings(): Promise<SearchSettings> {
  return request<SearchSettings>("/api/v1/search/settings");
}

/** Save the search settings (needs `search.manage`). */
export function saveSearchSettings(input: {
  weights: SearchWeights;
  enabled_providers: string[];
}): Promise<SearchSettings> {
  return request<SearchSettings>("/api/v1/search/settings", {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

/**
 * Download the current result set (or a selection of it) as CSV.
 *
 * The file comes from the API with the same filters the screen shows, so its rows and the
 * table's rows are the same answer; `rows` is the count the server reported in its own header.
 */
export async function downloadSearchExport(input: {
  q: string;
  filters?: SearchFilterInput;
  selected?: string[];
}): Promise<{ rows: number; truncated: boolean; blob: Blob; filename: string }> {
  const params = searchParams(input);
  if (input.selected && input.selected.length > 0) {
    params.set("selected", input.selected.join(","));
  }
  let response: Response;
  try {
    response = await fetch(`/api/v1/search/export?${params.toString()}`, {
      credentials: "same-origin",
      headers: { accept: "text/csv" },
    });
  } catch {
    throw new ApiError(0, "network_error", "The Omnion API could not be reached.");
  }
  if (!response.ok) {
    const text = await response.text();
    let code = "export_failed";
    let message = `The export answered with status ${response.status}.`;
    try {
      const body = JSON.parse(text) as ErrorBody;
      code = body.error?.code ?? code;
      message = body.error?.message ?? message;
    } catch {
      // A non-JSON error body is still an error; the status stays in the message.
    }
    throw new ApiError(response.status, code, message);
  }
  const rows = Number(response.headers.get("x-export-rows") ?? "0");
  const truncated = response.headers.get("x-export-truncated") === "true";
  const disposition = response.headers.get("content-disposition") ?? "";
  const match = /filename="?([^";]+)"?/.exec(disposition);
  return {
    rows,
    truncated,
    blob: await response.blob(),
    filename: match?.[1] ?? "omnion-search.csv",
  };
}

/** Title-only prefix suggestions, at most eight. */
export async function suggestTitles(q: string): Promise<SearchSuggestion[]> {
  const body = await request<{ suggestions: SearchSuggestion[] }>(
    `/api/v1/search/suggest?q=${encodeURIComponent(q)}`,
  );
  return body.suggestions;
}

/** The account's own recent searches, newest first. */
export async function fetchRecentSearches(): Promise<string[]> {
  const body = await request<{ queries: string[] }>("/api/v1/search/recent");
  return body.queries;
}

/** Forget every recent search of this account. */
export function clearRecentSearches(): Promise<null> {
  return request<null>("/api/v1/search/recent", { method: "DELETE" });
}

// ---------------------------------------------------------------------------------------------
// Command centre (docs/requests/REQ-032)
// ---------------------------------------------------------------------------------------------

/**
 * One command the palette may offer. The list arrives already projected through the caller's
 * effective permissions, so the panel never has to decide what an account may run.
 *
 * `kind` says what running it does: `navigate` opens `route`; `action` posts to the run endpoint
 * and acts through its owning service, with `confirm` telling the palette to show the question
 * first (the API refuses an unconfirmed run either way).
 */
export type CommandInfo = {
  id: string;
  title: string;
  group: string;
  hint: string;
  icon: string;
  kind: "navigate" | "action";
  confirm: boolean;
  /** A navigation command's destination, or the screen that reads an action's record back. */
  route: string;
  keywords: string[];
  aliases: string[];
  permission: string | null;
};

/** One row of the account's own palette history — a search or a command. */
export type CommandRecent = {
  kind: "query" | "command";
  query: string | null;
  command_id: string | null;
  /** A command recent carries its current title and route, so the row renders without a lookup. */
  title: string | null;
  route: string | null;
  result_count: number | null;
  created_at: string;
};

/** The commands this account may run, in registry order. */
export async function fetchCommands(): Promise<CommandInfo[]> {
  const body = await request<{ commands: CommandInfo[] }>("/api/v1/commands");
  return body.commands;
}

/** The commands worth suggesting on the route the caller is on. */
export async function fetchCommandContext(route: string): Promise<CommandInfo[]> {
  const body = await request<{ commands: CommandInfo[] }>(
    `/api/v1/command-center/context?route=${encodeURIComponent(route)}`,
  );
  return body.commands;
}

/** The account's own recent searches and commands, newest first. */
export async function fetchCommandRecents(): Promise<CommandRecent[]> {
  const body = await request<{ items: CommandRecent[] }>("/api/v1/command-center/recent");
  return body.items;
}

/** Remember one search or one command for this account; repeating one moves it up. */
export function recordCommandRecent(
  input:
    | { kind: "query"; query: string; result_count?: number }
    | { kind: "command"; command_id: string },
): Promise<null> {
  return request<null>("/api/v1/command-center/recent", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Forget everything this account did in the palette. */
export function clearCommandRecents(): Promise<null> {
  return request<null>("/api/v1/command-center/recent", { method: "DELETE" });
}

/** One action command's outcome: the owning service's own result plus the palette's own line. */
export type CommandRunResult = {
  command: string;
  kind: "action";
  outcome: "ok";
  message: string;
  result: unknown;
};

/**
 * Run one action command through its owning service.
 *
 * `confirm` is the caller's yes: the API refuses a command that asks before it runs until the
 * request carries it, so the confirmation the palette shows and the rule the API enforces are the
 * same fact.
 */
export function runCommand(commandId: string, confirm = true): Promise<CommandRunResult> {
  return request<CommandRunResult>(`/api/v1/commands/${encodeURIComponent(commandId)}/run`, {
    method: "POST",
    body: JSON.stringify({ confirm }),
  });
}

// ---------------------------------------------------------------------------------------------
// Natural-language resolution (REQ-032, slice 4)
// ---------------------------------------------------------------------------------------------

/** One filter an interpretation carried, in the words the card prints. */
export type IntentFilter = {
  key: string;
  label: string;
  value: string;
};

/** What the resolver understood, in the platform's own vocabulary. */
export type ResolvedIntent = {
  /** `search`, `command` or `unclear`. */
  kind: "search" | "command" | "unclear";
  /** The domain word the reader used, if any. */
  entity: string | null;
  /** The provider the domain maps to, when the index answers for it. */
  provider: string | null;
  /** The registry id a command interpretation named. */
  command_id: string | null;
  /** The words a search would carry. */
  query: string;
  filters: IntentFilter[];
  sort: "newest" | "oldest" | "title" | null;
  limit: number | null;
  confidence: number;
};

/** One thing the words could have meant instead — always a screen, never an action. */
export type ResolveAlternative = {
  label: string;
  kind: "search" | "command";
  route: string;
  provider: string | null;
  command_id: string | null;
  confidence: number;
};

/**
 * One interpretation of a typed phrase.
 *
 * `runnable` is the server's own answer to "may this be run", permission rules included; `route`
 * is where a runnable reading lands. `source` and `degraded` say where the reading came from, so
 * the card never dresses a local reading up as a model's.
 */
export type Resolution = {
  query: string;
  source: "local" | "model";
  degraded: boolean;
  note: string | null;
  preview_text: string;
  runnable: boolean;
  route: string | null;
  /** Where `Edit as search` goes — the words and filters, runnable or not. */
  search_route: string | null;
  confidence: number;
  intent: ResolvedIntent;
  alternatives: ResolveAlternative[];
  model: string | null;
};

/**
 * Ask the platform what one phrase means.
 *
 * Nothing executes here: the answer is an interpretation plus the ways to act on it. `signal`
 * aborts a request a newer keystroke has overtaken, so a slow reading never replaces a fresh one.
 */
export function resolveCommandQuery(q: string, signal?: AbortSignal): Promise<Resolution> {
  return request<Resolution>("/api/v1/command-center/resolve", {
    method: "POST",
    body: JSON.stringify({ q }),
    ...(signal ? { signal } : {}),
  });
}

// ---------------------------------------------------------------------------------------------
// AI Hub (docs/06-AI-HUB.md, P11)
// ---------------------------------------------------------------------------------------------

/** One connected AI provider. The key itself is never part of this shape. */
export type AiProvider = {
  id: string;
  name: string;
  protocol: string;
  base_url: string;
  has_api_key: boolean;
  enabled: boolean;
  is_default: boolean;
  model_count: number;
  created_at: string;
  updated_at: string;
};

/** One model of the registry, with the provider it belongs to. */
export type AiModel = {
  id: string;
  provider_id: string;
  provider_name: string;
  model_key: string;
  display_name: string;
  context_window: number | null;
  supports_tools: boolean;
  supports_vision: boolean;
  supports_streaming: boolean;
  supports_embeddings: boolean;
  enabled: boolean;
  is_default: boolean;
  model_id: string;
  created_at: string;
  updated_at: string;
};

/** A model to register on a provider. */
export type AiModelInput = {
  key: string;
  display_name?: string;
  context_window?: number;
  supports_tools?: boolean;
  supports_vision?: boolean;
  supports_streaming?: boolean;
  supports_embeddings?: boolean;
};

/** One message of a chat request. */
export type ChatMessageInput = {
  role: "system" | "user" | "assistant";
  content: string;
};

/** What a finished chat stream reported. */
export type ChatDone = {
  finish_reason: string | null;
  chars: number;
  usage: {
    prompt_tokens: number | null;
    completion_tokens: number | null;
    total_tokens: number | null;
  } | null;
};

/** The providers this installation connected. */
export async function fetchAiProviders(): Promise<AiProvider[]> {
  const body = await request<{ providers: AiProvider[] }>("/api/v1/ai/providers");
  return body.providers;
}

/** The model registry, across every provider. */
export async function fetchAiModels(): Promise<AiModel[]> {
  const body = await request<{ models: AiModel[] }>("/api/v1/ai/models");
  return body.models;
}

/** Connect a provider, optionally with the models it serves. */
export function connectAiProvider(input: {
  name: string;
  baseUrl: string;
  apiKey?: string;
  enabled?: boolean;
  isDefault?: boolean;
  models?: AiModelInput[];
}): Promise<AiProvider> {
  return request<AiProvider>("/api/v1/ai/providers", {
    method: "POST",
    body: JSON.stringify({
      name: input.name,
      base_url: input.baseUrl,
      api_key: input.apiKey && input.apiKey.trim() ? input.apiKey.trim() : null,
      enabled: input.enabled ?? true,
      is_default: input.isDefault ?? false,
      models: input.models ?? [],
    }),
  });
}

/** Change a provider. `apiKey: null` forgets the stored key, `undefined` keeps it. */
export function updateAiProvider(
  providerId: string,
  changes: {
    name?: string;
    baseUrl?: string;
    apiKey?: string | null;
    enabled?: boolean;
    isDefault?: boolean;
  },
): Promise<AiProvider> {
  const body: Record<string, unknown> = {};
  if (changes.name !== undefined) body.name = changes.name;
  if (changes.baseUrl !== undefined) body.base_url = changes.baseUrl;
  if (changes.apiKey !== undefined) body.api_key = changes.apiKey;
  if (changes.enabled !== undefined) body.enabled = changes.enabled;
  if (changes.isDefault !== undefined) body.is_default = changes.isDefault;
  return request<AiProvider>(`/api/v1/ai/providers/${encodeURIComponent(providerId)}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

/** Remove a provider and every model it serves. */
export function removeAiProvider(providerId: string): Promise<null> {
  return request<null>(`/api/v1/ai/providers/${encodeURIComponent(providerId)}`, {
    method: "DELETE",
  });
}

/** Replace the set of models one provider serves. */
export async function replaceAiProviderModels(
  providerId: string,
  models: AiModelInput[],
): Promise<AiModel[]> {
  const body = await request<{ models: AiModel[] }>(
    `/api/v1/ai/providers/${encodeURIComponent(providerId)}/models`,
    { method: "PUT", body: JSON.stringify({ models }) },
  );
  return body.models;
}

/** Ask a provider which models it serves. */
export function discoverAiProviderModels(
  providerId: string,
): Promise<{ provider_id: string; provider_name: string; models: string[] }> {
  return request(`/api/v1/ai/providers/${encodeURIComponent(providerId)}/discover-models`, {
    method: "POST",
    body: JSON.stringify({}),
  });
}

/** Switch a model on or off, or make it the installation's default. */
export function updateAiModel(
  modelId: string,
  changes: { enabled?: boolean; isDefault?: boolean },
): Promise<AiModel> {
  const body: Record<string, unknown> = {};
  if (changes.enabled !== undefined) body.enabled = changes.enabled;
  if (changes.isDefault !== undefined) body.is_default = changes.isDefault;
  return request<AiModel>(`/api/v1/ai/models/${encodeURIComponent(modelId)}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

/**
 * Run a chat and stream the answer back.
 *
 * The API answers as `text/event-stream`: `start` (which provider and model the router chose),
 * `delta` frames with the answer as it arrives, then `done` — or `error` with a stable code. A
 * provider that refuses after the stream opened arrives as that `error` frame, which this helper
 * raises as an `ApiError` so the caller handles one shape either way.
 */
export async function streamChat(
  input: { model?: string; messages: ChatMessageInput[] },
  handlers: {
    onStart?: (info: { provider: string; model: string; protocol: string }) => void;
    onDelta?: (content: string) => void;
    onDone?: (done: ChatDone) => void;
  } = {},
): Promise<void> {
  const response = await fetch("/api/v1/ai/chat", {
    method: "POST",
    credentials: "same-origin",
    headers: { "content-type": "application/json", accept: "text/event-stream" },
    body: JSON.stringify({
      model: input.model && input.model.trim() ? input.model.trim() : null,
      messages: input.messages,
    }),
  });

  if (!response.ok || !response.body) {
    const payload = (await readJson(response)) as ErrorBody | null;
    throw new ApiError(
      response.status,
      payload?.error?.code ?? "unknown_error",
      payload?.error?.message ?? `The API answered with status ${response.status}.`,
    );
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  for (;;) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    buffer += decoder.decode(value, { stream: true });

    let boundary = buffer.indexOf("\n\n");
    while (boundary >= 0) {
      const frame = buffer.slice(0, boundary);
      buffer = buffer.slice(boundary + 2);
      boundary = buffer.indexOf("\n\n");

      let event = "message";
      let data = "";
      for (const line of frame.split("\n")) {
        if (line.startsWith("event: ")) {
          event = line.slice(7).trim();
        } else if (line.startsWith("data: ")) {
          data += line.slice(6);
        }
      }
      if (!data) {
        continue;
      }

      const payload = JSON.parse(data) as Record<string, unknown>;
      if (event === "start") {
        handlers.onStart?.(payload as { provider: string; model: string; protocol: string });
      } else if (event === "delta") {
        handlers.onDelta?.(String(payload.content ?? ""));
      } else if (event === "done") {
        handlers.onDone?.(payload as unknown as ChatDone);
      } else if (event === "error") {
        throw new ApiError(
          502,
          String(payload.code ?? "provider_error"),
          String(payload.message ?? "The AI provider failed."),
        );
      }
    }
  }
}

// ---------------------------------------------------------------------------------------------
// Analytics (docs/requests/REQ-007): the reports behind the /analytics screens
// ---------------------------------------------------------------------------------------------

/** One report parameter, as the screens write it into the URL. */
export type AnalyticsQuery = Record<string, string | number | boolean | undefined | null>;

/** One point of a series. */
export type AnalyticsSeriesPoint = {
  bucket: string;
  label: string;
  visitors: number;
  pageviews: number;
  previous_visitors?: number;
  previous_pageviews?: number;
};

/** One headline number and, when comparing, the same number one period earlier. */
export type AnalyticsMetric = { value: number; previous: number | null };

/** One ranked value of a dimension. */
export type AnalyticsDimensionRow = {
  value: string;
  visitors: number | null;
  views: number | null;
};

/** The overview report. */
export type AnalyticsOverview = {
  range: { from: string; to: string };
  previous_range: { from: string; to: string };
  compare: boolean;
  exact: boolean;
  previous_has_data: boolean;
  granularity: "hour" | "day";
  kpis: {
    visitors: AnalyticsMetric;
    pageviews: AnalyticsMetric;
    conversions: AnalyticsMetric;
    forms: AnalyticsMetric;
    downloads: AnalyticsMetric;
  };
  series: AnalyticsSeriesPoint[];
  top_pages: AnalyticsDimensionRow[];
  top_sources: AnalyticsDimensionRow[];
  devices: AnalyticsDimensionRow[];
};

/** The filters a report echoes back. */
export type AnalyticsFilterEcho = {
  path?: string;
  title?: string;
  device?: string;
  country?: string;
  source?: string;
};

/** One row of the page report. */
export type AnalyticsPageRow = {
  path: string;
  title: string | null;
  views: number;
  visitors: number;
  views_per_visitor: number | null;
  avg_time_ms: number | null;
  bounce_rate: number | null;
  entrances: number;
  exits: number;
};

/** The page report. */
export type AnalyticsPagesReport = {
  range: { from: string; to: string };
  filters: AnalyticsFilterEcho;
  sort: string;
  direction: "asc" | "desc";
  page: number;
  per_page: number;
  total: number;
  rows: AnalyticsPageRow[];
};

/** One row of the sources report. */
export type AnalyticsSourceRow = {
  source: string;
  medium: string | null;
  campaign: string | null;
  term: string | null;
  content: string | null;
  visits: number;
  visitors: number;
  conversions: number;
  conversion_rate: number | null;
};

/** The sources report. */
export type AnalyticsSourcesReport = {
  range: { from: string; to: string };
  group: string;
  rows: AnalyticsSourceRow[];
};

/** One bar panel of the audience report. */
export type AnalyticsDimensionPanel = {
  kind: string;
  title: string;
  rows: AnalyticsDimensionRow[];
};

/** One country of the audience report. */
export type AnalyticsCountryRow = {
  code: string;
  visitors: number;
  views: number;
  share: number;
};

/** The audience report. */
export type AnalyticsAudienceReport = {
  range: { from: string; to: string };
  panels: AnalyticsDimensionPanel[];
  countries: AnalyticsCountryRow[];
  visitors: number;
};

/** One row of the events report. */
export type AnalyticsEventRow = {
  name: string;
  count: number;
  visitors: number;
  value_sum: number | null;
  last_seen: string | null;
};

/** The events report. */
export type AnalyticsEventsReport = {
  range: { from: string; to: string };
  rows: AnalyticsEventRow[];
};

/** One event, in detail. */
export type AnalyticsEventDetail = {
  range: { from: string; to: string };
  name: string;
  count: number;
  visitors: number;
  value_sum: number | null;
  last_seen: string | null;
  series: { bucket: string; label: string; count: number }[];
  properties: { key: string; value: string; count: number }[];
};

/** One row of the downloads report. */
export type AnalyticsDownloadRow = {
  value: string;
  downloads: number;
  visitors: number;
};

/** The downloads report. */
export type AnalyticsDownloadsReport = {
  range: { from: string; to: string };
  total: number;
  files: AnalyticsDownloadRow[];
  pages: AnalyticsDownloadRow[];
};

/** One row of the forms report. */
export type AnalyticsFormRow = {
  form: string;
  submissions: number;
  visitors: number;
  value_sum: number | null;
  starts: number;
  completion_rate: number | null;
  abandonment: number | null;
  last_seen: string | null;
};

/** The forms report. */
export type AnalyticsFormsReport = {
  range: { from: string; to: string };
  rows: AnalyticsFormRow[];
};

/** Build a report URL: only the parameters the caller actually set are written. */
function analyticsUrl(path: string, query: AnalyticsQuery): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) {
    if (value === undefined || value === null || value === "") {
      continue;
    }
    params.set(key, String(value));
  }
  const search = params.toString();

  return search ? `${path}?${search}` : path;
}

/** `GET /api/v1/analytics/overview`. */
export function fetchAnalyticsOverview(
  query: AnalyticsQuery,
): Promise<AnalyticsOverview> {
  return request<AnalyticsOverview>(analyticsUrl("/api/v1/analytics/overview", query));
}

/** `GET /api/v1/analytics/pages`. */
export function fetchAnalyticsPages(query: AnalyticsQuery): Promise<AnalyticsPagesReport> {
  return request<AnalyticsPagesReport>(analyticsUrl("/api/v1/analytics/pages", query));
}

/** `GET /api/v1/analytics/pages/series` — one page's own series. */
export function fetchAnalyticsPageSeries(
  query: AnalyticsQuery,
): Promise<AnalyticsSeriesPoint[]> {
  return request<AnalyticsSeriesPoint[]>(
    analyticsUrl("/api/v1/analytics/pages/series", query),
  );
}

/** `GET /api/v1/analytics/sources`. */
export function fetchAnalyticsSources(query: AnalyticsQuery): Promise<AnalyticsSourcesReport> {
  return request<AnalyticsSourcesReport>(analyticsUrl("/api/v1/analytics/sources", query));
}

/** `GET /api/v1/analytics/audience`. */
export function fetchAnalyticsAudience(
  query: AnalyticsQuery,
): Promise<AnalyticsAudienceReport> {
  return request<AnalyticsAudienceReport>(analyticsUrl("/api/v1/analytics/audience", query));
}

/** `GET /api/v1/analytics/events`. */
export function fetchAnalyticsEvents(query: AnalyticsQuery): Promise<AnalyticsEventsReport> {
  return request<AnalyticsEventsReport>(analyticsUrl("/api/v1/analytics/events", query));
}

/** `GET /api/v1/analytics/events/{name}`. */
export function fetchAnalyticsEvent(
  name: string,
  query: AnalyticsQuery,
): Promise<AnalyticsEventDetail> {
  return request<AnalyticsEventDetail>(
    analyticsUrl(`/api/v1/analytics/events/${encodeURIComponent(name)}`, query),
  );
}

/** `GET /api/v1/analytics/downloads`. */
export function fetchAnalyticsDownloads(
  query: AnalyticsQuery,
): Promise<AnalyticsDownloadsReport> {
  return request<AnalyticsDownloadsReport>(
    analyticsUrl("/api/v1/analytics/downloads", query),
  );
}

/** `GET /api/v1/analytics/forms`. */
export function fetchAnalyticsForms(query: AnalyticsQuery): Promise<AnalyticsFormsReport> {
  return request<AnalyticsFormsReport>(analyticsUrl("/api/v1/analytics/forms", query));
}

/**
 * `GET /api/v1/analytics/export` — the report the screen is showing, as a file.
 *
 * The browser gets the bytes and the row count the API put in a header, so the screen can say
 * what it just downloaded instead of guessing.
 */
export async function downloadAnalyticsExport(
  query: AnalyticsQuery,
): Promise<{ rows: number; blob: Blob; filename: string }> {
  let response: Response;
  try {
    response = await fetch(analyticsUrl("/api/v1/analytics/export", query), {
      credentials: "same-origin",
      headers: { accept: "text/csv" },
    });
  } catch {
    throw new ApiError(0, "network_error", "The Omnion API could not be reached.");
  }

  if (!response.ok) {
    const text = await response.text();
    let code = "export_failed";
    let message = `The export answered with status ${response.status}.`;
    try {
      const body = JSON.parse(text) as ErrorBody;
      code = body.error?.code ?? code;
      message = body.error?.message ?? message;
    } catch {
      // A non-JSON error body is still an error; the status stays in the message.
    }
    throw new ApiError(response.status, code, message);
  }

  const rows = Number(response.headers.get("x-export-rows") ?? "0");
  const disposition = response.headers.get("content-disposition") ?? "";
  const match = /filename="?([^";]+)"?/.exec(disposition);

  return {
    rows,
    blob: await response.blob(),
    filename: match?.[1] ?? "omnion-analytics.csv",
  };
}

/** `GET /api/v1/analytics/snippet` — what a site pastes into its pages. */
export type AnalyticsSnippet = {
  site: { key: string; name: string };
  script_url: string;
  collect_url: string;
  snippet: string;
};

/** The snippet of one site, with its script and collect addresses resolved. */
export function fetchAnalyticsSnippet(siteId: string): Promise<AnalyticsSnippet> {
  return request<AnalyticsSnippet>(
    `/api/v1/analytics/snippet?site_id=${encodeURIComponent(siteId)}`,
  );
}

// ---------------------------------------------------------------------------------------------
// Goals, funnels and realtime (docs/requests/REQ-007, slice 3)
// ---------------------------------------------------------------------------------------------

/** What a goal — or one of its steps — matches. Every missing pattern is simply not a condition. */
export type AnalyticsGoalMatch = {
  path?: string | null;
  name?: string | null;
  file?: string | null;
};

/** One step of a funnel. */
export type AnalyticsGoalStep = {
  position: number;
  kind: string;
  match: AnalyticsGoalMatch;
};

/** A goal with its funnel, as the editor reads it. */
export type AnalyticsGoal = {
  id: string;
  site_id: string;
  name: string;
  kind: string;
  match: AnalyticsGoalMatch;
  enabled: boolean;
  created_by: string | null;
  created_at: string;
  steps: AnalyticsGoalStep[];
};

/** A goal with how it did in the range — one row of the goal list. */
export type AnalyticsGoalSummary = AnalyticsGoal & {
  conversions: number;
  visitors: number;
  rate: number | null;
  last_hit: string | null;
};

/** `GET /api/v1/analytics/goals`. */
export type AnalyticsGoalsResponse = {
  from: string;
  to: string;
  goals: AnalyticsGoalSummary[];
};

/** One funnel step with the visitors that reached it. */
export type AnalyticsFunnelStep = {
  position: number;
  kind: string;
  match: AnalyticsGoalMatch;
  visitors: number;
  drop_off: number;
  rate: number | null;
};

/** `GET /api/v1/analytics/goals/{id}/funnel`. */
export type AnalyticsFunnel = {
  goal_id: string;
  name: string;
  enabled: boolean;
  from: string;
  to: string;
  visitors: number;
  conversions: number;
  rate: number | null;
  steps: AnalyticsFunnelStep[];
};

/** One step the editor sends: the funnel is replaced wholesale when `steps` is present. */
export type AnalyticsGoalStepInput = {
  kind: string;
  match: AnalyticsGoalMatch;
};

/** A create body, or the full description a rewrite carries. */
export type AnalyticsGoalInput = {
  name: string;
  kind: string;
  match: AnalyticsGoalMatch;
  enabled: boolean;
  steps: AnalyticsGoalStepInput[];
};

/** A partial update: what is present is replaced, what is absent is kept. */
export type AnalyticsGoalPatch = Partial<{
  name: string;
  kind: string;
  match: AnalyticsGoalMatch;
  enabled: boolean;
  steps: AnalyticsGoalStepInput[];
}>;

/** `GET /api/v1/analytics/goals`. */
export function fetchAnalyticsGoals(query: AnalyticsQuery): Promise<AnalyticsGoalsResponse> {
  return request<AnalyticsGoalsResponse>(analyticsUrl("/api/v1/analytics/goals", query));
}

/** `GET /api/v1/analytics/goals/{id}`. */
export function fetchAnalyticsGoal(id: string, query: AnalyticsQuery): Promise<AnalyticsGoal> {
  return request<AnalyticsGoal>(
    analyticsUrl(`/api/v1/analytics/goals/${encodeURIComponent(id)}`, query),
  );
}

/** `POST /api/v1/analytics/goals`. */
export function createAnalyticsGoal(
  query: AnalyticsQuery,
  changes: AnalyticsGoalInput,
): Promise<AnalyticsGoal> {
  return request<AnalyticsGoal>(analyticsUrl("/api/v1/analytics/goals", query), {
    method: "POST",
    body: JSON.stringify(changes),
  });
}

/** `PATCH /api/v1/analytics/goals/{id}`. */
export function updateAnalyticsGoal(
  id: string,
  query: AnalyticsQuery,
  changes: AnalyticsGoalPatch,
): Promise<AnalyticsGoal> {
  return request<AnalyticsGoal>(
    analyticsUrl(`/api/v1/analytics/goals/${encodeURIComponent(id)}`, query),
    { method: "PATCH", body: JSON.stringify(changes) },
  );
}

/** `DELETE /api/v1/analytics/goals/{id}` — removes the goal, its steps and its hits. */
export async function deleteAnalyticsGoal(id: string, query: AnalyticsQuery): Promise<void> {
  await request<null>(analyticsUrl(`/api/v1/analytics/goals/${encodeURIComponent(id)}`, query), {
    method: "DELETE",
  });
}

/** `GET /api/v1/analytics/goals/{id}/funnel`. */
export function fetchAnalyticsGoalFunnel(
  id: string,
  query: AnalyticsQuery,
): Promise<AnalyticsFunnel> {
  return request<AnalyticsFunnel>(
    analyticsUrl(`/api/v1/analytics/goals/${encodeURIComponent(id)}/funnel`, query),
  );
}

/** The counters of one realtime window. */
export type AnalyticsRealtimeCounters = {
  window_minutes: number;
  visitors: number;
  pageviews: number;
  events: number;
  conversions: number;
};

/** One page being read right now. */
export type AnalyticsRealtimePage = {
  path: string;
  visitors: number;
  views: number;
  last_seen: string;
};

/** One thing that just happened. */
export type AnalyticsRealtimeEvent = {
  name: string;
  path: string | null;
  value: number | null;
  occurred_at: string;
};

/** `GET /api/v1/analytics/realtime`. */
export type AnalyticsRealtimeSnapshot = {
  generated_at: string;
  last_5: AnalyticsRealtimeCounters;
  last_30: AnalyticsRealtimeCounters;
  pages: AnalyticsRealtimePage[];
  events: AnalyticsRealtimeEvent[];
};

/** `GET /api/v1/analytics/realtime` — the snapshot of the last half hour. */
export function fetchAnalyticsRealtime(siteId: string): Promise<AnalyticsRealtimeSnapshot> {
  return request<AnalyticsRealtimeSnapshot>(
    `/api/v1/analytics/realtime?site_id=${encodeURIComponent(siteId)}`,
  );
}

/**
 * The address of the realtime stream, for an `EventSource`.
 *
 * A stream cannot send headers, so it carries the site in the query and nothing else: the
 * session cookie is first-party (the panel proxies `/api/*`), and every connection re-checks the
 * reader's rights on the server.
 */
export function analyticsRealtimeStreamUrl(siteId: string): string {
  return `/api/v1/analytics/realtime/stream?site_id=${encodeURIComponent(siteId)}`;
}

// ---------------------------------------------------------------------------------------------
// Settings and privacy operations (docs/requests/REQ-007, slices 1 and 4)
// ---------------------------------------------------------------------------------------------

/** One site's analytics configuration, as the settings screen reads it. */
export type AnalyticsSettings = {
  site_id: string;
  tracking_enabled: boolean;
  /** `cookieless` (the default) or `cookie`. */
  mode: string;
  anonymize_ip: boolean;
  respect_dnt: boolean;
  bot_filter: boolean;
  sample_rate: number;
  retention_days: number;
  excluded_paths: string[];
  excluded_ips: string[];
  updated_by: string | null;
  updated_at: string;
};

/** A full settings update: the screen sends every field, so a partial write is impossible. */
export type AnalyticsSettingsChanges = {
  tracking_enabled: boolean;
  mode: string;
  anonymize_ip: boolean;
  respect_dnt: boolean;
  bot_filter: boolean;
  sample_rate: number;
  retention_days: number;
  excluded_paths: string[];
  excluded_ips: string[];
};

/** One row of the "what we store" table. */
export type AnalyticsStorageField = {
  table: string;
  column: string;
  purpose: string;
  personal: boolean;
};

/** One row of the purge and erasure audit trail. */
export type AnalyticsPurgeRecord = {
  id: string;
  site_id: string | null;
  kind: string;
  cutoff: string | null;
  rows_removed: number;
  actor_user_id: string | null;
  created_at: string;
};

/** `GET`/`PUT /api/v1/analytics/settings`. */
export type AnalyticsSettingsResponse = {
  settings: AnalyticsSettings;
  defaults: AnalyticsSettingsChanges;
  /** The cutoff `POST /analytics/purge` would use at this moment. */
  purge_cutoff: string;
  last_purge: AnalyticsPurgeRecord | null;
  storage: AnalyticsStorageField[];
};

/** What one retention run removed, table by table. */
export type AnalyticsPurgeOutcome = {
  purge_id: string;
  site_id: string;
  kind: string;
  cutoff: string;
  visits: number;
  pageviews: number;
  events: number;
  goal_hits: number;
  salts: number;
  rows_removed: number;
  created_at: string;
};

/** What one erasure removed. */
export type AnalyticsErasureOutcome = {
  purge_id: string;
  site_id: string;
  visitor: string;
  visits: number;
  pageviews: number;
  events: number;
  goal_hits: number;
  rows_removed: number;
  created_at: string;
};

/** `GET /api/v1/analytics/settings` — how this site counts. */
export function fetchAnalyticsSettings(siteId: string): Promise<AnalyticsSettingsResponse> {
  return request<AnalyticsSettingsResponse>(
    `/api/v1/analytics/settings?site_id=${encodeURIComponent(siteId)}`,
  );
}

/** `PUT /api/v1/analytics/settings` — replace the configuration in one write. */
export function updateAnalyticsSettings(
  siteId: string,
  changes: AnalyticsSettingsChanges,
): Promise<AnalyticsSettingsResponse> {
  return request<AnalyticsSettingsResponse>(
    `/api/v1/analytics/settings?site_id=${encodeURIComponent(siteId)}`,
    { method: "PUT", body: JSON.stringify(changes) },
  );
}

/** `POST /api/v1/analytics/purge` — run the retention purge now (audited). */
export function runAnalyticsPurge(siteId: string): Promise<AnalyticsPurgeOutcome> {
  return request<AnalyticsPurgeOutcome>(
    `/api/v1/analytics/purge?site_id=${encodeURIComponent(siteId)}`,
    { method: "POST" },
  );
}

/** `DELETE /api/v1/analytics/visitors/{hash}` — erase every row of one visitor handle. */
export function eraseAnalyticsVisitor(
  siteId: string,
  handle: string,
): Promise<AnalyticsErasureOutcome> {
  return request<AnalyticsErasureOutcome>(
    `/api/v1/analytics/visitors/${encodeURIComponent(handle)}?site_id=${encodeURIComponent(siteId)}`,
    { method: "DELETE" },
  );
}

// ---------------------------------------------------------------------------------------------
// IAM: role depth (docs/requests/REQ-006, slice 1)
// ---------------------------------------------------------------------------------------------

/** A role as the panel reads it. */
export type IamRole = {
  id: string;
  /** `null` for a platform role. */
  organization_id: string | null;
  key: string;
  name: string;
  description: string;
  priority: number;
  inherits_role_id: string | null;
  inherit_permissions: boolean;
  is_system: boolean;
  allowed_permissions: number;
  denied_permissions: number;
  created_at: string;
};

/** One entry of a role's own permission set. */
export type IamPermissionEntry = {
  key: string;
  effect: "allow" | "deny";
};

/** One catalogue entry — the vocabulary the matrix draws. */
export type IamPermissionDef = {
  key: string;
  category: string;
  description: string;
};

/** A role as a chain or member list refers to it. */
export type IamRoleRef = {
  id: string;
  key: string;
  name: string;
  priority: number;
  is_system: boolean;
};

/** What a save (or a preview) changes. */
export type IamDiff = {
  added: IamPermissionEntry[];
  changed: { key: string; from: string; to: string }[];
  removed: IamPermissionEntry[];
};

/** `GET /api/v1/iam/roles/{id}`. */
export type IamRoleDetail = {
  role: IamRole;
  permissions: IamPermissionEntry[];
  chain: IamRoleRef[];
  inherited_by: IamRoleRef[];
  member_count: number;
  version: number;
};

/** One version of a role, with the diff against the version before it. */
export type IamRoleVersion = {
  version: number;
  name: string;
  description: string;
  priority: number;
  change: string;
  changed_by: string | null;
  created_at: string;
  permissions: IamPermissionEntry[];
  diff: IamDiff;
  diff_total: number;
};

/**
 * The roles visible to the account: platform roles plus one organization's own.
 *
 * A tenant account always answers for its own organization; a platform account names the
 * tenant with `organizationId`.
 */
export async function fetchRoles(organizationId?: string | null): Promise<IamRole[]> {
  const query = organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
  const body = await request<{ roles: IamRole[] }>(`/api/v1/iam/roles${query}`);
  return body.roles;
}

/** One role in full: its entries, its chain, its members and its latest version. */
export function fetchRole(roleId: string): Promise<IamRoleDetail> {
  return request<IamRoleDetail>(`/api/v1/iam/roles/${encodeURIComponent(roleId)}`);
}

/** The permission catalogue the matrix is drawn from. */
export async function fetchPermissionCatalogue(): Promise<IamPermissionDef[]> {
  const body = await request<{ permissions: IamPermissionDef[] }>("/api/v1/iam/permissions");
  return body.permissions;
}

/** Create a custom role. */
export function createIamRole(input: {
  key: string;
  name: string;
  description?: string;
  priority?: number;
  inheritsRoleId?: string | null;
  /** Required from a platform account (no primary organization). */
  organizationId?: string | null;
}): Promise<IamRole> {
  return request<IamRole>("/api/v1/iam/roles", {
    method: "POST",
    body: JSON.stringify({
      key: input.key,
      name: input.name,
      description: input.description ?? "",
      priority: input.priority,
      inherits_role_id: input.inheritsRoleId ?? null,
      organization_id: input.organizationId ?? null,
    }),
  });
}

/** Change a role's fields and its parent link. */
export function updateIamRole(
  roleId: string,
  changes: {
    name?: string;
    description?: string;
    priority?: number;
    inheritPermissions?: boolean;
    inheritsRoleId?: string | null;
  },
): Promise<IamRole> {
  const body: Record<string, unknown> = {};
  if (changes.name !== undefined) body.name = changes.name;
  if (changes.description !== undefined) body.description = changes.description;
  if (changes.priority !== undefined) body.priority = changes.priority;
  if (changes.inheritPermissions !== undefined) body.inherit_permissions = changes.inheritPermissions;
  if (changes.inheritsRoleId === null) body.detach_parent = true;
  else if (changes.inheritsRoleId !== undefined) body.inherits_role_id = changes.inheritsRoleId;
  return request<IamRole>(`/api/v1/iam/roles/${encodeURIComponent(roleId)}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

/** Remove a custom role (refused while it carries live bindings). */
export async function deleteIamRole(roleId: string): Promise<void> {
  await request<null>(`/api/v1/iam/roles/${encodeURIComponent(roleId)}`, { method: "DELETE" });
}

/** Clone a role — the way an organization customises a platform role. */
export function duplicateIamRole(
  roleId: string,
  input: { key: string; name: string; organizationId?: string | null },
): Promise<IamRole> {
  return request<IamRole>(`/api/v1/iam/roles/${encodeURIComponent(roleId)}/duplicate`, {
    method: "POST",
    body: JSON.stringify({
      key: input.key,
      name: input.name,
      organization_id: input.organizationId ?? null,
    }),
  });
}

/** What saving a set would change, without writing anything. */
export function previewRolePermissions(
  roleId: string,
  permissions: IamPermissionEntry[],
): Promise<{ diff: IamDiff; problems: string[]; version: number; unchanged: boolean }> {
  return request(`/api/v1/iam/roles/${encodeURIComponent(roleId)}/preview`, {
    method: "POST",
    body: JSON.stringify({ permissions }),
  });
}

/** Save the matrix (atomic: a stale version or an unknown key refuses the whole set). */
export function saveRolePermissions(
  roleId: string,
  permissions: IamPermissionEntry[],
  expectedVersion: number,
): Promise<{ role: IamRole; permissions: IamPermissionEntry[]; diff: IamDiff; version: number }> {
  return request(`/api/v1/iam/roles/${encodeURIComponent(roleId)}/permissions`, {
    method: "PUT",
    body: JSON.stringify({ permissions, expected_version: expectedVersion }),
  });
}

/** The role's version history, newest first, each with its diff. */
export async function fetchRoleVersions(roleId: string): Promise<IamRoleVersion[]> {
  const body = await request<{ role_id: string; versions: IamRoleVersion[] }>(
    `/api/v1/iam/roles/${encodeURIComponent(roleId)}/versions`,
  );
  return body.versions;
}

/** One member of a role — a person, a group or a machine identity. */
export type IamRoleMember = {
  subject_type: "user" | "group" | "service_account";
  subject_id: string;
  user_id: string | null;
  label: string;
  scope: IamScope;
  expires_at: string | null;
  revoked_at: string | null;
  active: boolean;
  expired: boolean;
  created_at: string;
};

/** `GET /api/v1/iam/roles/{id}/members` — who carries the role. */
export function fetchRoleMembers(
  roleId: string,
): Promise<{ role_id: string; members: IamRoleMember[] }> {
  return request<{ role_id: string; members: IamRoleMember[] }>(
    `/api/v1/iam/roles/${encodeURIComponent(roleId)}/members`,
  );
}

// ---------------------------------------------------------------------------------------------
// Subjects, scopes and the simulator (REQ-006, slice 2)
// ---------------------------------------------------------------------------------------------

/** Where a binding applies. */
export type IamScope = {
  type: "global" | "organization" | "site" | "department" | "module" | "resource";
  organization_id: string | null;
  site_id: string | null;
  resource_type: string | null;
  resource_id: string | null;
};

/** One role binding, for any kind of subject. */
export type IamBinding = {
  id: string;
  role_id: string;
  subject_type: "user" | "group" | "service_account";
  subject_id: string;
  user_id: string | null;
  scope: IamScope;
  granted_by: string | null;
  expires_at: string | null;
  revoked_at: string | null;
  active: boolean;
  expired: boolean;
  created_at: string;
};

/** One role chip on a user row. */
export type IamRoleChip = {
  role_id: string;
  key: string;
  name: string;
  scope: IamScope;
  via: "direct" | "group";
  expires_at: string | null;
};

/** One account on the user list. */
export type IamUserRow = {
  id: string;
  email: string;
  display_name: string;
  status: string;
  organization_id: string | null;
  mfa_enforced: boolean;
  last_sign_in_at: string | null;
  failed_sign_in_count: number;
  locked_until: string | null;
  attributes: Record<string, unknown>;
  created_at: string;
  roles: IamRoleChip[];
  group_count: number;
};

/** Filters of the user list. */
export type IamUserQuery = {
  search?: string;
  status?: string;
  organizationId?: string | null;
  mfa?: boolean;
  roleId?: string;
};

/** `GET /api/v1/iam/users`. */
export function fetchIamUsers(
  query: IamUserQuery = {},
): Promise<{ users: IamUserRow[]; total: number }> {
  const params = new URLSearchParams();
  if (query.search) params.set("search", query.search);
  if (query.status) params.set("status", query.status);
  if (query.organizationId) params.set("organization_id", query.organizationId);
  if (query.mfa !== undefined) params.set("mfa", String(query.mfa));
  if (query.roleId) params.set("role_id", query.roleId);
  const suffix = params.size > 0 ? `?${params.toString()}` : "";
  return request<{ users: IamUserRow[]; total: number }>(`/api/v1/iam/users${suffix}`);
}

/** `POST /api/v1/iam/users` — create an account (invited, or with a password). */
export function createIamUser(input: {
  email: string;
  displayName: string;
  organizationId?: string | null;
  password?: string;
  roleId?: string;
  roleScopeType?: "global" | "organization";
  expiresAt?: string;
}): Promise<{ id: string; email: string; status: string; organization_id: string | null }> {
  return request("/api/v1/iam/users", {
    method: "POST",
    body: JSON.stringify({
      email: input.email,
      display_name: input.displayName,
      organization_id: input.organizationId ?? null,
      ...(input.password ? { password: input.password } : {}),
      ...(input.roleId ? { role_id: input.roleId } : {}),
      ...(input.roleScopeType ? { role_scope_type: input.roleScopeType } : {}),
      ...(input.expiresAt ? { expires_at: input.expiresAt } : {}),
    }),
  });
}

/** The detail of one account: profile, bindings and groups. */
export type IamUserDetail = {
  user: {
    id: string;
    email: string;
    display_name: string;
    status: string;
    organization_id: string | null;
    mfa_enforced: boolean;
    attributes: Record<string, unknown>;
    created_at: string;
  };
  bindings: IamBinding[];
  groups: { id: string; name: string; slug: string }[];
};

/** `GET /api/v1/iam/users/{id}`. */
export function fetchIamUser(userId: string): Promise<IamUserDetail> {
  return request<IamUserDetail>(`/api/v1/iam/users/${encodeURIComponent(userId)}`);
}

/** `PATCH /api/v1/iam/users/{id}` — profile, status, MFA requirement, attributes. */
export function updateIamUser(
  userId: string,
  input: {
    displayName?: string;
    status?: string;
    mfaEnforced?: boolean;
    attributes?: Record<string, unknown>;
  },
): Promise<Record<string, unknown>> {
  return request(`/api/v1/iam/users/${encodeURIComponent(userId)}`, {
    method: "PATCH",
    body: JSON.stringify({
      ...(input.displayName !== undefined ? { display_name: input.displayName } : {}),
      ...(input.status !== undefined ? { status: input.status } : {}),
      ...(input.mfaEnforced !== undefined ? { mfa_enforced: input.mfaEnforced } : {}),
      ...(input.attributes !== undefined ? { attributes: input.attributes } : {}),
    }),
  });
}

/** The resolved permission set of one account. */
export type IamEffectivePermissions = {
  user_id: string;
  scope: IamScope;
  granted: {
    key: string;
    source: { role_id: string; role_key: string; role_name: string; role_priority: number; via: string };
  }[];
  denied: {
    key: string;
    source: { role_id: string; role_key: string; role_name: string; role_priority: number; via: string };
  }[];
  granted_count: number;
};

/** Resolve one account's effective permissions, optionally in a resource context. */
export function fetchEffectivePermissions(input: {
  userId?: string;
  organizationId?: string | null;
  siteId?: string | null;
  path?: string;
}): Promise<IamEffectivePermissions> {
  const params = new URLSearchParams();
  if (input.userId) params.set("user_id", input.userId);
  if (input.organizationId) params.set("organization_id", input.organizationId);
  if (input.siteId) params.set("site_id", input.siteId);
  if (input.path) params.set("path", input.path);
  const suffix = params.size > 0 ? `?${params.toString()}` : "";
  return request<IamEffectivePermissions>(`/api/v1/iam/effective-permissions${suffix}`);
}

/** `GET /api/v1/iam/bindings` — role assignments of any subject. */
export function fetchIamBindings(query: {
  subjectType?: string;
  subjectId?: string;
  roleId?: string;
  live?: boolean;
  organizationId?: string | null;
}): Promise<{ subject_type: string | null; subject_id: string | null; bindings: IamBinding[] }> {
  const params = new URLSearchParams();
  if (query.subjectType) params.set("subject_type", query.subjectType);
  if (query.subjectId) params.set("subject_id", query.subjectId);
  if (query.roleId) params.set("role_id", query.roleId);
  if (query.live !== undefined) params.set("live", String(query.live));
  if (query.organizationId) params.set("organization_id", query.organizationId);
  const suffix = params.size > 0 ? `?${params.toString()}` : "";
  return request(`/api/v1/iam/bindings${suffix}`);
}

/** `POST /api/v1/iam/bindings` — attach a role to any subject at any scope. */
export function createIamBinding(input: {
  subjectType: "user" | "group" | "service_account";
  subjectId: string;
  roleId: string;
  scopeType: IamScope["type"];
  organizationId?: string | null;
  siteId?: string | null;
  department?: string;
  module?: string;
  resourceType?: string;
  resourceId?: string;
  expiresAt?: string;
}): Promise<IamBinding> {
  return request<IamBinding>("/api/v1/iam/bindings", {
    method: "POST",
    body: JSON.stringify({
      subject_type: input.subjectType,
      subject_id: input.subjectId,
      role_id: input.roleId,
      scope_type: input.scopeType,
      organization_id: input.organizationId ?? null,
      site_id: input.siteId ?? null,
      ...(input.department ? { department: input.department } : {}),
      ...(input.module ? { module: input.module } : {}),
      ...(input.resourceType ? { resource_type: input.resourceType } : {}),
      ...(input.resourceId ? { resource_id: input.resourceId } : {}),
      ...(input.expiresAt ? { expires_at: input.expiresAt } : {}),
    }),
  });
}

/** `DELETE /api/v1/iam/bindings/{id}` — revoke a role assignment. */
export function revokeIamBinding(bindingId: string): Promise<IamBinding> {
  return request<IamBinding>(`/api/v1/iam/bindings/${encodeURIComponent(bindingId)}`, {
    method: "DELETE",
  });
}

/** One group on the list. */
export type IamGroup = {
  id: string;
  name: string;
  slug: string;
  description: string;
  organization_id: string;
  member_count: number;
  role_count: number;
  created_at: string;
};

/** `GET /api/v1/iam/groups`. */
export async function fetchIamGroups(organizationId?: string | null): Promise<IamGroup[]> {
  const query = organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
  const body = await request<{ groups: IamGroup[]; organization_id: string }>(
    `/api/v1/iam/groups${query}`,
  );
  return body.groups;
}

/** `POST /api/v1/iam/groups`. */
export function createIamGroup(input: {
  name: string;
  description?: string;
  organizationId?: string | null;
}): Promise<{ id: string; name: string; slug: string }> {
  return request("/api/v1/iam/groups", {
    method: "POST",
    body: JSON.stringify({
      name: input.name,
      description: input.description ?? "",
      organization_id: input.organizationId ?? null,
    }),
  });
}

/** One membership row of a group. */
export type IamGroupMember = {
  user_id: string;
  email: string;
  display_name: string;
  status: string;
  joined_at: string;
};

/** One group in full: its members and the roles attached to it. */
export type IamGroupDetail = {
  group: { id: string; name: string; slug: string; description: string; organization_id: string };
  members: IamGroupMember[];
  roles: { binding_id: string; role_id: string; scope: IamScope; expires_at: string | null }[];
};

/** `GET /api/v1/iam/groups/{id}`. */
export function fetchIamGroup(groupId: string): Promise<IamGroupDetail> {
  return request<IamGroupDetail>(`/api/v1/iam/groups/${encodeURIComponent(groupId)}`);
}

/** `PATCH /api/v1/iam/groups/{id}`. */
export function updateIamGroup(
  groupId: string,
  input: { name?: string; description?: string },
): Promise<{ id: string; name: string; slug: string; description: string }> {
  return request(`/api/v1/iam/groups/${encodeURIComponent(groupId)}`, {
    method: "PATCH",
    body: JSON.stringify({
      ...(input.name !== undefined ? { name: input.name } : {}),
      ...(input.description !== undefined ? { description: input.description } : {}),
    }),
  });
}

/** `DELETE /api/v1/iam/groups/{id}`. */
export function deleteIamGroup(groupId: string): Promise<{ deleted: boolean }> {
  return request(`/api/v1/iam/groups/${encodeURIComponent(groupId)}`, { method: "DELETE" });
}

/** `PUT /api/v1/iam/groups/{id}/members` — replace the membership. */
export function setIamGroupMembers(
  groupId: string,
  userIds: string[],
): Promise<{ group_id: string; member_count: number }> {
  return request(`/api/v1/iam/groups/${encodeURIComponent(groupId)}/members`, {
    method: "PUT",
    body: JSON.stringify({ user_ids: userIds }),
  });
}

/** One machine identity on the list. */
export type IamServiceAccount = {
  id: string;
  name: string;
  description: string;
  prefix: string;
  organization_id: string;
  active: boolean;
  last_used_at: string | null;
  active_keys: number;
  role_count: number;
  created_at: string;
};

/** `GET /api/v1/iam/service-accounts`. */
export async function fetchIamServiceAccounts(
  organizationId?: string | null,
): Promise<IamServiceAccount[]> {
  const query = organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
  const body = await request<{ service_accounts: IamServiceAccount[]; organization_id: string }>(
    `/api/v1/iam/service-accounts${query}`,
  );
  return body.service_accounts;
}

/** `POST /api/v1/iam/service-accounts` — the key, when asked for, is shown once. */
export function createIamServiceAccount(input: {
  name: string;
  description?: string;
  organizationId?: string | null;
  keyLabel?: string;
}): Promise<{
  id: string;
  name: string;
  prefix: string;
  organization_id: string;
  key: string | null;
}> {
  return request("/api/v1/iam/service-accounts", {
    method: "POST",
    body: JSON.stringify({
      name: input.name,
      description: input.description ?? "",
      organization_id: input.organizationId ?? null,
      ...(input.keyLabel ? { key_label: input.keyLabel } : {}),
    }),
  });
}

/** One key of a machine identity. */
export type IamServiceAccountKey = {
  id: string;
  prefix: string;
  label: string;
  active: boolean;
  expires_at: string | null;
  last_used_at: string | null;
  revoked_at: string | null;
  created_at: string;
};

/** One machine identity in full: its keys and its roles. */
export type IamServiceAccountDetail = {
  account: {
    id: string;
    name: string;
    description: string;
    prefix: string;
    organization_id: string;
    active: boolean;
    last_used_at: string | null;
  };
  keys: IamServiceAccountKey[];
  roles: IamBinding[];
};

/** `GET /api/v1/iam/service-accounts/{id}`. */
export function fetchIamServiceAccount(accountId: string): Promise<IamServiceAccountDetail> {
  return request<IamServiceAccountDetail>(
    `/api/v1/iam/service-accounts/${encodeURIComponent(accountId)}`,
  );
}

/** `DELETE /api/v1/iam/service-accounts/{id}`. */
export function deleteIamServiceAccount(accountId: string): Promise<{ deleted: boolean }> {
  return request(`/api/v1/iam/service-accounts/${encodeURIComponent(accountId)}`, {
    method: "DELETE",
  });
}

/** `POST /api/v1/iam/service-accounts/{id}/keys` — the token comes back exactly once. */
export function issueIamServiceAccountKey(
  accountId: string,
  input: { label?: string; expiresAt?: string } = {},
): Promise<{
  id: string;
  prefix: string;
  label: string;
  token: string;
  expires_at: string | null;
}> {
  return request(`/api/v1/iam/service-accounts/${encodeURIComponent(accountId)}/keys`, {
    method: "POST",
    body: JSON.stringify({
      label: input.label ?? "",
      ...(input.expiresAt ? { expires_at: input.expiresAt } : {}),
    }),
  });
}

/** `DELETE /api/v1/iam/service-accounts/{id}/keys/{keyId}`. */
export function revokeIamServiceAccountKey(
  accountId: string,
  keyId: string,
): Promise<{ revoked: boolean }> {
  return request(
    `/api/v1/iam/service-accounts/${encodeURIComponent(accountId)}/keys/${encodeURIComponent(keyId)}`,
    { method: "DELETE" },
  );
}

/** One step of a simulator answer. */
export type IamSimulationStep = {
  binding_id: string;
  role_id: string;
  role_key: string;
  role_name: string;
  role_priority: number;
  subject: string;
  scope: string;
  state: "active" | "expired" | "revoked" | "out_of_scope";
  counts: boolean;
  effect: "allow" | "deny" | null;
  via: string | null;
  inherited_from: string | null;
};

/** The verdict of one simulator query, with the chain that produced it. */
export type IamSimulationReport = {
  allowed: boolean;
  reason: string;
  subject: string;
  permission: string;
  context: {
    organization_id: string | null;
    site_id: string | null;
    department: string | null;
    module: string | null;
    path: string | null;
  };
  source: {
    role_id: string;
    role_key: string;
    role_name: string;
    role_priority: number;
    via: string;
  } | null;
  chain: IamSimulationStep[];
  considered: number;
  counted: number;
  note: string;
};

/** `POST /api/v1/iam/simulations`. */
export function runIamSimulation(input: {
  subjectType: "user" | "group" | "service_account";
  subjectId: string;
  permission: string;
  organizationId?: string | null;
  siteId?: string | null;
  department?: string;
  module?: string;
  path?: string;
}): Promise<IamSimulationReport> {
  return request<IamSimulationReport>("/api/v1/iam/simulations", {
    method: "POST",
    body: JSON.stringify({
      subject_type: input.subjectType,
      subject_id: input.subjectId,
      permission: input.permission,
      ...(input.organizationId ? { organization_id: input.organizationId } : {}),
      ...(input.siteId ? { site_id: input.siteId } : {}),
      ...(input.department ? { department: input.department } : {}),
      ...(input.module ? { module: input.module } : {}),
      ...(input.path ? { path: input.path } : {}),
    }),
  });
}

/** The IAM overview: what exists, what runs out soon, what happened last. */
export type IamOverview = {
  organization_id: string | null;
  counts: {
    users: number;
    roles: number;
    groups: number;
    service_accounts: number;
    live_bindings: number;
    expiring_soon: number;
  };
  expiring: {
    binding_id: string;
    role_id: string;
    role_key: string | null;
    role_name: string | null;
    subject: string;
    scope: string;
    expires_at: string | null;
  }[];
  recent: {
    id: number;
    action: string;
    actor_user_id: string | null;
    target_type: string | null;
    target_id: string | null;
    created_at: string;
  }[];
};

/** `GET /api/v1/iam/overview`. */
export function fetchIamOverview(organizationId?: string | null): Promise<IamOverview> {
  const query = organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
  return request<IamOverview>(`/api/v1/iam/overview${query}`);
}

// ---------------------------------------------------------------------------------------------
// Security policy, sessions, devices and second factors (REQ-006, slice 3)
// ---------------------------------------------------------------------------------------------

/** The organization's security policy (`GET /api/v1/iam/security-policies`). */
export type IamSecurityPolicy = {
  organization_id: string;
  password_min_length: number;
  password_require_classes: number;
  password_history: number;
  password_expiry_days: number;
  lockout_attempts: number;
  lockout_minutes: number;
  ip_allowlist: string[];
  ip_denylist: string[];
  session_idle_minutes: number;
  session_absolute_days: number;
  session_concurrent_max: number;
  device_trust_days: number;
  mfa_required: boolean;
  updated_by: string | null;
  updated_at: string;
};

/** One field of the before/after diff a policy save answers with. */
export type IamPolicyChange = { field: string; before: string; after: string };

/** What a policy save answers. */
export type IamPolicySave = {
  before: IamSecurityPolicy;
  after: IamSecurityPolicy;
  changes: IamPolicyChange[];
};

/** Read the security policy (creating the defaults when the organization has none yet). */
export function fetchSecurityPolicy(organizationId?: string | null): Promise<IamSecurityPolicy> {
  const params = new URLSearchParams();
  if (organizationId) params.set("organization_id", organizationId);
  const suffix = params.size > 0 ? `?${params.toString()}` : "";
  return request<IamSecurityPolicy>(`/api/v1/iam/security-policies${suffix}`);
}

/** Save a partial policy and read back the diff it applied. */
export function updateSecurityPolicy(
  patch: Partial<Omit<IamSecurityPolicy, "organization_id" | "updated_by" | "updated_at">>,
  organizationId?: string | null,
): Promise<IamPolicySave> {
  const params = new URLSearchParams();
  if (organizationId) params.set("organization_id", organizationId);
  const suffix = params.size > 0 ? `?${params.toString()}` : "";
  return request<IamPolicySave>(`/api/v1/iam/security-policies${suffix}`, {
    method: "PUT",
    body: JSON.stringify(patch),
  });
}

/** One row of the session list. */
export type IamSession = {
  id: string;
  user_id: string;
  user_email: string;
  user_display_name: string;
  ip_address: string | null;
  user_agent: string | null;
  device_label: string | null;
  auth_methods: string[];
  created_at: string;
  last_seen_at: string | null;
  expires_at: string;
  absolute_expires_at: string | null;
  revoked_at: string | null;
  revoke_reason: string | null;
  step_up_at: string | null;
  state: string;
  current: boolean;
  revocable: boolean;
};

/** List sessions with the state each row is in. */
export function fetchIamSessions(query: {
  userId?: string;
  organizationId?: string | null;
  search?: string;
  state?: string;
  includeInactive?: boolean;
}): Promise<{ sessions: IamSession[]; total: number; idle_minutes: number }> {
  const params = new URLSearchParams();
  if (query.userId) params.set("user_id", query.userId);
  if (query.organizationId) params.set("organization_id", query.organizationId);
  if (query.search) params.set("search", query.search);
  if (query.state) params.set("state", query.state);
  if (query.includeInactive) params.set("include_inactive", "true");
  const suffix = params.size > 0 ? `?${params.toString()}` : "";
  return request(`/api/v1/iam/sessions${suffix}`);
}

/** Revoke one session. */
export function revokeIamSession(sessionId: string): Promise<IamSession> {
  return request<IamSession>(`/api/v1/iam/sessions/${encodeURIComponent(sessionId)}`, {
    method: "DELETE",
  });
}

/** Sign every live session of an account out. */
export function signOutAllSessions(
  userId: string,
): Promise<{ user_id: string; revoked: number; session_ids: string[] }> {
  return request(`/api/v1/iam/users/${encodeURIComponent(userId)}/sign-out-all`, {
    method: "POST",
  });
}

/** One row of the device list. */
export type IamDevice = {
  id: string;
  user_id: string;
  user_email: string;
  user_display_name: string;
  label: string;
  platform: string;
  browser: string;
  fingerprint_hint: string;
  first_seen_at: string;
  last_seen_at: string;
  trusted_until: string | null;
  revoked: boolean;
  session_count: number;
  trusted: boolean;
};

/** List known devices. */
export function fetchIamDevices(query: {
  userId?: string;
  organizationId?: string | null;
  search?: string;
  includeRevoked?: boolean;
}): Promise<{ devices: IamDevice[]; total: number; device_trust_days: number }> {
  const params = new URLSearchParams();
  if (query.userId) params.set("user_id", query.userId);
  if (query.organizationId) params.set("organization_id", query.organizationId);
  if (query.search) params.set("search", query.search);
  if (query.includeRevoked) params.set("include_revoked", "true");
  const suffix = params.size > 0 ? `?${params.toString()}` : "";
  return request(`/api/v1/iam/devices${suffix}`);
}

/** Set (or clear) a device's trust window. */
export function trustIamDevice(
  deviceId: string,
  input: { days?: number; trustedUntil?: string },
): Promise<IamDevice> {
  return request<IamDevice>(`/api/v1/iam/devices/${encodeURIComponent(deviceId)}/trust`, {
    method: "POST",
    body: JSON.stringify({
      ...(input.days !== undefined ? { days: input.days } : {}),
      ...(input.trustedUntil !== undefined ? { trusted_until: input.trustedUntil } : {}),
    }),
  });
}

/** Forget a device: it stops being trusted and its live sessions end. */
export function forgetIamDevice(deviceId: string): Promise<IamDevice> {
  return request<IamDevice>(`/api/v1/iam/devices/${encodeURIComponent(deviceId)}`, {
    method: "DELETE",
  });
}

/** One enrolled second factor. */
export type IamFactor = {
  id: string;
  kind: string;
  label: string;
  confirmed: boolean;
  confirmed_at: string | null;
  last_used_at: string | null;
  created_at: string;
};

/** What the factor list answers. */
export type IamFactorList = {
  user_id: string;
  factors: IamFactor[];
  recovery_codes_remaining: number;
  confirmed: number;
};

/** List an account's factors and its remaining recovery codes. */
export function fetchIamFactors(userId: string): Promise<IamFactorList> {
  return request<IamFactorList>(`/api/v1/iam/users/${encodeURIComponent(userId)}/mfa`);
}

/** Start TOTP enrolment; the secret comes back exactly once. */
export function enrollIamTotp(
  userId: string,
  label?: string,
): Promise<{ factor: IamFactor; secret: string; otpauth_uri: string }> {
  return request(`/api/v1/iam/users/${encodeURIComponent(userId)}/mfa`, {
    method: "POST",
    body: JSON.stringify({ label: label ?? "" }),
  });
}

/** Confirm a pending factor with a code; the recovery codes are shown once. */
export function confirmIamTotp(
  userId: string,
  factorId: string,
  code: string,
): Promise<{ factor_id: string; confirmed: boolean; recovery_codes: string[] }> {
  return request(
    `/api/v1/iam/users/${encodeURIComponent(userId)}/mfa/${encodeURIComponent(factorId)}/confirm`,
    { method: "POST", body: JSON.stringify({ code }) },
  );
}

/** Remove one factor (needs a fresh step-up for a confirmed one). */
export function revokeIamFactor(
  userId: string,
  factorId: string,
): Promise<{ factor_id: string; revoked: boolean }> {
  return request(
    `/api/v1/iam/users/${encodeURIComponent(userId)}/mfa/${encodeURIComponent(factorId)}`,
    { method: "DELETE" },
  );
}

/** Clear every factor of an account (needs a fresh step-up). */
export function resetIamMfa(userId: string): Promise<{ user_id: string; factors_revoked: number }> {
  return request(`/api/v1/iam/users/${encodeURIComponent(userId)}/reset-mfa`, {
    method: "POST",
  });
}

// ---------------------------------------------------------------------------------------------
// Passkeys (REQ-006, slice 3b)
// ---------------------------------------------------------------------------------------------

/**
 * A credential the browser built. The client data travels as the JSON text the ceremony signs
 * (that is the exact string the server hashes), while every binary part travels base64url —
 * which is how `apps/admin/lib/webauthn.ts` serialises a `PublicKeyCredential`.
 */
export type PasskeyCredential = {
  id: string;
  client_data_json: string;
  attestation_object?: string;
  authenticator_data?: string;
  signature?: string;
  transports?: string[];
};

/** The options a registration ceremony needs (`navigator.credentials.create`). */
export type PasskeyCreationOptions = {
  challenge: string;
  rp: { id: string; name: string };
  user: { id: string; name: string; displayName: string };
  pubKeyCredParams: { type: string; alg: number }[];
  timeout: number;
  attestation: string;
  authenticatorSelection: Record<string, string>;
  excludeCredentials: { type: string; id: string }[];
};

/** The options an assertion ceremony needs (`navigator.credentials.get`). */
export type PasskeyRequestOptions = {
  challenge: string;
  rpId: string;
  allowCredentials: { type: string; id: string; transports?: string[] }[];
  timeout: number;
  userVerification: string;
};

/** List the signed-in account's own passkeys. */
export function fetchPasskeys(): Promise<{ passkeys: IamFactor[] }> {
  return request("/api/v1/auth/webauthn/passkeys");
}

/** Start a registration ceremony for the signed-in account. */
export function beginPasskeyRegistration(label?: string): Promise<PasskeyCreationOptions> {
  return request("/api/v1/auth/webauthn/register/begin", {
    method: "POST",
    body: JSON.stringify({ label: label ?? "" }),
  });
}

/** Finish a registration ceremony; the passkey is stored confirmed. */
export function completePasskeyRegistration(input: {
  challenge: string;
  label?: string;
  credential: PasskeyCredential;
}): Promise<{ factor: IamFactor; algorithm: string; passkeys: number }> {
  return request("/api/v1/auth/webauthn/register/complete", {
    method: "POST",
    body: JSON.stringify({
      challenge: input.challenge,
      label: input.label ?? "",
      credential: input.credential,
    }),
  });
}

/** Remove one of the signed-in account's passkeys (needs a fresh step-up). */
export function revokePasskey(factorId: string): Promise<{ factor_id: string; revoked: boolean }> {
  return request(`/api/v1/auth/webauthn/passkeys/${encodeURIComponent(factorId)}`, {
    method: "DELETE",
  });
}

/** Finish a half-done sign-in with a code (TOTP or a recovery code). */
export function verifyMfaChallenge(
  challenge: string,
  code: string,
): Promise<{ user: User; method: string; recovery_codes_remaining: number }> {
  return request("/api/v1/auth/mfa/verify", {
    method: "POST",
    body: JSON.stringify({ challenge, code }),
  });
}

/** Ask for the assertion options a passkey sign-in needs. */
export function beginPasskeySignIn(challenge: string): Promise<PasskeyRequestOptions> {
  return request("/api/v1/auth/webauthn/authenticate/begin", {
    method: "POST",
    body: JSON.stringify({ challenge }),
  });
}

/** Finish the sign-in with a passkey assertion; the session cookie comes back with it. */
export function completePasskeySignIn(input: {
  challenge: string;
  ceremonyChallenge: string;
  credential: PasskeyCredential;
}): Promise<{ user: User; device: unknown; expires_in: number }> {
  return request("/api/v1/auth/webauthn/authenticate/complete", {
    method: "POST",
    body: JSON.stringify({
      challenge: input.challenge,
      ceremony_challenge: input.ceremonyChallenge,
      credential: input.credential,
    }),
  });
}

/**
 * Prove identity again for a dangerous operation.
 *
 * `password` is the caller's own; an enrolled `code` works instead. The fresh mark lasts
 * `window_minutes`, which every dangerous route reads.
 */
export function stepUpSession(input: {
  password?: string;
  code?: string;
}): Promise<{ session_id: string; step_up: boolean; window_minutes: number }> {
  return request("/api/v1/auth/step-up", {
    method: "POST",
    body: JSON.stringify({
      ...(input.password !== undefined ? { password: input.password } : {}),
      ...(input.code !== undefined ? { code: input.code } : {}),
    }),
  });
}

/**
 * ABAC policies (REQ-006, slice 4a): the condition tree, the THEN block and the dry run.
 *
 * A policy is evaluated after the roles have had their say — an `allow` policy can grant what
 * RBAC did not, a `deny` policy takes away what RBAC granted, and the highest priority decides
 * first (equal priorities resolve to deny).
 */
export type IamPolicy = {
  id: string;
  organization_id: string;
  name: string;
  description: string;
  effect: "allow" | "deny";
  priority: number;
  conditions: unknown;
  target_permissions: string[];
  enabled: boolean;
  version: number;
  created_at: string;
  updated_at: string;
};

/** What a create or update carries. */
export type IamPolicyInput = {
  name: string;
  description?: string;
  effect: "allow" | "deny";
  priority: number;
  conditions: unknown;
  target_permissions: string[];
  enabled: boolean;
  organizationId?: string | null;
};

/** One leaf of the condition tree, as the dry run evaluated it. */
export type IamPolicyLeafTrace = {
  path: string;
  attribute: string;
  operator: string;
  expected: unknown;
  resolved: unknown;
  satisfied: boolean;
};

/** The dry run's answer. */
export type IamPolicyTest = {
  policy_id: string;
  policy_name: string;
  permission: string;
  draft: boolean;
  enabled: boolean;
  targeted: boolean;
  conditions_satisfied: boolean;
  applies: boolean;
  effect: "allow" | "deny";
  decides: boolean;
  decision: {
    effect: "allow" | "deny";
    policy_id: string;
    policy_name: string;
    priority: number;
  } | null;
  trace: IamPolicyLeafTrace[];
  attributes: Record<string, unknown>;
  note: string;
};

/** One recorded policy version. */
export type IamPolicyVersion = {
  version: number;
  effect: "allow" | "deny";
  priority: number;
  conditions: unknown;
  target_permissions: string[];
  enabled: boolean;
  created_at: string;
  changed_by: string | null;
};

/** List the organization's policies. */
export function fetchIamPolicies(
  organizationId?: string | null,
): Promise<{ organization_id: string; policies: IamPolicy[] }> {
  const query = organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
  return request(`/api/v1/iam/policies${query}`);
}

/** Create a policy. */
export function createIamPolicy(input: IamPolicyInput): Promise<IamPolicy> {
  return request("/api/v1/iam/policies", {
    method: "POST",
    body: JSON.stringify({
      name: input.name,
      description: input.description ?? "",
      effect: input.effect,
      priority: input.priority,
      conditions: input.conditions,
      target_permissions: input.target_permissions,
      enabled: input.enabled,
      ...(input.organizationId ? { organization_id: input.organizationId } : {}),
    }),
  });
}

/** Save a policy (the version moves forward). */
export function updateIamPolicy(id: string, input: IamPolicyInput): Promise<IamPolicy> {
  return request(`/api/v1/iam/policies/${id}`, {
    method: "PUT",
    body: JSON.stringify({
      name: input.name,
      description: input.description ?? "",
      effect: input.effect,
      priority: input.priority,
      conditions: input.conditions,
      target_permissions: input.target_permissions,
      enabled: input.enabled,
    }),
  });
}

/** Remove a policy. */
export function deleteIamPolicy(id: string): Promise<{ deleted: boolean }> {
  return request(`/api/v1/iam/policies/${id}`, { method: "DELETE" });
}

/**
 * Dry run: what would this policy do, and would it decide?
 *
 * Nothing is written; `policy` may carry an unsaved draft so the builder can test before saving.
 */
export function testIamPolicy(
  id: string,
  input: { permission: string; attributes: Record<string, unknown>; policy?: IamPolicyInput },
): Promise<IamPolicyTest> {
  return request(`/api/v1/iam/policies/${id}/test`, {
    method: "POST",
    body: JSON.stringify({
      permission: input.permission,
      attributes: input.attributes,
      ...(input.policy
        ? {
            policy: {
              name: input.policy.name,
              description: input.policy.description ?? "",
              effect: input.policy.effect,
              priority: input.policy.priority,
              conditions: input.policy.conditions,
              target_permissions: input.policy.target_permissions,
              enabled: input.policy.enabled,
            },
          }
        : {}),
    }),
  });
}

/** The recorded versions of a policy, newest first. */
export function fetchIamPolicyVersions(
  id: string,
): Promise<{ policy_id: string; current_version: number; versions: IamPolicyVersion[] }> {
  return request(`/api/v1/iam/policies/${id}/versions`);
}

// ---------------------------------------------------------------------------------------------
// Permission requests and approvals (REQ-006, slice 4b)
// ---------------------------------------------------------------------------------------------

/** One permission request, as the inbox shows it. */
export type IamApprovalRequest = {
  id: string;
  organization_id: string;
  permission_key: string;
  resource_type: string | null;
  resource_id: string | null;
  justification: string;
  status: "pending" | "approved" | "rejected" | "expired";
  requester: { id: string; email: string; name: string };
  decided_by: { id: string; email: string | null } | null;
  decided_at: string | null;
  decision_note: string;
  grant_minutes: number | null;
  binding_id: string | null;
  grant_expires_at: string | null;
  grant_active: boolean;
  created_at: string;
};

/** Counts behind the inbox tabs. */
export type IamApprovalCounts = {
  pending: number;
  approved: number;
  rejected: number;
  expired: number;
};

/** The inbox: requests of the organization, newest first. */
export function fetchIamApprovals(input: {
  organizationId?: string | null;
  status?: string;
}): Promise<{ organization_id: string; requests: IamApprovalRequest[]; counts: IamApprovalCounts }> {
  const params = new URLSearchParams();
  if (input.organizationId) params.set("organization_id", input.organizationId);
  if (input.status) params.set("status", input.status);
  const query = params.toString();
  return request(`/api/v1/iam/approvals${query ? `?${query}` : ""}`);
}

/** Approve (with a window) or reject a request. */
export function decideIamApproval(
  id: string,
  input: { decision: "approve" | "reject"; grantMinutes?: number; note?: string },
): Promise<IamApprovalRequest> {
  return request(`/api/v1/iam/approvals/${id}/decide`, {
    method: "POST",
    body: JSON.stringify({
      decision: input.decision,
      ...(input.grantMinutes ? { grant_minutes: input.grantMinutes } : {}),
      note: input.note ?? "",
    }),
  });
}

/** Ask for a permission (any signed-in account may ask for itself). */
export function createIamRequest(input: {
  permissionKey: string;
  justification?: string;
  resourceType?: string | null;
  resourceId?: string | null;
  organizationId?: string | null;
}): Promise<IamApprovalRequest> {
  return request("/api/v1/iam/requests", {
    method: "POST",
    body: JSON.stringify({
      permission_key: input.permissionKey,
      justification: input.justification ?? "",
      resource_type: input.resourceType ?? null,
      resource_id: input.resourceId ?? null,
      ...(input.organizationId ? { organization_id: input.organizationId } : {}),
    }),
  });
}

/** The caller's own requests. */
export function fetchMyIamRequests(
  organizationId?: string | null,
): Promise<{ organization_id: string; requests: IamApprovalRequest[] }> {
  const query = organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
  return request(`/api/v1/iam/requests${query}`);
}

// ---------------------------------------------------------------------------------------------
// SCIM provisioning (REQ-006, slice 4b)
// ---------------------------------------------------------------------------------------------

/** One provisioning token; the secret is only ever returned at minting. */
export type IamProvisioningToken = {
  id: string;
  organization_id: string;
  name: string;
  prefix: string;
  created_by: string | null;
  last_used_at: string | null;
  revoked_at: string | null;
  created_at: string;
};

/** One line of the SCIM sync log. */
export type IamSyncLogEntry = {
  id: number;
  organization_id: string;
  direction: string;
  resource: string;
  external_id: string | null;
  entity_id: string | null;
  action: string;
  outcome: string;
  detail: string;
  created_at: string;
};

/** The organization's provisioning tokens. */
export function fetchIamProvisioningTokens(
  organizationId?: string | null,
): Promise<{ organization_id: string; tokens: IamProvisioningToken[] }> {
  const query = organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
  return request(`/api/v1/iam/provisioning/tokens${query}`);
}

/** Mint a token; `secret` is shown once and never stored in readable form. */
export function createIamProvisioningToken(input: {
  name?: string;
  organizationId?: string | null;
}): Promise<{ token: IamProvisioningToken; secret: string }> {
  return request("/api/v1/iam/provisioning/tokens", {
    method: "POST",
    body: JSON.stringify({
      name: input.name ?? "",
      ...(input.organizationId ? { organization_id: input.organizationId } : {}),
    }),
  });
}

/** Revoke a token. */
export function revokeIamProvisioningToken(
  id: string,
): Promise<{ revoked: boolean; token: IamProvisioningToken | null }> {
  return request(`/api/v1/iam/provisioning/tokens/${id}`, { method: "DELETE" });
}

/** The sync log, newest first. */
export function fetchIamProvisioningLog(input: {
  organizationId?: string | null;
  limit?: number;
}): Promise<{ organization_id: string; log: IamSyncLogEntry[] }> {
  const params = new URLSearchParams();
  if (input.organizationId) params.set("organization_id", input.organizationId);
  if (input.limit) params.set("limit", String(input.limit));
  const query = params.toString();
  return request(`/api/v1/iam/provisioning/log${query ? `?${query}` : ""}`);
}

// ---------------------------------------------------------------------------------------------
// Automations (docs/requests/REQ-003) — trigger → condition → action
// ---------------------------------------------------------------------------------------------

/** One payload field of a documented event, as the condition picker offers it. */
export type AutomationEventField = {
  /** The field as it appears in the payload, dotted for nesting. */
  key: string;
  /** `string`, `number`, `boolean`, `array` or `object`. */
  kind: string;
  /** What the field holds, in product language. */
  label: string;
};

/** One event of the library. */
export type AutomationEvent = {
  /** Event name as the bus records it. */
  name: string;
  /** What happened, in product language. */
  description: string;
  /** The group the picker files it under. */
  group: string;
  /** The payload fields a condition or binding may read. */
  fields: AutomationEventField[];
  /** `true` when the event belongs to a site. */
  site_scoped: boolean;
};

/** One operator of the closed comparison set. */
export type AutomationOperator = {
  /** Stable operator key. */
  key: string;
  /** `false` for the two existence operators, which take no value. */
  needs_value: boolean;
};

/** One action of the closed action set. */
export type AutomationAction = {
  /** Stable action key. */
  key: string;
  /** What it does, in product language. */
  description: string;
  /** `true` when the action touches the world rather than the run only. */
  host: boolean;
};

/** The closed vocabulary a rule is written in. */
export type AutomationCatalogue = {
  /** The event library, with the payload fields each event carries. */
  events: AutomationEvent[];
  /** The closed comparison set. */
  condition_operators: AutomationOperator[];
  /** The closed action set. */
  actions: AutomationAction[];
  /** How the rule starts. */
  trigger_kinds: string[];
  /** The condition group modes the editor offers. */
  group_modes: string[];
  /** How deep groups may nest. */
  max_group_depth: number;
  /** How many conditions and groups a rule may carry in total. */
  max_conditions: number;
  /** The event a webhook trigger listens for. */
  hook_event: string;
  /** The hook path with the token left out. */
  hook_path_template: string;
  /** How a payload field is named inside a condition or a binding. */
  binding_syntax: string;
  /** An example of the payload an inbound call produces. */
  hook_sample: Record<string, unknown>;
  /** The operators a `branch` step offers — the same nine the conditions use. */
  branch_operators: AutomationOperator[];
  /** The step kinds a definition may carry. */
  step_kinds: string[];
  /** The permission that may decide a parked `approval` step. */
  approval_permission: string;
  /** The default and the ceiling of a gate's lifetime, in hours. */
  approval_ttl_hours: number;
  max_approval_ttl_hours: number;
  /** What a step's own failure may do; `inherit` takes the rule's policy. */
  on_error_policies: AutomationStepOnError[];
  /** The longest a step may block, in milliseconds, and the default. */
  max_step_timeout_ms: number;
  default_step_timeout_ms: number;
  /** The methods an outbound call may use. */
  outbound_methods: string[];
  /** How many rules deep a `run_workflow` chain may go. */
  max_chain_depth: number;
};

/** One run of a rule, as the run detail reads it. */
export type AutomationExecution = {
  /** Run id. */
  id: string;
  /** Rule the run belongs to. */
  workflow_id: string;
  /** `running`, `completed`, `failed` or `cancelled`. */
  status: string;
  /**
   * How the run started.
   *
   * The list endpoint names this `trigger` and the detail `trigger_kind`; both are mapped
   * onto this one field by the fetcher below, so the panel never has to know which endpoint
   * answered.
   */
  trigger_kind: string;
  /** The list endpoint's spelling of {@link trigger_kind}, normalised onto it. */
  trigger?: string;
  /** When it started. */
  started_at: string;
  /** When it finished, if it has. */
  finished_at: string | null;
  /** The failing step's message, when the run failed. */
  error: string | null;
};

/** One step of a run's trace. */
export type AutomationRunStep = {
  /** 1-based position. */
  step_no: number;
  /** Step name. */
  name: string;
  /** `task`, `wait`, `branch`, `stop` or `approval`. */
  kind: string;
  /** Action key of a task step. */
  action: string | null;
  /** `pending`, `running`, `waiting`, `succeeded`, `failed` or `cancelled`. */
  status: string;
  /** Attempts made so far. */
  attempts: number;
  /** Attempts allowed in total. */
  max_attempts: number;
  /** What this step's own failure does. */
  on_error: string;
  /** How long this step may block, in milliseconds. */
  timeout_ms: number;
  /** `true` when the run deliberately outlived this step's failure. */
  ignored: boolean;
  /** The step's output, when it succeeded. */
  output: Record<string, unknown> | null;
  /** The last failure's message. */
  error: string | null;
  /**
   * When the current attempt started, and when the step reached a terminal state.
   *
   * Both are what makes the trace's duration column honest: without them every step reads
   * as having taken no time, which is indistinguishable from having run instantly.
   */
  started_at?: string | null;
  finished_at?: string | null;
};

/** The run detail: the run, its steps and the payload it started from. */
export type AutomationRunDetail = AutomationExecution & {
  /** Steps, in order. */
  steps: AutomationRunStep[];
  /** The event payload the run started from. */
  event_payload?: Record<string, unknown>;
  /** `true` when this run offers Retry. */
  can_retry: boolean;
  /** `true` when this run is still going. */
  can_cancel: boolean;
};

/** What "Run now" started. */
export type AutomationRunStarted = {
  /** The run that started. */
  execution_id: string;
  /** The rule it belongs to. */
  workflow_id: string;
  /** How many steps it carries. */
  steps: number;
};

/** What a retry or a resume re-queued. */
export type AutomationRetryResult = {
  /** The run that was re-opened. */
  execution_id: string;
  /** The step the operator pointed at. */
  step_no: number;
  /** How many steps went back on the queue. */
  requeued: number;
};

/** One comparison inside a condition group. */
export type AutomationCondition = {
  /** Field path into the event payload. */
  field: string;
  /** How the field is compared. */
  operator: string;
  /** Value to compare with; absent for the two existence operators. */
  value?: unknown;
};

/** One member of a condition group: a comparison or a nested group. */
export type AutomationNode = AutomationCondition | AutomationGroup;

/** A group of condition nodes. */
export type AutomationGroup = {
  /** `all` (every member holds) or `any` (one member holds). */
  mode?: "all" | "any";
  all?: AutomationNode[];
  any?: AutomationNode[];
  /** Members of a nested group, when the panel holds the flat editing shape. */
  nodes?: AutomationNode[];
};

/** The hook surface of a webhook-triggered rule. */
export type AutomationHook = {
  /** `true` once a token has been minted. */
  configured: boolean;
  /** How many calls the current window has spent. */
  window_used: number;
  /** The window's ceiling. */
  window_limit: number;
  /** When the window rolls over. */
  window_resets_at: string;
  /** The path template, with the token left out. */
  path_template: string;
};

/** One automation rule. */
export type Automation = {
  /** Rule id. */
  id: string;
  /** Organization that owns the rule. */
  organization_id: string;
  /** Site the rule is bound to, when it is. */
  site_id: string | null;
  /** Display name. */
  name: string;
  /** Free-form description. */
  description: string;
  /** Whether the rule fires. */
  enabled: boolean;
  /** Event the rule listens for. */
  event: string;
  /** How the rule starts: `event` or `inbound_webhook`. */
  trigger: string;
  /** The condition tree as stored. */
  conditions: AutomationGroup | AutomationCondition[];
  /** How many comparisons the tree carries. */
  condition_count: number;
  /** Actions to run, in order. */
  actions: AutomationNode[];
  /** The rule's own error policy; a step that inherits takes this. */
  on_error: AutomationOnError;
  /** Whose authority the rule's host actions run with; `null` means the author. */
  run_as_user_id: string | null;
  /** Which of the two it is, in a sentence the editor shows beside the picker. */
  run_as_description: string;
  /** What each host action needs, so the panel can say what a run-as account is asked for. */
  action_permissions: [string, string][];
  /** How many runs the trigger has started. */
  trigger_count: number;
  /**
   * The version a graph write must quote.
   *
   * Present on the list for the same reason it is on the row: a rule opened from a list row
   * and then saved has no other way to learn it, and a client that guessed `0` would be
   * refused with a conflict about a version the author was never shown.
   */
  graph_version: number;
  /** When the rule last fired. */
  last_triggered_at: string | null;
  /** Creation time. */
  created_at: string;
  /** Last change. */
  updated_at: string;
  /** The hook surface, for a webhook-triggered rule. */
  hook?: AutomationHook;
};

/** What a step's own failure does; `inherit` takes the rule's policy. */
export type AutomationStepOnError = "inherit" | "stop" | "continue";

/** The rule's own error policy. A rule may not choose `inherit` — that is a step's. */
export type AutomationOnError = "stop" | "continue";

/** One step of a rule's action list. */
export type AutomationStep = {
  /** Display name; unique within the rule. */
  name: string;
  /** `task`, `wait`, `branch`, `stop` or `approval`. */
  kind: string;
  /** Action key of a task step. */
  action?: string | null;
  /** Action parameters. */
  params: Record<string, unknown>;
  /** What this step's own failure does. */
  on_error?: AutomationStepOnError;
  /** How long this step may block, in milliseconds. */
  timeout_ms?: number;
  /** Attempts allowed in total. */
  max_attempts: number;
};

/** The condition report of a dry run: one row, answered. */
export type AutomationConditionReport = {
  /** The comparison as the author wrote it. */
  condition: AutomationCondition;
  /** `true` when the payload satisfied it. */
  holds: boolean;
  /** The payload's value for the field, when it had one. */
  found?: unknown;
};

/** One group of a dry-run report. */
export type AutomationGroupReport = {
  /** `all` or `any`. */
  mode: string;
  /** What the group answered. */
  holds: boolean;
  /** The members, in order. */
  nodes: Array<AutomationConditionReport | AutomationGroupReport>;
};

/** What one action of a dry run would have done. */
export type AutomationActionReport = {
  /** Step name, so the report lines up with the editor. */
  name: string;
  /** The action key. */
  action: string;
  /** `true` when the action touches the world. */
  host: boolean;
  /** `would_send`, `would_call`… — what the step would do. */
  outcome: string;
  /** The parameters after the payload was resolved into them. */
  params: Record<string, unknown>;
  /** A readable one-line summary, when the action has one. */
  summary?: string;
};

/** The whole dry-run report. */
export type AutomationDryRun = {
  /** `true` when the conditions held and the actions would have run. */
  would_run: boolean;
  /** Why the rule would not run, when it would not. */
  reason: string | null;
  /** The condition tree, answered row by row. */
  conditions: AutomationGroupReport;
  /** The actions, in order. */
  actions: AutomationActionReport[];
  /** Nothing in this report was sent, published or called. */
  simulated: boolean;
};

/** One stored test report or captured payload. */
export type AutomationTestEvent = {
  /** Row id. */
  id: string;
  /** `test` or `listen`. */
  kind: string;
  /** The payload the row carries. */
  payload: Record<string, unknown> | null;
  /** Event id, when a listener captured one. */
  event_id: number | null;
  /** Event name, when a listener captured one. */
  event_name: string | null;
  /** `true` while a listener waits for its next event. */
  armed: boolean;
  /** When the row was written. */
  created_at: string;
  /** When a listener filled in. */
  captured_at: string | null;
};

/** The answer of a dry run. */
export type AutomationTestResult = {
  /** The report, row by row. */
  report: AutomationDryRun;
  /** The stored report. */
  recorded: AutomationTestEvent;
};

/**
 * One gate a parked automation run is waiting on (REQ-003 slice 3).
 *
 * There is deliberately **no token** on this type. The panel decides through the decider's
 * own session and the gate's own id, so reading the queue can never hand out the credential
 * that opens it — the token is minted by the engine when the run parks and travels to the
 * decider out of band, exactly as the inbound hook token does.
 */
export type AutomationApproval = {
  /** Approval id — what the decision endpoint is addressed by. */
  id: string;
  /** The run that is parked. */
  execution_id: string;
  /** The step inside that run. */
  step_no: number;
  /** The step's name. */
  step_name: string;
  /** The rule that asked, when the rule still exists. */
  rule_id: string | null;
  /** Its name. */
  rule_name: string | null;
  /** Organization the gate belongs to. */
  organization_id: string;
  /** When the run parked. */
  requested_at: string;
  /** When the gate stops accepting decisions. */
  expires_at: string;
  /** The permission a decider must hold. */
  permission: string;
  /** The message the author wrote for the decider. */
  message: string;
  /** `true` when the deadline has passed; the panel then offers only Reject. */
  expired: boolean;
};

/** The answer of a decision: what happened to the run. */
export type AutomationDecisionResult = {
  /** The gate that was decided. */
  approval_id: string;
  /** What it was decided as. */
  decision: "approved" | "rejected";
  /** The run that was let go (approved) or ended (rejected). */
  execution_id: string;
  /** `running` after an approval, `cancelled` after a rejection. */
  execution_status: string;
};

/** `GET /api/v1/approvals` — the gates waiting in one organization. */
export function fetchApprovals(
  options: { organizationId?: string | null; status?: "pending" | "decided" } = {},
): Promise<{ approvals: AutomationApproval[]; total: number }> {
  const query = new URLSearchParams();
  if (options.organizationId) {
    query.set("organization_id", options.organizationId);
  }
  query.set("status", options.status ?? "pending");
  return request<{ approvals: AutomationApproval[]; total: number }>(
    `/api/v1/approvals?${query.toString()}`,
  );
}

/**
 * `POST /api/v1/approvals/{id}/decide` — let a parked run go on, or end it.
 *
 * The token is sent in the body, never in the path: a token in a URL is written to every
 * access log on the way in, and an approval is a credential that can let a message leave
 * the process. A repeat press is answered `200` with the decision the gate already has,
 * so a double click cannot apply twice.
 *
 * **No token is sent from the panel**, and that is the point of the split: the *authority*
 * to decide is the session's `workflows.approve`, which this screen's route guard already
 * checked, while the token is the second factor a notification carries. A decider who
 * followed a link brings one and it is checked; a decider who opened the panel brings their
 * session, which is the same power by a different route.
 */
export function decideApproval(
  approvalId: string,
  decision: "approved" | "rejected",
  note?: string,
): Promise<AutomationDecisionResult> {
  return request<AutomationDecisionResult>(
    `/api/v1/approvals/${encodeURIComponent(approvalId)}/decide`,
    { method: "POST", body: JSON.stringify({ decision, note: note ?? null }) },
  );
}

/** A newly minted hook token — the only response that carries one. */
export type AutomationHookToken = {
  /** The URL to give the caller, token included. */
  url: string;
  /** The token on its own. */
  token: string;
  /** The rule the URL belongs to. */
  automation_id: string;
};

/** The rule list. */
export function fetchAutomations(organizationId?: string): Promise<{ automations: Automation[] }> {
  const query = organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
  return request<{ automations: Automation[] }>(`/api/v1/automations${query}`);
}

/** The closed vocabulary a rule is written in. */
export function fetchAutomationCatalogue(): Promise<AutomationCatalogue> {
  return request<AutomationCatalogue>("/api/v1/automations/catalogue");
}

/** One rule. */
export function fetchAutomation(automationId: string): Promise<Automation> {
  return request<Automation>(`/api/v1/automations/${automationId}`);
}

/**
 * A rule to write.
 *
 * One shape for both `createAutomation` and `updateAutomation`, so a field added here
 * cannot reach one and miss the other — which is how a rule's error policy ends up
 * quietly resetting to the default every time somebody toggles a rule.
 */
export type AutomationInput = {
  organization_id?: string | null;
  site_id?: string | null;
  name: string;
  description?: string;
  enabled?: boolean;
  event: string;
  conditions?: unknown;
  hook_triggered?: boolean;
  /** The rule's own failure policy; a step that inherits takes this. */
  on_error?: AutomationOnError;
  /**
   * Whose authority the rule's host actions run with; `null` follows the author.
   *
   * The API resolves it at *run* time, so handing a rule to a service account changes what
   * happens from the next run — which is what a settings field is expected to do.
   */
  run_as_user_id?: string | null;
  actions: AutomationStep[];
};

/** Write a rule. */
export function createAutomation(input: AutomationInput): Promise<Automation> {
  return request<Automation>("/api/v1/automations", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Replace a rule. */
export function updateAutomation(
  automationId: string,
  input: AutomationInput,
): Promise<Automation> {
  return request<Automation>(`/api/v1/automations/${automationId}`, {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

/** Remove a rule and its run history. */
export function deleteAutomation(automationId: string): Promise<null> {
  return request<null>(`/api/v1/automations/${automationId}`, { method: "DELETE" });
}

/** Evaluate a hand-written payload against a rule without touching anything. */
export function testAutomation(automationId: string, payload: unknown): Promise<AutomationTestResult> {
  return request<AutomationTestResult>(`/api/v1/automations/${automationId}/test`, {
    method: "POST",
    body: JSON.stringify({ payload }),
  });
}

/** Arm a one-shot listener for a rule's next real event. */
export function listenAutomation(automationId: string): Promise<AutomationTestEvent> {
  return request<AutomationTestEvent>(`/api/v1/automations/${automationId}/listen`, {
    method: "POST",
  });
}

/** A rule's test reports and captured payloads, newest first. */
export function fetchAutomationTests(automationId: string): Promise<{ tests: AutomationTestEvent[] }> {
  return request<{ tests: AutomationTestEvent[] }>(`/api/v1/automations/${automationId}/tests`);
}

/** Mint a fresh inbound-webhook token; the response is the only place one appears. */
export function rotateAutomationHook(automationId: string): Promise<AutomationHookToken> {
  return request<AutomationHookToken>(`/api/v1/automations/${automationId}/rotate-hook`, {
    method: "POST",
  });
}

/**
 * Start one run now, in the real world.
 *
 * This is the one automations control that sends, publishes and calls for real — the
 * dry run beside it is the simulation, and this is not. The response is the run, so the
 * panel can link straight to its trace.
 */
export function runAutomation(automationId: string): Promise<AutomationRunStarted> {
  return request<AutomationRunStarted>(`/api/v1/automations/${automationId}/run`, {
    method: "POST",
    body: JSON.stringify({}),
  });
}

/** One run with its step trace. */
export function fetchAutomationRun(executionId: string): Promise<AutomationRunDetail> {
  return request<AutomationRunDetail>(`/api/v1/workflow-executions/${executionId}`);
}

/* ---------------------------------------------------------------------------------------------
 * *Listen for a real event* (REQ-004 slice 3, criterion 5)
 *
 * Arming and reading are **two calls and not one that does both**, and the reason is the
 * one that only shows up in production: a panel that polls by POSTing re-arms its own
 * listener on every tick, so the row the matcher fills is a row the previous tick deleted —
 * the capture appears for one frame and the author watches a spinner and nothing else. The
 * `readWorkflowListeners` call is the only one the panel repeats.
 */

/** One armed (or spent) listener, as the builder's inspector reads it. */
export interface WorkflowListener {
  id: string;
  node_id: string;
  event_name: string;
  /** `armed` | `captured` | `expired` — derived by the server, never stored. */
  status: "armed" | "captured" | "expired";
  armed_at: string;
  expires_at: string;
  /** Seconds left, clamped at zero by the server so a bar can be drawn from it. */
  expires_in_seconds: number;
  captured_at?: string;
  event_id?: number;
  payload: Record<string, unknown> | null;
  payload_text?: string;
}

/** What arming answers. The token is here and nowhere else. */
export interface WorkflowListenerArmed {
  listener: WorkflowListener;
  token: string;
  expires_in_seconds: number;
}

/** What the read answers. */
export interface WorkflowListenerList {
  listeners: WorkflowListener[];
  armed: number;
  captured?: WorkflowListener;
}

/** Arm a one-shot listener for one node. This is the only call that mints a token. */
export function armWorkflowListener(
  workflowId: string,
  nodeId: string,
): Promise<WorkflowListenerArmed> {
  return request<WorkflowListenerArmed>(
    `/api/v1/workflows/${encodeURIComponent(workflowId)}/listen`,
    { method: "POST", body: JSON.stringify({ node_id: nodeId }) },
  );
}

/** The rule's listeners and whatever they captured. Safe to repeat; arms nothing. */
export function readWorkflowListeners(workflowId: string): Promise<WorkflowListenerList> {
  return request<WorkflowListenerList>(
    `/api/v1/workflows/${encodeURIComponent(workflowId)}/listeners`,
  );
}

/** Read one listener back by its token — the handle a caller scripts against. */
export function readWorkflowListener(
  workflowId: string,
  token: string,
): Promise<WorkflowListener> {
  return request<WorkflowListener>(
    `/api/v1/workflows/${encodeURIComponent(workflowId)}/listeners/${encodeURIComponent(token)}`,
  );
}

/**
 * Try a failed run again from one step.
 *
 * The chosen step **and everything after it** go back on the queue: re-running only the
 * failed step would let a run whose middle failed march on to completion, which is not
 * what "try that again" means to anybody reading a trace. The steps that already
 * succeeded are left exactly as they are.
 */
export function retryAutomationStep(
  executionId: string,
  stepNo: number,
): Promise<AutomationRetryResult> {
  return request<AutomationRetryResult>(
    `/api/v1/workflow-executions/${executionId}/retry-step`,
    { method: "POST", body: JSON.stringify({ step_no: stepNo }) },
  );
}

/** The same write as {@link retryAutomationStep}, named for what the button says. */
export function resumeAutomationFrom(
  executionId: string,
  stepNo: number,
): Promise<AutomationRetryResult> {
  return request<AutomationRetryResult>(
    `/api/v1/workflow-executions/${executionId}/resume-from`,
    { method: "POST", body: JSON.stringify({ step_no: stepNo }) },
  );
}

/* ---------------------------------------------------------------------------------------------
 * Enterprise sign-in providers (REQ-006, slice 4b-2; docs/07-IAM.md §11)
 *
 * A client secret never appears in any of these shapes: `secret_ref` is the *name* of the
 * environment variable it lives in, and `secret_present` says whether this installation defines
 * it. That is what lets the screen tell "not set up" from "broken" without the value ever
 * crossing the wire.
 * ------------------------------------------------------------------------------------------- */

/** One connected sign-in provider. */
export type IamAuthProvider = {
  id: string;
  organization_id: string;
  slug: string;
  kind: "oidc" | "oauth2" | "saml";
  name: string;
  config: Record<string, unknown>;
  secret_ref: string | null;
  secret_present: boolean;
  scopes: string[];
  group_claim: string | null;
  default_role_id: string | null;
  jit_enabled: boolean;
  enabled: boolean;
  created_at: string;
  updated_at: string;
  sign_in_count: number;
  last_sign_in_at: string | null;
};

/** One line of a provider's sign-in log. */
export type IamProviderEvent = {
  outcome: "success" | "provisioned" | "updated" | "refused" | "error";
  reason: string | null;
  external_subject: string | null;
  user_id: string | null;
  roles_applied: string;
  ip_address: string | null;
  created_at: string;
};

/** The answer of the discovery test — a failed test is a `200` with `status: "failed"`. */
export type IamProviderTest = {
  provider_id: string;
  slug: string;
  kind: "oidc" | "oauth2" | "saml";
  status: "ok" | "failed";
  detail: string;
  endpoints?: Record<string, unknown> | null;
  secret_present: boolean;
};

/** The provider list, with the kinds the form offers. */
export function fetchIamProviders(
  organizationId?: string | null,
): Promise<{
  organization_id: string;
  providers: IamAuthProvider[];
  kinds: { value: string; label: string; default_scopes: string[] }[];
}> {
  const query = organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
  return request(`/api/v1/iam/providers${query}`);
}

/** Connect a provider. It is created switched off — `test` proves it, `enabled` publishes it. */
export function createIamProvider(input: {
  slug: string;
  kind: string;
  name: string;
  config: Record<string, unknown>;
  secretRef?: string | null;
  scopes?: string[];
  groupClaim?: string | null;
  jitEnabled?: boolean;
  organizationId?: string | null;
}): Promise<IamAuthProvider> {
  return request("/api/v1/iam/providers", {
    method: "POST",
    body: JSON.stringify({
      slug: input.slug,
      kind: input.kind,
      name: input.name,
      config: input.config,
      ...(input.secretRef ? { secret_ref: input.secretRef } : {}),
      ...(input.scopes?.length ? { scopes: input.scopes } : {}),
      ...(input.groupClaim ? { group_claim: input.groupClaim } : {}),
      ...(typeof input.jitEnabled === "boolean" ? { jit_enabled: input.jitEnabled } : {}),
      ...(input.organizationId ? { organization_id: input.organizationId } : {}),
    }),
  });
}

/** Change a provider. Only the fields present are changed. */
export function updateIamProvider(
  id: string,
  input: {
    name?: string;
    config?: Record<string, unknown>;
    secretRef?: string | null;
    scopes?: string[];
    groupClaim?: string | null;
    jitEnabled?: boolean;
    enabled?: boolean;
  },
): Promise<IamAuthProvider> {
  return request(`/api/v1/iam/providers/${id}`, {
    method: "PATCH",
    body: JSON.stringify({
      ...(input.name !== undefined ? { name: input.name } : {}),
      ...(input.config !== undefined ? { config: input.config } : {}),
      ...(input.secretRef !== undefined ? { secret_ref: input.secretRef } : {}),
      ...(input.scopes !== undefined ? { scopes: input.scopes } : {}),
      ...(input.groupClaim !== undefined ? { group_claim: input.groupClaim } : {}),
      ...(input.jitEnabled !== undefined ? { jit_enabled: input.jitEnabled } : {}),
      ...(input.enabled !== undefined ? { enabled: input.enabled } : {}),
    }),
  });
}

/** Remove a provider. Its challenges and its event log go with it. */
export function deleteIamProvider(id: string): Promise<null> {
  return request(`/api/v1/iam/providers/${id}`, { method: "DELETE" });
}

/** Ask the provider what it actually is. A broken provider is a result, not a transport error. */
export function testIamProvider(id: string): Promise<IamProviderTest> {
  return request(`/api/v1/iam/providers/${id}/test`, { method: "POST" });
}

/** A provider's sign-in log, newest first. */
export function fetchIamProviderEvents(
  id: string,
  input: { organizationId?: string | null; limit?: number } = {},
): Promise<{ provider_id: string; events: IamProviderEvent[] }> {
  const params = new URLSearchParams();
  if (input.organizationId) params.set("organization_id", input.organizationId);
  if (input.limit) params.set("event_limit", String(input.limit));
  const query = params.toString();
  return request(`/api/v1/iam/providers/${id}/events${query ? `?${query}` : ""}`);
}

/** The providers a person may sign in with — the public list the sign-in screen renders. */
export function fetchSsoProviders(): Promise<{
  organization_id: string;
  providers: { slug: string; name: string; kind: string; start_url: string }[];
}> {
  return request("/api/v1/auth/sso/providers");
}

/* ---------------------------------------------------------------------------------------------
 * The automation operations surfaces (REQ-003 slice 4)
 *
 * Three reads an operator reaches for when a rule is not doing what its author expected —
 * "what did this look like on Tuesday", "who changed it", "what would a starter look like" —
 * and one write among them, the restore. The restore is a **definition write**, so the API
 * guards it with `workflows.manage` and audits it exactly like a save; the reads carry
 * `workflows.read`, the same power that reads the rule.
 * ------------------------------------------------------------------------------------------- */

/** One stored version of a rule, as the Versions tab lists it. */
export type AutomationVersion = {
  /** Version row id — what *Restore* is addressed by. */
  id: string;
  /** The rule it belongs to. */
  automation_id: string;
  /** The number it was written as. */
  version: number;
  /** `created`, `updated` or `restored`. */
  change: string;
  /**
   * What changed, in words.
   *
   * `{ changed: [{ field, from, to }], count }`, and `{ first: true }` for the version a
   * rule was born as — which has nothing to be different from and says so rather than
   * reporting an empty change set that reads like "you changed nothing".
   */
  summary: AutomationVersionSummary;
  /** The whole definition as it was written. */
  definition: Record<string, unknown>;
  /** Who wrote it; `null` when the account has since been deleted. */
  created_by: string | null;
  /** When, RFC 3339. */
  created_at: string;
  /** The version whose content was restored, when this row is a restore. */
  restored_from: string | null;
  /** `true` when the rule as it stands is this row. */
  current: boolean;
};

/** One line of a version's diff: the field, and the two values it moved between. */
export type AutomationVersionChange = {
  /** Which field of the definition moved. */
  field: string;
  /** What it was. */
  from: unknown;
  /** What it became. */
  to: unknown;
};

/**
 * What one write changed.
 *
 * A fixed list of fields rather than a structural diff: a structural walk reports every key
 * a later slice added as a change on every edit, and needs rewriting the first time the
 * definition's shape changes — which is the thing that happens most often here.
 */
export type AutomationVersionSummary = {
  /** The fields that moved, in the order the panel lists them. */
  changed?: AutomationVersionChange[];
  /** How many, derived from the list so the number cannot disagree with the rows. */
  count?: number;
  /** `true` for the version a rule was created as. */
  first?: boolean;
};

/** The Versions tab payload. */
export type AutomationVersionList = {
  /** The rule the history belongs to. */
  automation_id: string;
  /** The number the rule is on now. */
  current_version: number;
  /** History, newest first. */
  versions: AutomationVersion[];
  /**
   * `true` when the rule has no history row at all.
   *
   * A rule written before this feature shipped is *untracked*, not *unchanged*, and the tab
   * says which one it is looking at.
   */
  untracked: boolean;
};

/** One version with its diff, as `GET …/versions/{id}` answers it. */
export type AutomationVersionComparison = {
  /** The version being looked at. */
  version: AutomationVersion;
  /** The number it was compared against, or `null` for the first version. */
  compared_to: number | null;
  /** What changed, in words. */
  summary: AutomationVersionSummary;
};

/** A rule's definition history, newest first. */
export function fetchAutomationVersions(
  automationId: string,
): Promise<AutomationVersionList> {
  return request<AutomationVersionList>(
    `/api/v1/automations/${encodeURIComponent(automationId)}/versions`,
  );
}

/** One version, with what it changed against the one before it. */
export function fetchAutomationVersion(
  automationId: string,
  versionId: string,
): Promise<AutomationVersionComparison> {
  return request<AutomationVersionComparison>(
    `/api/v1/automations/${encodeURIComponent(automationId)}/versions/${encodeURIComponent(
      versionId,
    )}`,
  );
}

/**
 * Put a stored definition back.
 *
 * **Appends** rather than rewinds: the old content becomes the next version and
 * `restored_from` says where it came from, so the history stays a line and "v3 → v1 → v3"
 * never looks like a bug. The rule keeps its id, its run history and its webhook token.
 *
 * The body is empty by design — a caller that could send its own definition here would be
 * able to write a rule the panel never validated.
 */
export function restoreAutomationVersion(
  automationId: string,
  versionId: string,
): Promise<AutomationVersion> {
  return request<AutomationVersion>(
    `/api/v1/automations/${encodeURIComponent(automationId)}/versions/${encodeURIComponent(
      versionId,
    )}/restore`,
    { method: "POST", body: JSON.stringify({}) },
  );
}

/** One audit row, as the Audit tab lists it. */
export type AutomationAuditEntry = {
  /** Row id. */
  id: number;
  /** Stable action name, e.g. `automation.updated`. */
  action: string;
  /** Who did it, when a person did. */
  actor_user_id: string | null;
  /** `user`, `agent`, `service` or `system`. */
  actor_type: string;
  /** What was acted on. */
  target_type: string | null;
  /** Its id, as text. */
  target_id: string | null;
  /** Structured detail; never carries secrets. */
  metadata: Record<string, unknown>;
  /** When, RFC 3339. */
  created_at: string;
};

/** Who changed this rule, and when. */
export function fetchAutomationAudit(
  automationId: string,
  input: { limit?: number } = {},
): Promise<{ automation_id: string; entries: AutomationAuditEntry[] }> {
  const query = input.limit ? `?limit=${input.limit}` : "";
  return request(`/api/v1/automations/${encodeURIComponent(automationId)}/audit${query}`);
}

/** One starter rule in the gallery. */
export type AutomationTemplate = {
  /** Stable key; the gallery's row identity. */
  key: string;
  /** Display name. */
  name: string;
  /** One line about what it does. */
  description: string;
  /** The category the gallery groups by. */
  category: string;
  /** The event the rule listens for. */
  event: string;
  /** How many conditions it starts with. */
  condition_count: number;
  /** How many actions it starts with. */
  action_count: number;
  /**
   * What has to be filled in before it can run.
   *
   * Named rather than guessed: a template that says "needs a destination" is honest, and the
   * request's criterion is that a starter is savable "without edits beyond its missing
   * credentials".
   */
  requires: string[];
  /** `false` when an action's host is not on the allow-list yet. */
  installable: boolean;
  /** Why it is not installable, when it is not. */
  blocked_reason: string | null;
  /** The request body `POST /api/v1/automations` takes, verbatim. */
  body: Record<string, unknown>;
};

/** The six starter rules, in gallery order. */
export function fetchAutomationTemplates(): Promise<{ templates: AutomationTemplate[] }> {
  return request<{ templates: AutomationTemplate[] }>("/api/v1/automations/templates");
}

/* ---------------------------------------------------------------------------------------------
 * The run history and the two controls that act on a run
 *
 * A rule is a workflow whose trigger is an event, so the run history is the *workflow*
 * execution list — inventing an automation-specific one would mean two answers to "what did
 * this rule do last week". The two controls that repair a run (retry and resume) are the same
 * write on purpose: re-running only the failed step would let a run whose middle failed march
 * on to completion, which is not what "try that again" means to anybody reading a trace.
 * ------------------------------------------------------------------------------------------- */

/** One run of a rule, as the run **list** answers it (no steps). */
export type AutomationRunSummary = {
  /** Execution id — the trace route is addressed by it. */
  id: string;
  /** The rule that ran. */
  workflow_id: string;
  /** `running`, `completed`, `failed`, `cancelled` or `awaiting_approval`. */
  status: string;
  /** `manual`, `schedule` or `event`. */
  trigger: string;
  /** When it started, RFC 3339. */
  started_at: string;
  /** When it settled. */
  finished_at: string | null;
  /** The failing step's message, when the run failed. */
  error: string | null;
  /** How many steps the run carries. */
  step_count?: number;
};

/**
 * The run history of one rule, newest first.
 *
 * `step_count` is filled here when the API sent a step array and left `undefined` when it
 * did not, so the column shows an em dash rather than a fabricated zero — "the API did not
 * say" and "the run had no steps" are different facts.
 */
export async function fetchAutomationRunHistory(
  automationId: string,
  limit = 50,
): Promise<AutomationRunSummary[]> {
  const answer = await request<{
    workflow_id: string;
    executions: (AutomationRunSummary & { steps?: unknown[] })[];
  }>(`/api/v1/workflows/${encodeURIComponent(automationId)}/executions?limit=${limit}`);

  return answer.executions.map((run) => ({
    ...run,
    step_count: Array.isArray(run.steps) ? run.steps.length : undefined,
  }));
}

/** Stop a running execution. */
export function cancelAutomationRun(executionId: string): Promise<AutomationRunDetail> {
  return request<AutomationRunDetail>(
    `/api/v1/workflow-executions/${encodeURIComponent(executionId)}/cancel`,
    { method: "POST", body: JSON.stringify({}) },
  );
}
// ---------------------------------------------------------------------------------------------
// Transformation presets (docs/requests/REQ-010, slice 3)
// ---------------------------------------------------------------------------------------------

/** Every transformation preset of a site. */
export function fetchMediaPresets(siteId: string): Promise<{ presets: MediaPreset[] }> {
  return request<{ presets: MediaPreset[] }>(
    `/api/v1/media/transformation-presets?${mediaQuery(siteId)}`,
  );
}

/** What a create or edit carries. Every field is optional except the name and a dimension. */
export type MediaPresetInput = {
  name: string;
  width?: number | null;
  height?: number | null;
  fit?: string;
  format?: string;
  quality?: number;
};

/** Create one preset. */
export function createMediaPreset(
  siteId: string,
  input: MediaPresetInput,
): Promise<MediaPreset> {
  return request<MediaPreset>(
    `/api/v1/media/transformation-presets?${mediaQuery(siteId)}`,
    { method: "POST", body: JSON.stringify(input) },
  );
}

/** Edit one preset. An edit leaves the old derivatives in place, addressed by their old key. */
export function updateMediaPreset(
  siteId: string,
  id: string,
  input: MediaPresetInput,
): Promise<MediaPreset> {
  return request<MediaPreset>(
    `/api/v1/media/transformation-presets/${encodeURIComponent(id)}?${mediaQuery(siteId)}`,
    { method: "PATCH", body: JSON.stringify(input) },
  );
}

/** Remove a preset, and with it every derivative built from it. */
export function deleteMediaPreset(siteId: string, id: string): Promise<null> {
  return request<null>(
    `/api/v1/media/transformation-presets/${encodeURIComponent(id)}?${mediaQuery(siteId)}`,
    { method: "DELETE" },
  );
}

// ---------------------------------------------------------------------------------------------
// Storage settings (docs/requests/REQ-010, slice 3)
// ---------------------------------------------------------------------------------------------

/** A site's storage settings. */
export function fetchMediaStorageSettings(siteId: string): Promise<MediaStorageSettings> {
  return request<MediaStorageSettings>(`/api/v1/media/settings?${mediaQuery(siteId)}`);
}

/**
 * Save a site's storage settings.
 *
 * A `PUT` whose body is folded onto the row field by field: a form that sends six of ten fields
 * does not reset the other four to a platform default, which is how a settings screen "saves"
 * and loses the bucket.
 */
export function saveMediaStorageSettings(
  siteId: string,
  input: MediaStorageSettingsInput,
): Promise<MediaStorageSettings> {
  return request<MediaStorageSettings>(`/api/v1/media/settings?${mediaQuery(siteId)}`, {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

/**
 * Prove that a configuration reaches a bucket — by writing, not by reading.
 *
 * The candidate is the body as sent, so the answer describes the form on screen rather than the
 * row that happens to be saved. A refused value answers with the same field a save would have
 * refused, so a person is never sent to fix a field on one path that the other accepted.
 */
export function testMediaStorageConnection(
  siteId: string,
  input: MediaStorageSettingsInput,
): Promise<MediaStorageProbe> {
  return request<MediaStorageProbe>(`/api/v1/media/settings/test-connection?${mediaQuery(siteId)}`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

// ---------------------------------------------------------------------------------------------
// Scanning (docs/requests/REQ-010, slice 4)
// ---------------------------------------------------------------------------------------------

/** A site's scanning policy. */
export function fetchMediaScanSettings(siteId: string): Promise<MediaScanSettings> {
  return request<MediaScanSettings>(`/api/v1/media/scan-settings?${mediaQuery(siteId)}`);
}

/**
 * Save a site's scanning policy.
 *
 * A `PUT` folded onto the row field by field, like every other settings save here: a form that
 * sends four of six fields does not reset the other two, which is how a site loses its scanner
 * endpoint on the day somebody edits the timeout.
 */
export function saveMediaScanSettings(
  siteId: string,
  input: MediaScanSettingsInput,
): Promise<MediaScanSettings> {
  return request<MediaScanSettings>(`/api/v1/media/scan-settings?${mediaQuery(siteId)}`, {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

/**
 * Prove a scanner answers, by posting a real generated payload to it.
 *
 * A health check proves the port is open; it does not prove the scanner accepts bytes, and a
 * scanner that answers `/health` and refuses actual uploads is exactly the configuration that
 * leaves a library where every file reads `error`.
 */
export function testMediaScanner(
  siteId: string,
  input: MediaScanSettingsInput,
): Promise<MediaScanProbe> {
  return request<MediaScanProbe>(`/api/v1/media/scan/test?${mediaQuery(siteId)}`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Sweep a site's pending files now and return what the pass wrote. */
export function runMediaScan(siteId: string): Promise<MediaSweepResult> {
  return request<MediaSweepResult>(`/api/v1/media/scan/run?${mediaQuery(siteId)}`, {
    method: "POST",
  });
}

/** The run log of a site, newest first. */
export function fetchMediaScanRuns(siteId: string): Promise<MediaScanRunList> {
  return request<MediaScanRunList>(`/api/v1/media/scan/runs?${mediaQuery(siteId)}`);
}

// ---------------------------------------------------------------------------------------------
// Retention (REQ-010, slice 4)
// ---------------------------------------------------------------------------------------------

/** A site's retention policies and the numbers around them. */
export function fetchMediaRetention(siteId: string): Promise<MediaRetentionList> {
  return request<MediaRetentionList>(`/api/v1/media/retention?${mediaQuery(siteId)}`);
}

/** Create a retention policy. */
export function createMediaRetentionPolicy(
  siteId: string,
  input: MediaRetentionPolicyInput,
): Promise<MediaRetentionPolicy> {
  return request<MediaRetentionPolicy>(`/api/v1/media/retention?${mediaQuery(siteId)}`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/**
 * Change a retention policy.
 *
 * The body is sent **verbatim**, including a `folder_id: null`. The API reads the scope from
 * the key's presence rather than from a deserialised value — serde's `Option` maps a `null`
 * to `None` whatever the inner type is, so `Option<Option<Uuid>>` cannot tell "leave the
 * scope alone" from "widen it to the whole site". Dropping a null here would make the second
 * edit an operator most often wants unreachable.
 */
export function saveMediaRetentionPolicy(
  siteId: string,
  policyId: string,
  input: MediaRetentionPolicyInput,
): Promise<MediaRetentionPolicy> {
  return request<MediaRetentionPolicy>(
    `/api/v1/media/retention/${policyId}?${mediaQuery(siteId)}`,
    { method: "PUT", body: JSON.stringify(input) },
  );
}

/** Delete a retention policy. */
export function deleteMediaRetentionPolicy(siteId: string, policyId: string): Promise<void> {
  return request<void>(`/api/v1/media/retention/${policyId}?${mediaQuery(siteId)}`, {
    method: "DELETE",
  });
}

/** Sweep a site now and return what the pass actually did. */
export function runMediaRetention(siteId: string): Promise<MediaRetentionRunResult> {
  return request<MediaRetentionRunResult>(`/api/v1/media/retention/run?${mediaQuery(siteId)}`, {
    method: "POST",
  });
}

/** The retention run log of a site, newest first. */
export function fetchMediaRetentionRuns(siteId: string): Promise<MediaRetentionRunList> {
  return request<MediaRetentionRunList>(`/api/v1/media/retention/runs?${mediaQuery(siteId)}`);
}

/** Drop the reference rows whose referent is gone. */
export function repairMediaReferences(siteId: string): Promise<MediaRetentionRepair> {
  return request<MediaRetentionRepair>(`/api/v1/media/retention/repair?${mediaQuery(siteId)}`, {
    method: "POST",
  });
}

/** Put a file under a legal hold, or take it off one. The reason is required either way. */
export function setMediaHold(
  fileId: string,
  hold: boolean,
  reason: string,
): Promise<{ media_id: string; legal_hold: boolean; changed: boolean }> {
  return request<{ media_id: string; legal_hold: boolean; changed: boolean }>(
    `/api/v1/media/files/${fileId}/hold`,
    { method: "PUT", body: JSON.stringify({ hold, reason }) },
  );
}

/** The open quarantines of a site, with their totals. */
export function fetchMediaQuarantine(siteId: string): Promise<MediaQuarantineList> {
  return request<MediaQuarantineList>(`/api/v1/media/quarantine?${mediaQuery(siteId)}`);
}

/**
 * Let a held file go back into circulation.
 *
 * The reason is required by the API and not merely by this signature: the quarantine row is the
 * only record the file was ever held, and a release with no stated reason is a release nobody
 * can review afterwards.
 */
export function releaseMediaQuarantine(
  quarantineId: string,
  reason: string,
): Promise<{ released: boolean; reason: string }> {
  return request<{ released: boolean; reason: string }>(
    `/api/v1/media/quarantine/${quarantineId}/release`,
    { method: "POST", body: JSON.stringify({ reason }) },
  );
}

// ---------------------------------------------------------------------------------------------
// The visual workflow builder (REQ-004)
// ---------------------------------------------------------------------------------------------

/** One output port of a node type. */
export interface GraphPort {
  /** What an edge's `source_port` refers to. */
  key: string;
  /** What leaving through this port means. */
  label: string;
  /** `true` when leaving here ends the run. */
  terminal: boolean;
}

/** One field of a node's parameter form. */
export interface GraphParamField {
  /** Key in the node's `params`. */
  key: string;
  /** Label above the input. */
  label: string;
  /** `text`, `textarea`, `number`, `select` or `boolean`. */
  kind: string;
  /** `true` when the node is refused without it. */
  required: boolean;
  /** The legal values of a `select`. */
  options: string[];
  /** Help text under the input. */
  help: string;
}

/** One node type the palette offers. */
export interface GraphNodeType {
  /** Registry key, stored on a node as `type`. */
  key: string;
  /** What the palette calls it. */
  label: string;
  /** Which rail group it sits in. */
  category: string;
  /** One line under the card. */
  summary: string;
  /** Output ports, in draw order. */
  outputs: GraphPort[];
  /** The parameter fields the inspector draws. */
  params: GraphParamField[];
  /** `true` when the engine never runs it. Always `false` for a plugin node. */
  inert: boolean;
  /** The parameters a freshly dropped card starts with. */
  defaults: Record<string, unknown>;
  /**
   * `Plugin: <name>` for a plugin node, `undefined` for a core one.
   *
   * Optional rather than an empty string so the palette can tell "not a plugin" from "a
   * plugin that failed to name itself" — and the second is refused server-side, so `undefined`
   * is safe to treat as an ordinary node.
   */
  badge?: string;
  /** The providing plugin's key, when this is a plugin node. Names the badge's tooltip. */
  provider?: string;
}

/** The palette's whole registry. */
export interface GraphNodeTypes {
  /** Every node type, in rail order: core first, then plugin types. */
  node_types: GraphNodeType[];
  /**
   * The rail's groups, in draw order. `Plugins` appears **only** when a plugin contributed a
   * node type — an organization with no plugins never sees a heading it cannot fill.
   */
  categories: string[];
}

/** One node of a graph. */
export interface GraphNode {
  /** Stable id within the graph. */
  id: string;
  /** The registry key. */
  type: string;
  /** What the canvas draws on the card. */
  label: string;
  /** The node's parameters. */
  params: Record<string, unknown>;
  /** Where it sits. */
  position: { x: number; y: number };
}

/** One connection between two nodes. */
export interface GraphEdge {
  /** Stable id within the graph. */
  id: string;
  /** Node the edge leaves. */
  source: string;
  /** Output port it leaves from. */
  source_port: string;
  /** Node it arrives at. */
  target: string;
}

/** One thing validation has to say about a graph. */
export interface GraphFinding {
  /** `error` or `warning`. */
  severity: "error" | "warning";
  /** Stable machine-readable code. */
  code: string;
  /** Human-readable explanation, naming the node. */
  message: string;
  /** The node the finding is about, when there is one. */
  node_id: string | null;
  /** The other end of a two-node finding. */
  related_node_id: string | null;
}

/** What a validation answers. */
export interface GraphValidation {
  /** `true` when nothing is an error. */
  valid: boolean;
  /** Every finding. */
  findings: GraphFinding[];
  /** How many are errors. */
  error_count: number;
  /** How many are warnings. */
  warning_count: number;
}

/** A definition as the builder reads it. */
export interface WorkflowGraph {
  /** The rule's id. */
  id: string;
  /** The nodes and the connections between them. */
  graph: { nodes: GraphNode[]; edges: GraphEdge[] };
  /** Positions, viewport and collapsed groups. */
  ui_state: Record<string, unknown>;
  /** The version a save must quote. */
  graph_version: number;
  /** When the definition was last validated. */
  validated_at: string | null;
  /** The first validation error, when there was one. */
  validation_error: string | null;
  /** The step list the projection derived. */
  steps: unknown[];
  /** How many nodes the canvas draws. */
  node_count: number;
  /** How many connections the canvas draws. */
  edge_count: number;
  /** What the author would see if they pressed Run now. */
  projection: { valid: boolean; step_count: number; reason: string | null };
  /**
   * Every finding, so the problems panel lists all of them. A save of a graph that is still
   * being wired succeeds and reports them here — a rule is built by being incomplete, and a
   * save that refused the work would refuse the first card the author adds.
   */
  findings?: GraphFinding[];
  /** How many of `findings` are errors. */
  error_count?: number;
}

/** The node-type registry the palette draws. */
export function fetchGraphNodeTypes(): Promise<GraphNodeTypes> {
  return request<GraphNodeTypes>("/api/v1/workflows/node-types");
}

/** One rule's graph, layout, version and projection. */
export function fetchWorkflowGraph(workflowId: string): Promise<WorkflowGraph> {
  return request<WorkflowGraph>(`/api/v1/workflows/${workflowId}/graph`);
}

/**
 * Replace a rule's graph.
 *
 * `graphVersion` is the version the editor loaded. A mismatch is a `409`, and the caller
 * keeps its local copy on screen rather than reloading over the author's work.
 */
export function saveWorkflowGraph(
  workflowId: string,
  input: {
    graph: WorkflowGraph["graph"];
    ui_state?: Record<string, unknown> | null;
    graph_version: number;
  },
): Promise<WorkflowGraph> {
  return request<WorkflowGraph>(`/api/v1/workflows/${workflowId}/graph`, {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

/**
 * Save only the layout.
 *
 * Positions are not semantics: this bumps neither the version nor the step list, so panning
 * the canvas all afternoon never invalidates a colleague's edit.
 */
export function saveWorkflowUiState(
  workflowId: string,
  uiState: Record<string, unknown>,
): Promise<null> {
  return request<null>(`/api/v1/workflows/${workflowId}/graph/ui-state`, {
    method: "PUT",
    body: JSON.stringify({ ui_state: uiState }),
  });
}

/**
 * Validate a graph, stored or not.
 *
 * A body validates what the editor is holding without storing it; no body validates the
 * stored graph, which is what the toolbar's Validate button sends.
 */
export function validateWorkflowGraph(
  workflowId: string,
  graph?: WorkflowGraph["graph"],
): Promise<GraphValidation> {
  return request<GraphValidation>(`/api/v1/workflows/${workflowId}/validate`, {
    method: "POST",
    body: JSON.stringify(graph ? { graph, graph_version: 0 } : {}),
  });
}

// ---------------------------------------------------------------------------------------------
// Notifications (docs/requests/REQ-021, slice 1)
// ---------------------------------------------------------------------------------------------

/** Build the list's query string from the filters that are actually set.
 *
 * Only the *set* filters are sent. A `category=""` in the query string is a filter nobody
 * meant to apply, and forwarding it would ask the server for the empty category — which the
 * server refuses with a 400, so a cleared `<select>` would turn the list into an error screen
 * instead of an unfiltered one.
 *
 * **`with_read` is the exception, and it is not a special case so much as the rule's own
 * limit.** A skipped `false` is right for every other flag, because there `false` is the same
 * as absent. For `with_read` absent means *show read and unread* and `false` means *unread
 * only*, so a generic falsy skip would drop the reader's inbox filter on the floor and hand
 * back a list they did not ask for. It is sent as `with_read=0` — the same value the server's
 * `parse_flag` reads as off — rather than as `false`, which would serialize to "false" and is
 * the kind of asymmetry that later reads as a bug in the wrong place.
 */
function notificationQuery(filters: NotificationFilters = {}): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(filters)) {
    if (key === "with_read") {
      if (value === false) params.set(key, "0");
      continue;
    }
    if (value === undefined || value === "" || value === false) continue;
    params.set(key, String(value));
  }
  const query = params.toString();
  return query ? `?${query}` : "";
}

/** One page of the caller's own notifications. */
export function fetchNotifications(
  filters: NotificationFilters = {},
): Promise<NotificationPage> {
  return request<NotificationPage>(`/api/v1/notifications${notificationQuery(filters)}`);
}

/** The bell's grouped counts. */
export function fetchNotificationSummary(): Promise<NotificationSummary> {
  return request<NotificationSummary>("/api/v1/notifications/summary");
}

/** One notification, or the 404 the API answers for a row that is not the caller's. */
export function fetchNotification(id: string): Promise<NotificationRow> {
  return request<NotificationRow>(`/api/v1/notifications/${encodeURIComponent(id)}`);
}

/** Mark one read or unread; the answer carries the summary so the badge needs no second read. */
export function setNotificationRead(
  id: string,
  read: boolean,
): Promise<NotificationSummary> {
  return request<NotificationSummary>(
    `/api/v1/notifications/${encodeURIComponent(id)}/read`,
    { method: "POST", body: JSON.stringify({ read }) },
  );
}

/** Clear the whole badge in one action. */
export function markAllNotificationsRead(): Promise<NotificationSummary> {
  return request<NotificationSummary>("/api/v1/notifications/mark-all-read", { method: "POST" });
}

/**
 * One action over a selection: `read`, `unread`, `archive` or `delete`.
 *
 * The answer's `changed` is what the API really changed, which is not always the size of the
 * selection — a row that was already archived is not archived again. The panel reports that
 * number rather than the selection length, so the message matches what happened.
 */
export function bulkNotifications(
  action: "read" | "unread" | "archive" | "delete",
  ids: string[],
): Promise<NotificationBulkResult> {
  return request<NotificationBulkResult>("/api/v1/notifications/bulk", {
    method: "POST",
    body: JSON.stringify({ action, ids }),
  });
}

/** Delete one notification. */
export function deleteNotification(id: string): Promise<null> {
  return request<null>(`/api/v1/notifications/${encodeURIComponent(id)}`, { method: "DELETE" });
}

/**
 * Send a notification to accounts.
 *
 * The panel does not use this — it is the route a module calls — but the developer portal
 * (REQ-022) does, and a client function that only exists when a screen needs it is a client
 * function that gets written twice.
 */
export function emitNotification(input: {
  category: string;
  title: string;
  body?: string;
  priority?: string;
  url?: string;
  user_ids: string[];
  dedupe_key?: string;
}): Promise<{ created: number; deduped: number }> {
  return request<{ created: number; deduped: number }>("/api/v1/notifications/emit", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/**
 * The caller's own channel configuration (REQ-021, slice 2).
 *
 * The answer is always a **complete** matrix, so the form renders what the server sent rather
 * than building a grid from the category and channel lists it happens to have. Two copies of
 * the closed vocabulary in two languages is how a channel ends up in one list and not the
 * other — and the failure is a form with a hole in it, not an error.
 */
export function fetchNotificationPreferences(): Promise<NotificationPreferences> {
  return request<NotificationPreferences>("/api/v1/notifications/preferences");
}

/**
 * Save the stated cells and the settings row.
 *
 * `settings` is required by the API, so this signature makes it required here too: a client
 * that could omit it would discover at runtime that omitting it is a `400`, and the fix would
 * be to stop sending cells.
 *
 * The answer carries the whole matrix back rather than a count, and the form renders from that
 * — the count is for the toast, the matrix is for the screen.
 */
export function saveNotificationPreferences(input: {
  cells: { category: string; channel: string; enabled: boolean }[];
  settings: NotificationSettingsRow;
}): Promise<NotificationPreferencesSaved> {
  return request<NotificationPreferencesSaved>("/api/v1/notifications/preferences", {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

// ---------------------------------------------------------------------------------------------
// Slice 3: the half that leaves the panel
// ---------------------------------------------------------------------------------------------

/**
 * The organization's delivery log.
 *
 * **The filters are appended by hand, not by a query-string builder.** The server reads a
 * repeated `status` out of `RawQuery` rather than through a `Query<T>` extractor, because
 * `serde_urlencoded` cannot put a repeated key into a `Vec` — the bug that made every filtered
 * inbox answer `400` until slice 2. Appending `status=` twice here is the supported way to ask
 * for two states, and `append()` (not `+=`) is what encodes the `&` correctly.
 */
export function fetchNotificationOutbox(filters: {
  statuses?: string[];
  channel?: string;
  limit?: number;
} = {}): Promise<NotificationOutbox> {
  const query = new URLSearchParams();
  for (const status of filters.statuses ?? []) query.append("status", status);
  if (filters.channel) query.set("channel", filters.channel);
  if (filters.limit) query.set("limit", String(filters.limit));
  const suffix = query.toString();
  return request<NotificationOutbox>(`/api/v1/notifications/outbox${suffix ? `?${suffix}` : ""}`);
}

/**
 * Requeue one failed delivery.
 *
 * The answer names what happened rather than throwing on a row that cannot be retried: a
 * `sent` row has already reached somebody, and the caller's next action is the same either
 * way — do not press the button again. Throwing would put an error toast on a button that
 * worked exactly as designed.
 */
export function retryNotificationDelivery(id: string): Promise<{ outcome: string }> {
  return request<{ outcome: string }>(`/api/v1/notifications/outbox/${id}/retry`, {
    method: "POST",
  });
}

/** This person's registered browsers. The endpoint itself is never in the answer. */
export function fetchNotificationDevices(): Promise<NotificationDevice[]> {
  return request<NotificationDevice[]>("/api/v1/notifications/push-subscriptions");
}

/** Register (or re-point) this browser. The four outcomes are the answer, not a boolean. */
export function registerNotificationDevice(input: {
  endpoint: string;
  p256dh: string;
  auth: string;
}): Promise<{ id: string; outcome: NotificationPushOutcome; endpoint_hint: string }> {
  return request("/api/v1/notifications/push-subscriptions", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Remove one device. `404` for somebody else's, so existence does not leak. */
export function removeNotificationDevice(id: string): Promise<void> {
  return request<void>(`/api/v1/notifications/push-subscriptions/${id}`, { method: "DELETE" });
}

/**
 * The installation's VAPID public key, which is what `applicationServerKey` needs.
 *
 * `available: false` is an answer, not an error: the settings screen renders the push block
 * either way, and a rejected request would put a toast on a panel that is working correctly
 * and simply has nothing configured yet.
 */
export function fetchNotificationPushKey(): Promise<NotificationPushKey> {
  return request<NotificationPushKey>("/api/v1/notifications/push-key");
}

/**
 * Send one test notification through one channel, now, and report what happened.
 *
 * **The answer carries the transport's own outcome, not a boolean the client invented.**
 * `delivered` plus `detail` are separate because the settings screen renders them
 * differently — the boolean decides the colour of the line, `detail` is the sentence under
 * it. A failure is a `200`, not an error status: "your SMTP host refused the message" is a
 * result the reader asked for, and a `502` would tell them their settings screen is broken.
 */
export function sendTestNotificationDelivery(input: {
  channel: string;
  title?: string;
  body?: string;
}): Promise<{
  channel: string;
  delivered: boolean;
  detail: string;
  response_status: number | null;
  notification_id: string;
  delivery_status: string;
}> {
  return request("/api/v1/notifications/preferences/test", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** What each channel can do on this installation, and the sentence explaining it. */
export function fetchNotificationChannels(): Promise<NotificationChannelReadiness[]> {
  return request<NotificationChannelReadiness[]>("/api/v1/notifications/channels");
}

/** Every routing rule, enabled or not. */
export function fetchNotificationRoutes(): Promise<NotificationRouteRule[]> {
  return request<NotificationRouteRule[]>("/api/v1/notifications/routes");
}

/**
 * Write one routing rule.
 *
 * `recipient` is one string, not a typed object, because the validation lives in the server's
 * parse: a client that guesses `{"kind":"group"}` gets a `400` naming the four legal prefixes,
 * which teaches it more than a schema that only admits the shapes this build knows.
 */
export function createNotificationRoute(input: {
  event_name: string;
  category: string;
  priority: string;
  recipient: string;
  title_template: string;
  url_template: string | null;
}): Promise<NotificationRouteRule> {
  return request<NotificationRouteRule>("/api/v1/notifications/routes", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Remove one rule. */
export function deleteNotificationRoute(id: string): Promise<void> {
  return request<void>(`/api/v1/notifications/routes/${id}`, { method: "DELETE" });
}

/**
 * Run one bus event through the router, now.
 *
 * This is the *proof* of slice 3, not a feature: the claim is that a bus fact becomes a
 * notification with no direct call between the two modules, and the only way to show that from
 * a browser is to hand the router the event a producer would have written. The answer's counts
 * are the deliverable — `created: 0` alone cannot tell a rule that is ahead of its producer
 * from a rule that resolves to nobody, which is why `unmatched_rules` and `unknown_event` are
 * separate fields rather than one `fired` boolean.
 */
export function runNotificationRoute(input: {
  event_name: string;
  event_id?: string;
  actor_user_id?: string;
  organization_id?: string;
  payload?: Record<string, unknown>;
}): Promise<NotificationRouteReport> {
  return request<NotificationRouteReport>("/api/v1/notifications/route", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

// ---------------------------------------------------------------------------------------------
// The event feed and the catalogue (REQ-016, slice 1)
// ---------------------------------------------------------------------------------------------

/** Build the feed's query string from its filters.
 *
 * `name` is repeated rather than joined: `?name=a&name=b` is the only shape that survives a
 * value containing a comma, and an event name is a controlled vocabulary that will never
 * contain one — but a filter that quietly mis-splits on a comma the day somebody registers a
 * plugin's own name is a filter that will be debugged from the wrong end.
 */
function eventQuery(filters: EventFilters = {}): string {
  const search = new URLSearchParams();
  for (const name of filters.name ?? []) {
    if (name) search.append("name", name);
  }
  if (filters.site_id) search.set("site_id", filters.site_id);
  if (filters.actor_user_id) search.set("actor_user_id", filters.actor_user_id);
  if (filters.from) search.set("from", filters.from);
  if (filters.to) search.set("to", filters.to);
  if (filters.cursor && filters.cursor > 0) search.set("cursor", String(filters.cursor));
  if (filters.limit) search.set("limit", String(filters.limit));
  const query = search.toString();
  return query ? `?${query}` : "";
}

/** One page of the platform's recent events, newest first. */
export function fetchEvents(filters: EventFilters = {}): Promise<EventPage> {
  return request<EventPage>(`/api/v1/events${eventQuery(filters)}`);
}

// ---------------------------------------------------------------------------------------------
// The webhook endpoints and their delivery operations (REQ-016, slice 2)
// ---------------------------------------------------------------------------------------------

/** Build a delivery history's query string from its filters.
 *
 * `status` and `name` repeat rather than joining, for the same reason the feed's `name` does:
 * a comma is not in either vocabulary, and a filter that quietly mis-splits on one the day a
 * plugin registers its own name is a filter debugged from the wrong end. The cursor travels as
 * its two halves (`cursor_at`, `cursor_id`) because the read sorts by both and the API refuses
 * a half rather than comparing against a null id.
 */
function deliveryQuery(filters: WebhookDeliveryFilters = {}): string {
  const search = new URLSearchParams();
  for (const status of filters.status ?? []) {
    if (status) search.append("status", status);
  }
  for (const name of filters.name ?? []) {
    if (name) search.append("name", name);
  }
  if (filters.from) search.set("from", filters.from);
  if (filters.to) search.set("to", filters.to);
  if (filters.q) search.set("q", filters.q);
  if (filters.cursor_at && filters.cursor_id) {
    search.set("cursor_at", filters.cursor_at);
    search.set("cursor_id", filters.cursor_id);
  }
  if (filters.limit) search.set("limit", String(filters.limit));
  const query = search.toString();
  return query ? `?${query}` : "";
}

/** Every endpoint this account may see, name order. */
export function fetchWebhookEndpoints(): Promise<WebhookList> {
  return request<WebhookList>("/api/v1/webhooks");
}

/** One endpoint. The secret is never in the answer. */
export function fetchWebhookEndpoint(id: string): Promise<WebhookEndpoint> {
  return request<WebhookEndpoint>(`/api/v1/webhooks/${id}`);
}

/** Connect an endpoint. The answer carries a `secret` when the platform generated one. */
export function createWebhookEndpoint(input: {
  name: string;
  url: string;
  events: string[];
  secret?: string;
}): Promise<WebhookEndpoint> {
  return request<WebhookEndpoint>("/api/v1/webhooks", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Change an endpoint's name, URL, subscriptions or enabled flag. */
export function updateWebhookEndpoint(
  id: string,
  changes: {
    name?: string;
    url?: string;
    events?: string[];
    enabled?: boolean;
  },
): Promise<WebhookEndpoint> {
  return request<WebhookEndpoint>(`/api/v1/webhooks/${id}`, {
    method: "PATCH",
    body: JSON.stringify(changes),
  });
}

/** Disconnect an endpoint and its queue rows. */
export function deleteWebhookEndpoint(id: string): Promise<void> {
  return request<void>(`/api/v1/webhooks/${id}`, { method: "DELETE" });
}

/**
 * Replace the signing secret and get the new one.
 *
 * A dedicated call rather than `updateWebhookEndpoint`, because a rotation is the one write
 * whose answer contains the secret: a receiver cannot be reconfigured with a value it never
 * saw, and folding this into the update would make the secret a field that appears or vanishes
 * depending on which verb the panel happened to use.
 */
export function rotateWebhookSecret(id: string): Promise<WebhookRotation> {
  return request<WebhookRotation>(`/api/v1/webhooks/${id}/secret/rotate`, { method: "POST" });
}

/** Queue one signed `webhook.test` delivery, whether or not the endpoint is enabled. */
export function testWebhookEndpoint(id: string): Promise<WebhookTestReport> {
  return request<WebhookTestReport>(`/api/v1/webhooks/${id}/test`, { method: "POST" });
}

/** One page of an endpoint's delivery history, newest first. */
export function fetchWebhookDeliveries(
  id: string,
  filters: WebhookDeliveryFilters = {},
): Promise<WebhookDeliveryPage> {
  return request<WebhookDeliveryPage>(
    `/api/v1/webhooks/${id}/deliveries${deliveryQuery(filters)}`,
  );
}

/** Force one delivery again. A `409` names which of the three refusals it was. */
export function redeliverWebhookDelivery(id: string, deliveryId: string): Promise<void> {
  return request<void>(`/api/v1/webhooks/${id}/deliveries/${deliveryId}/redeliver`, {
    method: "POST",
  });
}

/**
 * Force many deliveries again, per id.
 *
 * The declared literal route, not the `{delivery_id}` one: axum reads `/deliveries/redeliver`
 * through the parameterised path if the batch route is registered second, and the panel would
 * get `delivery_not_found` for a request that is perfectly valid.
 */
export function redeliverWebhookDeliveries(
  id: string,
  deliveryIds: string[],
): Promise<WebhookRedeliverBatch> {
  return request<WebhookRedeliverBatch>(`/api/v1/webhooks/${id}/deliveries/redeliver`, {
    method: "POST",
    body: JSON.stringify({ delivery_ids: deliveryIds }),
  });
}

/** What this endpoint's receiver has been doing, over `windowHours` (default 24). */
export function fetchWebhookStats(id: string, windowHours?: number): Promise<WebhookStats> {
  const query = windowHours ? `?window_hours=${windowHours}` : "";
  return request<WebhookStats>(`/api/v1/webhooks/${id}/stats${query}`);
}

/**
 * Every event name the platform knows, with its area, description and payload fields.
 *
 * The counts (`live_count`, `reserved_count`, `max_subscriptions`) are part of the answer
 * rather than something the screen recomputes: they are the registry's own totals, and a
 * screen that counted the rows it happened to receive would report a number that changes with
 * a filter the user cannot see.
 */
export function fetchEventCatalogue(): Promise<EventCatalogue> {
  return request<EventCatalogue>("/api/v1/events/catalogue");
}

// ---------------------------------------------------------------------------------------------
// The bus's own retention (REQ-016, slice 3)
// ---------------------------------------------------------------------------------------------

/**
 * How much history this organization keeps, and the last sweeps that ran.
 *
 * The read carries the bounds the API enforces (`min_days`, `max_days`) rather than the screen
 * inventing its own: a range written in two places is a range that will disagree, and the input
 * that disagrees with the server's rule is the one that gets a `400` nobody can act on.
 */
export function fetchRetention(): Promise<RetentionStatus> {
  return request<RetentionStatus>("/api/v1/events/retention");
}

/**
 * Set the window, in days.
 *
 * The refusal is passed through rather than caught: a window of `0` or `4000` is refused by
 * name, and a screen that clamped it would answer `200` with a number the operator did not
 * choose.
 */
export function setRetentionWindow(windowDays: number): Promise<RetentionStatus> {
  return request<RetentionStatus>("/api/v1/events/retention", {
    method: "PATCH",
    body: JSON.stringify({ window_days: windowDays }),
  });
}

/** Run one sweep now, and answer with what it actually removed. */
export function sweepRetention(): Promise<SweepResult> {
  return request<SweepResult>("/api/v1/events/retention/sweep", { method: "POST" });
}

// ---------------------------------------------------------------------------------------------
// Security centre (REQ-012, slice 1)
// ---------------------------------------------------------------------------------------------

/**
 * The posture overview.
 *
 * Always fetched with `cache: "no-store"`: the whole point of this screen is that it says what
 * is true *now*, and a cached overview is a security claim with an expiry nobody chose.
 */
export function fetchSecurityOverview(): Promise<SecurityOverview> {
  return request<SecurityOverview>("/api/v1/security/overview", { cache: "no-store" });
}

/**
 * Re-evaluate every check now.
 *
 * The answer is a full overview rather than a run id, so the panel replaces what it has with
 * what the server now believes — a client that merged the new states into the old rows would
 * keep a stale `pass` for a check the run could not evaluate.
 */
export function runSecurityChecks(): Promise<SecurityOverview> {
  return request<SecurityOverview>("/api/v1/security/checks/run", {
    method: "POST",
    cache: "no-store",
  });
}

/** The findings list. An unknown filter value is refused by the server by name. */
export function fetchSecurityFindings(
  filter: SecurityFindingFilter & { offset?: number; limit?: number } = {},
): Promise<SecurityFindingPage> {
  const params = new URLSearchParams();
  if (filter.severity) params.set("severity", filter.severity);
  if (filter.status) params.set("status", filter.status);
  if (filter.source) params.set("source", filter.source);
  if (filter.component) params.set("component", filter.component);
  if (filter.search) params.set("search", filter.search);
  if (filter.offset) params.set("offset", String(filter.offset));
  if (filter.limit) params.set("limit", String(filter.limit));
  const query = params.toString();
  return request<SecurityFindingPage>(
    `/api/v1/security/findings${query ? `?${query}` : ""}`,
    { cache: "no-store" },
  );
}

/** One finding, with the evidence the detail drawer shows. */
export function fetchSecurityFinding(id: string): Promise<SecurityFinding> {
  return request<SecurityFinding>(`/api/v1/security/findings/${encodeURIComponent(id)}`, {
    cache: "no-store",
  });
}

/**
 * The URL the export button points at, carrying the *current* filter.
 *
 * A plain href rather than a fetch: the answer is a file, and a client that fetched it and
 * built a blob URL would have to get the filename, the content type and the error case right
 * on its own. The filter travels in the query string, which is what makes "export what I am
 * looking at" true by construction rather than by two parsers agreeing — and the server
 * deliberately ignores `limit` here, because an export that respects the page size is how a
 * 50-row file gets read as a 300-row platform's whole posture.
 */
export function securityFindingsExportUrl(
  filter: SecurityFindingFilter = {},
): string {
  const params = new URLSearchParams();
  if (filter.severity) params.set("severity", filter.severity);
  if (filter.status) params.set("status", filter.status);
  if (filter.source) params.set("source", filter.source);
  if (filter.component) params.set("component", filter.component);
  if (filter.search) params.set("search", filter.search);
  const query = params.toString();
  return `/api/v1/security/findings.csv${query ? `?${query}` : ""}`;
}

/**
 * Change one finding's status.
 *
 * `ignore_reason` is **required by the server** for an ignore and the refusal names the field,
 * so the form can show it on the input rather than as a generic toast. The client does not
 * pre-validate it beyond the drawer's disabled button: a second rule that disagreed with the
 * server's would be a second place to be wrong.
 */
export function setSecurityFindingStatus(
  id: string,
  change: {
    status: SecurityFindingStatus;
    ignore_reason?: string;
    ignored_until?: string;
    note?: string;
  },
): Promise<SecurityFinding> {
  return request<SecurityFinding>(`/api/v1/security/findings/${encodeURIComponent(id)}`, {
    method: "PATCH",
    body: JSON.stringify(change),
  });
}

/** One change, many findings, and the per-row report of what actually happened. */
export function bulkSecurityFindingStatus(
  ids: string[],
  change: { status: SecurityFindingStatus; ignore_reason?: string; note?: string },
): Promise<SecurityBulkResult> {
  return request<SecurityBulkResult>("/api/v1/security/findings/bulk", {
    method: "POST",
    body: JSON.stringify({ ids, ...change }),
  });
}

/**
 * Ingest a CI report.
 *
 * The document is parsed by the platform, not by this client, and a report carrying anything
 * that looks like a credential is refused **whole** — which is why the file is read here and
 * sent as a value rather than being trusted field by field.
 */
export function importSecurityReport(
  report: unknown,
  source: "dependency" | "report" = "dependency",
): Promise<SecurityImportReport> {
  return request<SecurityImportReport>("/api/v1/security/findings/import", {
    method: "POST",
    body: JSON.stringify({ report, source }),
  });
}

// ---------------------------------------------------------------------------------------------
// Security centre (REQ-012, slice 2) — the header policy
// ---------------------------------------------------------------------------------------------

/**
 * Read the stored header policy and what it currently renders to.
 *
 * `no-store` for the same reason the overview is: the screen's whole claim is that these are
 * the lines the next response will carry, and a cached policy is a policy that may no longer be
 * the one in force.
 */
export function fetchHeaderPolicy(): Promise<HeaderPolicyDocument> {
  return request<HeaderPolicyDocument>("/api/v1/security/headers", { cache: "no-store" });
}

/**
 * Save the header policy.
 *
 * `expected_document` is the document the form was opened with and is the compare-and-swap key:
 * a form somebody else has since saved is **refused** rather than silently overwriting them.
 * The client never pre-validates a directive — the server owns every rule here, and a second
 * validator that disagreed with it would be a second place to be wrong.
 */
export function saveHeaderPolicy(save: HeaderPolicySave): Promise<HeaderPolicySaved> {
  return request<HeaderPolicySaved>("/api/v1/security/headers", {
    method: "PUT",
    body: JSON.stringify(save),
  });
}

// ---------------------------------------------------------------------------------------------
// Security centre (REQ-012, slice 3) — rate limiting and sign-in protection
// ---------------------------------------------------------------------------------------------

/**
 * The five scopes, merged with the baseline.
 *
 * `no-store` for the same reason the overview and the header policy carry it: a stale limiter
 * table is a screen that says "600 per minute" while the platform enforces something else, and
 * the whole value of the screen is that the two agree.
 */
export function fetchRateLimits(): Promise<RateLimitsDocument> {
  return request<RateLimitsDocument>("/api/v1/security/rate-limits", { cache: "no-store" });
}

/**
 * Save the limiter document.
 *
 * `expected_scopes` is the document the form was opened with and is the compare-and-swap key:
 * a form somebody else has since saved is **refused** rather than silently overwriting them. The
 * client validates nothing — the server owns every range, and a second rule that disagreed with
 * it would be a second place to be wrong about a limit that is refusing real traffic.
 */
export function saveRateLimits(save: RateLimitsSave): Promise<RateLimitsSaved> {
  return request<RateLimitsSaved>("/api/v1/security/rate-limits", {
    method: "PUT",
    body: JSON.stringify(save),
  });
}

/**
 * Dry-run one request through the limiter.
 *
 * This is a **server** call rather than a local computation on purpose, and the reason is the
 * acceptance criterion it satisfies: the tester's verdict must match the real middleware
 * decision. Only the server holds the same `decide` the middleware runs, so a client that
 * reimplemented the arithmetic would agree with it on the day it was written and drift the
 * first time somebody tunes a limit — which is the day somebody is relying on it.
 */
export function testRateLimit(body: RateLimitTestRequest): Promise<RateLimitTestResponse> {
  return request<RateLimitTestResponse>("/api/v1/security/rate-limits/test", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** The lockout document, its ranges, and how many accounts are locked right now. */
export function fetchSignInProtection(): Promise<SignInProtectionDocument> {
  return request<SignInProtectionDocument>("/api/v1/security/sign-in-protection", {
    cache: "no-store",
  });
}

/** Save the lockout document. `expected_policy` is the compare-and-swap key. */
export function saveSignInProtection(save: SignInProtectionSave): Promise<SignInProtectionSaved> {
  return request<SignInProtectionSaved>("/api/v1/security/sign-in-protection", {
    method: "PUT",
    body: JSON.stringify(save),
  });
}

/** Who is locked out right now, soonest to expire first. */
export function fetchLockedAccounts(): Promise<LockedAccountsPage> {
  return request<LockedAccountsPage>("/api/v1/security/locked-accounts", { cache: "no-store" });
}

/**
 * Release one account early.
 *
 * The REQ calls out that lockout can be weaponised against a known account, so the unlock path
 * is deliberately not hidden behind a confirmation dialog with no escape: it is one click, and
 * it is audited server-side with the actor. The remaining `lockout_minutes` is the thing a
 * cautious operator narrows, not this button.
 */
// -- REQ-012 slice 4: the IP access lists ----------------------------------------------------

/**
 * Both access lists and their counts.
 *
 * One call rather than two: the screen renders the two tables side by side and a summary line,
 * and reading them separately would let the counts describe a different moment than the rows.
 */
export function fetchIpRules(): Promise<IpRulesPage> {
  return request<IpRulesPage>("/api/v1/security/ip-rules", { cache: "no-store" });
}

/**
 * Add one access rule.
 *
 * The response carries `blocks_you` — whether the new rule covers the address this request came
 * from — and the screen surfaces it as a warning rather than hiding it. The rule is saved either
 * way: refusing it would leave the platform unable to express a legitimate self-lockout, and the
 * operator would only learn which input avoids the check.
 */
export function createIpRule(input: CreateIpRuleInput): Promise<CreateIpRuleResult> {
  return request<CreateIpRuleResult>("/api/v1/security/ip-rules", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Remove one rule by id. */
export function deleteIpRule(id: string): Promise<void> {
  return request<void>(`/api/v1/security/ip-rules/${id}`, { method: "DELETE" });
}

/**
 * Ask what one address would do.
 *
 * The same evaluator the request path runs, so the verdict is the platform's verdict rather than
 * a second implementation's. Takes a single address, not a network — listing a network is what
 * the form above is for, and the error message says so.
 */
export function testIpAddress(address: string): Promise<IpTestResult> {
  return request<IpTestResult>("/api/v1/security/ip-rules/test", {
    method: "POST",
    body: JSON.stringify({ address }),
  });
}

/**
 * The security-event timeline (REQ-012 slice 4).
 *
 * **Two sources, and that is the point.** The audit trail holds privileged actions somebody took;
 * it holds no sign-ins at all, because a failed sign-in happens before there is a session and so
 * before there is an actor to write an audit entry for. The server merges both tables and every row
 * names which one it came from, so this client does not choose — it cannot, and a screen that
 * could only read one of them would show an operator an empty sign-in list on a platform where
 * nothing is wrong.
 *
 * The filter is sent as a query string built here rather than assembled by the screen, so the
 * export and the table are guaranteed to be asking the server the same question.
 */
export function fetchSecurityEvents(
  filter: SecurityEventsFilter = {},
): Promise<SecurityEventsPage> {
  const params = new URLSearchParams();
  if (filter.q) params.set("q", filter.q);
  if (filter.category) params.set("category", filter.category);
  if (filter.source) params.set("source", filter.source);
  if (filter.since) params.set("since", filter.since);
  if (filter.until) params.set("until", filter.until);
  if (filter.limit) params.set("limit", String(filter.limit));
  const query = params.toString();
  return request<SecurityEventsPage>(
    `/api/v1/security/events${query ? `?${query}` : ""}`,
    { cache: "no-store" },
  );
}

/**
 * The same filter as a CSV download.
 *
 * A browser navigation rather than a fetch, because the response is a file with
 * `Content-Disposition: attachment` and the panel's own `request()` wrapper is built for JSON —
 * reading it as text would hand the operator the CSV body instead of saving it.
 */
/**
 * The secret inventory. Read-only, and the client has no mutation to offer.
 *
 * No filter parameter, deliberately: the inventory is small enough to render whole, and a filter
 * over a list whose point is "what does this platform hold" invites the reading that the screen
 * is hiding something rather than that it is complete.
 */
export function fetchSecretInventory(): Promise<SecretInventory> {
  return request<SecretInventory>("/api/v1/security/secrets");
}

export function securityEventsExportUrl(filter: SecurityEventsFilter = {}): string {
  const params = new URLSearchParams();
  if (filter.q) params.set("q", filter.q);
  if (filter.category) params.set("category", filter.category);
  if (filter.source) params.set("source", filter.source);
  if (filter.since) params.set("since", filter.since);
  if (filter.until) params.set("until", filter.until);
  // Deliberately NOT sent: the server drops the page size for the export, because an operator
  // who filters to "denials" and exports 50 of 300 has produced a document that reads as a
  // complete list and is not one.
  const query = params.toString();
  return `/api/v1/security/events.csv${query ? `?${query}` : ""}`;
}

export function unlockAccount(userId: string): Promise<LockedAccountsPage> {
  return request<LockedAccountsPage>(
    `/api/v1/security/locked-accounts/${encodeURIComponent(userId)}/unlock`,
    { method: "POST" },
  );
}


/* ---------------------------------------------------------------------------------------------
 * Backups (REQ-013)
 *
 * Six functions for the overview, the detail, the create drawer, the verify action and the
 * settings screen. The schedules' writes arrive with slice 3, which is also where the worker
 * that produces them arrives; a "Run now" button with no worker behind it is a dead button,
 * and this file does not grow one.
 * ------------------------------------------------------------------------------------------- */

/** The query the list screen sends. Every field is optional and every one is a filter. */
export interface BackupListQuery {
  /** Restrict to one terminal state. */
  status?: string;
  /** Restrict to one kind. */
  kind?: string;
  /** Restrict to runs that included this part. */
  scope?: string;
  /** Restrict to one destination. */
  destination?: string;
  /** Only runs created at or after this. */
  created_after?: string;
  /** Only runs created at or before this. */
  created_before?: string;
  /** Page size. */
  limit?: number;
  /** Page offset. */
  offset?: number;
}

/** The list, filtered and paged. */
export function fetchBackups(query: BackupListQuery = {}): Promise<BackupList> {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) {
    if (value === undefined || value === null || value === "") continue;
    search.set(key, String(value));
  }
  const suffix = search.toString();
  return request<BackupList>(`/api/v1/backups${suffix ? `?${suffix}` : ""}`);
}

/** The four cards at the top of the overview. */
export function fetchBackupStatus(): Promise<BackupStatus> {
  return request<BackupStatus>("/api/v1/backups/status");
}

/** One run with its parts and its manifest. */
export function fetchBackup(id: string): Promise<BackupDetail> {
  return request<BackupDetail>(`/api/v1/backups/${id}`);
}

/** The manifest on its own, for the copy button. */
export function fetchBackupManifest(id: string): Promise<unknown> {
  return request<unknown>(`/api/v1/backups/${id}/manifest`);
}

/** Take a backup now. The response is the FINISHED run, not a queued one. */
export function createBackup(input: {
  label?: string;
  scopes?: string[];
  destination?: string;
  protected?: boolean;
  retain_days?: number | null;
}): Promise<BackupCreateResult> {
  return request<BackupCreateResult>("/api/v1/backups", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/**
 * Re-read a run's artifacts and compare them with its manifest.
 *
 * A `200` with `clean: false` is a successful verification that found a problem, so this
 * returns normally in that case — the caller renders the verdict, it does not catch it.
 */
export function verifyBackup(id: string): Promise<BackupVerification> {
  return request<BackupVerification>(`/api/v1/backups/${id}/verify`, { method: "POST" });
}

/**
 * What a restore of this run would do — without doing it.
 *
 * A `GET` on purpose: the preview re-reads every artifact off the destination and counts the
 * live side, and neither of those writes anything. Putting it behind the destructive
 * permission would mean the first time an operator meets this screen is a 403 that never
 * showed them what they were agreeing to.
 */
export function previewRestore(id: string): Promise<RestorePreview> {
  return request<RestorePreview>(`/api/v1/backups/${id}/restore-preview`, { method: "GET" });
}

/**
 * Perform a restore of the ticked parts.
 *
 * The API takes the parts the operator **left ticked**, not "everything available": an empty
 * array is a refusal naming the parts on offer, because a panel that posted nothing and got
 * the whole archive back would be a panel that restored more than it showed.
 */
export function restoreBackup(
  id: string,
  parts: string[],
  confirmation: string,
): Promise<RestoreOutcome> {
  return request<RestoreOutcome>(`/api/v1/backups/${id}/restore`, {
    method: "POST",
    body: JSON.stringify({ parts, confirmation }),
  });
}

/**
 * Queue a restore instead of performing it — the one that can still be stopped (REQ-013
 * slice 2c).
 *
 * The distinction is the whole point and the panel is careful about it: `restoreBackup` runs
 * inside the request and has no cancel, because a `POST` in flight cannot be un-pressed. This
 * one answers `202` with the queued job, and the job can be stopped until its worker claims
 * it. Offering the cancellable one by default and the immediate one behind a named choice is
 * the honest arrangement; offering only the immediate one is a screen with no way back.
 */
export function queueRestore(
  id: string,
  parts: string[],
  confirmation: string,
): Promise<RestoreJob> {
  return request<RestoreJob>(`/api/v1/backups/${id}/restore-queue`, {
    method: "POST",
    body: JSON.stringify({ parts, confirmation }),
  });
}

/**
 * This run's queued and finished restores, newest first.
 *
 * `backup.read`, not `backup.restore`: reading that a restore is queued changes nothing, and
 * hiding it behind the destructive key means the first time an operator looks for the thing
 * they are waiting for is a 403.
 */
export function listRestoreJobs(id: string): Promise<RestoreJob[]> {
  return request<RestoreJob[]>(`/api/v1/backups/${id}/restore-jobs`, { method: "GET" });
}

/**
 * Stop a queued restore.
 *
 * Addressed by the JOB's id and not the run's, because they are different resources: a route
 * that accepted either would let a cancel for one run's job stop another run's restore.
 *
 * The response is the job as it now reads, so a call that lost the race to the worker comes
 * back `running` rather than a `200` claiming a cancellation that did not happen.
 */
export function cancelRestoreJob(jobId: string): Promise<RestoreJob> {
  return request<RestoreJob>(`/api/v1/restore-jobs/${jobId}/cancel`, {
    method: "POST",
    body: JSON.stringify({}),
  });
}

/**
 * Remove a run **and the artifacts it left on the destination**.
 *
 * The returned report is the point of this call: "the row is gone" and "the bytes are gone"
 * are two facts, and a `204` collapsed them. A partial removal comes back with
 * `failed_entries > 0` and the paths that are still on disk, so the screen can say which
 * files an operator has to clear by hand instead of rendering "removed" over a directory
 * that is still full of the media library.
 */
export function deleteBackup(id: string): Promise<BackupPurge> {
  return request<BackupPurge>(`/api/v1/backups/${id}`, { method: "DELETE" });
}

/**
 * Run the retention sweep now, for this tenant.
 *
 * The background sweep runs every six hours, and a six hour wait is not an answer an
 * operator can act on when the disk is filling. The full report comes back rather than a
 * count, because "pruned 4" and "1 of those 4 left a stuck file" are two different facts and
 * the screen renders both.
 */
export function sweepBackups(): Promise<BackupSweepReport> {
  return request<BackupSweepReport>("/api/v1/backups/sweep", { method: "POST" });
}

/** The schedules table. */
export function fetchBackupSchedules(): Promise<BackupSchedule[]> {
  return request<BackupSchedule[]>("/api/v1/backup-schedules");
}

/**
 * What the schedule editor sends.
 *
 * The conditional fields are sent as `null` rather than omitted, so a schedule changed from
 * weekly to daily does not keep a `day_of_week` the server has to decide about. The server
 * refuses a daily schedule that still carries one.
 */
export interface BackupScheduleInput {
  name: string;
  frequency: "hourly" | "daily" | "weekly" | "monthly";
  at_time: string | null;
  day_of_week: number | null;
  day_of_month: number | null;
  timezone: string;
  scopes: string[];
  retention_count: number;
  enabled: boolean;
}

/**
 * Create a schedule.
 *
 * The response carries the **computed** `next_run_at`, not a value the form supplied — the
 * form never computes one. A schedule whose cadence cannot be computed (an unknown timezone,
 * a daily row with no time) is refused with a `400` naming the field rather than stored with
 * a null next run, which would leave a row that looks live and never fires.
 */
export function createBackupSchedule(input: BackupScheduleInput): Promise<BackupSchedule> {
  return request<BackupSchedule>("/api/v1/backup-schedules", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/**
 * Edit a schedule, and get the recomputed next run back.
 *
 * The next run is recomputed on every save rather than left alone. An operator who moves a
 * schedule from 02:00 to 04:00 and does not see the next run move has been told the change
 * did not take, when in fact it was stored and the stale column is what the worker reads.
 */
export function updateBackupSchedule(
  id: string,
  input: BackupScheduleInput,
): Promise<BackupSchedule> {
  return request<BackupSchedule>(`/api/v1/backup-schedules/${id}`, {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

/**
 * Remove a schedule.
 *
 * Its runs keep their own `kind` and lose only the link: deleting a schedule stops future
 * backups and does not delete the restore points it produced. The schema is arranged so the
 * other outcome is not expressible here.
 */
export function deleteBackupSchedule(id: string): Promise<void> {
  return request<void>(`/api/v1/backup-schedules/${id}`, { method: "DELETE" });
}

/**
 * Take a backup on a schedule's terms, right now.
 *
 * `backup.create`, not `backup.manage`: this produces a backup and changes nothing else. It
 * does **not** advance the schedule — clicking this at 09:00 to test a 02:00 schedule must
 * not consume the 02:00 slot, which is the difference between a test and a silent skip.
 */
export function runBackupScheduleNow(id: string): Promise<BackupCreateResult> {
  return request<BackupCreateResult>(`/api/v1/backup-schedules/${id}/run`, { method: "POST" });
}

/** The settings record. */
export function fetchBackupSettings(): Promise<BackupSettings> {
  return request<BackupSettings>("/api/v1/backup-settings");
}

/**
 * Save the settings.
 *
 * The API probes the destination before it stores anything and refuses an unwritable one, so
 * a `200` here means the configuration was not only saved but proved writable.
 */
export function saveBackupSettings(input: {
  destination: string;
  local_root: string;
  s3_prefix?: string | null;
  credential_ref?: string | null;
  encryption: string;
  default_retention: number;
  verify_after_backup: boolean;
}): Promise<BackupSettings> {
  return request<BackupSettings>("/api/v1/backup-settings", {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

// ---------------------------------------------------------------------------------------------
// AI workflow builder (docs/requests/REQ-046) — the draft console
// ---------------------------------------------------------------------------------------------

/** One draft as the console list renders it. */
export type AiWorkflowDraft = {
  id: string;
  title: string;
  status: string;
  model_key: string | null;
  created_by: string | null;
  created_at: string;
  updated_at: string;
  workflow_id: string | null;
  has_definition: boolean;
  error: string | null;
  tokens: number;
};

/** One step of a draft's definition, as the review screen draws it. */
export type AiWorkflowStep = {
  position: number;
  name: string;
  kind: string;
  action: string | null;
  params: unknown;
};

/** One draft as the review screen renders it. */
export type AiWorkflowDraftDetail = AiWorkflowDraft & {
  organization_id: string;
  site_id: string | null;
  prompt: string;
  rationale: string | null;
  definition: unknown;
  tokens_input: number;
  tokens_output: number;
  revision_note: string | null;
  revision_count: number;
  decided_by: string | null;
  decision_reason: string | null;
  steps: AiWorkflowStep[];
  decided_at: string | null;
};

/** One page of drafts, with the vocabulary the console renders itself from. */
export type AiWorkflowDraftList = {
  drafts: AiWorkflowDraft[];
  total: number;
  statuses: string[];
  page_size: number;
};

/** One action of the engine's closed registry. */
export type AiWorkflowAction = {
  action: string;
  summary: string;
  host: boolean;
};

/** One worked example the empty state offers. */
export type AiWorkflowExample = {
  title: string;
  prompt: string;
  note: string;
};

/** The examples and the action vocabulary. */
export type AiWorkflowVocabulary = {
  examples: AiWorkflowExample[];
  actions: AiWorkflowAction[];
};

/** The "created by" select's options. */
export type AiWorkflowAuthor = { id: string; drafts: number };

/** Every list filter is a query parameter, because the console keeps them in the URL. */
export type AiWorkflowDraftQuery = {
  status?: string[];
  q?: string;
  by?: string;
  siteId?: string;
  organizationId?: string;
  offset?: number;
  limit?: number;
};

/** The query string a filter set produces, with empty values omitted. */
function draftQueryString(query: AiWorkflowDraftQuery): string {
  const params = new URLSearchParams();
  if (query.status && query.status.length > 0) {
    params.set("status", query.status.join(","));
  }
  if (query.q && query.q.trim()) params.set("q", query.q.trim());
  if (query.by) params.set("by", query.by);
  if (query.siteId) params.set("site_id", query.siteId);
  if (query.organizationId) params.set("organization_id", query.organizationId);
  if (query.offset) params.set("offset", String(query.offset));
  if (query.limit) params.set("limit", String(query.limit));
  return params.toString();
}

/** One page of drafts. */
export function fetchAiWorkflowDrafts(
  query: AiWorkflowDraftQuery = {},
): Promise<AiWorkflowDraftList> {
  const search = draftQueryString(query);
  return request<AiWorkflowDraftList>(
    `/api/v1/ai/workflows/drafts${search ? `?${search}` : ""}`,
  );
}

/** One draft, with its definition and its step list. */
export function fetchAiWorkflowDraft(draftId: string): Promise<AiWorkflowDraftDetail> {
  return request<AiWorkflowDraftDetail>(
    `/api/v1/ai/workflows/drafts/${encodeURIComponent(draftId)}`,
  );
}

/** The authors this organization's drafts carry. */
export function fetchAiWorkflowAuthors(organizationId?: string): Promise<AiWorkflowAuthor[]> {
  const search = organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
  return request<AiWorkflowAuthor[]>(`/api/v1/ai/workflows/drafts/authors${search}`);
}

/** The empty state's examples and the engine's closed action registry. */
export function fetchAiWorkflowVocabulary(): Promise<AiWorkflowVocabulary> {
  return request<AiWorkflowVocabulary>("/api/v1/ai/workflows/examples");
}

/** Delete a draft. Never the workflow it produced. */
export function removeAiWorkflowDraft(draftId: string): Promise<null> {
  return request<null>(`/api/v1/ai/workflows/drafts/${encodeURIComponent(draftId)}`, {
    method: "DELETE",
  });
}

// ---- The decision half (REQ-046 slice 4) ---------------------------------------------------

/** What a decision answers: the draft as it now reads, plus what the decision produced. */
export type AiWorkflowDecision = {
  draft: AiWorkflowDraftDetail;
  workflow_id?: string;
  enabled: boolean;
};

/** One planned step of a test run, as the review screen draws the plan. */
export type AiWorkflowTestRunStep = {
  position: number;
  name: string;
  kind: string;
  action: string | null;
  /** Whether the HOST runs this action — the line that says the step leaves the process. */
  host: boolean;
  /** The permission the action needs at run time, or `null` for the engine's own actions. */
  permission: string | null;
  params: unknown;
};

/**
 * What a test run reports.
 *
 * A test run validates the definition and projects it step by step; it does **not** dispatch
 * a run, so the `note` is the platform's own sentence about that and the screen shows it
 * rather than letting the operator assume a rule was exercised.
 */
export type AiWorkflowTestRun = {
  draft_id: string;
  workflow_id: string | null;
  definition: unknown;
  steps: AiWorkflowTestRunStep[];
  verdict: string;
  note: string;
};

/**
 * Approve a draft: it becomes a **disabled** workflow.
 *
 * `409` on a second call, naming the workflow the first call created — the message is the
 * reason the operator can act on the conflict.
 */
export function approveAiWorkflowDraft(
  draftId: string,
): Promise<AiWorkflowDecision> {
  return request<AiWorkflowDecision>(
    `/api/v1/ai/workflows/drafts/${encodeURIComponent(draftId)}/approve`,
    { method: "POST" },
  );
}

/** Reject a draft. The reason is required by the API, not merely by this form. */
export function rejectAiWorkflowDraft(
  draftId: string,
  reason: string,
): Promise<AiWorkflowDecision> {
  return request<AiWorkflowDecision>(
    `/api/v1/ai/workflows/drafts/${encodeURIComponent(draftId)}/reject`,
    { method: "POST", body: JSON.stringify({ reason }) },
  );
}

/** Save an operator-edited definition. The API revalidates and changes nothing on failure. */
export function saveAiWorkflowDefinition(
  draftId: string,
  definition: unknown,
): Promise<AiWorkflowDraftDetail> {
  return request<AiWorkflowDraftDetail>(
    `/api/v1/ai/workflows/drafts/${encodeURIComponent(draftId)}`,
    { method: "PATCH", body: JSON.stringify({ definition }) },
  );
}

/** Validate a draft's definition and get the plan back, without running anything. */
export function testRunAiWorkflowDraft(draftId: string): Promise<AiWorkflowTestRun> {
  return request<AiWorkflowTestRun>(
    `/api/v1/ai/workflows/drafts/${encodeURIComponent(draftId)}/test-run`,
    { method: "POST" },
  );
}

/** One stage of a generation, as the progress panel draws it. */
export type AiWorkflowStage = "plan" | "validate" | "repair";

/** What a finished generation reports. */
export type AiWorkflowDone = {
  draft_id: string;
  status: string;
  repaired: boolean;
  attempts: number;
  tokens: number;
};

/**
 * Generate a draft, streaming.
 *
 * The API answers `text/event-stream` with `stage` frames (what the platform is doing),
 * a `done` frame carrying the stored draft's id, or an `error` frame with a stable code. It
 * carries **no prose**: the answer is validated before `done`, so a client that rendered
 * deltas would be rendering a definition that may still be refused. A `409` before the first
 * frame is the console's no-provider state, which is raised here as an `ApiError` so the
 * caller handles one shape either way.
 *
 * `signal` is the Cancel button: aborting closes the stream, and the draft row keeps the
 * `generating` status its own cleanup answers — nothing is written from the client side.
 */
export async function streamGenerateWorkflowDraft(
  input: { prompt: string; model?: string; siteId?: string; organizationId?: string },
  handlers: AiWorkflowStreamHandlers = {},
  signal?: AbortSignal,
): Promise<void> {
  await streamDraft(
    "/api/v1/ai/workflows/generate",
    {
      prompt: input.prompt,
      model: input.model && input.model.trim() ? input.model.trim() : null,
      site_id: input.siteId || null,
      organization_id: input.organizationId || null,
    },
    handlers,
    signal,
  );
}

/**
 * Ask the model for a change to an existing draft, streaming.
 *
 * The same frames and the same reader as [`streamGenerateWorkflowDraft`], and that is the
 * point: a revision that answered after one round-trip would leave the operator staring at
 * a button they can press again, and the review screen's progress panel would have to be a
 * second implementation of the panel the console already has.
 */
export async function streamReviseWorkflowDraft(
  draftId: string,
  note: string,
  model: string | undefined,
  handlers: AiWorkflowStreamHandlers = {},
  signal?: AbortSignal,
): Promise<void> {
  await streamDraft(
    `/api/v1/ai/workflows/drafts/${encodeURIComponent(draftId)}/revise`,
    { note, model: model && model.trim() ? model.trim() : null },
    handlers,
    signal,
  );
}

/** The callbacks a generation stream reports through. */
export type AiWorkflowStreamHandlers = {
  onStage?: (stage: AiWorkflowStage) => void;
  onDone?: (done: AiWorkflowDone) => void;
};

/**
 * Read one generation or revision stream.
 *
 * One reader for both routes: the frames are the same contract (`stage` / `done` / `error`),
 * and two readers would be two places where an `error` frame is handled slightly differently
 * — which is exactly the kind of difference that shows up as "asking for changes does
 * nothing" while generating works.
 */
async function streamDraft(
  path: string,
  body: Record<string, unknown>,
  handlers: AiWorkflowStreamHandlers,
  signal?: AbortSignal,
): Promise<void> {
  const response = await fetch(path, {
    method: "POST",
    credentials: "same-origin",
    headers: {
      "content-type": "application/json",
      accept: "text/event-stream",
      // The METHOD is what makes this a mutating call; passing only `{ signal }` would read as
      // a `GET`, send no token, and this stream would be the one screen whose save answers
      // `403 csrf_failed` while every other write on the platform works.
      ...csrfHeader({ method: "POST" }),
    },
    body: JSON.stringify(body),
    signal,
  });

  if (!response.ok || !response.body) {
    const payload = (await readJson(response)) as ErrorBody | null;
    throw new ApiError(
      response.status,
      payload?.error?.code ?? "unknown_error",
      payload?.error?.message ?? `The API answered with status ${response.status}.`,
    );
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });

    let boundary = buffer.indexOf("\n\n");
    while (boundary >= 0) {
      const frame = buffer.slice(0, boundary);
      buffer = buffer.slice(boundary + 2);
      boundary = buffer.indexOf("\n\n");

      let event = "message";
      let data = "";
      for (const line of frame.split("\n")) {
        if (line.startsWith("event: ")) event = line.slice(7).trim();
        else if (line.startsWith("data: ")) data += line.slice(6);
      }
      if (!data) continue;

      const payload = JSON.parse(data) as Record<string, unknown>;
      if (event === "stage") {
        handlers.onStage?.(payload.stage as AiWorkflowStage);
      } else if (event === "done") {
        handlers.onDone?.(payload as unknown as AiWorkflowDone);
        return;
      } else if (event === "error") {
        throw new ApiError(
          502,
          (payload.code as string) ?? "generation_failed",
          (payload.message as string) ?? "The generation did not finish.",
        );
      }
    }
  }
}

// System health (REQ-014).
//
// The overview is fetched with `cache: "no-store"` and the POST carries the CSRF header
// `request()` already adds, because both of those are the difference between this screen
// showing the platform and showing a screenshot of it. A cached overview is a status
// screen that answers "how were things when this tab was last opened", which is the one
// question a health screen must never answer.
// ---------------------------------------------------------------------------------------------

/** Every service's state, the host's metrics and the banner, read live. */
export function fetchHealthOverview(): Promise<HealthOverview> {
  return request<HealthOverview>("/api/v1/health/overview", { cache: "no-store" });
}

/**
 * Run every probe now and record the samples.
 *
 * The answer is a full overview rather than a run id, so the panel replaces what it has
 * with what the server now believes. A client that merged the new states into the old rows
 * would keep the last stored `healthy` for a service that has just gone down.
 */
export function runHealthChecks(): Promise<HealthOverview> {
  return request<HealthOverview>("/api/v1/health/checks/run", {
    method: "POST",
    cache: "no-store",
  });
}

/** One service, with the checks it ran and the metrics it has published. */
export function fetchHealthService(key: string): Promise<HealthServiceDetail> {
  return request<HealthServiceDetail>(`/api/v1/health/services/${encodeURIComponent(key)}`, {
    cache: "no-store",
  });
}

/**
 * One metric's series, oldest first.
 *
 * The window is a **named range** (`1h` / `24h` / `7d`), the same vocabulary
 * `/health/metrics` uses, and the server refuses anything else with a message naming
 * what is offered. It used to take `hours` and clamp it, which is the silent-clamp
 * shape: a caller asking for a month got a week with a `200`, drew the wrong chart,
 * and had no way to tell from the response. The parameter's *name* is the reason this
 * was worth changing rather than leaving compatible — `hours=24` and `range=24h` are
 * the same window with two spellings, and the second one travels into the CSV
 * filename, so there must be exactly one.
 */
export function fetchHealthSamples(
  service: string,
  metric: string,
  range: HealthRangeKey = "24h",
): Promise<HealthSamplePoint[]> {
  const query = new URLSearchParams({ service, metric, range });
  return request<HealthSamplePoint[]>(`/api/v1/health/samples?${query.toString()}`, {
    cache: "no-store",
  });
}

/** The one-line summary the security overview and the operator dashboard embed. */
export function fetchHealthSummary(): Promise<HealthSummary> {
  return request<HealthSummary>("/api/v1/health/summary", { cache: "no-store" });
}

/** The host's raw kernel readings, including the notes for anything unreadable. */
export function fetchHealthHost(): Promise<Record<string, unknown>> {
  return request<Record<string, unknown>>("/api/v1/health/host", { cache: "no-store" });
}

/** Drop raw samples past the retention window. Destructive, so it is a POST. */
export function pruneHealthSamples(): Promise<HealthPruneResult> {
  return request<HealthPruneResult>("/api/v1/health/maintenance/prune", { method: "POST" });
}

/**
 * `GET /api/v1/health/metrics` — the aggregated table for a named range.
 *
 * The range is a **name** (`1h`, `24h`, `7d`) rather than a number of hours, and the server
 * refuses anything else. The client cannot quietly ask for a window the panel has no label
 * for, which is what stops a table headed `7d` from holding a day.
 */
export function fetchHealthMetrics(
  range: HealthRangeKey = "24h",
): Promise<HealthMetricsReport> {
  return request<HealthMetricsReport>(
    `/api/v1/health/metrics?range=${encodeURIComponent(range)}`,
    { cache: "no-store" },
  );
}

/**
 * `GET /api/v1/health/metrics.csv` — exactly the rows the table is showing.
 *
 * The **server** renders the file from the same query the table used, and repeats the window in
 * `X-Health-Range`. The client never builds CSV from the rows it holds: a client-built export is
 * a client-chosen file, and "the export matches the range shown" is precisely the property that
 * a client-built export cannot promise.
 */
export async function downloadHealthMetricsCsv(
  range: HealthRangeKey = "24h",
): Promise<{ rows: number; blob: Blob; filename: string; range: string }> {
  const url = `/api/v1/health/metrics.csv?range=${encodeURIComponent(range)}`;
  let response: Response;
  try {
    response = await fetch(url, { credentials: "same-origin", headers: { accept: "text/csv" } });
  } catch {
    throw new ApiError(0, "network_error", "The Omnion API could not be reached.");
  }

  if (!response.ok) {
    const text = await response.text();
    let code = "export_failed";
    let message = `The export answered with status ${response.status}.`;
    try {
      const body = JSON.parse(text) as ErrorBody;
      code = body.error?.code ?? code;
      message = body.error?.message ?? message;
    } catch {
      // A non-JSON error body is still an error; the status stays in the message.
    }
    throw new ApiError(response.status, code, message);
  }

  const disposition = response.headers.get("content-disposition") ?? "";
  const match = /filename="?([^";]+)"?/.exec(disposition);
  const served = response.headers.get("x-health-range") ?? range;
  const blob = await response.blob();

  // The row count is read from the file itself rather than trusted from a header, because the
  // header the API sends is the same code path that made the mistake.
  const text = await blob.text();
  const rows = Math.max(0, text.split("\n").filter((line) => line.trim() !== "").length - 1);

  return { rows, blob, filename: match?.[1] ?? `omnion-health-${served}.csv`, range: served };
}

// ---------------------------------------------------------------------------------------------
// Incidents and threshold policy (REQ-014, slice 3)
// ---------------------------------------------------------------------------------------------

/**
 * `GET /api/v1/health/incidents` — a page of the timeline.
 *
 * The filters go out as query parameters and the server **refuses** a malformed instant rather
 * than ignoring it: a `from=` that silently widens to "no lower bound" is the kind of filter
 * that makes an incident screen agree with itself while showing the wrong week.
 */
export function fetchHealthIncidents(
  filter: {
    service?: string | null;
    state?: string | null;
    from?: string | null;
    to?: string | null;
    limit?: number;
    offset?: number;
  } = {},
): Promise<HealthIncidentPage> {
  const query = new URLSearchParams();
  if (filter.service) query.set("service", filter.service);
  if (filter.state) query.set("state", filter.state);
  if (filter.from) query.set("from", filter.from);
  if (filter.to) query.set("to", filter.to);
  if (filter.limit !== undefined) query.set("limit", String(filter.limit));
  if (filter.offset !== undefined) query.set("offset", String(filter.offset));
  const suffix = query.toString();
  return request<HealthIncidentPage>(
    `/api/v1/health/incidents${suffix ? `?${suffix}` : ""}`,
    { cache: "no-store" },
  );
}

/** `GET /api/v1/health/incidents/{id}` — one incident with its own detail. */
export function fetchHealthIncident(id: string): Promise<HealthIncident> {
  return request<HealthIncident>(`/api/v1/health/incidents/${encodeURIComponent(id)}`, {
    cache: "no-store",
  });
}

/**
 * `PATCH /api/v1/health/incidents/{id}` — acknowledge with a note, or resolve by hand.
 *
 * One endpoint for both, because they are one decision: an operator looking at an incident
 * either claims it or closes it.
 */
export function patchHealthIncident(
  id: string,
  action: HealthIncidentAction,
  note?: string,
): Promise<HealthIncident> {
  return request<HealthIncident>(`/api/v1/health/incidents/${encodeURIComponent(id)}`, {
    method: "PATCH",
    body: JSON.stringify({ action, note: note ?? "" }),
  });
}

/** `GET /api/v1/health/settings` — the policy, its bounds and its suggestions. */
export function fetchHealthSettings(): Promise<HealthSettings> {
  return request<HealthSettings>("/api/v1/health/settings", { cache: "no-store" });
}

/**
 * `PUT /api/v1/health/settings` — save intervals, pairs and toggles.
 *
 * `thresholds` is omitted entirely when the caller did not change a pair, so a save of one
 * interval cannot silently reset every threshold to whatever the form's placeholders say. That
 * is why the argument is `null`-able rather than an empty array: an empty array is a request to
 * erase the policy.
 */
export function saveHealthSettings(update: {
  check_interval_seconds?: number;
  worker_stale_seconds?: number;
  thresholds?: HealthThreshold[] | null;
  notifications?: Record<string, boolean>;
}): Promise<HealthSettings> {
  const body: Record<string, unknown> = {};
  if (update.check_interval_seconds !== undefined) {
    body.check_interval_seconds = update.check_interval_seconds;
  }
  if (update.worker_stale_seconds !== undefined) {
    body.worker_stale_seconds = update.worker_stale_seconds;
  }
  if (update.thresholds !== undefined && update.thresholds !== null) {
    body.thresholds = update.thresholds.map((row) => ({
      metric: row.metric,
      warn: row.warn,
      crit: row.crit,
      direction: row.direction,
    }));
  }
  if (update.notifications !== undefined) body.notifications = update.notifications;
  return request<HealthSettings>("/api/v1/health/settings", {
    method: "PUT",
    body: JSON.stringify(body),
  });
}

/** `GET /api/v1/health/maintenance-windows` — the windows, newest first. */
export function fetchHealthMaintenanceWindows(): Promise<HealthMaintenanceWindow[]> {
  return request<HealthMaintenanceWindow[]>("/api/v1/health/maintenance-windows", {
    cache: "no-store",
  });
}

/**
 * `POST /api/v1/health/maintenance-windows` — create one.
 *
 * An empty `services` array is the deploy case and covers every service; that is a real choice
 * the form makes explicitly rather than a default the API invents.
 */
export function createHealthMaintenanceWindow(input: {
  starts_at: string;
  ends_at: string;
  services?: string[];
  note?: string;
}): Promise<HealthMaintenanceWindow> {
  return request<HealthMaintenanceWindow>("/api/v1/health/maintenance-windows", {
    method: "POST",
    body: JSON.stringify({
      starts_at: input.starts_at,
      ends_at: input.ends_at,
      services: input.services ?? [],
      note: input.note ?? "",
    }),
  });
}

/** `DELETE /api/v1/health/maintenance-windows/{id}` — withdraw a window. */
export function deleteHealthMaintenanceWindow(id: string): Promise<void> {
  return request<void>(`/api/v1/health/maintenance-windows/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

// ---------------------------------------------------------------------------------------------
// AI App Builder (REQ-045).
//
// The console and the review workspace talk to nine routes and one stream. Two decisions are
// worth naming because they are the reason the client looks like this:
//
// * **The decision routes answer with the counters and the blockers.** Accepting one artifact
//   usually removes one blocker, so a client that re-fetched the whole plan after every
//   decision would spend a round trip to redraw three numbers the answer already carried —
//   and would show a stale count for the duration of the request.
// * **`getAppBuilderPlan` is the only read that is not cached.** The review screen is a
//   decision surface: a plan that renders from a cached body is a plan whose artifacts may
//   have been regenerated since, which is precisely the change the reviewer came to see.
// ---------------------------------------------------------------------------------------------

/** The composer's vocabulary: sample prompts, kinds, statuses, field types. */
export function fetchAppBuilderVocabulary(): Promise<AppBuilderVocabulary> {
  return request<AppBuilderVocabulary>("/api/v1/app-builder/examples");
}

/**
 * The plan list. Every filter is in the URL, so a reload and a bookmark land on the same
 * view — the QA pass depends on it, and so does anybody comparing two screenshots.
 */
export function fetchAppBuilderPlans(query: {
  status?: string;
  q?: string;
  mine?: boolean;
  offset?: number;
  limit?: number;
  organizationId?: string;
} = {}): Promise<AppBuilderPlanList> {
  const params = new URLSearchParams();
  if (query.status) params.set("status", query.status);
  if (query.q && query.q.trim()) params.set("q", query.q.trim());
  if (query.mine) params.set("mine", "true");
  if (query.offset) params.set("offset", String(query.offset));
  if (query.limit) params.set("limit", String(query.limit));
  if (query.organizationId) params.set("organization_id", query.organizationId);
  const search = params.toString();
  return request<AppBuilderPlanList>(`/api/v1/app-builder/plans${search ? `?${search}` : ""}`);
}

/** One plan with its artifacts, counters and blockers — the review workspace's whole state. */
export function fetchAppBuilderPlan(planId: string): Promise<AppBuilderPlanDetail> {
  return request<AppBuilderPlanDetail>(
    `/api/v1/app-builder/plans/${encodeURIComponent(planId)}`,
    { cache: "no-store" },
  );
}

/** Accept one artifact. Refused with `422` naming the finding when the validator refused it. */
export function acceptAppBuilderArtifact(
  planId: string,
  artifactId: string,
): Promise<AppBuilderDecision> {
  return request<AppBuilderDecision>(
    `/api/v1/app-builder/plans/${encodeURIComponent(planId)}/artifacts/${encodeURIComponent(artifactId)}/accept`,
    { method: "POST" },
  );
}

/** Refuse one artifact. `reason` is required by the store: a bare refusal teaches nobody anything. */
export function rejectAppBuilderArtifact(
  planId: string,
  artifactId: string,
  reason: string,
): Promise<AppBuilderDecision> {
  return request<AppBuilderDecision>(
    `/api/v1/app-builder/plans/${encodeURIComponent(planId)}/artifacts/${encodeURIComponent(artifactId)}/reject`,
    { method: "POST", body: JSON.stringify({ reason }) },
  );
}

/**
 * Edit one draft artifact. The body is **untrusted input of the same kind the model's is**, so
 * it goes back through the same validator: a `422` here carries the field path that broke.
 */
export function editAppBuilderArtifact(
  planId: string,
  artifactId: string,
  spec: Record<string, unknown>,
): Promise<AppBuilderDecision> {
  return request<AppBuilderDecision>(
    `/api/v1/app-builder/plans/${encodeURIComponent(planId)}/artifacts/${encodeURIComponent(artifactId)}`,
    { method: "PATCH", body: JSON.stringify({ spec }) },
  );
}

/** Refuse the whole plan. */
export function rejectAppBuilderPlan(planId: string, reason: string): Promise<AppBuilderPlanDecision> {
  return request<AppBuilderPlanDecision>(
    `/api/v1/app-builder/plans/${encodeURIComponent(planId)}/reject`,
    { method: "POST", body: JSON.stringify({ reason }) },
  );
}

/** Delete a plan that was never applied; an applied plan answers `409` and stays. */
export function deleteAppBuilderPlan(planId: string): Promise<void> {
  return request<void>(`/api/v1/app-builder/plans/${encodeURIComponent(planId)}`, {
    method: "DELETE",
  });
}

/**
 * Remove a selection of drafts in one call (REQ-045 slice 4).
 *
 * **`POST /plans/bulk-delete` and not `DELETE /plans/{id}` repeated by the client.** Three
 * reasons, and the third is the one that decides it:
 *
 * * twenty row-level deletes are twenty round trips, twenty audit rows and twenty chances for
 *   a filter change to land between two of them;
 * * a per-row delete cannot report a *partial* outcome — it answers `204` or `409` for the
 *   whole page, so a selection mixing three drafts with the one applied plan would show the
 *   operator a refusal and leave the three untouched;
 * * the refusals are the server's. An applied plan has to stay, and the client does not own
 *   that rule — it re-derives it, it can only ever agree with the store until it does not.
 */
export function bulkDeleteAppBuilderPlans(ids: string[]): Promise<AppBuilderBulkDelete> {
  return request<AppBuilderBulkDelete>("/api/v1/app-builder/plans/bulk-delete", {
    method: "POST",
    body: JSON.stringify({ ids }),
  });
}

/**
 * The plan as a downloadable JSON document (REQ-045 slice 4).
 *
 * A raw `fetch` rather than `request<T>` for two reasons, both of which the health export
 * above already had to solve: the response is an **attachment**, so going through the JSON
 * helper would parse the file into an object and throw away the one thing the caller wants
 * (the bytes), and a `request` that cannot parse the body reports a parse failure as an
 * API error with a status of `200`.
 *
 * The artifact count is read back **out of the file**, not taken from a header, because the
 * header would be written by the same code path that made the mistake — the assertion in the
 * console's note ("22 artifacts") is only worth anything if it was counted from the
 * download the operator got.
 */
export async function downloadAppBuilderPlanExport(
  planId: string,
): Promise<{ blob: Blob; filename: string; artifacts: number; schema: string }> {
  const url = `/api/v1/app-builder/plans/${encodeURIComponent(planId)}/export`;
  let response: Response;
  try {
    response = await fetch(url, {
      credentials: "same-origin",
      headers: { accept: "application/json" },
      cache: "no-store",
    });
  } catch {
    throw new ApiError(0, "network_error", "The Omnion API could not be reached.");
  }

  if (!response.ok) {
    let code = "export_failed";
    let message = `The export answered with status ${response.status}.`;
    try {
      const body = JSON.parse(await response.text()) as ErrorBody;
      code = body.error?.code ?? code;
      message = body.error?.message ?? message;
    } catch {
      // A non-JSON error body is still an error; the status stays in the message.
    }
    throw new ApiError(response.status, code, message);
  }

  const disposition = response.headers.get("content-disposition") ?? "";
  const match = /filename="?([^";]+)"?/.exec(disposition);
  const blob = await response.blob();

  let artifacts = 0;
  let schema = "";
  try {
    const parsed = JSON.parse(await blob.text()) as {
      schema?: string;
      artifacts?: unknown[];
    };
    artifacts = Array.isArray(parsed.artifacts) ? parsed.artifacts.length : 0;
    schema = parsed.schema ?? "";
  } catch {
    // An unparseable download is reported by the caller's note as an artifact count of
    // zero, which is the honest reading: the file did not carry a plan this panel can name.
  }

  return { blob, filename: match?.[1] ?? `omnion-app-plan-${planId.slice(0, 8)}.json`, artifacts, schema };
}

/** One frame of a generation, as the two handlers report it. */
export type AppBuilderStreamHandlers = {
  /** What the platform is doing right now (`plan`). */
  onStage?: (stage: string) => void;
  /** The plan the generation produced or failed on — it exists either way. */
  onFailed?: (failure: { code: string; message: string }) => void;
};

/**
 * Generate a plan from a prompt, reading the stream to its end.
 *
 * The frames carry **no artifact bodies**: the answer is validated before it is stored, so a
 * client that rendered deltas would be rendering something apply may still refuse. What the
 * stream does carry is the failure — and the plan row exists whichever way it ends, so the
 * reviewer can *see* the attempt that failed instead of watching a banner and losing the
 * sentence they typed. `signal` is the Cancel button; aborting stops the read and nothing
 * is written from this side.
 */
export async function streamGenerateAppBuilderPlan(
  input: { prompt: string; model?: string; title?: string; organizationId?: string },
  handlers: AppBuilderStreamHandlers = {},
  signal?: AbortSignal,
): Promise<void> {
  const response = await fetch("/api/v1/app-builder/generate", {
    method: "POST",
    credentials: "same-origin",
    headers: {
      "content-type": "application/json",
      accept: "text/event-stream",
      // The METHOD is what makes this a mutating call; a `{ signal }`-only init reads as a
      // GET, sends no token, and this one screen answers `403 csrf_failed` while every other
      // write on the platform works.
      ...csrfHeader({ method: "POST" }),
    },
    body: JSON.stringify({
      prompt: input.prompt,
      model: input.model && input.model.trim() ? input.model.trim() : null,
      title: input.title && input.title.trim() ? input.title.trim() : null,
      organization_id: input.organizationId || null,
    }),
    signal,
  });

  if (!response.ok || !response.body) {
    const payload = (await readJson(response)) as ErrorBody | null;
    throw new ApiError(
      response.status,
      payload?.error?.code ?? "unknown_error",
      payload?.error?.message ?? `The API answered with status ${response.status}.`,
    );
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });

    let boundary = buffer.indexOf("\n\n");
    while (boundary >= 0) {
      const frame = buffer.slice(0, boundary);
      buffer = buffer.slice(boundary + 2);
      boundary = buffer.indexOf("\n\n");

      let event = "message";
      let data = "";
      for (const line of frame.split("\n")) {
        if (line.startsWith("event: ")) event = line.slice(7).trim();
        else if (line.startsWith("data: ")) data += line.slice(6);
      }
      if (!data) continue;

      const payload = JSON.parse(data) as Record<string, unknown>;
      if (event === "stage") {
        handlers.onStage?.(payload.stage as string);
      } else if (event === "error") {
        handlers.onFailed?.({
          code: (payload.code as string) ?? "generation_failed",
          message: (payload.message as string) ?? "The generation did not finish.",
        });
        return;
      }
    }
  }
}

/**
 * Ask the model for one artifact again, with a note, streaming.
 *
 * Same reader as the plan generation on purpose: a revision that answered after one round
 * trip would leave the operator pressing a button again, and the review screen would need a
 * second implementation of the progress panel the console already has. The answer carries
 * the artifact as it now reads, the counters and the blockers.
 */
export async function streamRegenerateAppBuilderArtifact(
  planId: string,
  artifactId: string,
  feedback: string,
  handlers: {
    onStage?: (stage: string) => void;
    onArtifact?: (decision: AppBuilderDecision) => void;
  } = {},
  model?: string,
  signal?: AbortSignal,
): Promise<void> {
  const response = await fetch(
    `/api/v1/app-builder/plans/${encodeURIComponent(planId)}/artifacts/${encodeURIComponent(artifactId)}/regenerate`,
    {
      method: "POST",
      credentials: "same-origin",
      headers: {
        "content-type": "application/json",
        accept: "text/event-stream",
        ...csrfHeader({ method: "POST" }),
      },
      body: JSON.stringify({
        feedback,
        model: model && model.trim() ? model.trim() : null,
      }),
      signal,
    },
  );

  if (!response.ok || !response.body) {
    const payload = (await readJson(response)) as ErrorBody | null;
    throw new ApiError(
      response.status,
      payload?.error?.code ?? "unknown_error",
      payload?.error?.message ?? `The API answered with status ${response.status}.`,
    );
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });

    let boundary = buffer.indexOf("\n\n");
    while (boundary >= 0) {
      const frame = buffer.slice(0, boundary);
      buffer = buffer.slice(boundary + 2);
      boundary = buffer.indexOf("\n\n");

      let event = "message";
      let data = "";
      for (const line of frame.split("\n")) {
        if (line.startsWith("event: ")) event = line.slice(7).trim();
        else if (line.startsWith("data: ")) data += line.slice(6);
      }
      if (!data) continue;

      const payload = JSON.parse(data) as Record<string, unknown>;
      if (event === "stage") {
        handlers.onStage?.(payload.stage as string);
      } else if (event === "artifact") {
        handlers.onArtifact?.(payload as unknown as AppBuilderDecision);
        return;
      } else if (event === "error") {
        throw new ApiError(
          502,
          (payload.code as string) ?? "regeneration_failed",
          (payload.message as string) ?? "The regeneration did not finish.",
        );
      }
    }
  }
}

// ---------------------------------------------------------------------------------------------
// Developer portal (REQ-022, slice 2)
// ---------------------------------------------------------------------------------------------
//
// These live here rather than in `lib/developer.ts` because this module owns the session cookie,
// the CSRF header and the `ApiError` shape; a second file calling `fetch` itself would be a
// second implementation of all three. The portal's *types* and its CSV builder live in
// `lib/developer.ts` — a shape and a pure function need neither a cookie nor an error type.

/** `GET /api/v1/developer/overview` — the card row and the recent failures. */
export function fetchDeveloperOverview(): Promise<DeveloperOverview> {
  return request<DeveloperOverview>("/api/v1/developer/overview", { cache: "no-store" });
}

/** `GET /api/v1/developer/api-keys` — the list, with its three optional filters. */
export function fetchDeveloperKeys(
  filters: {
    environment?: string | null;
    status?: string | null;
    search?: string | null;
  } = {},
): Promise<DeveloperKey[]> {
  const query = new URLSearchParams();
  if (filters.environment) query.set("environment", filters.environment);
  if (filters.status) query.set("status", filters.status);
  if (filters.search) query.set("search", filters.search);
  const suffix = query.toString();
  return request<DeveloperKey[]>(
    `/api/v1/developer/api-keys${suffix ? `?${suffix}` : ""}`,
    { cache: "no-store" },
  );
}

/**
 * `POST /api/v1/developer/api-keys` — the only call that returns a token.
 *
 * The caller must show it once and let it go; nothing here caches the answer, and the panel
 * keeps it in component state that is dropped when the reveal dialog closes.
 */
export function createDeveloperKey(
  input: CreateDeveloperKeyInput,
): Promise<IssuedDeveloperKey> {
  return request<IssuedDeveloperKey>("/api/v1/developer/api-keys", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** `GET /api/v1/developer/api-keys/{id}`. */
export function fetchDeveloperKey(id: string): Promise<DeveloperKeyDetail> {
  return request<DeveloperKeyDetail>(
    `/api/v1/developer/api-keys/${encodeURIComponent(id)}`,
    { cache: "no-store" },
  );
}

/** `POST /api/v1/developer/api-keys/{id}/rotate` — a new secret, the old one dead at once. */
export function rotateDeveloperKey(id: string): Promise<IssuedDeveloperKey> {
  return request<IssuedDeveloperKey>(
    `/api/v1/developer/api-keys/${encodeURIComponent(id)}/rotate`,
    { method: "POST" },
  );
}

/** `DELETE /api/v1/developer/api-keys/{id}` — a soft revoke; the row and its logs stay. */
export function revokeDeveloperKey(id: string): Promise<{ revoked: boolean; id: string }> {
  return request<{ revoked: boolean; id: string }>(
    `/api/v1/developer/api-keys/${encodeURIComponent(id)}`,
    { method: "DELETE" },
  );
}

/** `GET /api/v1/developer/scopes` — grouped in the API, never re-derived in the panel. */
export function fetchDeveloperScopes(): Promise<DeveloperScopeCatalogue> {
  return request<DeveloperScopeCatalogue>("/api/v1/developer/scopes", { cache: "no-store" });
}

/** `GET /api/v1/developer/logs` — one page, filtered. */
export function fetchDeveloperLogs(
  filters: DeveloperLogFilters = {},
  before?: number | null,
): Promise<DeveloperLogPage> {
  const query = new URLSearchParams();
  if (filters.api_key_id) query.set("api_key_id", filters.api_key_id);
  if (filters.method) query.set("method", filters.method);
  if (filters.path_prefix) query.set("path_prefix", filters.path_prefix);
  if (filters.status_class) query.set("status_class", filters.status_class);
  if (filters.window_days) query.set("window_days", String(filters.window_days));
  if (before) query.set("before", String(before));
  const suffix = query.toString();
  return request<DeveloperLogPage>(`/api/v1/developer/logs${suffix ? `?${suffix}` : ""}`, {
    cache: "no-store",
  });
}

/** `GET /api/v1/developer/logs/{id}` — the detail drawer. */
export function fetchDeveloperLog(id: number): Promise<DeveloperLogDetail> {
  return request<DeveloperLogDetail>(`/api/v1/developer/logs/${id}`, { cache: "no-store" });
}
