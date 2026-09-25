import type { Metadata } from "next";
import { notFound } from "next/navigation";

import { getPublishedPage } from "@/lib/api";
import { metadataFor } from "@/lib/metadata";
import { resolveTheme } from "@/lib/theme";

/**
 * The public renderer.
 *
 * `/` serves the site's `home` page and `/{slug}` any other published page. Rendering happens
 * on the server on every request (`force-dynamic`): the panel publishes revisions without a
 * redeploy, and a visitor must never see a cached draft. A page that is unknown or not
 * published ends in the site's not-found view — the API answers `404` for both.
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

export async function generateMetadata({ params }: RouteParams): Promise<Metadata> {
  const slug = slugOf((await params).slug);
  if (!slug) {
    return { title: "Not found" };
  }
  const content = await getPublishedPage(slug);
  return content ? metadataFor(content) : { title: "Not found" };
}

export default async function Page({ params }: RouteParams) {
  const slug = slugOf((await params).slug);
  const content = slug ? await getPublishedPage(slug) : null;
  if (!content) {
    notFound();
  }

  const theme = resolveTheme();
  const PageLayout = theme.PageLayout;

  return <PageLayout content={content} />;
}
