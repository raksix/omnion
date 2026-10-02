/**
 * Magazine — An editorial site: a serif display face, a rule above every section and a standfirst.
 *
 * An editorial site. The body face is a serif throughout, the article is narrow, and every
section is introduced by a rule rather than by a component — the layout a reader already
knows how to scan, so the theme can stay out of the way of the writing.
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
export function magazinePageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="ma-magazine-shell">
      <header className="ma-magazine-header">
        <a className="ma-magazine-brand" href="/">
          {site.name}
        </a>
        <span className="ma-magazine-issue">{page.page_type}</span>
      </header>
      <div className="ma-magazine-rule" aria-hidden="true" />

      <main className="ma-magazine-main">
        <article className="ma-magazine-article">
          <p className="ma-magazine-eyebrow">{page.page_type}</p>
          <h1 className="ma-magazine-title">{revision.title}</h1>
          {revision.summary ? <p className="ma-magazine-standfirst">{revision.summary}</p> : null}
          <p className="ma-magazine-meta">
            <span>Revision {revision.revision_no}</span>
            {published ? (
              <>
                <span aria-hidden="true"> · </span>
                <time dateTime={revision.published_at ?? undefined}>{published}</time>
              </>
            ) : null}
          </p>
          <div className="ma-magazine-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
        </article>
      </main>

      <footer className="ma-magazine-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
