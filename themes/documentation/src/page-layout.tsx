/**
 * Documentation — Docs: a sticky table of contents rail, a monospace affordance line and tight line height.
 *
 * Docs. A sticky header and a table-of-contents rail beside the article, so a long page
has a spine; the line height is the loosest of the nine, because a documentation page is
read rather than skimmed and code has to breathe.
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
export function documentationPageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="do-documentation-shell">
      <header className="do-documentation-header">
        <a className="do-documentation-brand" href="/">
          {site.name}
        </a>
        <span className="do-documentation-version">docs</span>
      </header>

      <div className="do-documentation-frame">
        <nav className="do-documentation-toc" aria-label="On this page">
          <p className="do-documentation-toc-title">On this page</p>
          <a href="#top">{revision.title}</a>
          <span className="do-documentation-toc-kind">{page.page_type}</span>
        </nav>

        <main className="do-documentation-main">
          <article className="do-documentation-article">
            <h1 className="do-documentation-title">{revision.title}</h1>
            {revision.summary ? <p className="do-documentation-summary">{revision.summary}</p> : null}
            <div className="do-documentation-body">
              {blocks.length > 0 ? (
                <BlockTree blocks={blocks} />
              ) : (
                paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
              )}
            </div>
            <p className="do-documentation-meta">
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
      </div>

      <footer className="do-documentation-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
