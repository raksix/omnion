/**
 * Typed client for the Omnion API.
 *
 * The browser always calls the API on the admin panel's own origin: `next.config.ts` forwards
 * `/api/*` to the API origin, so the HttpOnly session cookie is first-party everywhere.
 */
import type {
  CdnAdapterInfo,
  CdnCacheRule,
  CdnCacheRuleInput,
  CdnRulesResponse,
  CdnSettings,
  CdnSettingsInput,
  CreatedMediaShare,
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
  MediaRetentionRunList,
  MediaRetentionRunResult,
  MediaTrash,
  MediaVersionList,
  OnboardingStatus,
  Organization,
  OwnerSetupResult,
  Page,
  NotificationBulkResult,
  NotificationFilters,
  NotificationPage,
  NotificationRow,
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

  constructor(
    status: number,
    code: string,
    message: string,
    details: Record<string, unknown> | null = null,
  ) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
    this.details = details;
  }

  /** `true` when the session is missing or expired. */
  get isUnauthenticated(): boolean {
    return this.status === 401;
  }
}

type ErrorBody = {
  error?: {
    code?: string;
    message?: string;
    details?: Record<string, unknown>;
  };
};

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
    );
  }

  return payload as T;
}

/**
 * Download a CSV endpoint as a file.
 *
 * A separate function rather than a `format=csv` flag on the JSON caller, because the two cannot
 * share a body parser: the JSON path runs every response through `readJson`, which turns a CSV
 * into `null` and a `Blob` into a promise that never resolves. The error path *is* shared — a
 * refusal is JSON whatever the request asked for, so the caller still gets the API's own code and
 * message rather than a failed download.
 */
export async function downloadCsv(
  path: string,
  filename: string,
): Promise<{ filename: string; rows: number }> {
  let response: Response;
  try {
    response = await fetch(path, { credentials: "same-origin", headers: { accept: "text/csv" } });
  } catch {
    throw new ApiError(0, "network_error", "The Omnion API could not be reached.");
  }

  if (!response.ok) {
    const body = (await readJson(response)) as ErrorBody;
    throw new ApiError(
      response.status,
      body.error?.code ?? "unknown_error",
      body.error?.message ?? `The API answered with status ${response.status}.`,
      body.error?.details ?? null,
    );
  }

  const text = await response.text();
  const url = URL.createObjectURL(new Blob([text], { type: "text/csv;charset=utf-8" }));
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  anchor.click();
  // Revoked on the next tick rather than immediately: Firefox cancels an in-flight download whose
  // URL disappears in the same task, and the file then lands empty.
  setTimeout(() => URL.revokeObjectURL(url), 0);

  return {
    filename,
    rows: text.split("\n").filter((line) => line.trim() !== "").length - 1,
  };
}

/** One row of the Audit tab. */
export type OrganizationAuditEntry = {
  id: number;
  action: string;
  actor_type: string;
  actor_user_id: string | null;
  /** Display name of the actor, resolved server-side so the feed needs no second request. */
  actor_name: string | null;
  target_type: string | null;
  target_id: string | null;
  metadata: unknown;
  ip_address: string | null;
  created_at: string;
};

/** The Audit tab's payload: the rows, the filter's choices and the filtered count. */
export type OrganizationAuditPayload = {
  organization_id: string;
  entries: OrganizationAuditEntry[];
  actions: string[];
  total: number;
};

/** The audit filters the tab offers. Empty strings mean "no filter", not "matches nothing". */
export type OrganizationAuditFilters = {
  action?: string;
  actor?: string;
  since?: string;
};

/**
 * Read one organization's audit trail.
 *
 * The action list comes back with the rows on purpose: the filter then offers what this tenant
 * has actually done, and cannot drift from the platform as actions are added.
 */
