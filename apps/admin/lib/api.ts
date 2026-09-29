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
// Observability (REQ-126) and secrets (REQ-125)
// ---------------------------------------------------------------------------------------------
//
// Two surfaces share this section because they share one rule: **this client never receives a
// secret value.** A credential row carries a name, a kind, a provider and a validation state; a
// lease row carries a name, a version and a redemption budget. The single exception is
// `createDeploymentKey`, which answers the key exactly once at creation and nowhere else — the
// response type is named `CreatedDeploymentKey` rather than `DeploymentKey` for that reason, and
// the value is not cached in the module or in any store.
//
// Every type below mirrors a Rust view in `apps/api/src/routes/{observability,observability_
// traces,observability_alerts,secrets,secrets_audit,secrets_credentials,secrets_leases}.rs`. When
// a field exists on screen and not here, the screen is wrong; when it exists here and not on
// screen, the client is carrying a field no operator can act on. Both have happened.

/* ── metrics (REQ-126 slice 2) ────────────────────────────────────────────────────────────── */

/** One documented metric family, as the catalogue records it. */
export type MetricFamily = {
  /** The exposition name, e.g. `omnion_http_requests_total`. */
  name: string;
  /** `counter`, `gauge` or `histogram`. */
  kind: string;
  /** The unit shown next to the chart. */
  unit: string;
  /** The one-sentence description, so a name is never the only documentation. */
  description: string;
  /** The label names, positionally. */
  labels: string[];
  /** `core`, `module` or `worker`. */
  source: string;
  /** The series the registry holds right now. */
  cardinality_estimate: number;
  /** The cap this family is held to. */
  cardinality_budget: number;
  /** Whether the cap is enforced for this family. */
  budgeted: boolean;
  /** When the family last recorded a sample, if it ever has. `null` and an old date are different. */
  last_seen_at: string | null;
  /** `true` when this build emits the family. */
  live: boolean;
  /** `true` when the family is folding samples into its `other` series. */
  over_budget: boolean;
};

/**
 * The label values the registry has actually seen for one family.
 *
 * The selector builder is fed from this rather than from a hardcoded list, so a label an operator
 * cannot select is never offered — which is the difference between a filter and a guessing game.
 */
export type MetricLabelCatalogue = {
  metric: string;
  labels: string[];
  values: string[];
  bounded_set_cap: number;
};

/** The catalogue response. */
export type MetricCatalogResponse = {
  families: MetricFamily[];
  label_catalogues: MetricLabelCatalogue[];
  over_budget: string[];
  global_budget: number;
  max_points: number;
};

/** One point of a series: the instant and the value. */
export type MetricPoint = {
  /** RFC 3339 instant of the bucket. */
  at: string;
  /** The value in the family's unit. */
  value: number;
};

/** One line of a multi-series chart. */
export type MetricSeries = {
  /** The label values, positionally, matching the family's `labels`. */
  labels: string[];
  /** The running total, which a rate is computed from. */
  total: number;
  /** How many samples the buckets cover. */
  observations: number;
  /** The buckets, oldest first. */
  points: MetricPoint[];
};

/** A chart's worth of data, for one catalogue selector. */
export type MetricQueryResponse = {
  metric: string;
  kind: string;
  unit: string;
  labels: string[];
  window_minutes: number;
  max_points: number;
  series: MetricSeries[];
  /** PromQL for the same selection, ready to paste into a dashboard. */
  promql: string;
  /**
   * `true` when the family is declared but has never recorded a sample.
   *
   * This is a third state next to "no samples in this window": a blank graph and an idle metric
   * and an undeclared-in-this-build family look identical unless one of them says so.
   */
  no_samples: boolean;
};

/** `GET /api/v1/observability/metrics/catalog` — the documented families. */
export function fetchMetricCatalog(): Promise<MetricCatalogResponse> {
  return request<MetricCatalogResponse>("/api/v1/observability/metrics/catalog");
}

/**
 * `GET /api/v1/observability/overview` — the landing screen (docs/requests/REQ-126).
 *
 * Every headline is `number | null` and the difference is load-bearing: `null` means "this
 * family has no samples in the window", which on a fresh install is every one of them, and `0`
 * means "the counter is genuinely at zero". The screen renders the first as "—" with an
 * explanation and the second as a number, because a panel of zeroes on an instance that has
 * served nothing is indistinguishable from a healthy flat line.
 *
 * `unavailable` names the sources that could not be read. It exists because the screen is
 * assembled defensively server-side: a tile whose source is unreadable degrades to `null` and
 * adds its name here rather than failing the whole read, since the operator opening this page
 * during an incident must not get an error page.
 */
export interface ObservabilityOverview {
  window_minutes: number;
  requests_total: number | null;
  error_ratio: number | null;
  p95_latency_seconds: number | null;
  queue_depth: number | null;
  ai_cost_micros_today: number | null;
  exporters: OverviewExporter[];
  alerts: OverviewAlerts | null;
  unavailable: string[];
  no_traffic: boolean;
}

/** One exporter's chip and the drop count that says whether to trust it. */
export interface OverviewExporter {
  name: string;
  health: string;
  dropped_total: number;
  buffered: number;
}

/** The alert surface in one count per state, plus the newest firing incident. */
export interface OverviewAlerts {
  firing: number;
  pending: number;
  latest_firing: OverviewIncident | null;
}

/**
 * The newest firing incident.
 *
 * `rule` is nullable because the event stores a rule id and the rule may since have been
 * deleted; the incident stays linkable by `rule_id` either way.
 */
export interface OverviewIncident {
  id: string;
  rule_id: string;
  rule: string | null;
  started_at: string;
}

/** `GET /api/v1/observability/overview`. */
export function fetchObservabilityOverview(): Promise<ObservabilityOverview> {
  return request<ObservabilityOverview>("/api/v1/observability/overview");
}

/**
 * `GET /api/v1/observability/metrics/query` — a bounded chart for one family.
 *
 * `metric` is required: the API refuses an unknown family with `unknown_metric` rather than
 * answering an empty series, so a wrong name is a named error and never a blank graph.
 */
export function fetchMetricQuery(
  metric: string,
  windowMinutes: number,
): Promise<MetricQueryResponse> {
  const query = new URLSearchParams({
    metric,
    window_minutes: String(windowMinutes),
  });
  return request<MetricQueryResponse>(`/api/v1/observability/metrics/query?${query}`);
}

/** `POST /api/v1/observability/metrics/sync` — re-seed the catalogue from the live registry. */
export function syncMetricCatalog(): Promise<MetricCatalogResponse> {
  return request<MetricCatalogResponse>("/api/v1/observability/metrics/sync", { method: "POST" });
}

