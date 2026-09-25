import type { Metadata } from "next";
import type { PublishedPage } from "@omnion/types";

/**
 * Metadata of one published page (docs/03-FRONTEND.md quality bar: SEO and Open Graph).
 *
 * Canonical and Open Graph URLs need an absolute origin, so they are emitted when the
 * deployment sets `OMNION_SITE_URL`; the title and description always come from the content.
 */
export function metadataFor(content: PublishedPage): Metadata {
  const { site, page, revision } = content;
  const description = revision.summary ?? undefined;
  const origin = absoluteOrigin();

  const openGraph: Metadata["openGraph"] = {
    type: "article",
    title: revision.title,
    description,
    siteName: site.name,
  };
  if (origin) {
    openGraph.url = `${origin}/${page.slug}`;
  }

  return {
    title: `${revision.title} · ${site.name}`,
    description,
    ...(origin ? { alternates: { canonical: `${origin}/${page.slug}` } } : {}),
    openGraph,
  };
}

/** Absolute origin of the site, when the deployment declares one. */
function absoluteOrigin(): string | null {
  const value = process.env.OMNION_SITE_URL?.trim();
  if (!value) {
    return null;
  }
  try {
    return new URL(value).origin;
  } catch {
    return null;
  }
}
