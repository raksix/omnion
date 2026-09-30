/**
 * Agency — A studio site: an oversized display line, wide gutters and a card grid that breaks the measure.
 *
 * A studio site. The display line is the largest of the nine and the measure is the
narrowest, because an agency's own words should be the only thing competing for
attention; the card grid deliberately breaks out of the text column instead of sitting
inside it.
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
export function agencyPageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="ag-agency-shell">
      <header className="ag-agency-header">
        <a className="ag-agency-brand" href="/">
          {site.name}
        </a>
        <a className="ag-agency-contact" href="#contact">
          Start a project
        </a>
      </header>

      <main className="ag-agency-main">
        <article className="ag-agency-article">
          <p className="ag-agency-eyebrow">{page.page_type}</p>
          <h1 className="ag-agency-title">{revision.title}</h1>
          {revision.summary ? <p className="ag-agency-summary">{revision.summary}</p> : null}
          <div className="ag-agency-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
          <p className="ag-agency-meta">
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

      <footer className="ag-agency-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
