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
} from "./types";

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
    params.set(key, value === true ? "true" : String(value));
  }
  return params.toString();
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

/** Rename, move or edit the metadata of one file. */
export function updateMediaFile(
  fileId: string,
  patch: {
    filename?: string;
    alt_text?: string;
    caption?: string;
    description?: string;
    tags?: string[];
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
  kind: AiProviderKind;
  base_url: string;
  has_api_key: boolean;
  timeout_ms: number;
  max_retries: number;
  priority: number;
  last_health: AiHealthStatus;
  last_checked_at: string | null;
  last_error: string | null;
  enabled: boolean;
  is_default: boolean;
  model_count: number;
  created_at: string;
  updated_at: string;
};

/** Where a provider lives: a hosted API, or one on the operator's own network. */
export type AiProviderKind = "cloud" | "local";

/** What the last health probe found. `unknown` means it has never been probed. */
export type AiHealthStatus = "ok" | "degraded" | "down" | "unknown";

/** One protocol the form offers, with the note the panel shows under the select. */
export type AiProtocol = {
  protocol: string;
  note: string;
  chat_path: string;
  auth: string;
};

/** The numeric ranges the form validates against, from the same constants the API uses. */
export type AiProtocolBounds = {
  timeout_ms_min: number;
  timeout_ms_max: number;
  max_retries_max: number;
  priority_min: number;
  priority_max: number;
};

/** One step of the connection test. */
export type AiTestStep = {
  step: string;
  label: string;
  status: "pending" | "ok" | "failed" | "skipped";
  latency_ms: number;
  error?: string;
  note?: string;
};

/** The whole connection test, as the panel renders it. */
export type AiTestReport = {
  provider_id: string;
  provider_name: string;
  protocol: string;
  steps: AiTestStep[];
  total_ms: number;
  ok: boolean;
  failing_step?: string;
  model_count?: number;
  known_models?: number;
  summary: string;
};

/** One named thing a model can do. The vocabulary is closed and comes from the API. */
export type AiCapability =
  | "chat"
  | "streaming"
  | "tools"
  | "vision"
  | "json_mode"
  | "embeddings"
  | "image_generation"
  | "audio_generation"
  | "transcription"
  | "list_models";

/** One entry of the closed capability catalog the flag editor renders. */
export type AiCapabilityInfo = {
  capability: AiCapability;
  note: string;
  /** `false` for a fact about the endpoint rather than a model's to claim. */
  editable: boolean;
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
  supports_image_generation: boolean;
  supports_audio_generation: boolean;
  supports_transcription: boolean;
  supports_json_mode: boolean;
  max_output_tokens: number | null;
  enabled: boolean;
  is_default: boolean;
  model_id: string;
  /** The closed vocabulary, so a new flag needs no second edit in the panel. */
  capability_catalog: AiCapabilityInfo[];
  /** The capabilities this model actually claims, in catalog order. */
  capabilities: AiCapability[];
  /** What it costs, in both precisions (REQ-098 slice 1). */
  price: AiModelPrice;
  /** Where the capability flags came from: `manual`, `discovery` or `probe`. */
  capabilities_source: string;
  /** When the capability flags were last confirmed against something. */
  capabilities_verified_at: string | null;
  created_at: string;
  updated_at: string;
};

/**
 * One model's price.
 *
 * Both the per-million figure the column stores and the per-1K rendering the table shows, so
 * the panel can switch between them without rounding differently from the export. `complete` is
 * the important one: a model with only an input price has an *unknown* cost, and rendering the
 * missing half as zero would make every estimate built from this row too small.
 */
export type AiModelPrice = {
  input_micros_per_mtok: number | null;
  output_micros_per_mtok: number | null;
  input_micros_per_1k: number | null;
  output_micros_per_1k: number | null;
  /** `manual`, `discovery` or `probe`. */
  source: string;
  /** What that source means, so the panel does not have to hard-code the wording. */
  source_note: string;
  updated_at: string | null;
  complete: boolean;
  age_days: number | null;
  stale: boolean;
};

/** How a catalog listing is narrowed; every field is optional and independent. */
export type AiModelQuery = {
  /** Matches the model key, the display name and the provider name. */
  q?: string;
  /** Capabilities a row must **all** claim; empty means no filter. */
  capabilities?: AiCapability[];
  providerId?: string;
  /** `enabled` or `disabled`; absent is both. */
  status?: "enabled" | "disabled";
  /** Which column the table is sorted by. */
  sort?: "model" | "provider" | "context" | "price" | "updated";
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
  supports_image_generation?: boolean;
  supports_audio_generation?: boolean;
  supports_transcription?: boolean;
  supports_json_mode?: boolean;
  max_output_tokens?: number;
};

/** What one discovery line means for the registry. */
export type AiDiscoveryAction = "added" | "changed" | "removed";

/** One line of a discovery diff. */
export type AiDiscoveryLine = {
  model_key: string;
  action: AiDiscoveryAction;
  changed_fields: string[];
};

/** What a discovery run found, and what applying it would do. Nothing is written by the read. */
export type AiDiscoveryReport = {
  provider_id: string;
  provider_name: string;
  reported: string[];
  stored: string[];
  lines: AiDiscoveryLine[];
  reported_count: number;
  stored_count: number;
  added: number;
  removed: number;
  changed: number;
  /** `true` when applying would change nothing. */
  up_to_date: boolean;
};

/** One message of a chat request. */
export type ChatMessageInput = {
  role: "system" | "user" | "assistant";
  content: string;
};

/**
 * A change set the answer proposed, filed for review (REQ-101, slice 3g).
 *
 * The platform files it, not the client: a proposal read out of model text is a **claim**, so
 * it becomes a `draft` in the same inbox every other request lands in, and the reviewer edits
 * and confirms it there. A client that rendered its own "here are the changes" block would be
 * describing work the platform has no record of.
 */
export type ChatProposal = {
  change_set_id: string;
  title: string;
  operations: number;
  /** `true` when confirming this set would park at least one operation for a second person. */
  needs_approval: boolean;
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

/**
 * The model registry, narrowed (REQ-098 slice 1).
 *
 * The narrowing is sent to the server rather than applied here: the API and the table have to
 * agree about which rows a query means, and a client-side filter would make the acceptance
 * criterion that the capability chips narrow the *listing* true of the panel and false of the
 * endpoint. An empty result therefore means the server found nothing — never "the panel hid
 * them".
 */
export async function fetchAiModels(query: AiModelQuery = {}): Promise<AiModel[]> {
  const params = new URLSearchParams();
  // A blank search is omitted rather than sent as `q=`: an empty needle and no needle must mean
  // the same thing, and the server treats a whitespace-only value as no filter anyway.
  if (query.q?.trim()) params.set("q", query.q.trim());
  if (query.capabilities?.length) params.set("capability", query.capabilities.join(","));
  if (query.providerId) params.set("provider_id", query.providerId);
  if (query.status) params.set("status", query.status);
  if (query.sort) params.set("sort", query.sort);

  const suffix = params.toString();
  const body = await request<{ models: AiModel[] }>(
    suffix ? `/api/v1/ai/models?${suffix}` : "/api/v1/ai/models",
  );
  return body.models;
}

/** The protocols the provider form offers, and the ranges it validates against. */
export async function fetchAiProtocols(): Promise<{
  protocols: AiProtocol[];
  bounds: AiProtocolBounds;
}> {
  return request<{ protocols: AiProtocol[]; bounds: AiProtocolBounds }>("/api/v1/ai/protocols");
}

/** Run the connection test against a stored provider, server-side. */
export function testAiProvider(providerId: string): Promise<AiTestReport> {
  return request<AiTestReport>(`/api/v1/ai/providers/${encodeURIComponent(providerId)}/test`, {
    method: "POST",
    body: JSON.stringify({}),
  });
}

/** Connect a provider, optionally with the models it serves. */
export function connectAiProvider(input: {
  name: string;
  baseUrl: string;
  protocol?: string;
  kind?: AiProviderKind;
  apiKey?: string;
  timeoutMs?: number;
  maxRetries?: number;
  priority?: number;
  enabled?: boolean;
  isDefault?: boolean;
  models?: AiModelInput[];
}): Promise<AiProvider> {
  return request<AiProvider>("/api/v1/ai/providers", {
    method: "POST",
    body: JSON.stringify({
      name: input.name,
      base_url: input.baseUrl,
      protocol: input.protocol,
      kind: input.kind,
      api_key: input.apiKey && input.apiKey.trim() ? input.apiKey.trim() : null,
      timeout_ms: input.timeoutMs,
      max_retries: input.maxRetries,
      priority: input.priority,
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
    kind?: AiProviderKind;
    timeoutMs?: number;
    maxRetries?: number;
    priority?: number;
    enabled?: boolean;
    isDefault?: boolean;
  },
): Promise<AiProvider> {
  const body: Record<string, unknown> = {};
  if (changes.name !== undefined) body.name = changes.name;
  if (changes.baseUrl !== undefined) body.base_url = changes.baseUrl;
  if (changes.apiKey !== undefined) body.api_key = changes.apiKey;
  if (changes.kind !== undefined) body.kind = changes.kind;
  if (changes.timeoutMs !== undefined) body.timeout_ms = changes.timeoutMs;
  if (changes.maxRetries !== undefined) body.max_retries = changes.maxRetries;
  if (changes.priority !== undefined) body.priority = changes.priority;
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

// ---------------------------------------------------------------------------------------------
// AI Hub — health, usage and the failover chain (REQ-097 slice 3)
// ---------------------------------------------------------------------------------------------

/** One probe sample, newest first. `status` is what *this* probe saw. */
export type AiHealthSample = {
  id: number;
  provider_id: string;
  status: AiHealthStatus;
  latency_ms: number;
  http_status: number | null;
  error: string | null;
  checked_at: string;
};

/**
 * The header above the samples.
 *
 * `uptime_percent` is `null` — not `0` and not `100` — when the window holds no sample at all: a
 * provider nobody has probed yet has no uptime, and rendering it as a number is the first lie the
 * panel would tell.
 */
export type AiHealthSummary = {
  status: AiHealthStatus;
  uptime_percent: number | null;
  p95_latency_ms: number | null;
  sample_count: number;
  baseline_latency_ms: number | null;
  last_checked_at: string | null;
  last_error: string | null;
};

/** The Health tab's payload: header, samples and the windows it may offer, in one call. */
export type AiHealthView = {
  provider_id: string;
  provider_name: string;
  /** The window the *server* applied, echoed back so the label cannot lie. */
  window: string;
  summary: AiHealthSummary;
  samples: AiHealthSample[];
  windows: string[];
};

/** One day of a provider's usage. */
export type AiUsageDay = {
  day: string;
  requests: number;
  errors: number;
  prompt_tokens: number | null;
  completion_tokens: number | null;
};

/**
 * The Usage tab's totals.
 *
 * `missing_usage` is the number of calls that reported no token counts: they are excluded from the
 * sums and reported here, so a stream that ended without a usage frame shows as unknown rather
 * than quietly becoming free.
 */
export type AiUsageSummary = {
  requests: number;
  errors: number;
  prompt_tokens: number;
  completion_tokens: number;
  missing_usage: number;
  p95_latency_ms: number | null;
  error_rate_percent: number;
  /**
   * REQ-098 slice 5: what the window cost, summed from the snapshots stored on each usage row.
   *
   * `null` is not zero. It means no call in the window had a knowable cost — either the model was
   * never priced, or the endpoint reported no token counts. A window of only free models really
   * does cost `0`, and the panel shows those two differently, because only one of them is a fact
   * about the money and the other is a fact about the data.
   */
  cost_micros: number | null;
  /** How many calls in the window had no knowable cost. */
  uncosted_calls: number;
  by_day: AiUsageDay[];
};

/** The Usage tab's payload. */
export type AiUsageView = {
  provider_id: string;
  provider_name: string;
  window: string;
  summary: AiUsageSummary;
  windows: string[];
};

/** One step of the failover chain, as the preview draws it. */
export type AiFailoverEntry = {
  rank: number;
  id: string;
  name: string;
  priority: number;
  health: AiHealthStatus;
  is_default: boolean;
};

/** The chain and the membership it may be drawn from. */
export type AiFailoverView = {
  chain: AiFailoverEntry[];
  providers: string[];
};

/** What "Probe now" answers with — the refreshed header, so the tab needs no reload. */
export type AiProbeOutcome = {
  provider_id: string;
  ok: boolean;
  latency_ms: number;
  failing_step: string | null;
  transition: { from: AiHealthStatus; to: AiHealthStatus } | null;
  summary: AiHealthSummary;
  report: AiTestReport;
};

/** Read a provider's Health tab over a window (`1h`, `6h`, `24h`, `7d`, `30d`). */
export function fetchAiProviderHealth(
  providerId: string,
  window = "24h",
): Promise<AiHealthView> {
  return request<AiHealthView>(
    `/api/v1/ai/providers/${encodeURIComponent(providerId)}/health?window=${encodeURIComponent(window)}`,
  );
}

/** Read a provider's Usage tab over a window. */
export function fetchAiProviderUsage(
  providerId: string,
  window = "24h",
): Promise<AiUsageView> {
  return request<AiUsageView>(
    `/api/v1/ai/providers/${encodeURIComponent(providerId)}/usage?window=${encodeURIComponent(window)}`,
  );
}

/** Take one probe sample now, and get the refreshed header back with it. */
export function probeAiProvider(providerId: string): Promise<AiProbeOutcome> {
  return request<AiProbeOutcome>(
    `/api/v1/ai/providers/${encodeURIComponent(providerId)}/probe`,
    { method: "POST", body: JSON.stringify({}) },
  );
}

/** The failover chain as the router walks it right now. */
export function fetchAiFailover(): Promise<AiFailoverView> {
  return request<AiFailoverView>("/api/v1/ai/failover");
}

/**
 * Persist the failover order.
 *
 * The answer is the chain *as the server stored it* — a client that reordered optimistically must
 * render this, not its own guess, or a rejected order would look accepted.
 */
export function setAiFailoverOrder(providerIds: string[]): Promise<AiFailoverView> {
  return request<AiFailoverView>("/api/v1/ai/failover", {
    method: "PUT",
    body: JSON.stringify({ provider_ids: providerIds }),
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

/**
 * Ask a provider which models it serves and get back a **diff** — what applying it would add,
 * change and remove. Nothing is written: the panel shows the diff and the operator confirms by
 * calling `applyAiProviderDiscovery`.
 */
export function discoverAiProviderModels(providerId: string): Promise<AiDiscoveryReport> {
  return request<AiDiscoveryReport>(
    `/api/v1/ai/providers/${encodeURIComponent(providerId)}/discover-models`,
    { method: "POST", body: JSON.stringify({}) },
  );
}

/** Apply the diff a discovery run reported, after the operator confirmed it. */
export function applyAiProviderDiscovery(providerId: string): Promise<AiDiscoveryReport> {
  return request<AiDiscoveryReport>(
    `/api/v1/ai/providers/${encodeURIComponent(providerId)}/apply-discovery`,
    { method: "POST", body: JSON.stringify({}) },
  );
}

/**
 * Change one model: its capability flags, its token limits, whether it is on, and whether it is
 * the installation's default.
 *
 * `maxOutputTokens: null` forgets the stored ceiling and `undefined` leaves it alone, exactly
 * like the provider key's three cases.
 */
export function updateAiModel(
  modelId: string,
  changes: {
    enabled?: boolean;
    isDefault?: boolean;
    displayName?: string;
    contextWindow?: number | null;
    supportsTools?: boolean;
    supportsVision?: boolean;
    supportsStreaming?: boolean;
    supportsEmbeddings?: boolean;
    supportsImageGeneration?: boolean;
    supportsAudioGeneration?: boolean;
    supportsTranscription?: boolean;
    supportsJsonMode?: boolean;
    maxOutputTokens?: number | null;
    /**
     * Micros per million tokens, in and out. `null` forgets the stored half and `undefined`
     * leaves it alone — three cases, because a price has to be erasable or an operator who
     * mistyped it can only ever make it more wrong.
     *
     * A price edit changes the cost of the **next** request only. Nothing recomputes what an
     * earlier call was billed at, because a bill that changes after the fact is worse than one
     * that was approximately right.
     */
    inputCostMicrosPerMtok?: number | null;
    outputCostMicrosPerMtok?: number | null;
    /**
     * `manual`, `discovery` or `probe`. Omitted by a flag toggle on purpose: restamping the
     * source on every capability edit would claim the price had been re-verified today.
     */
    priceSource?: string;
    capabilitiesSource?: string;
    capabilitiesVerifiedAt?: string | null;
  },
): Promise<AiModel> {
  const body: Record<string, unknown> = {};
  if (changes.enabled !== undefined) body.enabled = changes.enabled;
  if (changes.isDefault !== undefined) body.is_default = changes.isDefault;
  if (changes.displayName !== undefined) body.display_name = changes.displayName;
  if (changes.contextWindow !== undefined) body.context_window = changes.contextWindow;
  if (changes.supportsTools !== undefined) body.supports_tools = changes.supportsTools;
  if (changes.supportsVision !== undefined) body.supports_vision = changes.supportsVision;
  if (changes.supportsStreaming !== undefined) body.supports_streaming = changes.supportsStreaming;
  if (changes.supportsEmbeddings !== undefined) body.supports_embeddings = changes.supportsEmbeddings;
  if (changes.supportsImageGeneration !== undefined)
    body.supports_image_generation = changes.supportsImageGeneration;
  if (changes.supportsAudioGeneration !== undefined)
    body.supports_audio_generation = changes.supportsAudioGeneration;
  if (changes.supportsTranscription !== undefined)
    body.supports_transcription = changes.supportsTranscription;
  if (changes.supportsJsonMode !== undefined) body.supports_json_mode = changes.supportsJsonMode;
  if (changes.maxOutputTokens !== undefined) body.max_output_tokens = changes.maxOutputTokens;
  if (changes.inputCostMicrosPerMtok !== undefined)
    body.input_cost_micros_per_mtok = changes.inputCostMicrosPerMtok;
  if (changes.outputCostMicrosPerMtok !== undefined)
    body.output_cost_micros_per_mtok = changes.outputCostMicrosPerMtok;
  if (changes.priceSource !== undefined) body.price_source = changes.priceSource;
  if (changes.capabilitiesSource !== undefined) body.capabilities_source = changes.capabilitiesSource;
  if (changes.capabilitiesVerifiedAt !== undefined)
    body.capabilities_verified_at = changes.capabilitiesVerifiedAt;
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
/**
 * The instruction a chat sends so the model can propose changes (REQ-101, slice 3g).
 *
 * Read from the API rather than written here on purpose: the parser has one exact fence tag
 * and one exact shape, and a copy of the sentence in the panel is a copy that goes stale
 * quietly — the screen keeps working and never once files a set.
 */
export type ChatProposalInstruction = {
  instruction: string;
  fence_tag: string;
  max_operations: number;
};

export function fetchChatProposalInstruction(): Promise<ChatProposalInstruction> {
  return request<ChatProposalInstruction>("/api/v1/ai/chat/proposal-instruction");
}

export async function streamChat(
  input: { model?: string; messages: ChatMessageInput[] },
  handlers: {
    onStart?: (info: { provider: string; model: string; protocol: string }) => void;
    onDelta?: (content: string) => void;
    onDone?: (done: ChatDone) => void;
    onProposal?: (proposal: ChatProposal) => void;
    onProposalError?: (message: string) => void;
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
      } else if (event === "proposal") {
        handlers.onProposal?.(payload as unknown as ChatProposal);
      } else if (event === "proposal_error") {
        // **Not** the `error` branch. That one throws, which the screen renders as a failed
        // answer — but the answer is complete and on screen; only the follow-up failed.
        // Throwing here would replace a good reply with an error and hide the proposal the
        // person was reading about.
        handlers.onProposalError?.(String(payload.message ?? "The proposal could not be filed."));
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
// Task routing and feature overrides (REQ-098, slice 2)
// ---------------------------------------------------------------------------------------------

/**
 * Which map a routing read or write addresses.
 *
 * The three cases are the three scopes the resolver walks, and they are spelled the way the
 * endpoints spell them so the panel and the API cannot disagree about which map is on screen.
 */
export type AiRoutingScope =
  | { kind: "installation" }
  | { kind: "organization"; organizationId: string }
  | { kind: "site"; siteId: string };

/** One candidate in a task's ordered list. */
export type AiRouteCandidate = {
  /** 1-based; the primary is 1 and the order is the fallback order. */
  position: number;
  /** The model, or null when the row survives a model that was later removed. */
  model_id: string | null;
  /** The model key as the panel shows it. */
  model_label: string | null;
  /** True when the candidate cannot answer as it stands — the "needs attention" badge. */
  needs_attention: boolean;
  /** Why it cannot answer, when it cannot. */
  refusal: string | null;
  /** Capabilities this task's requests require here. */
  requirements: string[];
  /** Whether the model is switched off. */
  enabled: boolean;
};

/** One task row of the routing screen. */
export type AiTaskRoute = {
  task: string;
  description: string;
  candidates: AiRouteCandidate[];
  /** True when these candidates were inherited from a wider scope. */
  inherited: boolean;
  /** True when nothing is configured for this task anywhere in the chain. */
  empty: boolean;
};

/** One feature pin. */
export type AiFeatureOverride = {
  feature: string;
  model_id: string;
  model_label: string;
  scope: Record<string, string>;
  updated_at: string;
};

/** A known feature key with its description, driving the override form. */
export type AiFeatureInfo = { key: string; description: string };

/** The whole routing screen in one response. */
export type AiRouting = {
  scope: Record<string, string>;
  tasks: AiTaskRoute[];
  overrides: AiFeatureOverride[];
  chain: Record<string, string>[];
  requirements: string[];
  features: AiFeatureInfo[];
  /** The resolution order, served so the legend cannot drift from the resolver. */
  rules: string[];
  /** Tasks that cannot resolve at this scope. */
  unresolved: string[];
};

/** One line of a resolution walk. */
export type AiWalkEntry = {
  position: number | null;
  model_id: string | null;
  outcome: "chosen" | "skipped";
  reason: string;
  scope: Record<string, string>;
  source: string;
};

/** The dry run's answer. */
export type AiRoutingPreview = {
  model: { model_id: string; position: number | null; source: string; scope: Record<string, string> } | null;
  walk: AiWalkEntry[];
  rule: string;
  unresolved: boolean;
  scope: Record<string, string>;
  rules: string[];
};

/** The query parameters that select a scope, appended to a routing path. */
function routingScopeParams(scope: AiRoutingScope): URLSearchParams {
  const params = new URLSearchParams();
  if (scope.kind === "organization") params.set("organization_id", scope.organizationId);
  if (scope.kind === "site") params.set("site_id", scope.siteId);
  return params;
}

/** The task map of one scope, with inherited rows marked. */
export async function fetchAiRouting(scope: AiRoutingScope): Promise<AiRouting> {
  const params = routingScopeParams(scope);
  const suffix = params.toString();
  return request<AiRouting>(
    suffix ? `/api/v1/ai/routing?${suffix}` : "/api/v1/ai/routing",
  );
}

// -------------------------------------------------------------------------------------------
// AI approvals (REQ-101 slice 1 — the review inbox, the review screen and the class policy)
// -------------------------------------------------------------------------------------------

/**
 * One approval row as the inbox and the review screen render it.
 *
 * `decidable`, `requires_confirmation` and `confirmation_phrase` are the server's answers, not
 * something the panel re-derives. A client that computed "may this be approved" from `status`
 * and `expires_at` would disagree with the server the moment a clock rounded differently — and a
 * reviewer who is told a row is decidable and then gets `expired` has been told a lie by the
 * screen rather than by the race.
 */
export type AiApproval = {
  id: string;
  organization_id: string;
  site_id: string | null;
  run_id: string | null;
  step_id: string | null;
  agent_id: string | null;
  identity_id: string | null;
  change_set_id: string | null;
  tool_key: string;
  tool_class: string;
  resource_type: string | null;
  resource_id: string | null;
  resource_label: string | null;
  risk: string;
  title: string;
  summary: string;
  operation_count: number;
  irreversible: boolean;
  preview: unknown;
  preview_hash: string;
  base_revision: string | null;
  status: string;
  requested_by: string | null;
  model_id: string | null;
  expires_at: string;
  decided_by: string | null;
  decided_at: string | null;
  decision_note: string | null;
  applied_at: string | null;
  error: string | null;
  created_at: string;
  /** Whether the row is still pending AND unexpired. */
  decidable: boolean;
  /** Whether the decision demands a typed phrase, and what it is. */
  requires_confirmation: boolean;
  confirmation_phrase: string | null;
};

/** The inbox filters, exactly the query the API accepts. */
export type AiApprovalQuery = {
  status?: string;
  tool?: string;
  toolClass?: string;
  q?: string;
  limit?: number;
};

/**
 * The inbox body.
 *
 * `viewer_missing` is the whole reason the row actions can be disabled *and named* rather than
 * enabled-then-refused: the API enforces the same keys with a 403 naming them, so the disabled
 * state is a promise it keeps.
 */
export type AiApprovalInbox = {
  approvals: AiApproval[];
  counts: Record<string, number>;
  viewer_permissions: string[];
  viewer_missing: string[];
};

/** One audit row of a request's trail. */
export type AiApprovalAuditRow = {
  id: number;
  actor_type: string;
  actor_user_id: string | null;
  action: string;
  target_type: string | null;
  target_id: string | null;
  metadata: Record<string, unknown>;
  created_at: string;
};

/** What a decision answers, whatever it decided. */
export type AiDecisionResult = {
  changed: boolean;
  code: string | null;
  approval: AiApproval;
  current_revision?: string;
};

/** The effective policy of one dangerous class, as the policy screen renders it. */
export type AiApprovalPolicy = {
  tool_class: string;
  label: string;
  /** `organization` when an organization row overrides the platform default. */
  source: string;
  mode: string;
  typed_confirmation: boolean;
  expires_minutes: number;
  /** True when the class is `allow` — the row stripes. */
  permissive: boolean;
  irreversible: boolean;
  updated_at: string;
  updated_by: string | null;
};

/** The six dangerous classes, served so the form never hard-codes their names a second time. */
export type AiApprovalClass = {
  key: string;
  label: string;
  irreversible: boolean;
};

/** The review inbox. */
export async function fetchAiApprovals(
  query: AiApprovalQuery = {},
): Promise<AiApprovalInbox> {
  const params = new URLSearchParams();
  if (query.status && query.status !== "all") params.set("status", query.status);
  if (query.tool?.trim()) params.set("tool", query.tool.trim());
  if (query.toolClass && query.toolClass !== "all") params.set("tool_class", query.toolClass);
  if (query.q?.trim()) params.set("q", query.q.trim());
  if (query.limit) params.set("limit", String(query.limit));

  const suffix = params.toString();
  return request<AiApprovalInbox>(
    suffix ? `/api/v1/ai/approvals?${suffix}` : "/api/v1/ai/approvals",
  );
}

/**
 * One request with its audit trail.
 *
 * The trail arrives in the same response as the row on purpose: a reviewer reading a decided
 * request asks "who did this and why" as part of the same screen, and a second request could be
 * answered from a different moment.
 */
export async function fetchAiApproval(id: string): Promise<{
  approval: AiApproval;
  audit: AiApprovalAuditRow[];
}> {
  return request(`/api/v1/ai/approvals/${encodeURIComponent(id)}`);
}

/**
 * Approve one request.
 *
 * `confirmation` is sent ONLY when it is non-empty. Sending `"yes"` — or sending the field's
 * placeholder — is exactly how a checkbox becomes a confirmation, and the API answers
 * `confirmation_mismatch` to a wrong phrase rather than accepting any non-empty string, so an
 * unconditional field would turn every irreversible class into a dead button.
 */
export function approveAiApproval(
  id: string,
  body: { confirmation?: string; currentRevision?: string } = {},
): Promise<AiDecisionResult> {
  return request(`/api/v1/ai/approvals/${encodeURIComponent(id)}/approve`, {
    method: "POST",
    body: JSON.stringify({
      ...(body.confirmation?.trim() ? { confirmation: body.confirmation.trim() } : {}),
      ...(body.currentRevision ? { current_revision: body.currentRevision } : {}),
    }),
  });
}

/**
 * Recompute a request's diff against the target as it is **now**.
 *
 * No body on purpose. The server rebuilds the operation from the frozen preview and reads the
 * row itself — a re-preview that accepted a client-supplied diff would be a diff nobody
 * approved. The answer is the row, so the screen re-renders from the same object the server
 * wrote rather than from a second read that could be a different instant.
 *
 * `refreshed: false` with `code: "unchanged"` is the refusal the request asks for: the
 * recomputed plan is identical, so nothing was written.
 */
export function rePreviewAiApproval(id: string): Promise<{
  refreshed: boolean;
  code: string | null;
  approval: AiApproval;
}> {
  return request(`/api/v1/ai/approvals/${encodeURIComponent(id)}/preview`, {
    method: "POST",
    body: JSON.stringify({}),
  });
}

/** Reject one request. The reason is mandatory server-side; a blank one is refused there. */
export function rejectAiApproval(
  id: string,
  reason: string,
): Promise<AiDecisionResult> {
  return request(`/api/v1/ai/approvals/${encodeURIComponent(id)}/reject`, {
    method: "POST",
    body: JSON.stringify({ reason }),
  });
}

/** The class policy table, with the six classes' labels. */
export async function fetchAiApprovalPolicies(): Promise<{
  policies: AiApprovalPolicy[];
  classes: AiApprovalClass[];
}> {
  return request("/api/v1/ai/approvals/policies");
}

/**
 * Set one class's organization policy.
 *
 * `confirmation` carries the typed phrase the API demands for `mode: "allow"` — the exact text
 * `set <class> to allow`. It is sent only when the caller typed it, for the same reason the
 * approve path withholds an empty phrase.
 */
export function putAiApprovalPolicy(
  toolClass: string,
  body: {
    mode: string;
    typedConfirmation?: boolean;
    expiresMinutes?: number;
    confirmation?: string;
  },
): Promise<{ policies: AiApprovalPolicy[] }> {
  return request(`/api/v1/ai/approvals/policies/${encodeURIComponent(toolClass)}`, {
    method: "PUT",
    body: JSON.stringify({
      mode: body.mode,
      ...(body.typedConfirmation === undefined
        ? {}
        : { typed_confirmation: body.typedConfirmation }),
      ...(body.expiresMinutes === undefined ? {} : { expires_minutes: body.expiresMinutes }),
      ...(body.confirmation?.trim() ? { confirmation: body.confirmation.trim() } : {}),
    }),
  });
}

/**
 * Drop one class's override, back to the platform default.
 *
 * A reset needs no phrase: removing an override can only tighten towards the fail-closed
 * default, so it carries no risk worth a typed confirmation — and the API enforces that too.
 */
export function resetAiApprovalPolicy(toolClass: string): Promise<{ policies: AiApprovalPolicy[] }> {
  return request(`/api/v1/ai/approvals/policies/${encodeURIComponent(toolClass)}`, {
    method: "DELETE",
  });
}

/** Expire what is due, now. */
export function sweepAiApprovals(): Promise<{ expired: number }> {
  return request("/api/v1/ai/approvals/sweep", { method: "POST", body: JSON.stringify({}) });
}

// -------------------------------------------------------------------------------------------
// The change-set editor (REQ-101 slice 3)
// -------------------------------------------------------------------------------------------

/** One operation of a proposed set, as the set stores it. */
export type AiChangeOp = {
  key: string;
  kind: "create" | "update" | "delete";
  resource_type: string;
  resource_id: string;
  args: Record<string, unknown>;
};

/** A change set row. */
export type AiChangeSet = {
  id: string;
  organization_id: string;
  site_id: string | null;
  title: string;
  status: "draft" | "pending" | "confirmed" | "applied" | "discarded" | "failed" | string;
  operations: AiChangeOp[];
  base_revisions: Record<string, string>;
  /** sha256 over the operations and the revisions they were pinned to. */
  content_hash: string;
  created_by: string | null;
  created_by_agent: string | null;
  created_by_run: string | null;
  updated_by: string | null;
  confirmed_at: string | null;
  applied_at: string | null;
  discarded_reason: string | null;
  created_at: string;
  updated_at: string;
  /** Whether the operation list may still be edited. Server-owned: it is the same list the
   * PATCH's `where` clause runs as, so the panel cannot drift from what the API accepts. */
  editable: boolean;
  /** Whether confirming would park at least one operation for a second person. */
  needs_approval: boolean;
  /** Whether confirming demands a typed phrase — the set deletes content. */
  irreversible: boolean;
  /** The phrase itself: the set's title, which the server compares against. */
  confirmation_phrase: string | null;
};

/** One field row of a resolved operation. */
export type AiPlannedField = {
  arg: string;
  field: string;
  before: unknown;
  after: unknown;
};

/**
 * One operation resolved against its target **as it is now**.
 *
 * This is what the editor renders instead of a client-side re-plan: `before` is read from the
 * database, not from the operation's own arguments, and `cascades` is a count the server
 * took. A browser cannot compute either.
 */
export type AiPlannedOp = {
  key: string;
  kind: string;
  resource_type: string;
  resource_id: string;
  label: string;
  diffs: AiPlannedField[];
  cascades: string[];
  base_revision: string;
  gated_class: string | null;
  no_op: boolean;
};

/** What a re-plan answers. */
export type AiRePreviewed = AiChangeSet & {
  planned: AiPlannedOp[];
  drifted: string[];
  needs_approval: boolean;
};

/** What a set list answers, with the decision keys the viewer holds. */
export type AiChangeSetList = {
  sets: AiChangeSet[];
  viewer_permissions: string[];
  /** The decision keys the viewer does NOT hold, named so a disabled control can explain
   * itself. The same split the approval inbox makes, from the same helper. */
  viewer_missing: string[];
};

/** What a confirm answers. */
export type AiSetConfirmed = AiChangeSet & {
  needs_approval: boolean;
  approvals: { id: string; operation_key: string; class: string; status: string }[];
  applied: boolean;
};

/**
 * Resolve a set's operations against the targets as they are now.
 *
 * No body: the server rebuilds each operation from the stored one and reads the row itself, so
 * a re-plan that accepted a client-supplied diff would be a diff nobody confirmed. The answer
 * is the resolved operations, not a mutated set — a preview that re-pinned the revisions it
 * observed would retire the staleness the confirm route enforces.
 */
export function replanAiChangeSet(id: string): Promise<AiRePreviewed> {
  return request(`/api/v1/ai/change-sets/${encodeURIComponent(id)}/preview`, {
    method: "POST",
    body: JSON.stringify({}),
  });
}

/**
 * Replace a set's operation list.
 *
 * The whole list is sent, not a patch: a set is a list the reviewer owns, and "move the third
 * one to the top" has no vocabulary in a partial patch. `baseContentHash` is the hash the
 * editor was looking at — a mismatch answers `409 content_moved` naming both, and nothing is
 * written.
 */
export function updateAiChangeSet(
  id: string,
  body: {
    title: string;
    operations: AiChangeOp[];
    baseRevisions?: Record<string, string>;
    baseContentHash?: string;
  },
): Promise<AiChangeSet> {
  return request(`/api/v1/ai/change-sets/${encodeURIComponent(id)}`, {
    method: "PATCH",
    body: JSON.stringify({
      title: body.title,
      operations: body.operations,
      base_revisions: body.baseRevisions ?? {},
      // Withheld when absent rather than sent as null: a client that has not implemented the
      // optimistic check keeps saving, and one that has gets it.
      ...(body.baseContentHash ? { base_content_hash: body.baseContentHash } : {}),
    }),
  });
}

/**
 * Confirm a set.
 *
 * `confirmationPhrase` is the set's own title and is sent **only** when the reviewer typed it —
 * for the same reason `approveAiApproval` withholds an empty phrase. A set that deletes content
 * is refused by the server without it, whatever the client rendered.
 */
export function confirmAiChangeSet(
  id: string,
  confirmationPhrase?: string,
): Promise<AiSetConfirmed> {
  return request(`/api/v1/ai/change-sets/${encodeURIComponent(id)}/confirm`, {
    method: "POST",
    body: JSON.stringify({
      ...(confirmationPhrase?.trim() ? { confirmation_phrase: confirmationPhrase.trim() } : {}),
    }),
  });
}

/** Drop a set. The reason is mandatory server-side; a blank one is refused there. */
export function discardAiChangeSet(id: string, reason: string): Promise<AiChangeSet> {
  return request(`/api/v1/ai/change-sets/${encodeURIComponent(id)}/discard`, {
    method: "POST",
    body: JSON.stringify({ reason }),
  });
}

/** The sets a person may see, newest first. */
export async function fetchAiChangeSets(
  query: { status?: string; q?: string; limit?: number } = {},
): Promise<AiChangeSetList> {
  const params = new URLSearchParams();
  if (query.status && query.status !== "all") params.set("status", query.status);
  if (query.q?.trim()) params.set("q", query.q.trim());
  if (query.limit) params.set("limit", String(query.limit));
  const suffix = params.toString();
  return request(
    suffix ? `/api/v1/ai/change-sets?${suffix}` : "/api/v1/ai/change-sets",
  );
}

/** Replace one task's candidate list at a scope. */
export async function putAiTaskMap(
  scope: AiRoutingScope,
  task: string,
  candidates: { modelId: string; requirements: string[] }[],
): Promise<AiRouting> {
  return request<AiRouting>("/api/v1/ai/routing", {
    method: "PUT",
    body: JSON.stringify({
      task,
      ...scopeParamsForBody(scope),
      candidates: candidates.map((candidate) => ({
        model_id: candidate.modelId,
        requirements: candidate.requirements,
      })),
    }),
  });
}

/** Pin (or unpin, with `null`) one feature at a scope. */
export async function putAiFeatureOverride(
  scope: AiRoutingScope,
  feature: string,
  modelId: string | null,
): Promise<AiFeatureOverride[]> {
  return request<AiFeatureOverride[]>("/api/v1/ai/routing/overrides", {
    method: "PUT",
    body: JSON.stringify({ feature, ...scopeParamsForBody(scope), model_id: modelId }),
  });
}

/** Resolve a hypothetical request without calling a provider. */
export async function previewAiRouting(input: {
  scope: AiRoutingScope;
  task?: string;
  feature?: string;
  requested?: string;
  requires?: string[];
}): Promise<AiRoutingPreview> {
  return request<AiRoutingPreview>("/api/v1/ai/routing/preview", {
    method: "POST",
    body: JSON.stringify({
      ...scopeParamsForBody(input.scope),
      ...(input.task ? { task: input.task } : {}),
      ...(input.feature ? { feature: input.feature } : {}),
      ...(input.requested?.trim() ? { requested: input.requested.trim() } : {}),
      requires: input.requires ?? [],
    }),
  });
}

// -------------------------------------------------------------------------------------------
// The route decision log (REQ-098, slice 3).
// -------------------------------------------------------------------------------------------

/** One row of the decision log. */
export type AiDecisionRow = {
  id: number;
  created_at: string;
  task: string | null;
  feature: string | null;
  requested: string | null;
  resolved_label: string | null;
  resolved_model_id: string | null;
  /** 0-based: 0 is the primary, anything above it is a fallback. */
  fallback_index: number;
  used_fallback: boolean;
  unresolved: boolean;
  rule: string;
  requirements: string[];
  reason: string;
  run_id: string | null;
};

/** One page of the log. */
export type AiDecisionPage = {
  rows: AiDecisionRow[];
  total: number;
  offset: number;
  limit: number;
};

/** One decision with its full candidate walk. */
export type AiDecisionDetail = AiDecisionRow & {
  walk: AiWalkEntry[];
  scope: string;
  rules: string[];
};

/** A task that could not resolve, with the reason. */
export type AiUnresolvedTask = {
  task: string;
  reason: string;
  occurrences: number;
  last_failed_at: string | null;
};

/** The filters the log screen offers. */
export type AiDecisionFilter = {
  task?: string;
  feature?: string;
  modelId?: string;
  fallback?: boolean;
  unresolved?: boolean;
  from?: string;
  to?: string;
  limit?: number;
  offset?: number;
};

/** The filters as a query string, shared by the table and the CSV export. */
function decisionFilterParams(filter: AiDecisionFilter): string {
  const params = new URLSearchParams();
  if (filter.task) params.set("task", filter.task);
  if (filter.feature) params.set("feature", filter.feature);
  if (filter.modelId) params.set("model_id", filter.modelId);
  if (filter.fallback) params.set("fallback", "true");
  if (filter.unresolved) params.set("unresolved", "true");
  if (filter.from) params.set("from", filter.from);
  if (filter.to) params.set("to", filter.to);
  if (filter.limit) params.set("limit", String(filter.limit));
  if (filter.offset) params.set("offset", String(filter.offset));
  return params.toString();
}

/** One page of the decision log. */
export async function fetchAiDecisions(
  filter: AiDecisionFilter = {},
): Promise<AiDecisionPage> {
  const suffix = decisionFilterParams(filter);
  return request<AiDecisionPage>(
    suffix ? `/api/v1/ai/logs/decisions?${suffix}` : "/api/v1/ai/logs/decisions",
  );
}

/**
 * The decision log as CSV, for the same filters the table is showing.
 *
 * Returns the text rather than triggering a download itself: the screen owns the button, so the
 * export is a control the walkthrough can click and assert on, and a `window.location`
 * navigation cannot be observed at all. The same filters go into the path as the table's — the
 * two reads are only the same read if the query strings are, which is why one helper builds it.
 */
export async function fetchAiDecisionsCsv(filter: AiDecisionFilter = {}): Promise<string> {
  const suffix = decisionFilterParams(filter);
  const path = suffix
    ? `/api/v1/ai/logs/decisions.csv?${suffix}`
    : "/api/v1/ai/logs/decisions.csv";

  let response: Response;
  try {
    response = await fetch(path, { credentials: "same-origin", headers: { accept: "text/csv" } });
  } catch {
    throw new ApiError(0, "network_error", "The Omnion API could not be reached.");
  }
  if (!response.ok) {
    // The refusal is usually a guard's plain-text 401/403 rather than the API's JSON error
    // shape, so the body is reported as it arrived instead of being parsed into nothing.
    const text = await response.text();
    let code = "export_failed";
    let message = `The export answered with status ${response.status}.`;
    try {
      const body = JSON.parse(text) as ErrorBody;
      code = body.error?.code ?? code;
      message = body.error?.message ?? message;
    } catch {
      // A non-JSON body is still an error; the status stays in the message.
    }
    throw new ApiError(response.status, code, message);
  }

  return response.text();
}

/** One decision with its walk. */
export async function fetchAiDecision(id: number): Promise<AiDecisionDetail> {
  return request<AiDecisionDetail>(`/api/v1/ai/logs/decisions/${id}`);
}

/** The tasks that cannot resolve, with the reason. */
export async function fetchAiUnresolved(): Promise<{
  scope: Record<string, string>;
  unresolved: AiUnresolvedTask[];
  ok: boolean;
}> {
  return request("/api/v1/ai/routing/unresolved");
}

/** The newest decision per task, for the routing screen's "Last resolved" column. */
export async function fetchAiLastResolved(): Promise<Record<string, AiDecisionRow>> {
  return request<Record<string, AiDecisionRow>>("/api/v1/ai/routing/last-resolved");
}

/**
 * The scope fields a JSON body carries.
 *
 * The same two keys as the query string, because the endpoints flatten one shape into both. A
 * site carries only its id: the endpoint reads the site's organization from the database rather
 * than trusting the payload, so a client cannot name a site and a different organization.
 */
function scopeParamsForBody(scope: AiRoutingScope): Record<string, string> {
  if (scope.kind === "organization") return { organization_id: scope.organizationId };
  if (scope.kind === "site") return { site_id: scope.siteId };
  return {};
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

// -------------------------------------------------------------------------------------------
// The agent runtime (REQ-099, slice 1).
//
// The screen is a queue's front end, so the client mirrors the store's own vocabulary rather
// than inventing labels: a status a human reads is a status the loop wrote. Every call carries
// the organization through `agentScopeParams`, because a platform-level account has to name one
// and an organization account passing a different one is refused with `403 cross_organization`.
// -------------------------------------------------------------------------------------------

/** One agent as the table renders it. */
export type AiAgent = {
  id: string;
  organization_id: string;
  site_id: string | null;
  key: string;
  name: string;
  description: string;
  system_prompt: string;
  model_id: string | null;
  temperature: number;
  max_steps: number;
  deadline_seconds: number;
  token_budget: number;
  tools: string[];
  /** How many of `tools` park the run for a person; the table shows it beside the count. */
  approvals_count: number;
  approvals: string[];
  memory_scope: string;
  enabled: boolean;
  created_at: string;
  updated_at: string;
  /**
   * The 30-day roll-up, present on the list and absent on the detail.
   *
   * Optional on purpose: the list route fills it from one bulk query, the detail route does not
   * compute it, and a type that demanded it would make the second one lie. The cell renders an
   * em dash for `null` rather than a zero, because "no runs yet" and "zero percent" are
   * different facts.
   */
  telemetry?: AiAgentTelemetry | null;
};

/**
 * One agent's 30-day roll-up.
 *
 * The denominator is **finished** runs, and `cancelled` is published beside it rather than
 * folded in: a person pressing stop stopped the run, and counting that against the agent's
 * reliability is the definition of a metric that gets gamed by impatience. A reader who wants
 * the other denominator subtracts `cancelled` from `runs`.
 */
export type AiAgentTelemetry = {
  /** Finished runs in the window. */
  runs: number;
  /** Of those, the ones that produced an answer. */
  completed: number;
  /** Of those, the ones a person stopped. */
  cancelled: number;
  /** Of those, the ones that failed. */
  failed: number;
  /** Total tokens in the window. */
  total_tokens: number;
  /** Cost in millionths in the window. */
  cost_micros: number;
  /** Steps in the window. */
  steps: number;
  /**
   * Completed as a percentage of finished runs, or `null` when nothing has finished.
   *
   * `null` is the honest answer for a brand-new agent: `0%` is a claim about a division that
   * never happened, and a cell reading "0%" on an agent nobody has run yet is a defect.
   */
  success_rate: number | null;
  /** When the window starts, so the client does not have to guess its own. */
  since: string;
};

/** How often each tool was called in the window, tenant-wide. */
export type AiToolUsage = {
  since: string;
  /** Call counts by tool key. */
  tools: Record<string, number>;
};

/** One run as the history list renders it. */
export type AiRun = {
  id: string;
  agent_id: string | null;
  user_id: string | null;
  /** `chat`, `agent`, `workflow` or `schedule` — who started it, not just that it started. */
  trigger: string;
  goal: string;
  status: string;
  stop_reason: string | null;
  model_id: string | null;
  current_step: number;
  resume_count: number;
  prompt_tokens: number;
  completion_tokens: number;
  cost_micros: number;
  started_at: string | null;
  finished_at: string | null;
  error: string | null;
};

/** One step of a run's trace. `arguments` is the **redacted** form the store wrote. */
export type AiRunStep = {
  step_no: number;
  kind: string;
  tool: string | null;
  arguments: Record<string, unknown> | null;
  result: unknown;
  status: string;
  prompt_tokens: number;
  completion_tokens: number;
  cost_micros: number;
  duration_ms: number | null;
  error: string | null;
};

/**
 * One workspace reference a run was told to read.
 *
 * `resolved: false` is the state that explains a failed run: the sheet named `q3.csv`, and the
 * file was deleted before the runner claimed the run. The reference survives the delete on
 * purpose, so the trace can name the path rather than showing a run that had no inputs.
 */
export type AiRunInput = {
  id: string;
  path: string;
  resolved: boolean;
  size_bytes: number;
};

/** One run with its trace and its named inputs — the run detail screen's whole payload. */
/**
 * One run's recomputed telemetry, as the run detail's header renders it.
 *
 * A separate block rather than more columns on `AiRun` because the run's own columns are the
 * *stored* totals and this is the *recomputed* one. Shipping both side by side turns "telemetry
 * equals the underlying rows" from a test into something an operator can check by eye: a drift
 * shows up as two different numbers in the same header instead of as one wrong number.
 */
export type AiRunTelemetry = {
  completed_steps: number;
  failed_steps: number;
  tool_calls: number;
  prompt_tokens: number;
  completion_tokens: number;
  cost_micros: number;
  duration_ms: number;
  total_tokens: number;
};

export type AiRunDetail = AiRun & {
  steps: AiRunStep[];
  inputs: AiRunInput[];
  telemetry: AiRunTelemetry;
};

/**
 * One agent's 30-day roll-up, on its own.
 *
 * The list already carries this number, so the call exists for the detail screen and for
 * anything that wants the roll-up without the whole agent list.
 */
export function fetchAiAgentTelemetry(
  agentId: string,
  organizationId?: string | null,
): Promise<AiAgentTelemetry> {
  return request<AiAgentTelemetry>(
    `/api/v1/ai/agents/${encodeURIComponent(agentId)}/telemetry${agentScopeParams(organizationId)}`,
  );
}

/** How often each tool was called in the window, tenant-wide. */
export function fetchAiToolUsage(organizationId?: string | null): Promise<AiToolUsage> {
  return request<AiToolUsage>(`/api/v1/ai/telemetry/tools${agentScopeParams(organizationId)}`);
}

/** The organization selector the agent and run routes accept. */
function agentScopeParams(organizationId?: string | null): string {
  if (!organizationId) return "";
  return `?organization_id=${encodeURIComponent(organizationId)}`;
}

/** The agents table. */
export function fetchAiAgents(organizationId?: string | null): Promise<AiAgent[]> {
  return request<AiAgent[]>(`/api/v1/ai/agents${agentScopeParams(organizationId)}`);
}

/** The run history, narrowed by whichever filters the list sets. */
export function fetchAiRuns(input: {
  organizationId?: string | null;
  agentId?: string;
  status?: string;
  stopReason?: string;
  limit?: number;
}): Promise<AiRun[]> {
  const params = new URLSearchParams();
  if (input.organizationId) params.set("organization_id", input.organizationId);
  if (input.agentId) params.set("agent_id", input.agentId);
  if (input.status) params.set("status", input.status);
  if (input.stopReason) params.set("stop_reason", input.stopReason);
  if (input.limit) params.set("limit", String(input.limit));
  const query = params.toString();
  return request<AiRun[]>(`/api/v1/ai/runs${query ? `?${query}` : ""}`);
}

/** One run with its trace. */
export function fetchAiRun(id: string, organizationId?: string | null): Promise<AiRunDetail> {
  return request<AiRunDetail>(
    `/api/v1/ai/runs/${encodeURIComponent(id)}${agentScopeParams(organizationId)}`,
  );
}

/** One agent in full — the config form's load. */
export function fetchAiAgent(id: string, organizationId?: string | null): Promise<AiAgent> {
  return request<AiAgent>(
    `/api/v1/ai/agents/${encodeURIComponent(id)}${agentScopeParams(organizationId)}`,
  );
}

/**
 * Create an agent.
 *
 * `tools` and `approvals` are the agent's own ordered lists rather than a catalogue pick, so a
 * form that drops an approval-gated tool is refused by the API with `agent.approval_not_allowed`
 * instead of silently losing the gate.
 */
export function createAiAgent(input: {
  organizationId?: string | null;
  key: string;
  name: string;
  description?: string;
  system_prompt?: string;
  model_id?: string | null;
  temperature?: number;
  max_steps?: number;
  deadline_seconds?: number;
  token_budget?: number;
  tools?: string[];
  approvals?: string[];
  memory_scope?: string;
  enabled?: boolean;
}): Promise<AiAgent> {
  return request<AiAgent>(`/api/v1/ai/agents${agentScopeParams(input.organizationId)}`, {
    method: "POST",
    body: JSON.stringify({
      key: input.key,
      name: input.name,
      description: input.description ?? "",
      system_prompt: input.system_prompt ?? "",
      model_id: input.model_id ?? null,
      temperature: input.temperature,
      max_steps: input.max_steps,
      deadline_seconds: input.deadline_seconds,
      token_budget: input.token_budget,
      tools: input.tools ?? [],
      approvals: input.approvals ?? [],
      memory_scope: input.memory_scope,
      enabled: input.enabled,
    }),
  });
}

/**
 * Change an agent.
 *
 * `model_id` is a double option on purpose: `null` un-pins the agent (let the router choose),
 * while omitting the key leaves the current pin alone. A single nullable field cannot express
 * the difference, and the difference is the difference between a pinned agent and a routed one.
 */
export function updateAiAgent(
  id: string,
  input: {
    organizationId?: string | null;
    name?: string;
    description?: string;
    system_prompt?: string;
    model_id?: string | null;
    temperature?: number;
    max_steps?: number;
    deadline_seconds?: number;
    token_budget?: number;
    tools?: string[];
    approvals?: string[];
    memory_scope?: string;
    enabled?: boolean;
  },
): Promise<AiAgent> {
  return request<AiAgent>(
    `/api/v1/ai/agents/${encodeURIComponent(id)}${agentScopeParams(input.organizationId)}`,
    { method: "PATCH", body: JSON.stringify(input) },
  );
}

/** Remove an agent. The API answers 409 while a run is still active for it. */
export function deleteAiAgent(id: string, organizationId?: string | null): Promise<null> {
  return request<null>(
    `/api/v1/ai/agents/${encodeURIComponent(id)}${agentScopeParams(organizationId)}`,
    { method: "DELETE" },
  );
}

/** Ask a run to stop at its next step boundary. */
export function cancelAiRun(id: string, organizationId?: string | null): Promise<AiRun> {
  return request<AiRun>(
    `/api/v1/ai/runs/${encodeURIComponent(id)}/cancel${agentScopeParams(organizationId)}`,
    { method: "POST" },
  );
}

/** Requeue an interrupted or approval-parked run. */
export function resumeAiRun(id: string, organizationId?: string | null): Promise<AiRun> {
  return request<AiRun>(
    `/api/v1/ai/runs/${encodeURIComponent(id)}/resume${agentScopeParams(organizationId)}`,
    { method: "POST" },
  );
}

/** The run's agent, for the history table's link column. */
export function fetchAiRunAgent(
  id: string,
  organizationId?: string | null,
): Promise<AiAgent | null> {
  return request<AiAgent | null>(
    `/api/v1/ai/runs/${encodeURIComponent(id)}/agent${agentScopeParams(organizationId)}`,
  );
}

/** One file in an agent's workspace (REQ-099 slice 2). */
export type AiAgentFile = {
  /** Row identity — what Download and Delete address. */
  id: string;
  /** The path inside the workspace, relative to the agent. */
  path: string;
  /** Size in bytes. */
  size_bytes: number;
  /** The declared content type. */
  content_type: string;
  /** Hex SHA-256 of the bytes, so two versions of a path are tellable apart. */
  checksum: string;
  /** The run that wrote it, when a run wrote it. */
  run_id: string | null;
  /** When it was added. */
  created_at: string;
  /** When a run last named it. */
  last_used_at: string | null;
};

/**
 * How full a workspace is, as the usage bar reads it.
 *
 * `percent` is already clamped by the API rather than computed here: the bar's width is a
 * percentage of a number the server owns, and a client that recomputes it can disagree with the
 * quota that produced the refusal.
 */
export type AiAgentFileUsage = {
  /** Bytes stored across the agent's files. */
  used_bytes: number;
  /** The per-agent ceiling (100 MB). */
  limit_bytes: number;
  /** How many files count against it. */
  file_count: number;
  /** Whole percent, 0–100. */
  percent: number;
  /** The per-file ceiling (10 MB), so the upload hint names the same number. */
  max_file_bytes: number;
};

/** The workspace listing and its quota, in one answer. */
export type AiAgentWorkspace = {
  /** The agent whose workspace this is. */
  agent_id: string;
  /** Its files, newest first. */
  files: AiAgentFile[];
  /** The quota, so the bar cannot disagree with the table above it. */
  usage: AiAgentFileUsage;
};

/** The agent's workspace. */
export function fetchAiAgentWorkspace(
  agentId: string,
  organizationId?: string | null,
): Promise<AiAgentWorkspace> {
  return request<AiAgentWorkspace>(
    `/api/v1/ai/agents/${encodeURIComponent(agentId)}/files${agentScopeParams(organizationId)}`,
  );
}

/**
 * Add or replace one workspace file.
 *
 * The path is sent as its own form part rather than being taken from the picked file's name: a
 * filename is the browser's opinion about the user's disk, and the API deliberately refuses a
 * request that omits the path so a name it did not choose cannot reach a namespace it does not
 * validate.
 */
export async function uploadAiAgentFile(input: {
  agentId: string;
  path: string;
  file: File;
  organizationId?: string | null;
}): Promise<AiAgentFile> {
  const form = new FormData();
  form.append("path", input.path);
  form.append("file", input.file);
  return request<AiAgentFile>(
    `/api/v1/ai/agents/${encodeURIComponent(input.agentId)}/files${agentScopeParams(input.organizationId)}`,
    { method: "POST", body: form },
  );
}

/** Remove one workspace file. */
export function deleteAiAgentFile(input: {
  agentId: string;
  path: string;
  organizationId?: string | null;
}): Promise<null> {
  return request<null>(
    `/api/v1/ai/agents/${encodeURIComponent(input.agentId)}/files/${encodeWorkspacePath(input.path)}${agentScopeParams(input.organizationId)}`,
    { method: "DELETE" },
  );
}

/**
 * The download address for one workspace file.
 *
 * Built rather than fetched, because the route answers the bytes directly and a download that
 * had to round-trip a `blob` first would hold a copy of a 10 MB file in the tab's memory. The
 * path is encoded **once**, segment by segment: encoding the whole string turns the separators
 * into `%2F`, which is a file named `data%2Fq3.csv` rather than the file at `data/q3.csv`.
 */
export function aiAgentFileHref(input: {
  agentId: string;
  path: string;
  organizationId?: string | null;
}): string {
  return `/api/v1/ai/agents/${encodeURIComponent(input.agentId)}/files/${encodeWorkspacePath(input.path)}${agentScopeParams(input.organizationId)}`;
}

/** Percent-encode each segment of a workspace path and keep the separators. */
function encodeWorkspacePath(path: string): string {
  return path
    .split("/")
    .map((segment) => encodeURIComponent(segment))
    .join("/");
}

/** One frame of a run's event stream, as the Run sheet and the live trace read it. */
export type AiRunFrame = {
  /**
   * `run`, `step_started`, `text`, `tool_call`, `tool_result`, `usage`, `awaiting_approval`,
   * `loop_done`, `error` or `done`.
   */
  event: string;
  data: Record<string, unknown>;
};

/** A consumer the stream calls as frames arrive. */
export type AiRunStreamHandlers = {
  onFrame?: (frame: AiRunFrame) => void;
};

/**
 * Read an SSE response the one way.
 *
 * The chat stream and the run stream are the same protocol in two places, so the frame parser
 * lives here once: a client that re-implemented it per screen is a client that will eventually
 * read a frame boundary differently from the writer. Split on the blank line, take the `event:`
 * and `data:` lines, hand the pair over — the caller branches on the *event name*, which is the
 * loop's own vocabulary rather than a transport word.
 */
async function readEventStream(
  response: Response,
  handlers: AiRunStreamHandlers,
): Promise<void> {
  if (!response.body) {
    return;
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
      const chunk = buffer.slice(0, boundary);
      buffer = buffer.slice(boundary + 2);
      boundary = buffer.indexOf("\n\n");

      let event = "message";
      let data = "";
      for (const line of chunk.split("\n")) {
        if (line.startsWith("event: ")) event = line.slice(7).trim();
        else if (line.startsWith("data: ")) data += line.slice(6);
      }
      if (!data) continue;

      let parsed: Record<string, unknown>;
      try {
        parsed = JSON.parse(data) as Record<string, unknown>;
      } catch {
        // A frame that is not JSON is a transport frame (a comment, a keep-alive), not a
        // lifecycle event. Dropping it is correct; throwing would kill a live run over a
        // keep-alive, which is the worst possible trade.
        continue;
      }
      handlers.onFrame?.({ event, data: parsed });
    }
  }
}

/**
 * Start a run and stream its steps.
 *
 * Everything decidable before the first byte arrives as a normal HTTP status: a disabled agent,
 * an empty goal, a runner that is switched off (`503 runner_disabled`), or a run already in
 * progress (`409 run_in_progress`, whose details carry the existing run's id so the client
 * attaches to it rather than pressing Run again). After the stream opens the frames are the
 * run's own lifecycle.
 */
export async function startAiRun(
  agentId: string,
  input: { goal: string; files?: string[]; organizationId?: string | null },
  handlers: AiRunStreamHandlers = {},
): Promise<void> {
  const response = await fetch(
    `/api/v1/ai/agents/${encodeURIComponent(agentId)}/runs${agentScopeParams(input.organizationId)}`,
    {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json", accept: "text/event-stream" },
      body: JSON.stringify({ goal: input.goal, files: input.files ?? [] }),
    },
  );

  if (!response.ok || !response.body) {
    const payload = (await readJson(response)) as ErrorBody | null;
    throw new ApiError(
      response.status,
      payload?.error?.code ?? "unknown_error",
      payload?.error?.message ?? `The API answered with status ${response.status}.`,
      payload?.error?.details ?? null,
    );
  }

  await readEventStream(response, handlers);
}

/**
 * Re-attach to a run that is still going.
 *
 * A replay, not a subscription: the API has nothing to publish to on a request that is not the
 * one running the loop, so the frames come from the step rows. That is the property the spec's
 * "replay matches SSE" box asks for — the same rows produce both.
 */
export async function attachAiRun(
  runId: string,
  handlers: AiRunStreamHandlers = {},
  organizationId?: string | null,
): Promise<void> {
  const response = await fetch(
    `/api/v1/ai/runs/${encodeURIComponent(runId)}/events${agentScopeParams(organizationId)}`,
    { credentials: "same-origin", headers: { accept: "text/event-stream" } },
  );
  if (!response.ok || !response.body) {
    const payload = (await readJson(response)) as ErrorBody | null;
    throw new ApiError(
      response.status,
      payload?.error?.code ?? "unknown_error",
      payload?.error?.message ?? `The API answered with status ${response.status}.`,
      payload?.error?.details ?? null,
    );
  }
  await readEventStream(response, handlers);
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
export function unlockAccount(userId: string): Promise<LockedAccountsPage> {
  return request<LockedAccountsPage>(
    `/api/v1/security/locked-accounts/${encodeURIComponent(userId)}/unlock`,
    { method: "POST" },
  );
}

/**
 * The skills registry (REQ-099, slice 3).
 *
 * The client mirrors the server's distinction between *attached* and *injected* rather than
 * collapsing them: a row can be attached and still contribute nothing to a prompt, and a tab
 * that renders "3 skills" above a prompt that carries one is the exact failure this API shape
 * exists to make impossible.
 */

/** One registry row, as the table renders it. */
export type AiSkill = {
  /** The stable identifier. */
  key: string;
  /** Display name. */
  name: string;
  /** One line about what it is for. */
  description: string;
  /** When the model should reach for it. */
  when_to_use: string;
  /** The instruction body — data, never code. */
  instructions: string;
  /** Tool keys this skill is relevant to. **Not** a grant. */
  tools: string[];
  /** The manual version. */
  version: number;
  /** The digest of the definition, shown in the drawer. */
  checksum: string;
  /** `built_in` or `custom` — which decides whether Delete is offered. */
  source: string;
  /** Whether the runtime will inject it. */
  enabled: boolean;
  /** How many agents hold it. */
  used_by: number;
  /** Whether the definition is read-only. */
  built_in: boolean;
  /** When it last changed. */
  updated_at: string;
};

/** One attachment, with the runtime's verdict on it. */
export type AiAgentSkill = {
  /** The registry key. */
  key: string;
  /** Display name, or the bare key when the row is gone. */
  name: string;
  /** Description, empty when stale. */
  description: string;
  /** When to use it, empty when stale. */
  when_to_use: string;
  /** Version, 0 when stale. */
  version: number;
  /** The tools it names. */
  tools: string[];
  /** `built_in` / `custom`, empty when stale. */
  source: string;
  /** The attachment order. */
  position: number;
  /** Whether the runtime would inject it. */
  injected: boolean;
  /** Why not, when it would not. */
  withheld_reason: string | null;
  /** A stable code for the reason. */
  withheld_code: string | null;
  /** Whether the row's own checksum still describes its body. */
  checksum_ok: boolean;
};

/**
 * The Skills tab payload.
 *
 * `prompt_block` is the *assembled* text the runtime would add, returned rather than
 * reconstructed: a panel that shows each skill separately can be right about all of them and
 * still display an order the runtime does not use.
 */
export type AiAgentSkills = {
  /** The agent. */
  agent_id: string;
  /** Every attachment, in runtime order. */
  skills: AiAgentSkill[];
  /** The prompt text, or null when nothing is injected. */
  prompt_block: string | null;
  /** Keys that are attached but withheld. */
  withheld: string[];
};

/** The registry list. */
export type AiSkillList = {
  /** This organization's skills plus the built-ins. */
  skills: AiSkill[];
};

/** What `POST /ai/skills/{key}/validate` answers. */
export type AiSkillValidation = {
  /** Whether the definition would be accepted. */
  valid: boolean;
  /** Every problem, in check order. */
  problems: string[];
  /** The digest the body currently has. */
  checksum: string;
  /** Whether it matched a supplied expectation. */
  checksum_matched: boolean;
};

/** The scope query every skills route shares. */
function skillScopeParams(organizationId?: string | null): string {
  return organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
}

// ---- The AI tool registry (REQ-100) ------------------------------------------------------
//
// `ungated_high_risk` and `used_by_agents` are computed by the API on purpose. The stripe and the
// disable confirmation are the two places this screen can quietly lie — a client that derived
// "is this high risk and ungated" from `risk === "high" && !requires_approval` would have to
// re-implement the disabled-wins rule to get the same answer, and one that derived the agent list
// from its own copy of the agents table would name agents from another organization.

/** One registry row. */
export type AiTool = {
  key: string;
  class: string;
  /** The single permission this tool needs. Never a list. */
  permission: string;
  risk: "low" | "medium" | "high";
  description: string;
  idempotent: boolean;
  requires_approval: boolean;
  enabled: boolean;
  timeout_ms: number;
  max_calls_per_run: number;
  /** Set once the tool has left the compiled catalogue. */
  retired_note: string | null;
  /** Enabled, high risk, no approval gate — the row that gets a warning stripe. */
  ungated_high_risk: boolean;
  calls_30d: number;
  /** `null` when the tool was never called, which is not the same as 0 %. */
  error_rate_30d: number | null;
  last_used: string | null;
  used_by_agents: { id: string; name: string }[];
};

/** The list body. `seeded: false` renders the "seeding has not run" banner. */
export type AiToolList = { tools: AiTool[]; seeded: boolean };

/** One row of a tool's Recent calls. Arguments are never returned. */
export type AiToolCall = {
  id: number;
  created_at: string;
  agent_id: string | null;
  agent_name: string | null;
  run_id: string | null;
  status: "ok" | "denied" | "failed" | "timeout" | "limited";
  error_code: string | null;
  duration_ms: number | null;
  args_bytes: number | null;
  result_bytes: number | null;
};

/** One tool with its schema, limits and recent calls. */
export type AiToolDetail = AiTool & {
  input_schema: unknown;
  example: unknown;
  /** `false` for a row whose tool has left the compiled catalogue. */
  compiled: boolean;
  recent_calls: AiToolCall[];
};

/** The class metadata the filters and grouping read. */
export type AiToolClass = { key: string; label: string; order: number; default_risk: string };

/** One tool's usage chart: totals plus the per-day series.
 *
 *  NOT `AiToolUsage` — REQ-099's agent telemetry already owns that name at line ~5507, and a
 *  second export under it would make the telemetry's `tools: Record<string, number>` shadow the
 *  registry's per-day series depending on import order. `AiToolUsageChart` says which of the two
 *  is meant at the call site. */
export type AiToolUsageChart = {
  key: string;
  days: number;
  calls: number;
  errors: number;
  error_rate: number | null;
  avg_duration_ms: number | null;
  series: { day: string; calls: number; errors: number; avg_duration_ms: number | null }[];
};

// ---- AI identities and the permission matrix (REQ-100 slice 2) ----------------------------
//
// The tri-state is a *string union*, not a boolean and not `boolean | null`. A cell that a
// client cannot distinguish from "unset" is a cell a client will guess about, and the guess is
// always "allow" — which for a permission matrix is the dangerous direction. The API sends
// `"inherit"` explicitly so a toggle back to the default state is a value, not an omission.

/** The three states one matrix cell can be in. Inherit writes no row. */
export type AiGrantEffect = "allow" | "deny" | "inherit";

/** One identity, as the list and the detail render it. */
export type AiIdentity = {
  id: string;
  /** `null` is the platform-level identity, shared by every organization. */
  organization_id: string | null;
  key: string;
  name: string;
  description: string;
  is_default: boolean;
  platform_level: boolean;
  /** Decided allow cells. Inherit is not counted: it is the absence of a decision. */
  allowed: number;
  denied: number;
  agents_using: number;
  updated_at: string;
};

/** The identity table's body. */
export type AiIdentityList = { identities: AiIdentity[]; own: number };

/** One row of the identity's grant editor — every tool, not only the decided ones. */
export type AiIdentityToolCell = {
  tool_key: string;
  class: string;
  risk: string;
  /** The permission the tool itself needs, shown so a cell's sensitivity is legible. */
  permission: string;
  enabled: boolean;
  requires_approval: boolean;
  effect: AiGrantEffect;
};

/** One identity with its whole grant editor. */
export type AiIdentityDetail = AiIdentity & { tools: AiIdentityToolCell[] };

/** A grant map: `tool_key → effect`. Inherit is present in the map and writes no row. */
export type AiIdentityGrants = { grants: Record<string, AiGrantEffect> };

/** One row of the matrix. */
export type AiMatrixTool = {
  key: string;
  class: string;
  risk: string;
  description: string;
  permission: string;
  enabled: boolean;
  requires_approval: boolean;
  ungated_high_risk: boolean;
};

/** One agent's column. A tool absent from `tools` is "not in the agent's list". */
export type AiMatrixAgentColumn = {
  id: string;
  key: string;
  name: string;
  enabled: boolean;
  tools: string[];
  approvals: string[];
};

/** One identity's column — the cells that actually carry a real tri-state. */
export type AiMatrixIdentityColumn = {
  id: string;
  key: string;
  name: string;
  is_default: boolean;
  platform_level: boolean;
  /** Decided cells only; anything else is inherit. */
  grants: Record<string, AiGrantEffect>;
};

/** The whole grid, with the viewer's own permissions so disabled cells can be explained. */
export type AiPermissionMatrix = {
  tools: AiMatrixTool[];
  agents: AiMatrixAgentColumn[];
  identities: AiMatrixIdentityColumn[];
  /** The tool permissions the caller holds. */
  viewer_permissions: string[];
  /** `tool_key → the permissions the caller is missing`, named so the cell can say which. */
  viewer_missing: Record<string, string[]>;
};

/** The identities table. */
export function fetchAiIdentities(
  organizationId?: string | null,
): Promise<AiIdentityList> {
  return request<AiIdentityList>(
    `/api/v1/ai/identities${toolScopeParams(organizationId)}`,
  );
}

/** One identity with its whole grant editor. */
export function fetchAiIdentity(
  id: string,
  organizationId?: string | null,
): Promise<AiIdentityDetail> {
  return request<AiIdentityDetail>(
    `/api/v1/ai/identities/${encodeURIComponent(id)}${toolScopeParams(organizationId)}`,
  );
}

/** Create an identity. */
export function createAiIdentity(
  body: { key: string; name: string; description?: string; is_default?: boolean },
  organizationId?: string | null,
): Promise<AiIdentity> {
  return request<AiIdentity>(`/api/v1/ai/identities${toolScopeParams(organizationId)}`, {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** Rename, re-describe or promote an identity. An absent field is left as it is. */
export function updateAiIdentity(
  id: string,
  changes: { name?: string; description?: string; is_default?: boolean },
  organizationId?: string | null,
): Promise<AiIdentity> {
  return request<AiIdentity>(
    `/api/v1/ai/identities/${encodeURIComponent(id)}${toolScopeParams(organizationId)}`,
    { method: "PATCH", body: JSON.stringify(changes) },
  );
}

/** Remove an identity and, with it, its grants. */
export function deleteAiIdentity(
  id: string,
  organizationId?: string | null,
): Promise<void> {
  return request<void>(
    `/api/v1/ai/identities/${encodeURIComponent(id)}${toolScopeParams(organizationId)}`,
    { method: "DELETE" },
  );
}

/** One identity's decided grants. */
export function fetchAiIdentityGrants(
  id: string,
  organizationId?: string | null,
): Promise<AiIdentityGrants> {
  return request<AiIdentityGrants>(
    `/api/v1/ai/identities/${encodeURIComponent(id)}/tools${toolScopeParams(organizationId)}`,
  );
}

/** Replace one identity's whole grant map. Inherit in the map writes no row. */
export function saveAiIdentityGrants(
  id: string,
  grants: Record<string, AiGrantEffect>,
  organizationId?: string | null,
): Promise<AiIdentityGrants> {
  return request<AiIdentityGrants>(
    `/api/v1/ai/identities/${encodeURIComponent(id)}/tools${toolScopeParams(organizationId)}`,
    { method: "PUT", body: JSON.stringify({ grants }) },
  );
}

/** The full tool × (agents, identities) grid. */
export function fetchAiPermissionMatrix(
  organizationId?: string | null,
): Promise<AiPermissionMatrix> {
  return request<AiPermissionMatrix>(
    `/api/v1/ai/permissions/matrix${toolScopeParams(organizationId)}`,
  );
}


function toolScopeParams(organizationId?: string | null): string {
  return organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
}

/** The registry, with every filter the spec's table lists. */
export function fetchAiTools(options?: {
  organizationId?: string | null;
  q?: string;
  class?: string;
  risk?: string;
  gated?: boolean;
  enabled?: boolean;
}): Promise<AiToolList> {
  const params = new URLSearchParams();
  if (options?.organizationId) params.set("organization_id", options.organizationId);
  if (options?.q) params.set("q", options.q);
  if (options?.class) params.set("class", options.class);
  if (options?.risk) params.set("risk", options.risk);
  if (options?.gated !== undefined) params.set("gated", String(options.gated));
  if (options?.enabled !== undefined) params.set("enabled", String(options.enabled));
  const query = params.toString();
  return request<AiToolList>(`/api/v1/ai/tools${query ? `?${query}` : ""}`);
}

/** One tool. */
export function fetchAiTool(
  key: string,
  organizationId?: string | null,
): Promise<AiToolDetail> {
  return request<AiToolDetail>(
    `/api/v1/ai/tools/${encodeURIComponent(key)}${toolScopeParams(organizationId)}`,
  );
}

/** The class list, for the filter chips and the grouping order. */
export function fetchAiToolClasses(organizationId?: string | null): Promise<AiToolClass[]> {
  return request<AiToolClass[]>(
    `/api/v1/ai/tools/classes${toolScopeParams(organizationId)}`,
  );
}

/** The four operator-owned decisions. An absent field is left as it is. */
export async function updateAiTool(
  key: string,
  changes: {
    enabled?: boolean;
    requires_approval?: boolean;
    timeout_ms?: number;
    max_calls_per_run?: number;
  },
  organizationId?: string | null,
): Promise<AiTool> {
  return request<AiTool>(`/api/v1/ai/tools/${encodeURIComponent(key)}${toolScopeParams(organizationId)}`, {
    method: "PATCH",
    body: JSON.stringify(changes),
  });
}

/** One tool's usage chart over a window.
 *
 *  NOT `fetchAiToolUsage` — REQ-099's agent telemetry already exports that name for the
 *  tenant-wide `{ since, tools: Record<string, number> }` shape. Two exports under one name in a
 *  6700-line module is a duplicate-implementation error, and the *right* fix is the more specific
 *  name rather than renaming the one three other files already import. */
export function fetchAiToolUsageChart(
  key: string,
  options?: { organizationId?: string | null; days?: number },
): Promise<AiToolUsageChart> {
  const params = new URLSearchParams();
  if (options?.organizationId) params.set("organization_id", options.organizationId);
  if (options?.days) params.set("days", String(options.days));
  const query = params.toString();
  return request<AiToolUsageChart>(
    `/api/v1/ai/tools/${encodeURIComponent(key)}/usage${query ? `?${query}` : ""}`,
  );
}

/** The registry, optionally filtered to the rows that are enabled. */
export function fetchAiSkills(options?: {
  organizationId?: string | null;
  enabledOnly?: boolean;
}): Promise<AiSkillList> {
  const params = new URLSearchParams();
  if (options?.organizationId) params.set("organization_id", options.organizationId);
  if (options?.enabledOnly) params.set("enabled_only", "true");
  const query = params.toString();
  return request<AiSkillList>(`/api/v1/ai/skills${query ? `?${query}` : ""}`);
}

/** One definition. */
export function fetchAiSkill(
  key: string,
  organizationId?: string | null,
): Promise<AiSkill> {
  return request<AiSkill>(
    `/api/v1/ai/skills/${encodeURIComponent(key)}${skillScopeParams(organizationId)}`,
  );
}

/** Register a custom definition. */
export async function createAiSkill(input: {
  key: string;
  name: string;
  description?: string;
  when_to_use?: string;
  instructions: string;
  tools?: string[];
  enabled?: boolean;
  organizationId?: string | null;
}): Promise<AiSkill> {
  return request<AiSkill>("/api/v1/ai/skills", {
    method: "POST",
    body: JSON.stringify({
      key: input.key,
      name: input.name,
      description: input.description ?? "",
      when_to_use: input.when_to_use ?? "",
      instructions: input.instructions,
      tools: input.tools ?? [],
      enabled: input.enabled ?? true,
    }),
  });
}

/** Change a definition, or enable/disable a built-in. */
export async function updateAiSkill(
  key: string,
  changes: {
    name?: string;
    description?: string;
    when_to_use?: string;
    instructions?: string;
    tools?: string[];
    version?: number;
    enabled?: boolean;
  },
  organizationId?: string | null,
): Promise<AiSkill> {
  return request<AiSkill>(`/api/v1/ai/skills/${encodeURIComponent(key)}${skillScopeParams(organizationId)}`, {
    method: "PATCH",
    body: JSON.stringify(changes),
  });
}

/** Remove a custom skill. A built-in answers 403. */
export async function deleteAiSkill(
  key: string,
  organizationId?: string | null,
): Promise<void> {
  await request<null>(
    `/api/v1/ai/skills/${encodeURIComponent(key)}${skillScopeParams(organizationId)}`,
    { method: "DELETE" },
  );
}

/** "Would this definition be accepted?" — writes nothing. */
export function validateAiSkill(
  key: string,
  draft: {
    name?: string;
    description?: string;
    when_to_use?: string;
    instructions?: string;
    tools?: string[];
    expected_checksum?: string | null;
  },
  organizationId?: string | null,
): Promise<AiSkillValidation> {
  return request<AiSkillValidation>(
    `/api/v1/ai/skills/${encodeURIComponent(key)}/validate${skillScopeParams(organizationId)}`,
    {
      method: "POST",
      body: JSON.stringify({
        key,
        name: draft.name ?? "",
        description: draft.description ?? "",
        when_to_use: draft.when_to_use ?? "",
        instructions: draft.instructions ?? "",
        tools: draft.tools ?? [],
        expected_checksum: draft.expected_checksum ?? null,
      }),
    },
  );
}

/** The Skills tab payload for one agent. */
export function fetchAiAgentSkills(
  agentId: string,
  organizationId?: string | null,
): Promise<AiAgentSkills> {
  return request<AiAgentSkills>(
    `/api/v1/ai/agents/${encodeURIComponent(agentId)}/skills${skillScopeParams(organizationId)}`,
  );
}

/**
 * Attach one skill.
 *
 * The response reports `tools_not_in_agent`: the skill names a tool this agent cannot call. That
 * is a *warning*, not a refusal — the operator may be about to grant it — but it has to be
 * sayable out loud, because the alternative is a run that mysteriously ignores half its
 * instructions.
 */
export function attachAiAgentSkill(
  agentId: string,
  skillKey: string,
  organizationId?: string | null,
): Promise<{ skill: AiAgentSkill; tools_not_in_agent: string[] }> {
  return request<{ skill: AiAgentSkill; tools_not_in_agent: string[] }>(
    `/api/v1/ai/agents/${encodeURIComponent(agentId)}/skills${skillScopeParams(organizationId)}`,
    { method: "POST", body: JSON.stringify({ skill_key: skillKey }) },
  );
}

/** Replace the whole order — a drag produces a list, so the whole list goes. */
export function setAiAgentSkills(
  agentId: string,
  skills: string[],
  organizationId?: string | null,
): Promise<AiAgentSkills> {
  return request<AiAgentSkills>(
    `/api/v1/ai/agents/${encodeURIComponent(agentId)}/skills${skillScopeParams(organizationId)}`,
    { method: "PUT", body: JSON.stringify({ skills }) },
  );
}

/** Detach one skill. */
export async function detachAiAgentSkill(
  agentId: string,
  skillKey: string,
  organizationId?: string | null,
): Promise<void> {
  await request<null>(
    `/api/v1/ai/agents/${encodeURIComponent(agentId)}/skills/${encodeURIComponent(skillKey)}${skillScopeParams(organizationId)}`,
    { method: "DELETE" },
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