/* ── the log explorer (REQ-126 slice 1) ────────────────────────────────────────────────── */

/**
 * One stored line, in the panel's shape.
 *
 * Every field here is a **column** of `obs_log_entries` except `fields`, which was redacted at
 * write time — this client never receives a value to mask, so there is no second rule here to
 * keep in sync with the one in `crates/telemetry::redact`. A screen that wanted to "show the raw
 * message" would find nothing to show, which is the point.
 */
export type LogEntry = {
  id: number;
  /** RFC 3339 instant the line was emitted. */
  ts: string;
  /** `trace`, `debug`, `info`, `warn` or `error` — the set is closed and the API rejects anything else. */
  level: string;
  /** The Rust module path, so a line can be narrowed to one module. */
  target: string;
  /** The message. Redacted before it was ever stored. */
  message: string;
  request_id: string | null;
  trace_id: string | null;
  user_id: string | null;
  organization_id: string | null;
  /** The route TEMPLATE (`/api/v1/secrets/{id}`), never a raw path. */
  route: string | null;
  method: string | null;
  status: number | null;
  duration_ms: number | null;
  /** Which process emitted it: `api`, `worker` or `cli`. */
  source: string;
  host: string | null;
  version: string | null;
  /** Structured detail, already redacted. */
  fields: unknown;
};

/**
 * The explorer's query string.
 *
 * `level` is an array because the screen's control is a multi-select, and `?level=info` also
 * works for a hand-typed link — the API takes both forms. `requestId` is a separate field rather
 * than a text search because it routes to `/logs/requests/{id}`, which returns the lines in the
 * opposite order (a timeline, not a list) and covers the workers as well as the API.
 */
export type LogFilters = {
  levels?: string[];
  target?: string;
  requestId?: string;
  traceId?: string;
  /** RFC 3339 lower bound. */
  since?: string;
  /** RFC 3339 upper bound. */
  until?: string;
  /** `api`, `worker` or `cli`. */
  source?: string;
  /** Substring of the message. */
  text?: string;
  limit?: number;
};

/**
 * The explorer response.
 *
 * `levels` and `targets` come from the store rather than from a constant, so the filter's chips
 * are the values this instance has actually recorded — a hard-coded level list offers an option
 * that can never return a row, which is how a filter becomes a guessing game.
 */
export type LogListResponse = {
  /** The rows, newest first — except through `fetchRequestLines`, which is oldest first. */
  entries: LogEntry[];
  levels: string[];
  targets: string[];
  /** How many lines the store holds in total, so an empty page is not an empty store. */
  stored_total: number;
  /** The window the store will answer, in days. */
  max_window_days: number;
  /** The cap on one search, in rows. */
  max_rows: number;
};

/**
 * `GET /api/v1/observability/logs` — the bounded explorer.
 *
 * An empty parameter is dropped rather than sent blank: `?target=` is a filter for the empty
 * string, and the difference between "no target filter" and "the target is ''" is one the store
 * answers differently.
 */
export function fetchLogs(filters: LogFilters = {}): Promise<LogListResponse> {
  const query = new URLSearchParams();
  for (const level of filters.levels ?? []) query.append("level", level);
  if (filters.target) query.set("target", filters.target);
  if (filters.traceId) query.set("trace_id", filters.traceId);
  if (filters.since) query.set("since", filters.since);
  if (filters.until) query.set("until", filters.until);
  if (filters.source) query.set("source", filters.source);
  if (filters.text) query.set("text", filters.text);
  if (filters.limit) query.set("limit", String(filters.limit));

  // A request id takes the dedicated route rather than this one: the ordering is the opposite and
  // a caller who has to remember `&order=asc` to read a timeline will eventually get it wrong.
  if (filters.requestId) return fetchRequestLines(filters.requestId);

  const suffix = query.toString();
  return request<LogListResponse>(`/api/v1/observability/logs${suffix ? `?${suffix}` : ""}`);
}

/**
 * `GET /api/v1/observability/logs/requests/{id}` — **one request's lines, oldest first**.
 *
 * This is the route an error banner is for: every line the request produced, across the API and
 * the workers, in the order it happened. The same response type is returned deliberately — the
 * shape of a line does not change with the direction of the list.
 */
export function fetchRequestLines(requestId: string): Promise<LogListResponse> {
  return request<LogListResponse>(
    `/api/v1/observability/logs/requests/${encodeURIComponent(requestId)}`,
  );
}

/**
 * The one log settings row, as the explorer reads it.
 *
 * `max_retention_days` is carried so the screen can say WHY a number is refused rather than only
 * that it was — "must be 14 or less" without the cap in hand is a validation message that reads
 * as an arbitrary rule, and an operator who cannot see the cap will not know whether to argue
 * with the value or the operator.
 */
export type LogSettingsView = {
  /** The level a module logs at unless it is raised. */
  log_level_default: string;
  /** Per-module raises, including the ones that have already expired. */
  log_level_overrides: Record<string, unknown>;
  /** How many days are kept. */
  logs_retention_days: number;
  /** When the row was last written, RFC 3339. */
  updated_at: string;
  /** The cap retention is allowed to reach. */
  max_retention_days: number;
};

/**
 * `GET /api/v1/observability/logs/settings` — the one settings row, read-only from here.
 *
 * `PUT /logs/settings` is a separate route under `observability.manage` and is the whole settings
 * screen's job; this client deliberately does not wrap it, so a log explorer cannot silently
 * change retention. The screen shows the retention window so "the store only goes back N days"
 * is visible next to the results it is limiting.
 */
export function fetchLogSettings(): Promise<LogSettingsView> {
  return request<LogSettingsView>("/api/v1/observability/logs/settings");
}

/* ── traces and exporters (REQ-126 slice 3) ───────────────────────────────────────────────── */

/** The trace list's filter. `undefined` means "no filter on this column", never "match nothing". */
export type TraceFilters = {
  request_id?: string;
  route?: string;
  /** `ok` or `error`. */
  status?: string;
  min_duration_ms?: number;
  window_minutes?: number;
  limit?: number;
};

/** One row of the trace list. */
export type TraceSummary = {
  trace_id: string;
  root_name: string;
  route: string | null;
  request_id: string | null;
  started_at: string;
  duration_ms: number;
  span_count: number;
  /** The spans the platform kept; lower than `span_count` when the trace was truncated. */
  spans_kept: number;
  /** `true` when the waterfall on screen is incomplete, so the screen must say so. */
  spans_truncated: boolean;
  status: string;
  /** The sampling decision, so an operator can tell "fast" from "unsampled". */
  sampling: string;
};

