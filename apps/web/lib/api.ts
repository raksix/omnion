/**
 * The public content reader of the renderer.
 *
 * Server-side only: the renderer asks the API for one page and turns it into HTML, so the
 * session cookie of the panel never has to enter the picture.
 */
import { headers } from "next/headers";
import type { PublishedPage } from "@omnion/types";

/** Resolve the API origin the renderer reads published content from. */
const apiOrigin = process.env.OMNION_API_URL?.replace(/\/+$/, "") ?? "http://127.0.0.1:8080";

/**
 * Site hint for installations where several sites share one renderer: `OMNION_SITE` (a site
 * key like `main`, or one of its domains) wins. Without it the visitor's own host is forwarded
 * — the same signal the API resolves domains with.
 *
 * A `?site=` on the page's own URL is honoured first, and only as a hint. Host resolution is
 * the right default and stays the default; what a hint is *for* is an address that cannot
 * resolve one — a loopback preview (`127.0.0.1:3200`, where `visitorHost` returns `null` and
 * the API is asked for a page with no site at all), and a shareable multi-site link. Without
 * it, `http://127.0.0.1:3200/about?site=main` 404s on a multi-site installation while the same
 * slug renders on its own domain, which reads as the page being gone rather than as the
 * renderer not knowing which site it is drawing. A visitor can name any site this way, so the
 * hint only ever selects *which* site to read; the API still applies the site's own publication
 * and draft rules, and a slug the named site does not have is a 404 there too.
 */
const configuredSite = process.env.OMNION_SITE?.trim();

/**
 * Read one published page from the public content surface.
 *
 * `viewport` is the screen the caller is rendering for. It is not a display preference the
 * browser could apply afterwards: the API *removes* the blocks the author hid from that
 * viewport, so a page asked for as `mobile` is a smaller payload and a smaller accessibility
 * tree — which is the only way "hidden on phones" means hidden rather than invisible.
 *
 * `null` means the API answered `404`: the address is unknown or not published — the caller
 * renders the site's not-found view. Everything else that fails is a real error and is thrown,
 * so a broken API never looks like an empty site.
 */
export async function getPublishedPage(
  slug: string,
  viewport: Viewport = "desktop",
  siteHint?: string | string[],
): Promise<PublishedPage | null> {
  const site = normalizeSiteHint(siteHint) || configuredSite || (await visitorHost());
  const query = new URLSearchParams();
  if (site) {
    query.set("site", site);
  }
  // The default render is the wide one, and asking for it is what the *first* request wants —
  // a `?viewport=desktop` on every URL would be noise in a theme's links and in a cache key
  // for a payload that is the same one the default already returns.
  if (viewport === "mobile") {
    query.set("viewport", viewport);
  }
  const suffix = query.size > 0 ? `?${query.toString()}` : "";
  const response = await fetch(`${apiOrigin}/api/v1/public/pages/${encodeURIComponent(slug)}${suffix}`, {
    cache: "no-store",
    headers: { accept: "application/json" },
  });

  if (response.status === 404) {
    return null;
  }
  if (!response.ok) {
    throw new Error(`the content API answered ${response.status} for ${slug}`);
  }
  return (await response.json()) as PublishedPage;
}

/** The screen a page is being rendered for. */
export type Viewport = "desktop" | "mobile";

/**
 * The viewport the visitor is on.
 *
 * Read from the client hints a phone sends and from the width a server-side render can see.
 * The two answers have to agree or a page jumps when hydration lands, so the *server* is the
 * one that decides: `apps/web` renders with `mobile` when the request looks like a phone, and
 * the client only re-asks on navigation. A UA sniff would call a desktop browser with a narrow
 * window a phone, which is the classic way to ship a page that hides its navigation from
 * someone who resized their window.
 */
export function viewportForRequest(request: {
  headers: Headers;
  screenWidth: number | null;
  userAgent: string;
}): Viewport {
  const hints = request.headers.get("sec-ch-viewport-width") ?? request.headers.get("viewport-width");
  if (hints) {
    const width = Number.parseInt(hints, 10);
    if (Number.isFinite(width)) {
      return width <= PHONE_MAX_WIDTH ? "mobile" : "desktop";
    }
  }
  if (request.screenWidth !== null) {
    return request.screenWidth <= PHONE_MAX_WIDTH ? "mobile" : "desktop";
  }
  return isPhoneUserAgent(request.userAgent) ? "mobile" : "desktop";
}

/**
 * Widest screen still treated as a phone.
 *
 * 900px is the same boundary the REQ uses for the editor's own read-only notice, and the same
 * one the block system documents: a tablet in landscape gets the wide page.
 */
const PHONE_MAX_WIDTH = 900;

/** `true` for the user-agent tokens a phone sends that a desktop browser does not. */
function isPhoneUserAgent(userAgent: string): boolean {
  return /Android.*Mobile|iPhone|iPod|Windows Phone|BlackBerry|Opera Mini/i.test(userAgent);
}

/** The host the visitor asked for, or `null` when it is a local development address. */
async function visitorHost(): Promise<string | null> {
  const incoming = await headers();
  const host = hostWithoutPort(incoming.get("x-forwarded-host") ?? incoming.get("host"));
  if (!host || isLocalHost(host)) {
    return null;
  }
  return host;
}

/**
 * The `?site=` the visitor put on the page's own URL, or `null` when they put nothing usable.
 *
 * Bounded on purpose. This string is forwarded to the API as a query parameter, and an
 * unbounded one would let a URL name a hundred-kilobyte "site", so it is trimmed and cut to
 * the shape a site key or domain actually has: letters, digits, dots and dashes, at most 64
 * characters. Anything else is not a site this renderer will ask for — `?site=<script>` is a
 * 404, which is the correct answer for an address that is not a site, and it keeps the value
 * that reaches the API a token rather than arbitrary text.
 */
function normalizeSiteHint(value: string | string[] | undefined): string | null {
  const first = Array.isArray(value) ? value[0] : value;
  if (typeof first !== "string") {
    return null;
  }
  const trimmed = first.trim().toLowerCase();
  if (!trimmed || trimmed.length > 64 || !/^[a-z0-9.-]+$/.test(trimmed)) {
    return null;
  }
  return trimmed;
}

/** Drop the port (and any proxy list) from a host value. */
function hostWithoutPort(value: string | null): string | null {
  const first = value?.split(",")[0]?.trim().toLowerCase();
  if (!first) {
    return null;
  }
  if (first.startsWith("[")) {
    const end = first.indexOf("]");
    return end > 0 ? first.slice(1, end) : first;
  }
  return first.split(":")[0] || null;
}

/** `true` for addresses a browser only uses on the machine it runs on. */
function isLocalHost(host: string): boolean {
  return (
    host === "localhost" ||
    host === "0.0.0.0" ||
    host === "::1" ||
    host.startsWith("127.") ||
    host.endsWith(".localhost") ||
    host.endsWith(".local")
  );
}