export async function fetchOrganizationAudit(
  organizationId: string,
  filters: OrganizationAuditFilters = {},
): Promise<OrganizationAuditPayload> {
  const params = new URLSearchParams();
  if (filters.action) params.set("action", filters.action);
  if (filters.actor) params.set("actor", filters.actor);
  // The date input speaks `YYYY-MM-DD`; the API speaks RFC 3339. Converting here keeps the
  // server's parser strict — a half-understood date is a filter that quietly matches nothing.
  if (filters.since) params.set("since", `${filters.since}T00:00:00Z`);
  const query = params.toString();

  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/audit${query ? `?${query}` : ""}`,
  );
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

/** One tenant in full. */
export function fetchOrganization(organizationId: string): Promise<Organization> {
  return request<Organization>(`/api/v1/organizations/${encodeURIComponent(organizationId)}`);
}

/** Open a new tenant — `POST /api/v1/organizations` (platform accounts only). */
export function createOrganization(input: { name: string; slug: string }): Promise<Organization> {
  return request<Organization>("/api/v1/organizations", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Change a tenant (name, status) — `PATCH /api/v1/organizations/{id}`. */
export function updateOrganization(
  organizationId: string,
  changes: { name?: string; status?: string },
): Promise<Organization> {
  return request<Organization>(`/api/v1/organizations/${encodeURIComponent(organizationId)}`, {
    method: "PATCH",
    body: JSON.stringify(changes),
  });
}

/** Delete a tenant that owns no sites — `DELETE /api/v1/organizations/{id}`. */
export async function deleteOrganization(organizationId: string): Promise<void> {
  await request(`/api/v1/organizations/${encodeURIComponent(organizationId)}`, {
    method: "DELETE",
  });
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

// ---------------------------------------------------------------------------------------------
// Organization memberships, invitations and the switcher (REQ-005, slice 1)
// ---------------------------------------------------------------------------------------------

/** A role chip: which role, and what it is called. */
export type MemberRole = {
  id: string;
  key: string;
  name: string;
};

/** One row of the Members tab. */
export type OrganizationMember = {
  id: string;
  user_id: string;
  display_name: string;
  email: string;
  user_status: string;
  status: string;
  is_primary: boolean;
  joined_at: string | null;
  last_active_at: string | null;
  roles: MemberRole[];
};

/** One invitation row. */
export type OrganizationInvitation = {
  id: string;
  email: string;
  role_id: string | null;
  role_name: string | null;
  invited_by: string | null;
  invited_by_name: string | null;
  status: string;
  message: string;
  expires_at: string;
  accepted_by: string | null;
  accepted_at: string | null;
  created_at: string;
};

/** An invitation with the token that was created with it — returned exactly once. */
export type CreatedOrganizationInvitation = {
  invitation: OrganizationInvitation;
  token: string;
  accept_url: string;
};

/** One membership of the signed-in account. */
export type AccountOrganization = {
  organization_id: string;
  name: string;
  slug: string;
  organization_status: string;
  membership_status: string;
  is_primary: boolean;
  roles: MemberRole[];
};

/** The members of one organization. */
export async function fetchOrganizationMembers(
  organizationId: string,
): Promise<{ organization_id: string; members: OrganizationMember[] }> {
  return request(`/api/v1/organizations/${encodeURIComponent(organizationId)}/members`);
}

/** Add an account to an organization. */
export async function addOrganizationMember(
  organizationId: string,
  input: { user_id: string; status?: string; is_primary?: boolean },
): Promise<OrganizationMember> {
  return request(`/api/v1/organizations/${encodeURIComponent(organizationId)}/members`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Change a membership's status, or promote/demote it as the account's home. */
export async function updateOrganizationMember(
  organizationId: string,
  userId: string,
  input: { status?: string; is_primary?: boolean },
): Promise<OrganizationMember> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/members/${encodeURIComponent(userId)}`,
    { method: "PATCH", body: JSON.stringify(input) },
  );
}

