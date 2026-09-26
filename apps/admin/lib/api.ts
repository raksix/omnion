/**
 * Typed client for the Omnion API.
 *
 * The browser always calls the API on the admin panel's own origin: `next.config.ts` forwards
 * `/api/*` to the API origin, so the HttpOnly session cookie is first-party everywhere.
 */
import type {
  Media,
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

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
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
    );
  }

  return payload as T;
}

/** Sign in; the API answers with the account and the session cookie. */
export async function login(email: string, password: string): Promise<User> {
  const body = await request<{ user: User }>("/api/v1/auth/login", {
    method: "POST",
    body: JSON.stringify({ email, password }),
  });
  return body.user;
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

/** Browser URL of one file's bytes, read with the session cookie. */
export function mediaRawUrl(mediaId: string): string {
  return `/api/v1/media/${encodeURIComponent(mediaId)}/raw`;
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
};

/** Build the query string of a search or export call. */
function searchParams(input: SearchQueryInput): URLSearchParams {
  const params = new URLSearchParams({ q: input.q });
  if (input.page) params.set("page", String(input.page));
  if (input.per_page) params.set("per_page", String(input.per_page));
  if (input.sort) params.set("sort", input.sort);
  if (input.facets) params.set("facets", "true");
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
