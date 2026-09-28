/**
 * Typed client for the Omnion API.
 *
 * The browser always calls the API on the admin panel's own origin: `next.config.ts` forwards
 * `/api/*` to the API origin, so the HttpOnly session cookie is first-party everywhere.
 */
import type {
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
  /** `true` when the engine never runs it. */
  inert: boolean;
  /** The parameters a freshly dropped card starts with. */
  defaults: Record<string, unknown>;
}

/** The palette's whole registry. */
export interface GraphNodeTypes {
  /** Every node type, in rail order. */
  node_types: GraphNodeType[];
  /** The rail's groups, in draw order. */
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
