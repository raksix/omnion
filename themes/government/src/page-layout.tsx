/**
 * Government — A public-sector portal: a three-part institutional header, high contrast and a visible focus ring.
 *
 * A public-sector portal. A three-part masthead (identity, service, utility) is the
conventional shape for this audience and departing from it would be a legibility bug, so
the theme's own distinctiveness is contrast and a visible focus ring: this is the one
theme where a keyboard user's outline is a design feature rather than a browser default.
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
export function governmentPageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="go-government-shell">
      <div className="go-government-utility">
        <span>Official website</span>
        <span className="go-government-utility-right">{page.page_type}</span>
      </div>
      <header className="go-government-header">
        <a className="go-government-brand" href="/">
          {site.name}
        </a>
        <a className="go-government-service" href="#services">
          Services
        </a>
      </header>
      <div className="go-government-rule" aria-hidden="true" />

      <main className="go-government-main">
        <article className="go-government-article">
          <p className="go-government-eyebrow">{page.page_type}</p>
          <h1 className="go-government-title">{revision.title}</h1>
          {revision.summary ? <p className="go-government-summary">{revision.summary}</p> : null}
          <div className="go-government-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
          <p className="go-government-meta">
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

      <footer className="go-government-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
