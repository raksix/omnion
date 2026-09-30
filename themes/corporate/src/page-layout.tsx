/**
 * Corporate — An institutional company site: a full-width masthead, a narrow reading column and section rules.
 *
 * An institutional company site. The masthead is a full-width band above a hairline —
the shape a reader has seen on ten thousand company sites, which is the point: the
theme's job is to be the most credible thing on the page, and the layout is a full-bleed
coloured band, a narrow measure and a rule above every section rather than a hero.
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
export function corporatePageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="co-corporate-shell">
      <div className="co-corporate-band">
        <a className="co-corporate-wordmark" href="/">
          {site.name}
        </a>
        <span className="co-corporate-band-tag">Official site</span>
      </div>
      <div className="co-corporate-rule" />
      <header className="co-corporate-header">
        <a className="co-corporate-brand" href="/">
          {site.name}
        </a>
        <span className="co-corporate-kind">{page.page_type}</span>
      </header>

      <main className="co-corporate-main">
        <article className="co-corporate-article">
          <p className="co-corporate-eyebrow">{page.page_type}</p>
          <h1 className="co-corporate-title">{revision.title}</h1>
          {revision.summary ? <p className="co-corporate-summary">{revision.summary}</p> : null}
          <p className="co-corporate-meta">
            <span>Revision {revision.revision_no}</span>
            {published ? (
              <>
                <span aria-hidden="true"> · </span>
                <time dateTime={revision.published_at ?? undefined}>{published}</time>
              </>
            ) : null}
          </p>
          <div className="co-corporate-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
        </article>
      </main>

      <footer className="co-corporate-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