/** Remove an account from an organization. */
export async function removeOrganizationMember(
  organizationId: string,
  userId: string,
): Promise<void> {
  await request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/members/${encodeURIComponent(userId)}`,
    { method: "DELETE" },
  );
}

// ---------------------------------------------------------------------------------------------
// The member drawer (REQ-005, slice 4)
// ---------------------------------------------------------------------------------------------

/** One role binding as the drawer shows it, with the role's key and name resolved. */
export type MemberBinding = {
  id: string;
  role_id: string;
  role_key: string;
  role_name: string;
  subject_type: string;
  subject_id: string;
  scope_type: string;
  organization_id: string | null;
  site_id: string | null;
  department: string | null;
  module: string | null;
  resource_id: string | null;
  expires_at: string | null;
  revoked_at: string | null;
  /** Whether it still applies right now. */
  active: boolean;
  /** Whether a temporary window has run out. */
  expired: boolean;
  created_at: string;
};

/** One row of the drawer's trail. */
export type MemberAuditRow = {
  action: string;
  actor_user_id: string | null;
  /** The account's name, or `system` for a row nobody performed. */
  actor_name: string;
  target_type: string | null;
  target_id: string | null;
  created_at: string;
};

/**
 * Everything the member drawer renders, in one response.
 *
 * One request rather than four: a person looking at a colleague must never see a half-filled
 * panel, and an empty binding list that arrived while the identity succeeded is
 * indistinguishable from "this person holds nothing".
 */
export type OrganizationMemberDetail = {
  organization_id: string;
  membership_id: string;
  user_id: string;
  display_name: string;
  email: string;
  user_status: string;
  status: string;
  is_primary: boolean;
  joined_at: string | null;
  last_active_at: string | null;
  bindings: MemberBinding[];
  departments: MemberDepartment[];
  recent_audit: MemberAuditRow[];
};

/** `GET /api/v1/organizations/{id}/members/{user_id}` — the member drawer. */
export async function fetchOrganizationMember(
  organizationId: string,
  userId: string,
): Promise<OrganizationMemberDetail> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/members/${encodeURIComponent(userId)}`,
  );
}

/**
 * Grant a role to a member of this organization.
 *
 * The scope defaults to `organization` and the server refuses `global`: inside a tenant,
 * "this person may do this here" is the shape that cannot escape, and a tenant administrator
 * asking for a platform grant is asking for something the platform owns.
 */
export async function grantMemberRole(
  organizationId: string,
  userId: string,
  input: {
    role_id: string;
    scope_type?: "organization" | "site" | "department";
    site_id?: string;
    department?: string;
    expires_at?: string;
  },
): Promise<MemberBinding> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/members/${encodeURIComponent(userId)}/role-bindings`,
    { method: "POST", body: JSON.stringify(input) },
  );
}

/**
 * Move a temporary grant's expiry.
 *
 * The server patches the same row and refuses a date in the past, so "extend" cannot leave two
 * live bindings for one role — which the effective-permissions screen would render as the same
 * role twice with two different windows, neither of them the truth.
 */
export async function extendMemberRole(
  organizationId: string,
  userId: string,
  bindingId: string,
  expiresAt: string,
): Promise<MemberBinding> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/members/${encodeURIComponent(userId)}/role-bindings/${encodeURIComponent(bindingId)}`,
    { method: "PATCH", body: JSON.stringify({ expires_at: expiresAt }) },
  );
}

/** Revoke a grant. The row stays in the trail; `revoked_at` is what changes. */
export async function revokeMemberRole(
  organizationId: string,
  userId: string,
  bindingId: string,
): Promise<MemberBinding> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/members/${encodeURIComponent(userId)}/role-bindings/${encodeURIComponent(bindingId)}`,
    { method: "DELETE" },
  );
}

/** The invitations of one organization. */
export async function fetchOrganizationInvitations(
  organizationId: string,
): Promise<{ organization_id: string; invitations: OrganizationInvitation[] }> {
  return request(`/api/v1/organizations/${encodeURIComponent(organizationId)}/invitations`);
}

/** Invite an address into an organization. */
export async function createOrganizationInvitation(
  organizationId: string,
  input: { email: string; role_id?: string | null; message?: string },
): Promise<CreatedOrganizationInvitation> {
  return request(`/api/v1/organizations/${encodeURIComponent(organizationId)}/invitations`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/**
 * The invitations waiting for an owner, oldest first.
 *
 * The `owner_approval` queue is a work list, so the panel asks for it directly instead of
 * filtering the whole invitation history on the client — a tenant that has invited two hundred
 * people would otherwise download two hundred rows to render the three that need a decision.
 */
export async function fetchQueuedOrganizationInvitations(
  organizationId: string,
): Promise<{ organization_id: string; invitations: OrganizationInvitation[] }> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/invitations/queue`,
  );
}

