/**
 * Startup — A launch site: a centred canvas, an oversized centred headline and a two-step call to action.
 *
 * A launch site. Everything centres, the headline is the second largest of the nine and
the only one set above a centred canvas, and the call to action is a filled pill — the
layout a product wants a visitor to read once and act on.
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
export function startupPageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="st-startup-shell">
      <header className="st-startup-header">
        <a className="st-startup-brand" href="/">
          {site.name}
        </a>
      </header>

      <main className="st-startup-main">
        <article className="st-startup-article" id="start">
          <p className="st-startup-eyebrow">{page.page_type}</p>
          <h1 className="st-startup-title">{revision.title}</h1>
          {revision.summary ? <p className="st-startup-summary">{revision.summary}</p> : null}
          <div className="st-startup-actions">
            <a className="st-startup-action" href="#top">
              Start free
            </a>
            <a className="st-startup-action-ghost" href="#top">
              See how it works
            </a>
          </div>
          <div className="st-startup-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
          <p className="st-startup-meta">
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

      <footer className="st-startup-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