/** The trace list response. */
export type TracesResponse = {
  traces: TraceSummary[];
  total: number;
  window_minutes: number;
};

/** One span of a waterfall. */
export type TraceSpan = {
  span_id: string;
  parent_span_id: string | null;
  name: string;
  service: string;
  offset_ms: number;
  duration_ms: number;
  attributes: Record<string, unknown>;
  failed: boolean;
  root: boolean;
};

/** One trace with its waterfall. */
export type TraceDetail = TraceSummary & {
  service: string;
  /** Where the full trace lives in the operator's tracing backend, when one is configured. */
  backend_trace_url: string | null;
  spans: TraceSpan[];
};

/** `GET /api/v1/observability/traces`. */
export function fetchTraces(filters: TraceFilters = {}): Promise<TracesResponse> {
  const query = new URLSearchParams();
  // Only the filters that were set travel: an empty string in a query parameter is a filter that
  // matches nothing, which is why the screens normalise blanks to `undefined` before calling.
  for (const [key, value] of Object.entries(filters)) {
    if (value === undefined || value === null || value === "") continue;
    query.set(key, String(value));
  }
  const suffix = query.toString();
  return request<TracesResponse>(`/api/v1/observability/traces${suffix ? `?${suffix}` : ""}`);
}

/**
 * `GET /api/v1/observability/traces/{trace_id}`.
 *
 * A trace that is not in the index is `404 trace_not_found` — an unsampled trace and an expired
 * one are both "not here", and the message says which the operator should expect.
 */
export function fetchTrace(traceId: string): Promise<TraceDetail> {
  return request<TraceDetail>(`/api/v1/observability/traces/${encodeURIComponent(traceId)}`);
}

/** One configured exporter, with its live health and drop counters. */
export type ExporterRow = {
  id: string;
  name: string;
  kind: string;
  endpoint: string;
  protocol: string | null;
  /** `true` when an auth credential is referenced — never the credential itself. */
  auth_configured: boolean;
  batch_ms: number;
  timeout_ms: number;
  enabled: boolean;
  /** `healthy`, `degraded`, `disabled` or `unknown` before this process loaded it. */
  health: string;
  buffered: number;
  capacity: number;
  dropped_total: number;
  last_flush_at: string | null;
  last_error: string | null;
};

/** The exporter form's body. Every field is what the API's `ExporterInput` accepts. */
export type ExporterInput = {
  name: string;
  kind: string;
  endpoint: string;
  protocol?: string | null;
  /**
   * Omitted on an edit that leaves the box empty.
   *
   * An empty box means "keep the current reference", so it is not sent at all: sending `null`
   * would silently de-authenticate the exporter over a typo.
   */
  auth_secret_id?: string;
  batch_ms?: number;
  timeout_ms?: number;
  enabled?: boolean;
};

/** The exporter list response. */
export type ExportersResponse = {
  exporters: ExporterRow[];
  kinds: string[];
  /** The sentence about what leaves this instance, stated on the payload and not only in a component. */
  egress_notice: string;
};

/**
 * The result of a test send.
 *
 * A failed send is a `200` carrying the backend's own words, not an error page: a `Test` button
 * that renders an error page has told the operator nothing they could not have learned by waiting.
 */
export type ExporterTestResult = {
  name: string;
  kind: string;
  ok: boolean;
  detail: string;
  health: string;
};

/** `GET /api/v1/observability/exporters`. */
export function fetchExporters(): Promise<ExportersResponse> {
  return request<ExportersResponse>("/api/v1/observability/exporters");
}