/**
 * Release a queued invitation and receive its single-use link — once.
 *
 * The link is minted by the release, not recovered from the create: a queued invitation never
 * had a working link, so this is the only moment one exists and it is never readable again.
 */
export async function releaseQueuedOrganizationInvitation(
  organizationId: string,
  invitationId: string,
): Promise<CreatedOrganizationInvitation> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/invitations/${encodeURIComponent(
      invitationId,
    )}/release`,
    { method: "POST" },
  );
}

/** Revoke a pending invitation. */
export async function revokeOrganizationInvitation(
  organizationId: string,
  invitationId: string,
): Promise<void> {
  await request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/invitations/${encodeURIComponent(invitationId)}`,
    { method: "DELETE" },
  );
}

// ---------------------------------------------------------------------------------------------
// Departments (REQ-005, slice 2)
// ---------------------------------------------------------------------------------------------

/** One row of the Departments tab. */
export type OrganizationDepartment = {
  id: string;
  /** Stable address inside the organization — what a role binding stores. */
  key: string;
  name: string;
  description: string;
  status: string;
  parent_id: string | null;
  parent_key: string | null;
  member_count: number;
  role_count: number;
  /** Depth in the tree, 0 for a root. */
  depth: number;
  created_at: string;
  updated_at: string;
};

/** A role bound to a department as a whole. */
export type DepartmentRole = {
  binding_id: string;
  role_id: string;
  role_key: string;
  role_name: string;
  granted_by: string | null;
  expires_at: string | null;
};

/** One account inside a department. */
export type DepartmentMember = {
  user_id: string;
  display_name: string;
  email: string;
  user_status: string;
  membership_status: string;
  last_active_at: string | null;
};

/** One department in full. */
export type DepartmentDetail = {
  department: OrganizationDepartment;
  members: DepartmentMember[];
  roles: DepartmentRole[];
};

/** One department of one member — what the member drawer reads. */
export type MemberDepartment = {
  id: string;
  key: string;
  name: string;
  status: string;
};

/** Filters of the Departments tab. */
export type DepartmentFilters = {
  status?: string;
  q?: string;
};

// ---------------------------------------------------------------------------------------------
// Organization settings, modules, limits and usage (REQ-005, slice 3)
// ---------------------------------------------------------------------------------------------

/** One row of the Settings tab. */
export type OrganizationSettings = {
  locale: string;
  timezone: string;
  invite_policy: string;
  default_invite_role_id: string | null;
  logo_media_id: string | null;
  accent_color: string | null;
  audit_retention_days: number;
  updated_at: string;
};

/** The Settings payload, with the choices the form offers. */
export type OrganizationSettingsPayload = {
  organization_id: string;
  settings: OrganizationSettings;
  available_locales: string[];
  invite_policies: { key: string; description: string }[];
};

/** One row of the Modules tab. */
export type OrganizationModule = {
  key: string;
  name: string;
  description: string;
  enabled: boolean;
  /** Whether the organization made an explicit decision about this module. */
  explicit: boolean;
};

/** The Modules payload. */
export type OrganizationModulesPayload = {
  organization_id: string;
  modules: OrganizationModule[];
};

/** The plan and its ceilings; a `null` limit is unlimited. */
export type OrganizationLimits = {
  plan: string;
  seat_limit: number | null;
  site_limit: number | null;
  storage_bytes_limit: number | null;
  ai_monthly_limit_micros: number | null;
  updated_at: string;
};

