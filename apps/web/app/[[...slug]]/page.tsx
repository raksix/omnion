import type { Metadata } from "next";
import { headers } from "next/headers";
import { notFound } from "next/navigation";

import { getPublishedPage, viewportForRequest } from "@/lib/api";
import { metadataFor } from "@/lib/metadata";
import { resolveTheme } from "@/lib/theme";

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

export async function generateMetadata({ params }: RouteParams): Promise<Metadata> {
  const slug = slugOf((await params).slug);
  if (!slug) {
    return { title: "Not found" };
  }
  // Metadata does not depend on the viewport: a title the phone hides is a title the crawler
  // would miss, and the block system has no opinion on a page's name.
  const content = await getPublishedPage(slug);
  return content ? metadataFor(content) : { title: "Not found" };
}

export default async function Page({ params }: RouteParams) {
  const slug = slugOf((await params).slug);
  const viewport = await viewportOf();
  const content = slug ? await getPublishedPage(slug, viewport) : null;
  if (!content) {
    notFound();
  }

  const theme = resolveTheme(content.site.theme);
  const PageLayout = theme.PageLayout;

  return <PageLayout content={content} />;
}
