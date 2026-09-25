/**
 * Shared types of the Omnion platform.
 *
 * The API (`apps/api`) is the source of truth for every shape here; this package is the
 * TypeScript mirror the apps and themes import instead of restating the JSON by hand. It is
 * types-only on purpose — nothing in here reaches a bundle.
 */

/**
 * Site a piece of public content belongs to.
 *
 * The public surface answers with the site's handle and display name only: internal
 * identifiers stay inside the panel (docs/05-VERSIONING.md §6).
 */
export interface PublicSite {
  /** Stable handle of the site inside its organization (`main`). */
  key: string;
  /** Display name of the site. */
  name: string;
}

/** Public identity of a page. */
export interface PublicPage {
  /** Address of the page inside its site (`about-us`). */
  slug: string;
  /** Content type key (`page` today; the content type builder extends the set). */
  page_type: string;
  /** Last change, RFC 3339. */
  updated_at: string;
}

/** The published revision of a page — what a visitor actually reads. */
export interface PublicRevision {
  /** Monotonic revision number inside the page. */
  revision_no: number;
  /** Title. */
  title: string;
  /** Body (plain text until the block editor lands). */
  body: string;
  /** Summary, when the author wrote one. */
  summary: string | null;
  /** When the revision was published, RFC 3339 — `null` while it never was. */
  published_at: string | null;
}

/** Response of `GET /api/v1/public/pages/{slug}` — one renderable page. */
export interface PublishedPage {
  /** Site the page belongs to. */
  site: PublicSite;
  /** The page itself. */
  page: PublicPage;
  /** The revision visitors see. */
  revision: PublicRevision;
}
