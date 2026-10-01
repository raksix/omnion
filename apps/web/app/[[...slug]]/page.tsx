import type { Metadata } from "next";
import { headers } from "next/headers";
import { notFound } from "next/navigation";

import { getPublishedPage, viewportForRequest } from "@/lib/api";
import { metadataFor } from "@/lib/metadata";
import { resolveThemeOrWarn } from "@/lib/theme";
import { SITE_HINT_HEADER } from "@/proxy";

/**
 * The public renderer.
 *
 * `/` serves the site's `home` page and `/{slug}` any other published page. Rendering happens
 * on the server on every request (`force-dynamic`): the panel publishes revisions without a
 * redeploy, and a visitor must never see a cached draft. A page that is unknown or not
 * published ends in the site's not-found view — the API answers `404` for both.
 *
 * The request is also what decides the *viewport*, because a block hidden on phones is absent
 * from the phone render rather than invisible on it. Deciding that here — from the client's own
 * viewport hint — is what makes the answer the same on the first paint and after hydration; a
 * client-side width check would ship the desktop page first and then delete half of it.
 */
export const dynamic = "force-dynamic";

/** Address of the page the site root serves. */
const HOME_SLUG = "home";

type RouteParams = { params: Promise<{ slug?: string[] }> };
type RouteSearch = { searchParams?: Promise<Record<string, string | string[] | undefined>> };

/** The `?site=` hint on this request, read the one way both callers must read it. */
async function siteHintOf(search: RouteSearch["searchParams"]): Promise<string | string[] | undefined> {
  return (await search)?.site;
}

/** The slug a request addresses; `null` for a path that cannot be a page. */
function slugOf(segments: string[] | undefined): string | null {
  if (!segments || segments.length === 0) {
    return HOME_SLUG;
  }
  if (segments.length > 1) {
    return null;
  }
  return segments[0] ?? null;
}

/**
 * The screen this request is being rendered for.
 *
 * Next 15 exposes the request headers through `headers()`, which is the only place a
 * server-rendered page can see the client hints — `userAgent` from `next/headers` is the
 * `User-Agent` and nothing else, so it is read from the same object rather than a second API.
 */
async function viewportOf() {
  const incoming = await headers();
  return viewportForRequest({
    headers: incoming,
    // A server render has no screen of its own; the client hint is the only width there is,
    // and a request without one (a crawler, a curl) gets the wide page, which is the full one.
    screenWidth: null,
    userAgent: incoming.get("user-agent") ?? "",
  });
}

export async function generateMetadata({ params, searchParams }: RouteParams & RouteSearch): Promise<Metadata> {
  const slug = slugOf((await params).slug);
  if (!slug) {
    return { title: "Not found" };
  }
  // Metadata does not depend on the viewport: a title the phone hides is a title the crawler
  // would miss, and the block system has no opinion on a page's name. It DOES depend on the
  // site, though: the same slug can exist on two sites with two different titles, so a title
  // resolved without the `?site=` hint would be the other site's name.
  const content = await getPublishedPage(slug, "desktop", await siteHintOf(searchParams));
  return content ? metadataFor(content) : { title: "Not found" };
}

export default async function Page({ params, searchParams }: RouteParams & RouteSearch) {
  const slug = slugOf((await params).slug);
  const incoming = await headers();
  const viewport = await viewportOf();
  // The `?site=` is read from the header `proxy.ts` forwarded it as, so the page and the
  // layout above it resolve the same site from the same source. They used to read it from two
  // different places — the layout from the installation default, the page from the query — and
  // could therefore draw a page in one site's theme under another site's name.
  const siteHint = incoming.get(SITE_HINT_HEADER) ?? undefined;
  const content = slug ? await getPublishedPage(slug, viewport, siteHint) : null;
  if (!content) {
    notFound();
  }

  const theme = resolveThemeOrWarn(content.site.theme, "this page");
  const PageLayout = theme.PageLayout;

  return <PageLayout content={content} />;
}
