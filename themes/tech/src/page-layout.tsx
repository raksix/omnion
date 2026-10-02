/**
 * Tech — A product site for software: a dark-canvas hero, a mono accent line and a dense type scale.
 *
 * A product site for software. The header is a bar with the wordmark left and a pill
right, the title is set at the tightest tracking of the nine (product names are read as
shapes), and cards are raised off the canvas with a shadow rather than outlined.
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
export function techPageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="te-tech-shell">
      <header className="te-tech-header">
        <a className="te-tech-brand" href="/">
          <span className="te-tech-brand-dot" aria-hidden="true" />
          {site.name}
        </a>
        <a className="te-tech-pill" href="#start">
          Get started
        </a>
      </header>

      <main className="te-tech-main">
        <article className="te-tech-article">
          <p className="te-tech-eyebrow">{page.page_type}</p>
          <h1 className="te-tech-title">{revision.title}</h1>
          {revision.summary ? <p className="te-tech-summary">{revision.summary}</p> : null}
          <p className="te-tech-meta">
            <span>Revision {revision.revision_no}</span>
            {published ? (
              <>
                <span aria-hidden="true"> · </span>
                <time dateTime={revision.published_at ?? undefined}>{published}</time>
              </>
            ) : null}
          </p>
          <div className="te-tech-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
        </article>
      </main>

      <footer className="te-tech-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