/** `POST /api/v1/observability/exporters` — answers `201` with the new row's id and name. */
export function createExporter(input: ExporterInput): Promise<{ id: string; name: string }> {
  return request<{ id: string; name: string }>("/api/v1/observability/exporters", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** `PATCH /api/v1/observability/exporters/{id}`. */
export function updateExporter(
  id: string,
  input: ExporterInput,
): Promise<{ id: string; name: string }> {
  return request<{ id: string; name: string }>(
    `/api/v1/observability/exporters/${encodeURIComponent(id)}`,
    { method: "PATCH", body: JSON.stringify(input) },
  );
}

/** `DELETE /api/v1/observability/exporters/{id}`. */
export function deleteExporter(id: string): Promise<null> {
  return request<null>(`/api/v1/observability/exporters/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

/** `POST /api/v1/observability/exporters/{id}/test` — a real send of a synthetic batch. */
export function testExporter(id: string): Promise<ExporterTestResult> {
  return request<ExporterTestResult>(
    `/api/v1/observability/exporters/${encodeURIComponent(id)}/test`,
    { method: "POST" },
  );
}

/* ── alert rules, alerts and silences (REQ-126 slice 4) ────────────────────────────────────── */

/** One rule, with the live evaluation state the catalogue does not hold. */
export type AlertRuleRow = {
  id: string;
  name: string;
  expr: string;
  parsed: string | null;
  severity: string;
  for_seconds: number;
  summary: string;
  runbook_url: string | null;
  labels: Record<string, unknown>;
  /** `built_in` for the rules the platform ships, `custom` for an operator's own. */
  source: string;
  enabled: boolean;
  state: string | null;
  value: number | null;
  silenced: boolean;
  silenced_until: string | null;
  /** Whether the expression parsed. A rule that does not parse is stored, and flagged. */
  expression_valid: boolean;
  expression_error: string | null;
};

/** The rule list response. */
export type AlertRulesResponse = {
  rules: AlertRuleRow[];
  families: string[];
  severities: string[];
  max_for_seconds: number;
};

/** The rule form's body. */
export type AlertRuleInput = {
  name: string;
  expr: string;
  severity: string;
  for_seconds: number;
  summary: string;
  runbook_url?: string | null;
  labels?: Record<string, unknown>;
};

/** The rule patch. Every field is optional, and a misspelled one is refused by the API. */
export type AlertRulePatch = Partial<AlertRuleInput> & { enabled?: boolean };

/** One firing, pending or resolved alert. */
export type AlertEventRow = {
  id: number;
  rule_id: string;
  state: string;
  value: number | null;
  /** The value at the moment it crossed, which is not always the current one. */
  firing_value: number | null;
  started_at: string;
  ended_at: string | null;
  fired_at: string | null;
  notified: boolean;
  reason: string;
  context: Record<string, unknown>;
};

/** One suppression window. */
export type SilenceRow = {
  id: string;
  rule_id: string | null;
  reason: string;
  starts_at: string | null;
  ends_at: string;
  active: boolean;
  minutes_remaining: number;
};

/** The alerts response, with the counts the header strip reads. */
export type AlertsResponse = {
  firing: AlertEventRow[];
  pending: AlertEventRow[];
  resolved: AlertEventRow[];
  silences: SilenceRow[];
  counts: {
    firing: number;
    pending: number;
    silenced: number;
    worst_severity: string | null;
  };
};

/** What an expression evaluates to right now, before it is saved. */
export type AlertPreview = {
  /** The expression as the evaluator understood it. */
  rendered: string;
  family: string;
  value: number | null;
  series: number;
  breaching: boolean;
  /** `true` when the family has no samples, so `breaching: false` means "no data". */
  no_data: boolean;
};

/** `GET /api/v1/observability/alert-rules`. */
export function fetchAlertRules(): Promise<AlertRulesResponse> {
  return request<AlertRulesResponse>("/api/v1/observability/alert-rules");
}

/** `POST /api/v1/observability/alert-rules` — answers `201`, like every other create here. */
export function createAlertRule(input: AlertRuleInput): Promise<AlertRuleRow> {
  return request<AlertRuleRow>("/api/v1/observability/alert-rules", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** `PATCH /api/v1/observability/alert-rules/{id}` — the enable/disable toggle lives here. */
export function updateAlertRule(id: string, patch: AlertRulePatch): Promise<AlertRuleRow> {
  return request<AlertRuleRow>(
    `/api/v1/observability/alert-rules/${encodeURIComponent(id)}`,
    { method: "PATCH", body: JSON.stringify(patch) },
  );
}

/** `DELETE /api/v1/observability/alert-rules/{id}` — answers `204`. */
export function deleteAlertRule(id: string): Promise<null> {
  return request<null>(`/api/v1/observability/alert-rules/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

/** `POST /api/v1/observability/alert-rules/preview` — evaluate without saving. */
export function previewAlertRule(expr: string): Promise<AlertPreview> {
  return request<AlertPreview>("/api/v1/observability/alert-rules/preview", {
    method: "POST",
    body: JSON.stringify({ expr }),
  });
}

/** `GET /api/v1/observability/alerts`. */
export function fetchAlerts(): Promise<AlertsResponse> {
  return request<AlertsResponse>("/api/v1/observability/alerts");
}

/** The silence form's body. `rule_id: null` silences every rule, which the screen states. */
export type SilenceInput = {
  rule_id: string | null;
  reason: string;
  ends_at: string;
  starts_at?: string | null;
};

/** `POST /api/v1/observability/silences` — answers `201`. */
export function createSilence(input: SilenceInput): Promise<SilenceRow> {
  return request<SilenceRow>("/api/v1/observability/silences", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** `DELETE /api/v1/observability/silences/{id}` — answers `204`. */
export function deleteSilence(id: string): Promise<null> {
  return request<null>(`/api/v1/observability/silences/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

/* ── observability settings and the probe contract (REQ-126 slice 4) ──────────────────────── */

/** The caps the settings screen enforces client-side, so a refusal is a field message. */
export type ObservabilityCaps = {
  logs_retention_max: number;
  traces_retention_max: number;
  sampling_max: number;
  cardinality_max: number;
  log_levels: string[];
};

/** One module's level override. */
export type LogLevelOverrideRow = {
  target: string;
  level: string;
  expires_at: string | null;
  /** `true` once the override lapsed — it is still listed so the screen can offer a clean-up. */
  expired: boolean;
};

/** The one settings row. */
export type ObservabilitySettings = {
  /** The share of non-error requests whose trace is kept. Errors are always kept. */
  sampling_ratio: number;
  logs_retention_days: number;
  traces_retention_days: number;
  log_level_default: string;
  log_level_overrides: Record<string, unknown>;
  cardinality_budget: number;
  prometheus_public: boolean;
  caps: ObservabilityCaps;
  /** The sentence about egress, stated on the payload so it can be audited. */
  egress_note: string;
  level_overrides: LogLevelOverrideRow[];
};

/** The settings form's body. */
export type ObservabilitySettingsInput = {
  sampling_ratio: number;
  logs_retention_days: number;
  traces_retention_days: number;
  log_level_default: string;
  log_level_overrides: Record<string, unknown>;
  cardinality_budget: number;
  prometheus_public: boolean;
};

/** Where this process is in its own shutdown, and the probe paths that describe it. */
export type LifecycleResponse = {
  draining: boolean;
  in_flight: number;
  drain_timeout_ms: number;
  probes: {
    liveness: { path: string; alias: string; fails_on_drain: boolean };
    readiness: { path: string; alias: string; fails_on_drain: boolean };
  };
  /** Why liveness deliberately does not report the drain. */
  note: string;
  summary: Record<string, unknown>;
};

/** `GET /api/v1/observability/settings`. */
export function fetchObservabilitySettings(): Promise<ObservabilitySettings> {
  return request<ObservabilitySettings>("/api/v1/observability/settings");
}

/** `PUT /api/v1/observability/settings` — a write, and audited like one. */
export function saveObservabilitySettings(
  input: ObservabilitySettingsInput,
): Promise<ObservabilitySettings> {
  return request<ObservabilitySettings>("/api/v1/observability/settings", {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

/** `GET /api/v1/observability/lifecycle` — the probe contract, as data. */
export function fetchLifecycle(): Promise<LifecycleResponse> {
  return request<LifecycleResponse>("/api/v1/observability/lifecycle");
}

/* ── credentials (REQ-125) ───────────────────────────────────────────────────────────────── */

/** One credential, described without ever carrying its value. */
export type Credential = {
  id: string;
  name: string;
  kind: string;
  kind_description: string;
  /** `true` when the kind can be checked without a network call. */
  offline_checkable: boolean;
  /** The masked values, for display only. */
  fields: Record<string, unknown>;
  /** The non-secret fields as `[name, value]` pairs, for the detail row. */
  field_pairs: [string, string][];
  validation_state: string;
  validation_message: string;
  validation_checked_at: string | null;
  validation_interval_days: number;
  next_validation_at: string | null;
  provider: string;
  provider_locator: string | null;
  read_only: boolean;
  version: number;
  created_at: string;
  slots: string[];
};

/** One of the five credential kinds, with the fields it wants. */
export type CredentialKindOption = {
  kind: string;
  description: string;
  fields: string[];
  offline: boolean;
};

/** The credential list response. */
export type CredentialsResponse = {
  credentials: Credential[];
  kinds: CredentialKindOption[];
  total: number;
  valid: number;
  invalid: number;
  unknown: number;
};

/** What a validation run answered. */
export type CredentialValidation = {
  id: string;
  validation_state: string;
  validation_message: string;
  checked_at: string;
  valid: boolean;
};

/** `GET /api/v1/secrets/credentials`. */
export function fetchCredentials(): Promise<CredentialsResponse> {
  return request<CredentialsResponse>("/api/v1/secrets/credentials");
}

/**
 * `POST /api/v1/secrets/{id}/validate` — check a credential against its provider.
 *
 * A failed check is still a `200` carrying the provider's own words; only a broken store is an
 * error, because "the check failed" is the answer an operator asked for.
 */
export function validateCredential(id: string): Promise<CredentialValidation> {
  return request<CredentialValidation>(`/api/v1/secrets/${encodeURIComponent(id)}/validate`, {
    method: "POST",
  });
}

/** The typed profile body: a kind and its non-secret fields. */
export type CredentialProfileInput = {
  kind: string;
  fields: Record<string, unknown>;
};

/**
 * `POST /api/v1/secrets/{id}/credential` — pin a secret to a kind — answers `201`.
 *
 * `fields` is a `Record` on purpose: the five kinds want different field names, and a fixed shape
 * would force a screen to know the union of all of them to send one.
 */
export function attachCredentialProfile(
  secretId: string,
  kind: string,
  fields: Record<string, unknown>,
): Promise<Credential> {
  return request<Credential>(`/api/v1/secrets/${encodeURIComponent(secretId)}/credential`, {
    method: "POST",
    body: JSON.stringify({ kind, fields }),
  });
}

/* ── credential slots (REQ-125) ──────────────────────────────────────────────────────────── */

/** One slot's definition, independent of any scope. */
export type SlotDef = {
  slot: string;
  description: string;
  /** Which parts of the platform read this slot, so the cost of leaving it empty is legible. */
  consumers: string;
};

/** One assignment row. */
export type CredentialSlot = {
  id: string;
  scope_type: string;
  scope_id: string;
  slot: string;
  description: string;
  consumers: string;
  primary_secret_id: string | null;
  primary_name: string | null;
  primary_version: number | null;
  fallback_secret_id: string | null;
  fallback_name: string | null;
  fallback_version: number | null;
  last_resolved_by: string | null;
  last_resolved_at: string | null;
  /** Why an empty slot is empty — "unassigned" is not an answer an operator can act on. */
  empty_reason: string;
};

/** A credential that may be assigned to a slot. */
export type AssignableCredential = {
  id: string;
  name: string;
  kind: string;
  read_only: boolean;
  provider: string;
};

/** The slot matrix response. */
export type SlotsResponse = {
  slots: CredentialSlot[];
  catalog: SlotDef[];
  assignable: AssignableCredential[];
  assigned: number;
};

/** What a consumer would actually get, without pretending to be one. */
export type SlotResolution = {
  scope_type: string;
  scope_id: string;
  slot: string;
  secret_id: string;
  name: string;
  version: number;
  /** `true` when the primary was unavailable and the fallback answered instead. */
  fell_back: boolean;
  summary: string;
};

/** `GET /api/v1/credential-slots` — the assignment matrix. */
export function fetchSlots(): Promise<SlotsResponse> {
  return request<SlotsResponse>("/api/v1/credential-slots");
}

/**
 * `PUT /api/v1/credential-slots/{scope}/{slot}` — assign a scope's slot.
 *
 * `primary` and `fallback` are `string | null` rather than optional: passing `null` is how a slot
 * is *cleared*, and omitting the argument is not the same intent. A slot that points at itself is
 * refused by the API with `credential_slot_self_reference`, which belongs next to the field.
 */
export function assignSlot(
  scopeType: string,
  slot: string,
  scopeId: string,
  primary: string | null,
  fallback: string | null,
): Promise<CredentialSlot> {
  return request<CredentialSlot>(
    `/api/v1/credential-slots/${encodeURIComponent(scopeType)}/${encodeURIComponent(slot)}`,
    {
      method: "PUT",
      body: JSON.stringify({
        scope_id: scopeId,
        primary_secret_id: primary,
        fallback_secret_id: fallback,
      }),
    },
  );
}

/** `GET /api/v1/credential-slots/{scope}/{slot}/resolve/{scope_id}` — show a resolution. */
export function resolveSlot(
  scopeType: string,
  slot: string,
  scopeId: string,
): Promise<SlotResolution> {
  return request<SlotResolution>(
    `/api/v1/credential-slots/${encodeURIComponent(scopeType)}/${encodeURIComponent(
      slot,
    )}/resolve/${encodeURIComponent(scopeId)}`,
  );
}

/* ── leases and deployment keys (REQ-125) ───────────────────────────────────────────────── */

/** One lease. A lease is a live copy of a credential, which is why it has a redemption budget. */
export type SecretLease = {
  id: string;
  secret_id: string;
  name: string;
  consumer: string;
  environment: string;
  state: string;
  max_uses: number;
  uses: number;
  expires_in_seconds: number;
  expires_at: string;
  issued_at: string;
  revoked_at: string | null;
  revoke_reason: string | null;
  last_redeemed_at: string | null;
  /**
   * The address the last redemption came from.
   *
   * This is the column an operator reads to find a leak, and it is written by the redemption
   * itself — a keyless loopback redemption writes no use-log row, so this is the only place the
   * answer exists.
   */
  last_address: string | null;
  deployment_key_id: string | null;
  version: number;
};

/** The lease list response. */
export type LeasesResponse = {
  leases: SecretLease[];
  total: number;
  live: number;
  spent: number;
  revoked: number;
  environments: string[];
};

/** `GET /api/v1/secret-leases`. */
export function fetchSecretLeases(): Promise<LeasesResponse> {
  return request<LeasesResponse>("/api/v1/secret-leases");
}

/**
 * `POST /api/v1/secret-leases/{id}/revoke` — with a reason.
 *
 * The reason is mandatory in spirit and optional in shape because the API stores it either way;
 * a bare "revoked" is not worth keeping, and the screen asks for it for that reason.
 */
export function revokeLease(id: string, reason: string): Promise<SecretLease> {
  return request<SecretLease>(`/api/v1/secret-leases/${encodeURIComponent(id)}/revoke`, {
    method: "POST",
    body: JSON.stringify({ reason }),
  });
}

/** One deployment key, described without the key. */
export type DeploymentKey = {
  id: string;
  name: string;
  environment: string;
  /** Credential scopes with a `prefix.*` wildcard — not permission names. */
  scopes: string[];
  state: string;
  expires_at: string;
  expires_in_seconds: number;
  uses: number;
  last_used_at: string | null;
  allowed_ips: string[];
  /** The visible prefix, so an operator can tell two keys apart without either of them. */
  key_prefix: string;
  fingerprint: string;
  created_at: string;
  revoked_at: string | null;
  revoke_reason: string | null;
  /** `false` while a key is live: a live key can only be revoked, not deleted. */
  deletable: boolean;
};

/** The deployment key list response, with the header a caller must send. */
export type DeploymentKeysResponse = {
  keys: DeploymentKey[];
  total: number;
  active: number;
  expired: number;
  revoked: number;
  header: string;
  guidance: string;
};

/**
 * The one response in this file that carries a value.
 *
 * The key is answered here, at creation, and nowhere else — not on the list, not on the detail,
 * not in any store on this side. The name says so, so a reader can see the exception in the type.
 */
export type CreatedDeploymentKey = DeploymentKey & {
  value: string;
  header: string;
};

/** One use of a deployment key. */
export type DeploymentKeyUse = {
  action: string;
  lease_id: string | null;
  identity: string;
  address: string | null;
  result: string;
  created_at: string;
};

/** The deployment key form's body. */
export type DeploymentKeyInput = {
  name: string;
  environment: string;
  scopes: string[];
  /** RFC 3339. A backdated key is refused at creation — "dead on arrival" is caught early. */
  expires_at: string;
  allowed_ips?: string | null;
};

/** `GET /api/v1/deployment-keys`. */
export function fetchDeploymentKeys(): Promise<DeploymentKeysResponse> {
  return request<DeploymentKeysResponse>("/api/v1/deployment-keys");
}

/** `POST /api/v1/deployment-keys` — answers `201` and the value, exactly once. */
export function createDeploymentKey(input: DeploymentKeyInput): Promise<CreatedDeploymentKey> {
  return request<CreatedDeploymentKey>("/api/v1/deployment-keys", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** `POST /api/v1/deployment-keys/{id}/revoke` — answers `204`. */
export function revokeDeploymentKey(id: string, reason: string): Promise<null> {
  return request<null>(`/api/v1/deployment-keys/${encodeURIComponent(id)}/revoke`, {
    method: "POST",
    body: JSON.stringify({ reason }),
  });
}

/** `DELETE /api/v1/deployment-keys/{id}` — a revoked key's record; answers `204`. */
export function deleteDeploymentKey(id: string): Promise<null> {
  return request<null>(`/api/v1/deployment-keys/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

/** `GET /api/v1/deployment-keys/{id}/uses` — the use log, as a bare array. */
export function fetchDeploymentKeyUses(id: string): Promise<DeploymentKeyUse[]> {
  return request<DeploymentKeyUse[]>(
    `/api/v1/deployment-keys/${encodeURIComponent(id)}/uses`,
  );
}

/* ── the secrets audit trail and its detectors (REQ-125) ─────────────────────────────────── */

/** The audit filter. `undefined` means "no filter", never "match nothing". */
export type SecretAuditFilter = {
  actions?: string[];
  secret_id?: string;
  actor_user_id?: string;
  address?: string;
  request_id?: string;
  since?: string;
  limit?: number;
};

/** One trail entry. Metadata is a `Record` because each action documents different fields. */
export type SecretAuditEntry = {
  id: number;
  action: string;
  target_type: string | null;
  target_id: string | null;
  actor_user_id: string | null;
  /** `user`, `deployment_key` or `system` — who acted is never inferred from the row. */
  actor_type: string;
  ip_address: string | null;
  request_id: string | null;
  lease_id: string | null;
  deployment_key_id: string | null;
  /** Which lifecycle the action belongs to, for the filter chips. */
  pipeline: string | null;
  metadata: Record<string, unknown>;
  created_at: string;
};

/** One flag a detector raised. Flags are advisory: acknowledging one deletes nothing. */
export type SecretAnomaly = {
  id: number;
  pattern: string;
  severity: string;
  secret_id: string | null;
  secret_name: string | null;
  actor_user_id: string | null;
  address: string | null;
  detail: Record<string, unknown>;
  request_id: string | null;
  created_at: string;
  acknowledged: boolean;
  acknowledged_by: string | null;
  acknowledged_at: string | null;
};

/** The audit response. */
export type SecretAuditResponse = {
  entries: SecretAuditEntry[];
  open_anomalies: number;
  filters: string[];
  detectors: {
    business_hours_start: number;
    business_hours_end: number;
    reveal_burst_per_hour: number;
    detect_new_network: boolean;
    /** `true` when the platform refuses a reveal outright, not merely flags it. */
    hard_rule_enforced: boolean;
    explanation: string;
  };
  local_hour: number;
};

/** `GET /api/v1/secrets/audit`. */
export function fetchSecretAudit(filter: SecretAuditFilter = {}): Promise<SecretAuditResponse> {
  const query = new URLSearchParams();
  // The API accepts `?action=a&action=b` for the multi-select; a repeated key is the only
  // encoding that carries a list without inventing an envelope the server does not have.
  for (const action of filter.actions ?? []) query.append("action", action);
  for (const [key, value] of [
    ["secret_id", filter.secret_id],
    ["actor_user_id", filter.actor_user_id],
    ["address", filter.address],
    ["request_id", filter.request_id],
    ["since", filter.since],
    ["limit", filter.limit],
  ] as const) {
    if (value === undefined || value === null || value === "") continue;
    query.set(key, String(value));
  }
  const suffix = query.toString();
  return request<SecretAuditResponse>(`/api/v1/secrets/audit${suffix ? `?${suffix}` : ""}`);
}

/** `GET /api/v1/secrets/audit/anomalies` — the flags, unacknowledged first. */
export function fetchSecretAnomalies(): Promise<{ anomalies: SecretAnomaly[] }> {
  return request<{ anomalies: SecretAnomaly[] }>("/api/v1/secrets/audit/anomalies");
}

/**
 * `PATCH /api/v1/secrets/audit/anomalies/{id}/acknowledge` — clear one flag.
 *
 * The answer distinguishes "cleared" from "already cleared", because a second click on an
 * acknowledged row is a normal thing to do and should say so rather than report a change that did
 * not happen.
 */
export function acknowledgeSecretAnomaly(
  id: number,
): Promise<{ state: "acknowledged" | "already_acknowledged"; id: number }> {
  return request<{ state: "acknowledged" | "already_acknowledged"; id: number }>(
    `/api/v1/secrets/audit/anomalies/${id}/acknowledge`,
    { method: "PATCH", body: JSON.stringify({}) },
  );
}

/**
 * `GET /api/v1/secrets/audit/export` — the SIEM feed, as **text**.
 *
 * This is the one call here that does not go through `request()`: the endpoint answers
 * newline-delimited JSON, not a JSON document, and running it through the JSON reader would turn
 * a real export into `null`. A read is still audited by the server, and the read itself is
 * therefore an event an operator can find in the trail.
 */
export async function downloadSecretAuditExport(filter: SecretAuditFilter = {}): Promise<string> {
  const query = new URLSearchParams();
  for (const action of filter.actions ?? []) query.append("action", action);
  for (const [key, value] of [
    ["secret_id", filter.secret_id],
    ["actor_user_id", filter.actor_user_id],
    ["address", filter.address],
    ["request_id", filter.request_id],
    ["limit", filter.limit],
  ] as const) {
    if (value === undefined || value === null || value === "") continue;
    query.set(key, String(value));
  }
  const suffix = query.toString();
  const response = await fetch(
    `/api/v1/secrets/audit/export${suffix ? `?${suffix}` : ""}`,
    { credentials: "same-origin", headers: { accept: "application/x-ndjson" } },
  );
  if (!response.ok) {
    // Shaped like every other refusal so the screen's error path needs no second code path.
    const body = (await readJson(response)) as ErrorBody;
    throw new ApiError(
      response.status,
      body.error?.code ?? "unknown_error",
      body.error?.message ?? `The export failed with status ${response.status}.`,
      body.error?.details ?? null,
    );
  }
  return response.text();
}

/* ── the root key and its rewrap jobs (REQ-125) ──────────────────────────────────────────── */

/** One key version. A fingerprint, never material. */
export type RootKey = {
  key_id: string;
  status: string;
  fingerprint: string;
  version_count: number;
  created_at: string;
  retired_at: string | null;
  retired_reason: string | null;
};

/** Whether the sealed versions can still be opened, and what to do when they cannot. */
export type RootKeySeal = {
  healthy: boolean;
  sealed: number;
  /** `(key_id, source)` pairs: which key and which file refused to unseal. */
  unsealed: [string, string][];
  source: string | null;
  guidance: string;
};

/** One rewrap job — the ceremony that moves sealed versions onto a new key. */
export type RewrapJob = {
  id: string;
  status: string;
  from_key_id: string;
  to_key_id: string;
  rewrapped_count: number;
  total_count: number;
  /** 0..1. A job that paused reports its progress so the screen can say how far it got. */
  progress: number;
  resume_note: string | null;
  pause_reason: string | null;
  last_error: string | null;
  started_at: string;
  completed_at: string | null;
};

/** The whole root-key state, as one read. */
export type RootKeyState = {
  keys: RootKey[];
  seal: RootKeySeal;
  /** The job in flight, if any. */
  job: RewrapJob | null;
  recent_jobs: RewrapJob[];
  versions_to_rewrap: number;
  has_active_key: boolean;
};

/** `GET /api/v1/secrets/root-key`. */
export function fetchRootKeyState(): Promise<RootKeyState> {
  return request<RootKeyState>("/api/v1/secrets/root-key");
}

/** `POST /api/v1/secrets/root-key/rotate` — start the ceremony. */
export function rotateRootKey(): Promise<RewrapJob> {
  return request<RewrapJob>("/api/v1/secrets/root-key/rotate", { method: "POST" });
}

/** `POST /api/v1/secrets/root-key/rewrap-jobs/{id}/pause`. */
export function pauseRewrapJob(id: string): Promise<RewrapJob> {
  return request<RewrapJob>(
    `/api/v1/secrets/root-key/rewrap-jobs/${encodeURIComponent(id)}/pause`,
    { method: "POST" },
  );
}

/** `POST /api/v1/secrets/root-key/rewrap-jobs/{id}/resume`. */
export function resumeRewrapJob(id: string): Promise<RewrapJob> {
  return request<RewrapJob>(
    `/api/v1/secrets/root-key/rewrap-jobs/${encodeURIComponent(id)}/resume`,
    { method: "POST" },
  );
}
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

/** Remove a run. The artifacts on the destination are not removed by this call. */
export function deleteBackup(id: string): Promise<void> {
  return request<void>(`/api/v1/backups/${id}`, { method: "DELETE" });
}

/** The schedules table. Slice 3 adds the writes. */
export function fetchBackupSchedules(): Promise<BackupSchedule[]> {
  return request<BackupSchedule[]>("/api/v1/backup-schedules");
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
// REQ-127 — the platform-wide rate limiter (slice 1)
//
// Every function here talks to `/api/v1/reliability/rate-limits/*`, which is a DIFFERENT surface
// from `/api/v1/security/rate-limits`: the security one is the gateway's per-route document
// (REQ-040) and the reliability one is the platform-wide budgets (user / organization / IP /
// route). Both can refuse the same caller, so both exist, and every refusal the reliability
// layer writes names which of the two answered.
// ---------------------------------------------------------------------------------------------

/**
 * One policy, in resolution order, with the ceiling already added.
 *
 * `enforced_here` is the field that stops a stored-but-unspent row from reading as protection:
 * a `route`-scoped policy is stored and listed, but the platform limiter runs before the router
 * publishes a matched route and has no template to match on, so the panel says so on the row
 * rather than leaving the operator to find out from a caller that was never refused.
 */
export interface ReliabilityPolicy {
  id: string | null;
  name: string;
  scope: string;
  target_id: string | null;
  route_pattern: string | null;
  limit_count: number;
  window_seconds: number;
  burst: number;
  /** `limit_count + burst`, computed by the API so a table never makes the reader add it up. */
  ceiling: number;
  priority: number;
  is_default: boolean;
  enabled: boolean;
  enforced_here: boolean;
}

/** The policy list: the rows, the scope vocabulary and this deployment's failure mode. */
export interface ReliabilityPolicies {
  policies: ReliabilityPolicy[];
  vocabulary: string[];
  limiter: string;
  /** Whether an unreadable counter fails open (availability) or closed (protection). */
  fail_mode: string;
}

/** A write. The API owns every range; the client sends numbers and shows what came back. */
export interface ReliabilityPolicyInput {
  name: string;
  scope: string;
  target_id?: string | null;
  route_pattern?: string | null;
  limit_count: number;
  window_seconds: number;
  burst?: number;
  priority?: number;
  enabled?: boolean;
}

/** The dry-run's input: the same four facts the middleware reads off a request. */
export interface ReliabilityEvaluateInput {
  scope?: string | null;
  user_id?: string | null;
  ip?: string | null;
  route?: string | null;
  /** The counter to assume, for explaining a refusal a caller is already seeing. */
  count?: number | null;
}

/**
 * The dry-run's verdict, as the API's `Verdict` enum actually serialises: an externally tagged
 * enum, so the variant is the KEY and its fields are the value.
 *
 * It is an enum rather than a flat answer because the four shapes are genuinely different and a
 * client-side "allowed + remaining" collapses the two that matter:
 *
 * - `Unlimited` — no policy applies. The request proceeds and there is **no budget number at
 *   all**; rendering it as "0 remaining" sends an operator hunting for a policy that does not
 *   exist.
 * - `Uncounted` — a policy applies, the counter was unreadable, the deployment fails **open**.
 *   The request proceeds and nothing was counted, so `remaining` is absent on purpose: a number
 *   here would be a measurement nobody took.
 * - `RefusedUncounted` — the same outage with the deployment failing **closed**. Refused, with
 *   no `retry_after`, because a wait the platform cannot compute is not a promise.
 * - `Allowed` / `Limited` — the two answers that carry a real measurement, and the only two the
 *   middleware is allowed to write `X-RateLimit-*` headers for.
 */
/**
 * The dry-run's verdict, as the API's `Verdict` actually serialises.
 *
 * It is `#[serde(tag = "decision", rename_all = "snake_case")]` — internally tagged, so every
 * variant carries a `decision` KEY with the variant name in snake_case, and the variant's own
 * fields sit alongside it. A client written against serde's *external* tagging (the variant as
 * the key) parses nothing here, and nothing about the response looks wrong: every field is
 * simply `undefined`.
 *
 * It is an enum rather than a flat answer because the five shapes are genuinely different and a
 * client-side "allowed + remaining" collapses the two that matter:
 *
 * - `unlimited` — no policy applies. The request proceeds and there is **no budget number at
 *   all**; rendering it as "0 remaining" sends an operator hunting for a policy that does not
 *   exist.
 * - `uncounted` — a policy applies, the counter was unreadable, the deployment fails **open**.
 *   The request proceeds and nothing was counted, so `remaining` is absent on purpose: a number
 *   here would be a measurement nobody took.
 * - `refused_uncounted` — the same outage failing **closed**. Refused, with no `retry_after`,
 *   because a wait the platform cannot compute is not a promise.
 * - `allowed` / `limited` — the two answers that carry a real measurement, and the only two the
 *   middleware is allowed to write `X-RateLimit-*` headers for.
 */
export interface ReliabilityVerdictBase {
  decision: "allowed" | "limited" | "unlimited" | "uncounted" | "refused_uncounted";
  policy_id: string | null;
  /** Which scope answered, or `null` when no policy matched at all. */
  scope: string | null;
  /** Requests left in the window. Present only on the two answers with a real measurement. */
  remaining?: number;
  limit?: number;
  ceiling?: number;
  retry_after?: number;
}

export type ReliabilityVerdict = ReliabilityVerdictBase;

/** The variant name, which is what the screen renders and what a test asserts on. */
export function reliabilityVerdictKind(verdict: ReliabilityVerdict): string {
  return verdict?.decision ?? "unknown";
}

export interface ReliabilityEvaluated {
  policy: ReliabilityPolicy | null;
  verdict: ReliabilityVerdict;
  counted: { count: number; authoritative: boolean };
  window_start: string | null;
  counter_key: string | null;
  fail_mode: string;
}

/** One refusal rollup: a scope, a route and one window. */
export interface ReliabilityRefusal {
  scope: string;
  target_id: string | null;
  route: string;
  window_start: string;
  refusals: number;
  last_refusal_at: string;
}

export interface ReliabilityRefusals {
  refusals: ReliabilityRefusal[];
  last_24_hours: number;
}

/**
 * The policies, in the order the resolver walks them.
 *
 * `no-store`: a cached table is a screen that says "600 per minute" while the platform enforces
 * something else, and the whole value of the screen is that the two agree.
 */
export function fetchReliabilityPolicies(): Promise<ReliabilityPolicies> {
  return request<ReliabilityPolicies>("/api/v1/reliability/rate-limits", { cache: "no-store" });
}

/** Create a policy. The body never carries an id — the route decides which row it writes. */
export function createReliabilityPolicy(
  input: ReliabilityPolicyInput,
): Promise<ReliabilityPolicy> {
  return request<ReliabilityPolicy>("/api/v1/reliability/rate-limits", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Edit a policy. A default row is edited in place; deleting one disables it instead. */
export function updateReliabilityPolicy(
  id: string,
  input: ReliabilityPolicyInput,
): Promise<ReliabilityPolicy> {
  return request<ReliabilityPolicy>(`/api/v1/reliability/rate-limits/${encodeURIComponent(id)}`, {
    method: "PATCH",
    body: JSON.stringify(input),
  });
}

/**
 * Remove a custom policy.
 *
 * A default row is **disabled, not removed** — the shipped budgets are the document that
 * protects a deployment nobody has configured yet, and a delete that dropped them would turn an
 * install with no configuration into one with no protection. The API reports which it did.
 */
export function deleteReliabilityPolicy(
  id: string,
): Promise<{ id: string; disabled: boolean; message: string }> {
  return request<{ id: string; disabled: boolean; message: string }>(
    `/api/v1/reliability/rate-limits/${encodeURIComponent(id)}`,
    { method: "DELETE" },
  );
}

/**
 * Dry-run one request through the limiter.
 *
 * A **server** call on purpose: the acceptance criterion is that the tool names the same policy
 * the middleware resolves, and only the server holds that resolver. A client-side reimplementation
 * agrees on the day it is written and drifts the first time somebody tunes a limit — which is
 * the day somebody is relying on it. It also does not spend the budget it measures.
 */
export function evaluateReliabilityLimit(
  input: ReliabilityEvaluateInput,
): Promise<ReliabilityEvaluated> {
  return request<ReliabilityEvaluated>("/api/v1/reliability/rate-limits/evaluate", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** The refusal rollups — one row per scope, route and window, never one per request. */
export function fetchReliabilityRefusals(limit = 50): Promise<ReliabilityRefusals> {
  return request<ReliabilityRefusals>(
    `/api/v1/reliability/rate-limits/refusals?limit=${encodeURIComponent(String(limit))}`,
    { cache: "no-store" },
  );
}
