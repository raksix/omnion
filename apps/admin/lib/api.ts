/**
 * Typed client for the Omnion API.
 *
 * The browser always calls the API on the admin panel's own origin: `next.config.ts` forwards
 * `/api/*` to the API origin, so the HttpOnly session cookie is first-party everywhere.
 */
import type { Organization, Page, Site, User } from "./types";

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
        ...(init.body === undefined ? {} : { "content-type": "application/json" }),
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
