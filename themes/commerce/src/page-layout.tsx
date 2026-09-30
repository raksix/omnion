/**
 * Commerce — A storefront: a compact utility header, product grids and a price set in a tabular figure.
 *
 * A storefront. The header is a utility strip: small type, tight spacing, everything it
carries the same weight. The article frame is the widest of the nine because a grid of
products needs the room, and prices are set with a tabular figure so columns of them line
up.
 */
import type { PageLayoutProps } from "@omnion/theme-sdk";
import type { ContentBlock } from "@omnion/types";

import { BlockTree, bodyParagraphs } from "./block-renderer";

/** Format a publish timestamp for the page's own locale-independent line. */
function formatPublished(value: string | null): string | null {
  if (!value) return null;
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return null;
  return new Intl.DateTimeFormat("en-GB", {
    day: "numeric",
    month: "long",
    year: "numeric",
    timeZone: "UTC",
  }).format(date);
}

/** The revision's block tree, or an empty list when the page was published before blocks. */
function blocksOf(revision: { blocks?: unknown }): ContentBlock[] {
  return Array.isArray(revision.blocks) ? (revision.blocks as ContentBlock[]) : [];
}

export { bodyParagraphs };

/** Render one published page. */
export function commercePageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="co-commerce-shell">
      <header className="co-commerce-header">
        <a className="co-commerce-brand" href="/">
          {site.name}
        </a>
        <nav className="co-commerce-utility" aria-label="Store">
          <a href="#catalogue">Catalogue</a>
          <a href="#cart">Cart</a>
        </nav>
      </header>
      <div className="co-commerce-rule" aria-hidden="true" />

      <main className="co-commerce-main">
        <article className="co-commerce-article">
          <p className="co-commerce-eyebrow">{page.page_type}</p>
          <h1 className="co-commerce-title">{revision.title}</h1>
          {revision.summary ? <p className="co-commerce-summary">{revision.summary}</p> : null}
          <div className="co-commerce-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
          <p className="co-commerce-meta">
            <span>Revision {revision.revision_no}</span>
            {published ? (
              <>
                <span aria-hidden="true"> · </span>
                <time dateTime={revision.published_at ?? undefined}>{published}</time>
              </>
            ) : null}
          </p>
        </article>
      </main>

      <footer className="co-commerce-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