/** The Limits payload, with the plans the selector offers. */
export type OrganizationLimitsPayload = {
  organization_id: string;
  limits: OrganizationLimits;
  available_plans: { key: string; description: string }[];
};

/** What the organization holds, next to the ceilings it is measured against. */
export type OrganizationUsage = {
  organization_id: string;
  seats_used: number;
  sites_used: number;
  storage_used_bytes: number;
  ai_micros_this_month: number;
  limits: OrganizationLimits;
  /** Per-metric label naming which limit the bar reads. */
  sources: {
    seats: string;
    sites: string;
    storage_bytes: string;
    ai_monthly_micros: string;
  };
};

/** The organization settings and the choices the form may offer. */
export function fetchOrganizationSettings(
  organizationId: string,
): Promise<OrganizationSettingsPayload> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/settings`,
  );
}

/** Save the whole settings row. */
export function updateOrganizationSettings(
  organizationId: string,
  input: {
    locale: string;
    timezone: string;
    invite_policy: string;
    default_invite_role_id?: string | null;
    logo_media_id?: string | null;
    accent_color?: string | null;
    audit_retention_days: number;
  },
): Promise<OrganizationSettingsPayload> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/settings`,
    { method: "PUT", body: JSON.stringify(input) },
  );
}

/** The installed modules with this organization's decision about each. */
export function fetchOrganizationModules(
  organizationId: string,
): Promise<OrganizationModulesPayload> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/modules`,
  );
}

/**
 * Switch a set of modules.
 *
 * The whole batch is sent at once on purpose: the API validates every key before writing any
 * of them, so a batch naming one module this installation does not ship leaves the others
 * untouched — which a row of independent `PUT`s could not promise.
 */
export function updateOrganizationModules(
  organizationId: string,
  modules: { module_key: string; enabled: boolean }[],
): Promise<OrganizationModulesPayload> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/modules`,
    { method: "PUT", body: JSON.stringify({ modules }) },
  );
}

/** The plan and its ceilings. */
export function fetchOrganizationLimits(
  organizationId: string,
): Promise<OrganizationLimitsPayload> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/limits`,
  );
}

/** Save the plan and every ceiling. */
export function updateOrganizationLimits(
  organizationId: string,
  input: {
    plan: string;
    seat_limit: number | null;
    site_limit: number | null;
    storage_bytes_limit: number | null;
    ai_monthly_limit_micros: number | null;
  },
): Promise<OrganizationLimitsPayload> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/limits`,
    { method: "PUT", body: JSON.stringify(input) },
  );
}

/** What the organization holds, next to its ceilings. */
export function fetchOrganizationUsage(organizationId: string): Promise<OrganizationUsage> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/usage`,
  );
}

/**
 * The same numbers as a spreadsheet, from the same call the screen renders.
 *
 * The file is fetched rather than built here: a CSV assembled in the browser from an older
 * payload would be a second reading of the data, and the Billing tab's whole claim is that the
 * two agree.
 */
export async function downloadOrganizationUsage(organizationId: string): Promise<Blob> {
  const response = await fetch(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/usage?format=csv`,
    { credentials: "include" },
  );
  if (!response.ok) {
    throw new ApiError(response.status, "usage_export_failed", "The usage file could not be downloaded.");
  }
  return response.blob();
}

/** The department tree, parents before children. */
export async function fetchOrganizationDepartments(
  organizationId: string,
  filters: DepartmentFilters = {},
): Promise<{ organization_id: string; departments: OrganizationDepartment[] }> {
  const params = new URLSearchParams();
  if (filters.status) params.set("status", filters.status);
  if (filters.q) params.set("q", filters.q);
  const query = params.toString();
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/departments${query ? `?${query}` : ""}`,
  );
}

/** One department with its members and the roles bound to it. */
export async function fetchDepartment(
  organizationId: string,
  departmentId: string,
): Promise<DepartmentDetail> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/departments/${encodeURIComponent(departmentId)}`,
  );
}

