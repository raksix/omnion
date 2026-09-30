/**
 * Portfolio — A personal site: one column, the name set large, and work presented as numbered entries.
 *
 * A personal site. Serif titles against a sans body, a centred header lockup, no radius
anywhere, and work presented as numbered entries — a person's output has a shape, and a
card grid with shadows would make it look like a product catalogue.
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
export function portfolioPageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="po-portfolio-shell">
      <header className="po-portfolio-header">
        <a className="po-portfolio-brand" href="/">
          {site.name}
        </a>
        <span className="po-portfolio-kind">{page.page_type}</span>
      </header>

      <main className="po-portfolio-main">
        <article className="po-portfolio-article">
          <h1 className="po-portfolio-title">{revision.title}</h1>
          {revision.summary ? <p className="po-portfolio-summary">{revision.summary}</p> : null}
          <div className="po-portfolio-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
          <p className="po-portfolio-meta">
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

      <footer className="po-portfolio-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
