/**
 * Typed client for the Omnion API.
 *
 * The browser always calls the API on the admin panel's own origin: `next.config.ts` forwards
 * `/api/*` to the API origin, so the HttpOnly session cookie is first-party everywhere.
 */
import type {
  BlockRegistry,
  BlockValidationResult,
  ContentBlock,
  ContentPattern,

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

  CommentBan,
  CommentBulkResult,
  CommentInbox,
  CommentInboxRow,
  CommentSettingsDocument,
  CreatedMediaShare,
  ImportReport,
  Member,
  MemberDelivery,
  MemberDetail,
  MemberList,
  MemberSettingsDocument,
  NewsletterIssue,
  NewsletterList,
  NewsletterListRow,
  NewsletterSubscriber,
  NewNewsletterList,
  SubscriberPage,
  Form,
  NewCommentReply,
  FeaturedCandidate,
  FeaturedCandidatesBody,
  FeaturedChanges,
  FeaturedImage,
  FeaturedMedia,
  FeaturedMediaBody,
  PageSeo,
  PageSeoBody,
  SeoBrokenLink,
  SeoOverview,
  SeoRedirect,
  SeoRedirectTest,
  SeoSettings,
  FormDetail,
  Inbox,
  Submission,
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
  MediaReplaceResult,
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
  Menu,
  MenuDetail,
  MenuItem,
  PagePreview,
  PublishingEntry,
  RenderedMenu,
  RenderedMenuItem,
  PageTemplateSummary,
  PatternBlocksResponse,
  PatternListResponse,
  Revision,
  RevisionDiff,
  Site,
  TemplateListResponse,
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

/** One page with its working draft and the revision visitors see. */
export function fetchPage(pageId: string): Promise<Page> {
  return request<Page>(`/api/v1/pages/${encodeURIComponent(pageId)}`);
}

// ---------------------------------------------------------------------------------------------
// Featured media (REQ-064, slice 4d)
// ---------------------------------------------------------------------------------------------

/**
 * Read one page's featured image, its crop and what a renderer would do with them.
 *
 * The response carries the renderer's own payload (`render`) rather than the fields alone, so
 * this screen's preview and the public page's markup are the same object rather than two
 * re-derivations that can agree with each other and disagree with the site.
 */
export function fetchFeaturedMedia(pageId: string): Promise<FeaturedMediaBody> {
  return request<FeaturedMediaBody>(
    `/api/v1/pages/${encodeURIComponent(pageId)}/featured-media`,
  );
}

/**
 * Write one page's featured image.
 *
 * `focal_x`/`focal_y` are omitted rather than sent when the editor did not touch the crop: the
 * server reads a MISSING key as "leave it" and an explicit `null` as "clear it", and sending both
 * as `null` on every save would clear the crop of every page the panel ever touched.
 */
export function saveFeaturedMedia(
  pageId: string,
  changes: FeaturedChanges,
): Promise<FeaturedMediaBody> {
  const body: FeaturedChanges = {};
  if (changes.media_id !== undefined) {
    body.media_id = changes.media_id;
  }
  if (changes.alt !== undefined) {
    body.alt = changes.alt;
  }
  if (changes.legend !== undefined) {
    body.legend = changes.legend;
  }
  if (changes.focal_x !== undefined) {
    body.focal_x = changes.focal_x;
  }
  if (changes.focal_y !== undefined) {
    body.focal_y = changes.focal_y;
  }
  if (changes.clear !== undefined) {
    body.clear = changes.clear;
  }
  return request<FeaturedMediaBody>(
    `/api/v1/pages/${encodeURIComponent(pageId)}/featured-media`,
    { method: "PUT", body: JSON.stringify(body) },
  );
}

/**
 * The images a page could feature.
 *
 * A separate read from the page's own because it is a different question with a different power:
 * this one is about the LIBRARY and needs `media.read`, while the page's own read is about the
 * page. `limit` is clamped server-side, so the picker pages rather than asking for everything.
 */
export function fetchFeaturedCandidates(
  siteId: string,
  limit?: number,
): Promise<FeaturedCandidatesBody> {
  const query = new URLSearchParams();
  if (limit !== undefined) {
    query.set("limit", String(limit));
  }
  const suffix = query.size > 0 ? `?${query.toString()}` : "";
  return request<FeaturedCandidatesBody>(
    `/api/v1/sites/${encodeURIComponent(siteId)}/featured-media/candidates${suffix}`,
  );
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
  changes: {
    slug?: string;
    title?: string;
    body?: string;
    summary?: string;
    blocks?: unknown[];
  },
): Promise<Page> {
  const body: Record<string, unknown> = {};
  if (changes.slug !== undefined) body.slug = changes.slug;
  if (changes.title !== undefined) body.title = changes.title;
  if (changes.body !== undefined) body.body = changes.body;
  if (changes.summary !== undefined) body.summary = changes.summary;
  if (changes.blocks !== undefined) body.blocks = changes.blocks;
  return request<Page>(`/api/v1/pages/${encodeURIComponent(pageId)}`, {
    method: "PATCH",
    body: JSON.stringify(body),
  });
}

// -- Patterns and page templates (REQ-063, slice 3) --------------------------------------------------

/**
 * The pattern library, optionally narrowed to one category and one tenant.
 *
 * The tenant is a parameter rather than something the server infers because the platform Owner
 * has *no* primary organization — that is what makes it an Owner — so a route that only falls back
 * to the account's own tenant answers the Owner `400 organization_required` on the one screen it
 * opens first. Every tenant-addressed read in this client takes it the same way (`fetchSites`).
 */
export async function fetchPatterns(category?: string, organizationId?: string): Promise<ContentPattern[]> {
  const query = new URLSearchParams();
  if (category) query.set("category", category);
  if (organizationId) query.set("organization_id", organizationId);
  const suffix = query.size ? `?${query.toString()}` : "";
  const body = await request<PatternListResponse>(`/api/v1/patterns${suffix}`);
  return body.patterns;
}

/** One pattern, with its block group. */
export function fetchPattern(patternId: string): Promise<ContentPattern> {
  return request<ContentPattern>(`/api/v1/patterns/${encodeURIComponent(patternId)}`);
}

/**
 * The blocks a pattern contributes to a page, with ids minted server-side.
 *
 * Deliberately a server call rather than a client-side rename of the pattern's own payload: the
 * ids that matter are the ones the page will *store*, and a browser that re-mints them has a
 * second implementation of "copy this pattern" which will disagree with the server's the first
 * time either one learns a rule the other does not have.
 */
export function fetchPatternBlocks(
  patternId: string,
  organizationId?: string,
): Promise<PatternBlocksResponse> {
  // A path-addressed read has no body to name a tenant in, so it rides the query string. Without
  // it the route answers `400 organization_required` to the platform account that owns the
  // pattern — the fifth call site to forget this, and the reason the editor's "insert pattern"
  // could not read a pattern the same screen had just saved.
  const query = organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
  return request<PatternBlocksResponse>(
    `/api/v1/patterns/${encodeURIComponent(patternId)}/blocks${query}`,
  );
}

/**
 * Save a pattern. The key is its identity, so a `POST` with an existing key replaces that
 * pattern — which is why the library's "save my changes" and "create a new one" are one call.
 */
export function savePattern(input: {
  key: string;
  name: string;
  category?: string;
  description?: string;
  blocks: ContentBlock[];
  organizationId?: string;
}): Promise<ContentPattern> {
  const body: Record<string, unknown> = {
    key: input.key,
    name: input.name,
    blocks: input.blocks,
  };
  if (input.category !== undefined) body.category = input.category;
  if (input.description !== undefined) body.description = input.description;
  if (input.organizationId !== undefined) body.organization_id = input.organizationId;
  return request<ContentPattern>("/api/v1/patterns", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** Edit a pattern's name, category, description or block group. */
export function updatePattern(
  patternId: string,
  changes: {
    name?: string;
    category?: string;
    description?: string;
    blocks?: ContentBlock[];
  },
): Promise<ContentPattern> {
  const body: Record<string, unknown> = {};
  if (changes.name !== undefined) body.name = changes.name;
  if (changes.category !== undefined) body.category = changes.category;
  if (changes.description !== undefined) body.description = changes.description;
  if (changes.blocks !== undefined) body.blocks = changes.blocks;
  return request<ContentPattern>(`/api/v1/patterns/${encodeURIComponent(patternId)}`, {
    method: "PUT",
    body: JSON.stringify(body),
  });
}

/** Remove a pattern from the library. */
export function deletePattern(patternId: string): Promise<void> {
  return request<void>(`/api/v1/patterns/${encodeURIComponent(patternId)}`, {
    method: "DELETE",
  });
}

/** The page template gallery, with the platform's own starting points. */
export async function fetchPageTemplates(organizationId?: string): Promise<PageTemplateSummary[]> {
  const query = organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
  const body = await request<TemplateListResponse>(`/api/v1/page-templates${query}`);
  return body.templates;
}

/** Create a page from a template: a real draft page whose blocks match the template's. */
export function createPageFromTemplate(input: {
  siteId: string;
  templateId: string;
  slug: string;
  title: string;
}): Promise<Page> {
  return request<Page>("/api/v1/pages/from-template", {
    method: "POST",
    body: JSON.stringify({
      site_id: input.siteId,
      template_id: input.templateId,
      slug: input.slug,
      title: input.title,
    }),
  });
}

// -- The block system (REQ-063) ---------------------------------------------------------------------

/** The block registry: every type the platform ships, with its props schema. */
export function fetchBlockRegistry(): Promise<BlockRegistry> {
  return request<BlockRegistry>("/api/v1/blocks");
}

/** Validate a block tree without writing it — the editor's live validation. */
export function validateBlocks(blocks: unknown[]): Promise<BlockValidationResult> {
  return request<BlockValidationResult>("/api/v1/blocks/validate", {
    method: "POST",
    body: JSON.stringify({ blocks }),
  });
}

/**
 * Compare two revisions block by block (REQ-063).
 *
 * `against` is optional: the API defaults to the revision before the one being read, which is
 * the question an author opening a revision actually has. It is passed only when the revisions
 * screen's picker names a different base.
 */
export function fetchRevisionDiff(
  pageId: string,
  revisionId: string,
  against?: string,
): Promise<RevisionDiff> {
  const query = against
    ? `?against=${encodeURIComponent(against)}`
    : "";
  return request<RevisionDiff>(
    `/api/v1/pages/${encodeURIComponent(pageId)}/revisions/${encodeURIComponent(revisionId)}/diff${query}`,
  );
}

/**
 * The renderer-frame payload of a page's working draft (REQ-063, slice 2).
 *
 * `viewport` picks the screen: the server filters the tree for it, so a phone frame does not
 * carry a desktop block in the DOM wearing a `display: none`. The screen switch has to be a
 * round trip on purpose — the filter lives in the same place the public renderer's filter lives,
 * and a client-side copy is exactly how a preview starts disagreeing with the site.
 */
export function fetchPagePreview(
  pageId: string,
  viewport: "desktop" | "mobile" = "desktop",
): Promise<PagePreview> {
  const query = viewport === "mobile" ? "?viewport=mobile" : "";
  return request<PagePreview>(
    `/api/v1/pages/${encodeURIComponent(pageId)}/preview${query}`,
  );
}

/** The revision history of a page, newest first. */
export function fetchRevisions(pageId: string): Promise<Revision[]> {
  return request<{ revisions: Revision[] }>(
    `/api/v1/pages/${encodeURIComponent(pageId)}/revisions`,
  ).then((body) => body.revisions);
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
// Forms and the submission inbox (REQ-064, slice 2)
// ---------------------------------------------------------------------------------------------

/** The forms of one site. */
export function fetchForms(siteId: string): Promise<Form[]> {
  return request<Form[]>(`/api/v1/forms?site_id=${encodeURIComponent(siteId)}`);
}

/**
 * One form with its fields and the vocabulary the builder draws from.
 *
 * The vocabulary travels with the document rather than being hard-coded in the panel, for the
 * same reason the menu's does: a palette that offers a field type the server then refuses is a
 * palette whose rejection arrives as an unexplained 400.
 */
export function fetchForm(formId: string): Promise<FormDetail> {
  return request<FormDetail>(`/api/v1/forms/${encodeURIComponent(formId)}`);
}

/** A field as the builder submits it. */
export type FormFieldInput = {
  key: string;
  label: string;
  field_type: string;
  required: boolean;
  placeholder?: string | null;
  help_text?: string | null;
  width: string;
  rules: Record<string, unknown>;
  options: unknown;
};

export function createForm(input: {
  site_id: string;
  key: string;
  name: string;
  fields: FormFieldInput[];
}): Promise<FormDetail> {
  return request<FormDetail>("/api/v1/forms", { method: "POST", body: JSON.stringify(input) });
}

/**
 * Save a form's settings.
 *
 * The server validates the *pair*: a message action with no message and a redirect action with no
 * URL are both refused. So the panel sends both fields on every save rather than pretending the
 * two are independent, and the store decides which one the form actually uses.
 */
export function updateForm(
  formId: string,
  input: {
    name?: string;
    key?: string;
    submit_action?: string;
    submit_message?: string | null;
    redirect_url?: string | null;
    notify_emails?: string[];
    notify_subject?: string | null;
    honeypot?: boolean;
    min_fill_seconds?: number;
    rate_limit_per_hour?: number;
    retention_days?: number;
  },
): Promise<FormDetail> {
  return request<FormDetail>(`/api/v1/forms/${encodeURIComponent(formId)}`, {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

/** Replace a form's whole canvas — the builder's Save. */
export function saveFormFields(formId: string, fields: FormFieldInput[]): Promise<FormDetail> {
  return request<FormDetail>(`/api/v1/forms/${encodeURIComponent(formId)}/fields`, {
    method: "PUT",
    body: JSON.stringify({ fields }),
  });
}

/** Publish or unpublish a form. */
export function setFormStatus(formId: string, status: string): Promise<FormDetail> {
  return request<FormDetail>(`/api/v1/forms/${encodeURIComponent(formId)}/publish`, {
    method: "POST",
    body: JSON.stringify({ status }),
  });
}

export function deleteForm(formId: string): Promise<void> {
  return request<void>(`/api/v1/forms/${encodeURIComponent(formId)}`, { method: "DELETE" });
}

/** The inbox filters, as the screen and the export both send them. */
export type SubmissionFilters = {
  status?: string;
  search?: string;
  since?: string;
  until?: string;
  limit?: number;
  offset?: number;
};

/** The query string of a filter set, shared by the list and the export. */
function submissionQuery(filters: SubmissionFilters): string {
  const parts = Object.entries(filters)
    .filter(([, value]) => value !== undefined && value !== "" && value !== null)
    .map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(String(value))}`);
  return parts.length === 0 ? "" : `?${parts.join("&")}`;
}

/** The inbox, under exactly the filters the screen shows. */
export function fetchSubmissions(formId: string, filters: SubmissionFilters = {}): Promise<Inbox> {
  return request<Inbox>(
    `/api/v1/forms/${encodeURIComponent(formId)}/submissions${submissionQuery(filters)}`,
  );
}

/** Change one submission's inbox state. */
export function setSubmissionStatus(
  formId: string,
  submissionId: string,
  status: string,
): Promise<Submission> {
  return request<Submission>(
    `/api/v1/forms/${encodeURIComponent(formId)}/submissions/${encodeURIComponent(submissionId)}`,
    { method: "PATCH", body: JSON.stringify({ status }) },
  );
}

/** Move several submissions at once — the inbox's bulk bar. */
export function bulkSubmissionStatus(
  formId: string,
  ids: string[],
  status: string,
): Promise<Inbox> {
  return request<Inbox>(`/api/v1/forms/${encodeURIComponent(formId)}/submissions`, {
    method: "PATCH",
    body: JSON.stringify({ ids, status }),
  });
}

export function deleteSubmission(formId: string, submissionId: string): Promise<void> {
  return request<void>(
    `/api/v1/forms/${encodeURIComponent(formId)}/submissions/${encodeURIComponent(submissionId)}`,
    { method: "DELETE" },
  );
}

/**
 * Download the filtered inbox as CSV.
 *
 * A plain fetch and a blob, not the JSON client: the endpoint answers `text/csv`, and routing it
 * through `request` would hand the owner a JSON parse error instead of a file. The filters are
 * the ones the list sends, and that is the whole contract of the button — a download that ignored
 * them would be a way to export the unfiltered inbox from a screen that says "Export 12".
 */
export async function exportSubmissionsCsv(
  formId: string,
  filters: SubmissionFilters = {},
): Promise<void> {
  const response = await fetch(
    `/api/v1/forms/${encodeURIComponent(formId)}/submissions/export${submissionQuery(filters)}`,
    { credentials: "include" },
  );
  if (!response.ok) {
    // The CSV route answers the platform's own error envelope, so the message is read out of it
    // rather than dumping raw HTML into an error strip: a filter the server refused has a code
    // and a sentence, and printing the body would show the visitor markup.
    const text = await response.text();
    let code = "export_failed";
    let message = "the export failed";
    try {
      const body = JSON.parse(text) as { code?: string; message?: string };
      if (body?.code) code = body.code;
      if (body?.message) message = body.message;
    } catch {
      // Not JSON: keep the defaults rather than showing markup.
    }
    throw new ApiError(response.status, code, message);
  }
  const blob = await response.blob();
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = "submissions.csv";
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  URL.revokeObjectURL(url);
}
// ---------------------------------------------------------------------------------------------
// Menus and the scheduled publishing queue (REQ-064, slice 1)
// ---------------------------------------------------------------------------------------------

/** The menus of one site. */
export function fetchMenus(siteId: string): Promise<Menu[]> {
  return request<Menu[]>(`/api/v1/menus?site_id=${encodeURIComponent(siteId)}`);
}

/**
 * One menu with its items and the vocabulary the editor draws from.
 *
 * The vocabulary travels with the document rather than being hard-coded in the panel: a list the
 * server would then refuse is a picker that offers an option the save rejects, and the rejection
 * is a 400 the editor sees as an unexplained failure.
 */
export function fetchMenu(menuId: string): Promise<MenuDetail> {
  return request<MenuDetail>(`/api/v1/menus/${encodeURIComponent(menuId)}`);
}

export function createMenu(input: {
  site_id: string;
  key: string;
  name: string;
}): Promise<Menu> {
  return request<Menu>("/api/v1/menus", { method: "POST", body: JSON.stringify(input) });
}

export function updateMenu(
  menuId: string,
  input: { name?: string; key?: string; locations?: string[] },
): Promise<Menu> {
  return request<Menu>(`/api/v1/menus/${encodeURIComponent(menuId)}`, {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

export function deleteMenu(menuId: string): Promise<void> {
  return request<void>(`/api/v1/menus/${encodeURIComponent(menuId)}`, { method: "DELETE" });
}

/**
 * Save the whole item tree and the claimed locations in one write.
 *
 * One request rather than one per row: a reorder of six items is one transaction that either
 * lands whole or not at all, and six partial updates that can half-apply are how a site's header
 * ends up with a duplicate and a hole.
 */
export function saveMenuDocument(
  menuId: string,
  input: { items: MenuItemInput[]; locations: string[] },
): Promise<MenuDetail> {
  return request<MenuDetail>(`/api/v1/menus/${encodeURIComponent(menuId)}/items`, {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

/** `Add pages…` — only published pages of this site are inserted, labels come from their titles. */
export function addPagesToMenu(
  menuId: string,
  input: { page_ids: string[]; parent_id?: string | null; position?: number | null },
): Promise<MenuDetail> {
  return request<MenuDetail>(`/api/v1/menus/${encodeURIComponent(menuId)}/items/from-pages`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/**
 * What the theme renders for a location, filtered for an audience.
 *
 * The editor's preview calls the *public* endpoint rather than re-implementing the filter, so a
 * preview and the live site cannot disagree about who sees a members-only link.
 *
 * `site` is a site's **global key or a host**, never a uuid. The public addressing rule
 * (`routes::public::classify_hint`) reads a dot as "this is a host" and anything else as a key, so
 * a uuid is looked up as a key, matches nothing and answers 404 — while the authenticated half of
 * the same screen works, because `/menus` takes `site_id`. Two endpoints naming "the site"
 * differently is the defect; this comment is what stops it coming back.
 */
export function fetchRenderedMenu(
  location: string,
  audience: "visitor" | "member",
  site?: string,
): Promise<RenderedMenu | null> {
  const query = new URLSearchParams({ audience });
  if (site) query.set("site", site);
  return request<RenderedMenu | null>(
    `/api/v1/public/menus/${encodeURIComponent(location)}?${query.toString()}`,
  );
}

/** The queue. Filters are the server's, so the count on screen is the count in the database. */
export function fetchPublishingQueue(filters: {
  status?: string;
  page_type?: string;
  limit?: number;
  /**
   * The site the panel is showing. Required in practice: the queue is scoped to a site like every
   * other content screen, and the server reads the organization from this site — which is also
   * what lets the platform owner (an account with no primary organization) read the queue at all.
   */
  site_id?: string;
} = {}): Promise<PublishingEntry[]> {
  const query = new URLSearchParams();
  if (filters.site_id) query.set("site_id", filters.site_id);
  if (filters.status) query.set("status", filters.status);
  if (filters.page_type) query.set("page_type", filters.page_type);
  if (filters.limit) query.set("limit", String(filters.limit));
  const suffix = query.toString();
  return request<PublishingEntry[]>(`/api/v1/publishing/queue${suffix ? `?${suffix}` : ""}`);
}

export function rescheduleEntry(entryId: string, scheduledAt: string): Promise<PublishingEntry> {
  return request<PublishingEntry>(`/api/v1/publishing/queue/${encodeURIComponent(entryId)}`, {
    method: "PUT",
    body: JSON.stringify({ scheduled_at: scheduledAt }),
  });
}

export function cancelEntry(entryId: string): Promise<PublishingEntry> {
  return request<PublishingEntry>(
    `/api/v1/publishing/queue/${encodeURIComponent(entryId)}/cancel`,
    { method: "POST" },
  );
}

/**
 * Make the entry due and let the runner do the work.
 *
 * Deliberately not a second publish path: a button with its own lighter publish would leave two
 * definitions of "published" in one platform, and they would disagree within a week.
 */
export function publishEntryNow(entryId: string): Promise<PublishingEntry> {
  return request<PublishingEntry>(
    `/api/v1/publishing/queue/${encodeURIComponent(entryId)}/publish-now`,
    { method: "POST" },
  );
}

export function retryEntry(entryId: string): Promise<PublishingEntry> {
  return request<PublishingEntry>(
    `/api/v1/publishing/queue/${encodeURIComponent(entryId)}/retry`,
    { method: "POST" },
  );
}

/** One item as the editor submits it — the same shape the stored item has. */
export type MenuItemInput = MenuItem;
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
// SEO toolkit (REQ-064, slice 3)
// ---------------------------------------------------------------------------------------------

/**
 * The whole SEO screen in one read.
 *
 * One endpoint rather than six: the screen is a set of panels over ONE site, and a panel that
 * fetches its own slice is a screen with five loading states and five ways to show a number
 * from a different moment than its neighbour.
 */
export function fetchSeoOverview(siteId: string): Promise<SeoOverview> {
  return request<SeoOverview>(`/api/v1/seo/settings?site_id=${encodeURIComponent(siteId)}`);
}

/** A page's SEO fields and the tags they produce. */
export function fetchPageSeo(pageId: string): Promise<PageSeoBody> {
  return request<PageSeoBody>(`/api/v1/pages/${encodeURIComponent(pageId)}/seo`);
}

/**
 * Save a page's SEO fields and get the tags back from the same call.
 *
 * The response carries the generated tag set so the SERP preview and the JSON-LD view update
 * from the call that saved them. A panel that re-derives the preview itself has two
 * implementations of "what a crawler sees", and they drift on the first edge case.
 */
export function savePageSeo(pageId: string, seo: PageSeo): Promise<PageSeoBody> {
  return request<PageSeoBody>(`/api/v1/pages/${encodeURIComponent(pageId)}/seo`, {
    method: "PUT",
    body: JSON.stringify(seo),
  });
}

export function createSeoRedirect(input: {
  site_id: string;
  from_path: string;
  to_path: string;
  status_code?: number;
  pattern?: string;
  enabled?: boolean;
}): Promise<SeoRedirect> {
  return request<SeoRedirect>("/api/v1/seo/redirects", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

export function updateSeoRedirect(
  ruleId: string,
  input: {
    from_path: string;
    to_path: string;
    status_code?: number;
    pattern?: string;
    enabled?: boolean;
  },
): Promise<SeoRedirect> {
  return request<SeoRedirect>(`/api/v1/seo/redirects/${encodeURIComponent(ruleId)}`, {
    method: "PUT",
    body: JSON.stringify(input),
  });
}

export function deleteSeoRedirect(ruleId: string): Promise<void> {
  return request<void>(`/api/v1/seo/redirects/${encodeURIComponent(ruleId)}`, {
    method: "DELETE",
  });
}

/**
 * Ask what would answer a path — and what else would.
 *
 * This does not count a hit, which is the whole reason it is a separate entry point from the
 * public resolver: an owner trying three candidate rules must not leave three hits in the column
 * they are reading to decide whether any of the rules is needed.
 */
export function testSeoRedirect(ruleId: string, path: string): Promise<SeoRedirectTest> {
  return request<SeoRedirectTest>(
    `/api/v1/seo/redirects/${encodeURIComponent(ruleId)}/test`,
    { method: "POST", body: JSON.stringify({ path }) },
  );
}

/** Save a site's sitemap settings and robots.txt. */
export function saveSeoSettings(
  siteId: string,
  input: {
    sitemap_types: string[];
    default_priority: number;
    default_change_frequency: string;
    robots_txt: string;
  },
): Promise<SeoSettings> {
  return request<SeoSettings>(
    `/api/v1/sites/${encodeURIComponent(siteId)}/seo/settings`,
    { method: "PUT", body: JSON.stringify(input) },
  );
}

/** Rebuild the sitemap and store it. */
export function regenerateSitemap(siteId: string): Promise<SeoSettings> {
  return request<SeoSettings>(
    `/api/v1/sites/${encodeURIComponent(siteId)}/seo/sitemap/regenerate`,
    { method: "POST" },
  );
}

/** Run the internal-link crawl now. */
export function scanBrokenLinks(siteId: string): Promise<SeoBrokenLink[]> {
  return request<SeoBrokenLink[]>("/api/v1/seo/broken-links", {
    method: "POST",
    body: JSON.stringify({ site_id: siteId }),
  });
}

/** Dismiss a broken link, or bring a dismissed one back. */
export function setBrokenLinkIgnored(linkId: string, ignored: boolean): Promise<void> {
  return request<void>(`/api/v1/seo/broken-links/${encodeURIComponent(linkId)}`, {
    method: "PATCH",
    body: JSON.stringify({ ignored }),
  });
}

// ---------------------------------------------------------------------------------------------
// Page comments (REQ-064, slice 4a)
// ---------------------------------------------------------------------------------------------

/**
 * The moderation inbox: one tab, plus the counts for all four.
 *
 * One endpoint rather than a list and a separate counts call, because the tab bar and the table
 * are one screen and two reads can show two moments — a count that says 4 over a table that
 * shows 3 is a moderator wondering whether they lost one.
 */
export function fetchCommentInbox(filters: {
  site_id: string;
  status?: string;
  search?: string;
  page_id?: string;
  limit?: number;
  offset?: number;
}): Promise<CommentInbox> {
  const query = new URLSearchParams({ site_id: filters.site_id });
  for (const key of ["status", "search", "page_id", "limit", "offset"] as const) {
    const value = filters[key];
    if (value !== undefined && value !== "") query.set(key, String(value));
  }
  return request<CommentInbox>(`/api/v1/comments?${query.toString()}`);
}

/** One comment, with the page it was left on. */
export function fetchComment(id: string, siteId: string): Promise<CommentInboxRow> {
  return request<CommentInboxRow>(
    `/api/v1/comments/${encodeURIComponent(id)}?site_id=${encodeURIComponent(siteId)}`,
    { cache: "no-store" },
  );
}

/**
 * Move a comment to a state.
 *
 * `reason` is only meaningful for `spam` and `trash`, and the panel sends it when the moderator
 * typed one: a spam row with no reason is a row a moderator has to investigate to learn what the
 * platform already knew.
 */
export function moderateComment(
  id: string,
  siteId: string,
  status: string,
  reason?: string,
): Promise<CommentInboxRow> {
  return request<CommentInboxRow>(
    `/api/v1/comments/${encodeURIComponent(id)}?site_id=${encodeURIComponent(siteId)}`,
    {
      method: "PATCH",
      body: JSON.stringify({ status, reason: reason || undefined }),
    },
  );
}

/**
 * Moderate a selection.
 *
 * The answer reports per-comment outcomes rather than a bare count, and the panel shows the
 * partial case: a bulk action that moved 8 of 10 and reports "10 moderated" is worse than no
 * bulk action, because the two it did not touch are invisible.
 */
export function bulkModerateComments(
  siteId: string,
  commentIds: string[],
  status: string,
): Promise<CommentBulkResult> {
  return request<CommentBulkResult>(
    `/api/v1/comments/bulk?site_id=${encodeURIComponent(siteId)}`,
    {
      method: "POST",
      body: JSON.stringify({ status, comment_ids: commentIds }),
    },
  );
}

/** Answer a comment as the site. Published immediately. */
export function replyToComment(
  id: string,
  reply: NewCommentReply,
): Promise<CommentInboxRow> {
  return request<CommentInboxRow>(`/api/v1/comments/${encodeURIComponent(id)}/reply`, {
    method: "POST",
    body: JSON.stringify(reply),
  });
}

/** Remove a comment for good. The Trash tab only. */
export function deleteComment(id: string, siteId: string): Promise<void> {
  return request<void>(
    `/api/v1/comments/${encodeURIComponent(id)}?site_id=${encodeURIComponent(siteId)}`,
    { method: "DELETE" },
  );
}

/** The moderation policy and the bans, in one read. */
export function fetchCommentSettings(siteId: string): Promise<CommentSettingsDocument> {
  return request<CommentSettingsDocument>(
    `/api/v1/sites/${encodeURIComponent(siteId)}/comment-settings`,
    { cache: "no-store" },
  );
}

/**
 * Save the moderation policy.
 *
 * The panel sends the whole policy rather than a diff, and the server applies each field it was
 * given over the stored row — so a panel that grows a new toggle next year cannot reset the
 * settings this year's panel knew nothing about.
 */
export function saveCommentSettings(
  siteId: string,
  settings: Partial<CommentSettingsDocument["settings"]> & { comments_enabled: boolean },
): Promise<CommentSettingsDocument> {
  return request<CommentSettingsDocument>(
    `/api/v1/sites/${encodeURIComponent(siteId)}/comment-settings`,
    { method: "PUT", body: JSON.stringify(settings) },
  );
}

/** Place a ban. An `ip` value is fingerprinted by the server, never stored raw. */
export function addCommentBan(
  siteId: string,
  input: { kind: "email" | "ip"; value: string; reason?: string; expires_at?: string | null },
): Promise<CommentBan> {
  return request<CommentBan>(`/api/v1/sites/${encodeURIComponent(siteId)}/comment-bans`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** Lift a ban. */
export function removeCommentBan(siteId: string, banId: string): Promise<void> {
  return request<void>(
    `/api/v1/sites/${encodeURIComponent(siteId)}/comment-bans/${encodeURIComponent(banId)}`,
    { method: "DELETE" },
  );
}

// ---------------------------------------------------------------------------------------------
// Members (REQ-064, slice 4c)
// ---------------------------------------------------------------------------------------------

/**
 * The members table and its three state counts.
 *
 * One endpoint rather than a list plus a separate counts call: the chips and the table are one
 * screen, and two reads can show two moments — a chip that says 4 over three rows leaves an
 * operator wondering whether they lost a member.
 */
export function fetchMemberList(filters: {
  site_id: string;
  status?: string;
  search?: string;
  role?: string;
  limit?: number;
  offset?: number;
}): Promise<MemberList> {
  const query = new URLSearchParams({ site_id: filters.site_id });
  for (const key of ["status", "search", "role", "limit", "offset"] as const) {
    const value = filters[key];
    if (value !== undefined && value !== "") query.set(key, String(value));
  }
  return request<MemberList>(`/api/v1/members?${query.toString()}`);
}

/** One member, with the last ten sign-ins. */
export function fetchMember(id: string, siteId: string): Promise<MemberDetail> {
  return request<MemberDetail>(
    `/api/v1/members/${encodeURIComponent(id)}?site_id=${encodeURIComponent(siteId)}`,
    { cache: "no-store" },
  );
}

/**
 * The operator creates an account, with or without a password.
 *
 * `password` is optional on purpose: creating a row WITH a password makes a usable account the
 * operator chose the address for, and omitting it makes an invitation the member must claim. The
 * panel asks which, because the difference is whether the first sign-in needs a link.
 */
export function createMember(input: {
  site_id: string;
  email: string;
  name?: string;
  password?: string;
  roles?: string[];
}): Promise<Member> {
  return request<Member>("/api/v1/members", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/**
 * Edit a member.
 *
 * `roles` REPLACES the whole list rather than appending, and that is why the panel always sends
 * it: a member editor that only ever adds a role cannot take one away, and a visitor who must
 * keep a role they were granted by mistake holds it for as long as the site exists.
 */
export function patchMember(
  id: string,
  siteId: string,
  changes: {
    name?: string | null;
    status?: string;
    roles?: string[];
    signin_note?: string | null;
  },
): Promise<Member> {
  return request<Member>(
    `/api/v1/members/${encodeURIComponent(id)}?site_id=${encodeURIComponent(siteId)}`,
    { method: "PATCH", body: JSON.stringify(changes) },
  );
}

/**
 * Block a member, naming the reason.
 *
 * A separate route rather than `patchMember({status:"blocked"})` because it is the one
 * destructive button in the table and it carries the reason — a block with no stated reason is a
 * decision an operator has to reverse twice.
 */
export function blockMember(id: string, siteId: string, reason?: string): Promise<Member> {
  return request<Member>(
    `/api/v1/members/${encodeURIComponent(id)}/block?site_id=${encodeURIComponent(siteId)}`,
    { method: "POST", body: JSON.stringify({ reason: reason || undefined }) },
  );
}

/** The operator vouches for an address they know. */
export function verifyMember(id: string, siteId: string): Promise<Member> {
  return request<Member>(
    `/api/v1/members/${encodeURIComponent(id)}/verify?site_id=${encodeURIComponent(siteId)}`,
    { method: "POST" },
  );
}

/**
 * Mint a fresh verification link.
 *
 * The answer says whether mail went out, so the panel can tell an operator that nothing was sent
 * rather than leaving them to find out from a member who never received anything.
 */
export function sendMemberVerification(id: string, siteId: string): Promise<MemberDelivery> {
  return request<MemberDelivery>(
    `/api/v1/members/${encodeURIComponent(id)}/send-verification?site_id=${encodeURIComponent(siteId)}`,
    { method: "POST" },
  );
}

/** Mint a password reset link. */
export function sendMemberReset(id: string, siteId: string): Promise<MemberDelivery> {
  return request<MemberDelivery>(
    `/api/v1/members/${encodeURIComponent(id)}/send-reset?site_id=${encodeURIComponent(siteId)}`,
    { method: "POST" },
  );
}

/** Kill every live member session. A blocked member's cookie stops on the next request. */
export function signOutMemberEverywhere(
  id: string,
  siteId: string,
): Promise<{ member_id: string; sessions_removed: number }> {
  return request<{ member_id: string; sessions_removed: number }>(
    `/api/v1/members/${encodeURIComponent(id)}/sign-out-everywhere?site_id=${encodeURIComponent(siteId)}`,
    { method: "POST" },
  );
}

/**
 * Delete a visitor account and everything it owns.
 *
 * The only irreversible action on the screen, and the only one the panel puts behind a
 * confirmation that names the address it is about to erase.
 */
export function deleteMember(id: string, siteId: string): Promise<void> {
  return request<void>(
    `/api/v1/members/${encodeURIComponent(id)}?site_id=${encodeURIComponent(siteId)}`,
    { method: "DELETE" },
  );
}

/** The site's membership policy. */
export function fetchMemberSettings(siteId: string): Promise<MemberSettingsDocument> {
  return request<MemberSettingsDocument>(
    `/api/v1/sites/${encodeURIComponent(siteId)}/members/settings`,
    { cache: "no-store" },
  );
}

/**
 * Save the policy.
 *
 * The panel sends the whole document and the server applies each field it was given over the
 * stored row — a panel that grows a field next year cannot reset the ones this year's panel knew
 * nothing about.
 */
export function saveMemberSettings(
  siteId: string,
  settings: Partial<MemberSettingsDocument["settings"]>,
): Promise<MemberSettingsDocument> {
  return request<MemberSettingsDocument>(
    `/api/v1/sites/${encodeURIComponent(siteId)}/members/settings`,
    { method: "PUT", body: JSON.stringify(settings) },
  );
}

// ---------------------------------------------------------------------------------------------
// Newsletter (REQ-064, slice 4b)
// ---------------------------------------------------------------------------------------------

/**
 * Every list of a site, each carrying its four counts.
 *
 * One endpoint rather than a list plus a counts call per row, because the counts are printed
 * beside the list they belong to: a "3" over a table that now shows 4 is an owner wondering
 * whether they lost a subscriber.
 */
export function fetchNewsletterLists(siteId: string): Promise<NewsletterList[]> {
  return request<NewsletterList[]>(
    `/api/v1/newsletter/lists?site_id=${encodeURIComponent(siteId)}`,
    { cache: "no-store" },
  );
}

/** One list. */
export function fetchNewsletterList(id: string, siteId: string): Promise<NewsletterList> {
  return request<NewsletterList>(
    `/api/v1/newsletter/lists/${encodeURIComponent(id)}?site_id=${encodeURIComponent(siteId)}`,
    { cache: "no-store" },
  );
}

/**
 * Create a list.
 *
 * `key` is deliberately optional: the server derives one from the name and resolves a
 * collision to `weekly-news-2` rather than refusing. A panel that demanded a unique key would
 * make the owner invent a slug to get past a form that should just work.
 */
export function createNewsletterList(body: NewNewsletterList): Promise<NewsletterList> {
  return request<NewsletterList>("/api/v1/newsletter/lists", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/**
 * Change a list.
 *
 * The client sends only the fields the form owns, so a `double_opt_in` an owner never looked at
 * is not silently reset by saving a name.
 */
export function patchNewsletterList(
  id: string,
  siteId: string,
  patch: { name?: string; description?: string; double_opt_in?: boolean },
): Promise<NewsletterList> {
  return request<NewsletterList>(
    `/api/v1/newsletter/lists/${encodeURIComponent(id)}?site_id=${encodeURIComponent(siteId)}`,
    { method: "PUT", body: JSON.stringify(patch) },
  );
}

/**
 * Delete a list.
 *
 * The list's subscribers go with it (`on delete cascade`): the screen's confirmation names the
 * count, because "delete this list" and "delete these 412 addresses" are different decisions
 * and the second one is irreversible.
 */
export function deleteNewsletterList(id: string, siteId: string): Promise<void> {
  return request<void>(
    `/api/v1/newsletter/lists/${encodeURIComponent(id)}?site_id=${encodeURIComponent(siteId)}`,
    { method: "DELETE" },
  );
}

/** One filtered page of subscribers. */
export function fetchSubscribers(filters: {
  site_id: string;
  list_id?: string;
  status?: string;
  search?: string;
  limit?: number;
  offset?: number;
}): Promise<SubscriberPage> {
  const query = new URLSearchParams({ site_id: filters.site_id });
  for (const key of ["list_id", "status", "search", "limit", "offset"] as const) {
    const value = filters[key];
    if (value !== undefined && value !== "") query.set(key, String(value));
  }
  return request<SubscriberPage>(`/api/v1/newsletter/subscribers?${query.toString()}`, {
    cache: "no-store",
  });
}

/**
 * Add one address by hand.
 *
 * The response is a 202-shaped `pending` row on a double-opt-in list: the panel says "a
 * confirmation link is on its way" rather than "added", because the address cannot receive an
 * issue until somebody clicks it, and a table that claimed otherwise would be lying.
 */
export function addSubscriber(
  listId: string,
  siteId: string,
  body: { email: string; name?: string; source?: string },
): Promise<NewsletterSubscriber> {
  return request<NewsletterSubscriber>(
    `/api/v1/newsletter/lists/${encodeURIComponent(listId)}/subscribers?site_id=${encodeURIComponent(siteId)}`,
    { method: "POST", body: JSON.stringify(body) },
  );
}

/** Move a subscriber to a state. `reason` is stored and shown on the row. */
export function setSubscriberStatus(
  id: string,
  siteId: string,
  status: string,
  reason?: string,
): Promise<NewsletterSubscriber> {
  return request<NewsletterSubscriber>(
    `/api/v1/newsletter/subscribers/${encodeURIComponent(id)}?site_id=${encodeURIComponent(siteId)}`,
    { method: "PATCH", body: JSON.stringify({ status, reason: reason || undefined }) },
  );
}

/** Remove a subscriber row for good — the panel's own action, not the unsubscribe link. */
export function deleteSubscriber(id: string, siteId: string): Promise<void> {
  return request<void>(
    `/api/v1/newsletter/subscribers/${encodeURIComponent(id)}?site_id=${encodeURIComponent(siteId)}`,
    { method: "DELETE" },
  );
}

/**
 * Import addresses from CSV text.
 *
 * The report is returned rather than summarised to a count: an import that says "18 added" over
 * a file with 40 rows has lost 22 addresses somewhere, and those 22 are named in `skipped`.
 */
export function importSubscribers(
  listId: string,
  siteId: string,
  csv: string,
  source?: string,
): Promise<ImportReport> {
  return request<ImportReport>(
    `/api/v1/newsletter/lists/${encodeURIComponent(listId)}/import?site_id=${encodeURIComponent(siteId)}`,
    { method: "POST", body: JSON.stringify({ csv, source: source || undefined }) },
  );
}

/**
 * Export the filtered rows as CSV text.
 *
 * The server answers `{ csv: "..." }` rather than a `text/csv` response, because a
 * `Content-Disposition` header is invisible to `fetch`: the file would arrive as a string the
 * operator has to save by hand, which is not what "Export" means to them.
 */
export async function exportSubscribers(filters: {
  site_id: string;
  list_id?: string;
  status?: string;
  search?: string;
}): Promise<string> {
  const query = new URLSearchParams({ site_id: filters.site_id });
  for (const key of ["list_id", "status", "search"] as const) {
    const value = filters[key];
    if (value !== undefined && value !== "") query.set(key, String(value));
  }
  const body = await request<{ csv: string }>(
    `/api/v1/newsletter/subscribers/export?${query.toString()}`,
  );
  return body.csv;
}

/** The sent-issue archive. */
export function fetchNewsletterIssues(filters: {
  site_id: string;
  limit?: number;
}): Promise<NewsletterIssue[]> {
  const query = new URLSearchParams({ site_id: filters.site_id });
  if (filters.limit) query.set("limit", String(filters.limit));
  return request<NewsletterIssue[]>(`/api/v1/newsletter/issues?${query.toString()}`, {
    cache: "no-store",
  });
}

/**
 * Send an issue and archive it.
 *
 * `recipient_count` comes back from the send rather than being computed by the browser: the
 * list changed after the send, so a count read now is a count about a different question.
 */
export function sendNewsletterIssue(body: {
  site_id: string;
  list_id: string;
  subject: string;
  body_html: string;
  archive_slug?: string;
}): Promise<{ id: string; recipient_count: number; archive_slug: string }> {
  return request<{ id: string; recipient_count: number; archive_slug: string }>(
    "/api/v1/newsletter/issues",
    { method: "POST", body: JSON.stringify(body) },
  );
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
