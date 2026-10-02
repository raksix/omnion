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
 */
const configuredSite = process.env.OMNION_SITE?.trim();

/**
 * Read one published page from the public content surface.
 *
 * `null` means the API answered `404`: the address is unknown or not published — the caller
 * renders the site's not-found view. Everything else that fails is a real error and is thrown,
 * so a broken API never looks like an empty site.
 */
export async function getPublishedPage(slug: string): Promise<PublishedPage | null> {
  const site = configuredSite || (await visitorHost());
  const query = site ? `?site=${encodeURIComponent(site)}` : "";
  const response = await fetch(
    `${apiOrigin}/api/v1/public/pages/${encodeURIComponent(slug)}${query}`,
    {
      cache: "no-store",
      headers: { accept: "application/json" },
    },
  );

  if (response.status === 404) {
    return null;
  }
  if (!response.ok) {
    throw new Error(`the content API answered ${response.status} for ${slug}`);
  }
  return (await response.json()) as PublishedPage;
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