/** Create a department. The key is normalized and never changes afterwards. */
export async function createOrganizationDepartment(
  organizationId: string,
  input: { key: string; name: string; description?: string; parent_id?: string | null },
): Promise<OrganizationDepartment> {
  return request(`/api/v1/organizations/${encodeURIComponent(organizationId)}/departments`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Rename, re-describe, re-parent or archive a department. */
export async function updateOrganizationDepartment(
  organizationId: string,
  departmentId: string,
  change: {
    name?: string;
    description?: string;
    parent_id?: string | null;
    status?: string;
  },
): Promise<OrganizationDepartment> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/departments/${encodeURIComponent(departmentId)}`,
    { method: "PATCH", body: JSON.stringify(change) },
  );
}

/** Archive a department: it keeps its structure but stops granting. */
export async function archiveOrganizationDepartment(
  organizationId: string,
  departmentId: string,
): Promise<OrganizationDepartment> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/departments/${encodeURIComponent(departmentId)}`,
    { method: "POST" },
  );
}

/** Delete a department; refused while it still holds people, roles or children. */
export async function deleteOrganizationDepartment(
  organizationId: string,
  departmentId: string,
): Promise<void> {
  await request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/departments/${encodeURIComponent(departmentId)}`,
    { method: "DELETE" },
  );
}

/** Put an account into a department. */
export async function addDepartmentMember(
  organizationId: string,
  departmentId: string,
  userId: string,
): Promise<void> {
  await request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/departments/${encodeURIComponent(departmentId)}/members`,
    { method: "POST", body: JSON.stringify({ user_id: userId }) },
  );
}

/** Take an account out of a department. */
export async function removeDepartmentMember(
  organizationId: string,
  departmentId: string,
  userId: string,
): Promise<void> {
  await request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/departments/${encodeURIComponent(departmentId)}/members/${encodeURIComponent(userId)}`,
    { method: "DELETE" },
  );
}

/** The departments one account sits in. */
export async function fetchMemberDepartments(
  organizationId: string,
  userId: string,
): Promise<MemberDepartment[]> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/members/${encodeURIComponent(userId)}/departments`,
  );
}

