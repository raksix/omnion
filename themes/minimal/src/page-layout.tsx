/**
 * The Minimal theme's page layout.
 *
 * One column, generous spacing, a serif headline — the whole page as semantic markup with
 * `mn-*` hooks the stylesheet owns. The body is plain text for now: blank lines separate
 * paragraphs, everything else is preserved by the stylesheet (`white-space: pre-line`). The
 * block editor replaces this renderer in a later phase without touching the contract.
 */
import type { PageLayoutProps } from "@omnion/theme-sdk";

/** Split a plain-text body into paragraphs on blank lines. */
export function bodyParagraphs(body: string): string[] {
  return body
    .split(/\n\s*\n/)
    .map((block) => block.trim())
    .filter((block) => block.length > 0);
}

/** Format a publish timestamp for the page's own locale-independent line. */
function formatPublished(value: string | null): string | null {
  if (!value) {
    return null;
  }
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return null;
  }
  return new Intl.DateTimeFormat("en-GB", {
    day: "numeric",
    month: "long",
    year: "numeric",
    timeZone: "UTC",
  }).format(date);
}

/** Render one published page. */
export function MinimalPageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="mn-shell">
      <header className="mn-header">
        <a className="mn-brand" href="/">
          {site.name}
        </a>
        <span className="mn-kind">{page.page_type}</span>
      </header>

      <main className="mn-main">
        <article className="mn-article">
          <h1 className="mn-title">{revision.title}</h1>
          {revision.summary ? <p className="mn-summary">{revision.summary}</p> : null}
          <p className="mn-meta">
            <span>Revision {revision.revision_no}</span>
            {published ? (
              <>
                <span aria-hidden="true"> · </span>
                <time dateTime={revision.published_at ?? undefined}>{published}</time>
              </>
            ) : null}
          </p>
          <div className="mn-body">
            {paragraphs.map((paragraph, index) => (
              <p key={index}>{paragraph}</p>
            ))}
          </div>
        </article>
      </main>

      <footer className="mn-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