/** Bind a role to a department as a whole, optionally until a date. */
export async function bindDepartmentRole(
  organizationId: string,
  departmentId: string,
  input: { role_id: string; expires_at?: string | null },
): Promise<DepartmentRole> {
  return request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/departments/${encodeURIComponent(departmentId)}/roles`,
    { method: "POST", body: JSON.stringify(input) },
  );
}

/** Revoke a role bound to a department. */
export async function unbindDepartmentRole(
  organizationId: string,
  departmentId: string,
  bindingId: string,
): Promise<void> {
  await request(
    `/api/v1/organizations/${encodeURIComponent(organizationId)}/departments/${encodeURIComponent(departmentId)}/roles/${encodeURIComponent(bindingId)}`,
    { method: "DELETE" },
  );
}

/** The public preview of an invitation link. */
export async function fetchInvitationPreview(token: string): Promise<{
  organization_name: string | null;
  organization_slug: string | null;
  invited_by_name: string | null;
  role_name: string | null;
  email_masked: string | null;
  expires_at: string | null;
  usable: boolean;
  /**
   * Why the token cannot be used — one coarse value on purpose. `unusable` covers queued,
   * revoked, expired and never-issued alike, so a public link cannot be walked to find out which
   * organizations exist. The panel says "ask for a new one" rather than guessing.
   */
  reason: string | null;
}> {
  return request(`/api/v1/invitations/${encodeURIComponent(token)}`);
}

/** Accept an invitation — signed in, or with a new account in the same request. */
export async function acceptInvitation(
  token: string,
  input: { display_name?: string; password?: string },
): Promise<{ organization_id: string; organization_name: string; user_id: string }> {
  return request(`/api/v1/invitations/${encodeURIComponent(token)}/accept`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** The caller's own organizations — the switcher's list. */
export async function fetchMyOrganizations(): Promise<{
  current_organization_id: string | null;
  organizations: AccountOrganization[];
  /** Module keys the current tenant has switched off (REQ-005, slice 4). */
  disabled_modules: string[];
}> {
  return request("/api/v1/me/organizations");
}

/** Switch the session's organization. */
export async function switchOrganization(
  organizationId: string,
): Promise<{ organization_id: string; name: string }> {
  return request("/api/v1/me/organization", {
    method: "POST",
    body: JSON.stringify({ organization_id: organizationId }),
  });
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
 */
function notificationQuery(filters: NotificationFilters = {}): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(filters)) {
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

// ---------------------------------------------------------------------------------------------
// CDN / edge (REQ-011)
// ---------------------------------------------------------------------------------------------

/**
 * A site's cache rules, in precedence order.
 *
 * `unreadable` is part of the response rather than an error: one rule whose pattern no
 * longer compiles must not take the whole list away, and it must not be dropped either —
 * a rule that silently stopped matching is what an operator discovers days later.
 */
export async function fetchCdnRules(siteId: string): Promise<CdnRulesResponse> {
  return request(`/api/v1/cdn/rules?site_id=${encodeURIComponent(siteId)}`);
}

/** Create a cache rule. */
export async function createCdnRule(input: CdnCacheRuleInput): Promise<CdnCacheRule> {
  return request("/api/v1/cdn/rules", { method: "POST", body: JSON.stringify(input) });
}

/** Change one rule. The site is named in the body, not in the path. */
export async function updateCdnRule(
  ruleId: string,
  input: CdnCacheRuleInput,
): Promise<CdnCacheRule> {
  return request(`/api/v1/cdn/rules/${encodeURIComponent(ruleId)}`, {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

/** Delete a rule. */
export async function deleteCdnRule(ruleId: string, siteId: string): Promise<void> {
  await request(
    `/api/v1/cdn/rules/${encodeURIComponent(ruleId)}?site_id=${encodeURIComponent(siteId)}`,
    { method: "DELETE" },
  );
}

/** Turn a rule on or off without touching the rest of it. */
export async function setCdnRuleEnabled(
  ruleId: string,
  siteId: string,
  enabled: boolean,
): Promise<CdnCacheRule> {
  return request(`/api/v1/cdn/rules/${encodeURIComponent(ruleId)}/toggle`, {
    method: "POST",
    body: JSON.stringify({ site_id: siteId, enabled }),
  });
}

/**
 * Persist a new precedence order.
 *
 * The order is sent as the complete list rather than as a delta: a reorder that renumbers
 * one row is the operation that can leave two rules claiming the same priority, and the
 * matcher would then pick between them by row order — which is not what the drag showed.
 */
export async function reorderCdnRules(siteId: string, order: string[]): Promise<CdnRulesResponse> {
  return request("/api/v1/cdn/rules/reorder", {
    method: "POST",
    body: JSON.stringify({ site_id: siteId, order }),
  });
}

/** The settings row for a site, or the installation default when `siteId` is null. */
export async function fetchCdnSettings(siteId: string | null): Promise<CdnSettings> {
  const query = siteId === null ? "" : `?site_id=${encodeURIComponent(siteId)}`;
  return request(`/api/v1/cdn/settings${query}`);
}

/**
 * Save the settings row.
 *
 * `credential` is only sent when the operator typed one: sending the empty string would
 * clear a stored credential, because "the field is empty" and "the operator cleared it" are
 * the same request. The panel therefore omits the field entirely when it is untouched.
 */
export async function saveCdnSettings(input: CdnSettingsInput): Promise<CdnSettings> {
  return request("/api/v1/cdn/settings", { method: "PUT", body: JSON.stringify(input) });
}

/** The shipped provider adapters, with the fields each one needs. */
export async function fetchCdnAdapters(): Promise<CdnAdapterInfo[]> {
  const body = await request<{ adapters: CdnAdapterInfo[] }>("/api/v1/cdn/adapters");
  return body.adapters;
}
