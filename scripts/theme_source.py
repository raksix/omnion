"""
The per-theme half of `scripts/author-themes.py` — the layouts and the stylesheets.

Everything in `author-themes.py` is a contract every theme shares. Everything HERE is the
design, and it is deliberately written nine times rather than templated once: a loop over a
`layout` field would produce nine themes with one layout, which is the "colour-swapped
clones" the owner brief names as the failure mode in docs/03-FRONTEND.md.

What genuinely varies per theme, and why the walkthrough can see it:

  * the header — a band, a bar, a bare line, a centred lockup, a rule, a utility strip, a
    sticky rail, a minimal wordmark, a three-part institutional masthead;
  * the article frame — a narrow measure, a wide one, a sidebar beside the text, a centred
    canvas;
  * the title — scale, weight, tracking, and the face it is set in (serif or sans);
  * the card system — bordered, elevated, outline, framed, hairline, product, plain,
    gradient, listed;
  * the tokens — a different palette and a different dark mode for each.
"""
from __future__ import annotations


def prefix_initials(key: str) -> str:
    """The SDK's own rule (tools/cli/src/create_theme.rs `prefix_initials`), restated.

    Two themes that share an initials prefix share a class namespace in one bundle, so the
    dash is skipped rather than counted. A script that reimplemented this loosely is how
    `non-profit` and `nonprofit` end up both prefixed `no` — the exact defect the CLI's own
    test caught, reproduced by a second copy of the rule.

    The CLI is the runtime scaffolder and cannot import from a script directory, so the rule
    is stated in both places deliberately, and `scripts/author-themes.py` asserts they agree
    rather than trusting that they do.
    """
    letters = [c for c in key if c.isalpha()]
    return "".join(letters[:2]).lower()


def class_prefix(key: str) -> str:
    """The class prefix a theme's stylesheet owns, and the only handle on its own stylesheet."""
    return f"{prefix_initials(key)}-{key}"


# ---------------------------------------------------------------------------------------------
# Shared React helpers
# ---------------------------------------------------------------------------------------------

PREAMBLE = '''/**
 * {name} — {blurb}
 *
 * {rationale}
 */
import type {{ PageLayoutProps }} from "@omnion/theme-sdk";
import type {{ ContentBlock }} from "@omnion/types";

import {{ BlockTree, bodyParagraphs }} from "./block-renderer";

/** Format a publish timestamp for the page's own locale-independent line. */
function formatPublished(value: string | null): string | null {{
  if (!value) return null;
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return null;
  return new Intl.DateTimeFormat("en-GB", {{
    day: "numeric",
    month: "long",
    year: "numeric",
    timeZone: "UTC",
  }}).format(date);
}}

/** The revision's block tree, or an empty list when the page was published before blocks. */
function blocksOf(revision: {{ blocks?: unknown }}): ContentBlock[] {{
  return Array.isArray(revision.blocks) ? (revision.blocks as ContentBlock[]) : [];
}}

export {{ bodyParagraphs }};
'''


def preamble(theme: dict) -> str:
    return PREAMBLE.format(
        name=theme["name"],
        blurb=theme["description"],
        rationale=RATIONALES[theme["key"]],
    )


RATIONALES = {
    "corporate": (
        "An institutional company site. The masthead is a full-width band above a hairline —\n"
        "the shape a reader has seen on ten thousand company sites, which is the point: the\n"
        "theme's job is to be the most credible thing on the page, and the layout is a full-bleed\n"
        "coloured band, a narrow measure and a rule above every section rather than a hero."
    ),
    "tech": (
        "A product site for software. The header is a bar with the wordmark left and a pill\n"
        "right, the title is set at the tightest tracking of the nine (product names are read as\n"
        "shapes), and cards are raised off the canvas with a shadow rather than outlined."
    ),
    "agency": (
        "A studio site. The display line is the largest of the nine and the measure is the\n"
        "narrowest, because an agency's own words should be the only thing competing for\n"
        "attention; the card grid deliberately breaks out of the text column instead of sitting\n"
        "inside it."
    ),
    "portfolio": (
        "A personal site. Serif titles against a sans body, a centred header lockup, no radius\n"
        "anywhere, and work presented as numbered entries — a person's output has a shape, and a\n"
        "card grid with shadows would make it look like a product catalogue."
    ),
    "magazine": (
        "An editorial site. The body face is a serif throughout, the article is narrow, and every\n"
        "section is introduced by a rule rather than by a component — the layout a reader already\n"
        "knows how to scan, so the theme can stay out of the way of the writing."
    ),
    "commerce": (
        "A storefront. The header is a utility strip: small type, tight spacing, everything it\n"
        "carries the same weight. The article frame is the widest of the nine because a grid of\n"
        "products needs the room, and prices are set with a tabular figure so columns of them line\n"
        "up."
    ),
    "documentation": (
        "Docs. A sticky header and a table-of-contents rail beside the article, so a long page\n"
        "has a spine; the line height is the loosest of the nine, because a documentation page is\n"
        "read rather than skimmed and code has to breathe."
    ),
    "startup": (
        "A launch site. Everything centres, the headline is the second largest of the nine and\n"
        "the only one set above a centred canvas, and the call to action is a filled pill — the\n"
        "layout a product wants a visitor to read once and act on."
    ),
    "government": (
        "A public-sector portal. A three-part masthead (identity, service, utility) is the\n"
        "conventional shape for this audience and departing from it would be a legibility bug, so\n"
        "the theme's own distinctiveness is contrast and a visible focus ring: this is the one\n"
        "theme where a keyboard user's outline is a design feature rather than a browser default."
    ),
}

# ---------------------------------------------------------------------------------------------
# The nine layouts. Each is written out; none is generated from a field.
# ---------------------------------------------------------------------------------------------

LAYOUTS: dict[str, str] = {}

LAYOUTS["corporate"] = '''
/** Render one published page. */
export function corporatePageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="{p}-shell">
      <div className="{p}-band">
        <a className="{p}-wordmark" href="/">
          {site.name}
        </a>
        <span className="{p}-band-tag">Official site</span>
      </div>
      <div className="{p}-rule" />
      <header className="{p}-header">
        <a className="{p}-brand" href="/">
          {site.name}
        </a>
        <span className="{p}-kind">{page.page_type}</span>
      </header>

      <main className="{p}-main">
        <article className="{p}-article">
          <p className="{p}-eyebrow">{page.page_type}</p>
          <h1 className="{p}-title">{revision.title}</h1>
          {revision.summary ? <p className="{p}-summary">{revision.summary}</p> : null}
          <p className="{p}-meta">
            <span>Revision {revision.revision_no}</span>
            {published ? (
              <>
                <span aria-hidden="true"> · </span>
                <time dateTime={revision.published_at ?? undefined}>{published}</time>
              </>
            ) : null}
          </p>
          <div className="{p}-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
        </article>
      </main>

      <footer className="{p}-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
'''

LAYOUTS["tech"] = '''
/** Render one published page. */
export function techPageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="{p}-shell">
      <header className="{p}-header">
        <a className="{p}-brand" href="/">
          <span className="{p}-brand-dot" aria-hidden="true" />
          {site.name}
        </a>
        <a className="{p}-pill" href="#start">
          Get started
        </a>
      </header>

      <main className="{p}-main">
        <article className="{p}-article">
          <p className="{p}-eyebrow">{page.page_type}</p>
          <h1 className="{p}-title">{revision.title}</h1>
          {revision.summary ? <p className="{p}-summary">{revision.summary}</p> : null}
          <p className="{p}-meta">
            <span>Revision {revision.revision_no}</span>
            {published ? (
              <>
                <span aria-hidden="true"> · </span>
                <time dateTime={revision.published_at ?? undefined}>{published}</time>
              </>
            ) : null}
          </p>
          <div className="{p}-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
        </article>
      </main>

      <footer className="{p}-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
'''

LAYOUTS["agency"] = '''
/** Render one published page. */
export function agencyPageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="{p}-shell">
      <header className="{p}-header">
        <a className="{p}-brand" href="/">
          {site.name}
        </a>
        <a className="{p}-contact" href="#contact">
          Start a project
        </a>
      </header>

      <main className="{p}-main">
        <article className="{p}-article">
          <p className="{p}-eyebrow">{page.page_type}</p>
          <h1 className="{p}-title">{revision.title}</h1>
          {revision.summary ? <p className="{p}-summary">{revision.summary}</p> : null}
          <div className="{p}-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
          <p className="{p}-meta">
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

      <footer className="{p}-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
'''

LAYOUTS["portfolio"] = '''
/** Render one published page. */
export function portfolioPageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="{p}-shell">
      <header className="{p}-header">
        <a className="{p}-brand" href="/">
          {site.name}
        </a>
        <span className="{p}-kind">{page.page_type}</span>
      </header>

      <main className="{p}-main">
        <article className="{p}-article">
          <h1 className="{p}-title">{revision.title}</h1>
          {revision.summary ? <p className="{p}-summary">{revision.summary}</p> : null}
          <div className="{p}-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
          <p className="{p}-meta">
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

      <footer className="{p}-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
'''

LAYOUTS["magazine"] = '''
/** Render one published page. */
export function magazinePageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="{p}-shell">
      <header className="{p}-header">
        <a className="{p}-brand" href="/">
          {site.name}
        </a>
        <span className="{p}-issue">{page.page_type}</span>
      </header>
      <div className="{p}-rule" aria-hidden="true" />

      <main className="{p}-main">
        <article className="{p}-article">
          <p className="{p}-eyebrow">{page.page_type}</p>
          <h1 className="{p}-title">{revision.title}</h1>
          {revision.summary ? <p className="{p}-standfirst">{revision.summary}</p> : null}
          <p className="{p}-meta">
            <span>Revision {revision.revision_no}</span>
            {published ? (
              <>
                <span aria-hidden="true"> · </span>
                <time dateTime={revision.published_at ?? undefined}>{published}</time>
              </>
            ) : null}
          </p>
          <div className="{p}-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
        </article>
      </main>

      <footer className="{p}-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
'''

LAYOUTS["commerce"] = '''
/** Render one published page. */
export function commercePageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="{p}-shell">
      <header className="{p}-header">
        <a className="{p}-brand" href="/">
          {site.name}
        </a>
        <nav className="{p}-utility" aria-label="Store">
          <a href="#catalogue">Catalogue</a>
          <a href="#cart">Cart</a>
        </nav>
      </header>
      <div className="{p}-rule" aria-hidden="true" />

      <main className="{p}-main">
        <article className="{p}-article">
          <p className="{p}-eyebrow">{page.page_type}</p>
          <h1 className="{p}-title">{revision.title}</h1>
          {revision.summary ? <p className="{p}-summary">{revision.summary}</p> : null}
          <div className="{p}-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
          <p className="{p}-meta">
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

      <footer className="{p}-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
'''

LAYOUTS["documentation"] = '''
/** Render one published page. */
export function documentationPageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="{p}-shell">
      <header className="{p}-header">
        <a className="{p}-brand" href="/">
          {site.name}
        </a>
        <span className="{p}-version">docs</span>
      </header>

      <div className="{p}-frame">
        <nav className="{p}-toc" aria-label="On this page">
          <p className="{p}-toc-title">On this page</p>
          <a href="#top">{revision.title}</a>
          <span className="{p}-toc-kind">{page.page_type}</span>
        </nav>

        <main className="{p}-main">
          <article className="{p}-article">
            <h1 className="{p}-title">{revision.title}</h1>
            {revision.summary ? <p className="{p}-summary">{revision.summary}</p> : null}
            <div className="{p}-body">
              {blocks.length > 0 ? (
                <BlockTree blocks={blocks} />
              ) : (
                paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
              )}
            </div>
            <p className="{p}-meta">
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

      <footer className="{p}-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
'''

LAYOUTS["startup"] = '''
/** Render one published page. */
export function startupPageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="{p}-shell">
      <header className="{p}-header">
        <a className="{p}-brand" href="/">
          {site.name}
        </a>
      </header>

      <main className="{p}-main">
        <article className="{p}-article" id="start">
          <p className="{p}-eyebrow">{page.page_type}</p>
          <h1 className="{p}-title">{revision.title}</h1>
          {revision.summary ? <p className="{p}-summary">{revision.summary}</p> : null}
          <div className="{p}-actions">
            <a className="{p}-action" href="#top">
              Start free
            </a>
            <a className="{p}-action-ghost" href="#top">
              See how it works
            </a>
          </div>
          <div className="{p}-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
          <p className="{p}-meta">
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

      <footer className="{p}-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
'''

LAYOUTS["government"] = '''
/** Render one published page. */
export function governmentPageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  const blocks = blocksOf(revision);
  const paragraphs = bodyParagraphs(revision.body);
  const published = formatPublished(revision.published_at);

  return (
    <div className="{p}-shell">
      <div className="{p}-utility">
        <span>Official website</span>
        <span className="{p}-utility-right">{page.page_type}</span>
      </div>
      <header className="{p}-header">
        <a className="{p}-brand" href="/">
          {site.name}
        </a>
        <a className="{p}-service" href="#services">
          Services
        </a>
      </header>
      <div className="{p}-rule" aria-hidden="true" />

      <main className="{p}-main">
        <article className="{p}-article">
          <p className="{p}-eyebrow">{page.page_type}</p>
          <h1 className="{p}-title">{revision.title}</h1>
          {revision.summary ? <p className="{p}-summary">{revision.summary}</p> : null}
          <div className="{p}-body">
            {blocks.length > 0 ? (
              <BlockTree blocks={blocks} />
            ) : (
              paragraphs.map((paragraph, index) => <p key={index}>{paragraph}</p>)
            )}
          </div>
          <p className="{p}-meta">
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

      <footer className="{p}-footer">
        <span>{site.name}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}
'''


def layout_tsx(theme: dict) -> str:
    """The preamble (helpers) plus this theme's own markup."""
    return preamble(theme) + LAYOUTS[theme["key"]].replace("{p}", class_prefix(theme["key"]))


# ---------------------------------------------------------------------------------------------
# The per-theme half of the authoring script — the layouts and the stylesheets.
# ---------------------------------------------------------------------------------------------

TOKENS_CSS = """/* The theme's tokens. A theme is its palette: everything below reads these, and a
   settings revision overrides them on the same custom property names, which is why they are
   declared once here rather than spelled into each rule.

   ## Why these rules are scoped to `data-theme`

   All ten bundled stylesheets are loaded by the renderer's single import graph, because a
   CSS import is static and `resolveTheme()` is a runtime call. Ten sheets declaring `body`
   would therefore fight, and the winner would be whichever sheet the bundler happened to
   order last — a site's theme would render with whichever palette sorted highest, which is
   the "colour-swapped clones" failure in its purest form.

   Scoping to `html[data-theme="<key>"]` is what makes one bundle carry ten themes: only the
   sheet whose key matches the active theme matches anything. `data-theme` is written by
   `apps/web/app/layout.tsx` from the same `resolveTheme()` the page is drawn with, so the
   stylesheet and the markup can never be chosen by different code.
*/
html[data-theme="{key}"] {{
{tokens}
  --{p}-font-body: {font_body};
  --{p}-font-title: {font_title};
  --{p}-measure: {measure};
  --{p}-title-scale: {title_scale};
  --{p}-title-weight: {title_weight};
  --{p}-title-tracking: {title_tracking};
  --{p}-radius: {radius};
  --{p}-base-size: {base_size};
  --{p}-container: 1120px;
}}

/* Dark mode is a token swap, never a second set of rules: the pair lives in the manifest and
   the stylesheet only decides which pair the visitor gets. */
@media (prefers-color-scheme: dark) {{
  html[data-theme="{key}"] {{
{dark_tokens}
  }}
}}
"""

RESET_CSS = """
/* The reset is scoped to the active theme for the reason the tokens are: all ten sheets ship
   in one bundle, and an unscoped `body` rule would let every one of them style every page. */
html[data-theme="{key}"] *,
html[data-theme="{key}"] *::before,
html[data-theme="{key}"] *::after {{
  box-sizing: border-box;
}}

html[data-theme="{key}"] {{
  background-color: var(--{p}-canvas);
  color-scheme: light dark;
}}

html[data-theme="{key}"] body {{
  margin: 0;
  background: var(--{p}-canvas);
  color: var(--{p}-ink);
  font-family: var(--{p}-font-body);
  font-size: var(--{p}-base-size);
  line-height: 1.65;
  -webkit-font-smoothing: antialiased;
}}

html[data-theme="{key}"] ::selection {{
  background-color: var(--{p}-accent-soft);
}}

html[data-theme="{key}"] a {{
  color: var(--{p}-accent);
  text-underline-offset: 3px;
}}

/* The focus ring is the one rule every theme shares unscoped in spirit and scoped in practice:
   a theme that removed it would fail the accessibility bar rather than look different. */
html[data-theme="{key}"] :focus-visible {{
  outline: 3px solid var(--{p}-accent);
  outline-offset: 2px;
}}

html[data-theme="{key}"] img {{
  max-width: 100%;
  height: auto;
}}
"""

SHELL_CSS = """
/* The shell: a full-height column that pins the footer to the bottom on a short page. */
.{p}-shell {{
  display: flex;
  min-height: 100dvh;
  flex-direction: column;
}}

.{p}-header,
.{p}-footer {{
  display: flex;
  width: 100%;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
  padding: 1.25rem 1.5rem;
}}

.{p}-header {{
  {header_css}
}}

.{p}-footer {{
  margin-top: auto;
  border-top: 1px solid var(--{p}-line);
  color: var(--{p}-muted);
  font-size: 0.8125rem;
}}

.{p}-main {{
  width: 100%;
  max-width: var(--{p}-container);
  margin-inline: auto;
  padding: 3rem 1.5rem 4rem;
}}

.{p}-article {{
  max-width: var(--{p}-measure);
}}

.{p}-title {{
  margin: 0;
  font-family: var(--{p}-font-title);
  font-size: var(--{p}-title-scale);
  font-weight: var(--{p}-title-weight);
  line-height: 1.1;
  letter-spacing: var(--{p}-title-tracking);
}}

.{p}-eyebrow {{
  margin: 0 0 0.75rem;
  color: var(--{p}-accent);
  font-size: 0.75rem;
  font-weight: 600;
  letter-spacing: 0.09em;
  text-transform: uppercase;
}}

.{p}-summary {{
  margin: 1rem 0 0;
  color: var(--{p}-muted);
  font-size: 1.15em;
}}

.{p}-meta {{
  margin: 1.5rem 0 0;
  color: var(--{p}-muted);
  font-size: 0.8125rem;
  letter-spacing: 0.02em;
}}

.{p}-body {{
  margin-top: 2.5rem;
}}

.{p}-body p {{
  margin: 0 0 1.25rem;
  white-space: pre-line;
}}

.{p}-body p:last-child {{
  margin-bottom: 0;
}}
"""

BLOCKS_CSS = """
/* Blocks (REQ-063). The markup is the SDK's; everything here is this theme's styling of it. */
.{p}-block {{
  margin: 0 0 1.75rem;
}}

.{p}-block:last-child {{
  margin-bottom: 0;
}}

.{p}-heading {{
  margin: 2.5rem 0 0.75rem;
  font-family: var(--{p}-font-title);
  line-height: 1.2;
  letter-spacing: -0.01em;
}}

.{p}-heading:first-child {{
  margin-top: 0;
}}

.{p}-align-center {{
  text-align: center;
}}

.{p}-align-right {{
  text-align: right;
}}

.{p}-figure {{
  margin-inline: 0;
}}

.{p}-figure figcaption {{
  margin-top: 0.5rem;
  color: var(--{p}-muted);
  font-size: 0.8125rem;
}}

.{p}-testimonial {{
  margin-inline: 0;
  padding: 1.25rem 1.5rem;
  border-left: 3px solid var(--{p}-accent);
  background: var(--{p}-surface);
  border-radius: var(--{p}-radius);
}}

.{p}-testimonial blockquote {{
  margin: 0;
  font-size: 1.05em;
}}

.{p}-testimonial figcaption {{
  margin-top: 0.5rem;
  color: var(--{p}-muted);
  font-size: 0.8125rem;
}}

.{p}-gallery {{
  display: grid;
  gap: 0.75rem;
  grid-template-columns: repeat(3, 1fr);
}}

.{p}-gallery img {{
  border-radius: var(--{p}-radius);
}}

.{p}-columns {{
  display: grid;
  gap: 1.5rem;
  grid-template-columns: repeat(2, 1fr);
}}

.{p}-cta {{
  padding: 1.75rem;
  border-radius: var(--{p}-radius);
  background: var(--{p}-accent-soft);
}}

.{p}-cta h2 {{
  margin: 0 0 0.5rem;
}}

.{p}-cta p {{
  margin: 0 0 1rem;
  color: var(--{p}-muted);
}}

.{p}-cta-link {{
  display: inline-block;
  padding: 0.5rem 1rem;
  border-radius: var(--{p}-radius);
  background: var(--{p}-accent);
  color: var(--{p}-canvas);
  font-weight: 600;
  text-decoration: none;
}}

.{p}-cards {{
  display: grid;
  gap: 1.25rem;
  grid-template-columns: repeat(3, 1fr);
}}

.{p}-card {{
  padding: 1.25rem;
  border: 1px solid var(--{p}-line);
  border-radius: var(--{p}-radius);
  background: var(--{p}-surface);
}}

.{p}-card h3 {{
  margin: 0 0 0.5rem;
}}

.{p}-card p {{
  margin: 0 0 0.75rem;
  color: var(--{p}-muted);
  font-size: 0.9375rem;
}}

.{p}-plans {{
  display: grid;
  gap: 1.25rem;
  grid-template-columns: repeat(3, 1fr);
}}

.{p}-plans ul {{
  margin: 0;
  padding: 0;
  list-style: none;
}}

.{p}-plan {{
  padding: 1.25rem;
  border: 1px solid var(--{p}-line);
  border-radius: var(--{p}-radius);
}}

.{p}-plan h3 {{
  margin: 0;
}}

.{p}-price {{
  margin: 0.25rem 0 0.75rem;
  font-size: 1.5rem;
  font-weight: 700;
  font-variant-numeric: tabular-nums;
}}

.{p}-plan ul li {{
  padding: 0.25rem 0;
  color: var(--{p}-muted);
  font-size: 0.9375rem;
}}

.{p}-faq > div {{
  padding: 0.875rem 0;
  border-bottom: 1px solid var(--{p}-line);
}}

.{p}-faq dt {{
  font-weight: 600;
}}

.{p}-faq dd {{
  margin: 0.35rem 0 0;
  color: var(--{p}-muted);
}}

.{p}-raw pre {{
  overflow-x: auto;
  padding: 1rem;
  border-radius: var(--{p}-radius);
  background: var(--{p}-surface);
  font-size: 0.875rem;
}}

.{p}-raw-note {{
  margin: 0 0 0.5rem;
  color: var(--{p}-muted);
  font-size: 0.75rem;
  text-transform: uppercase;
  letter-spacing: 0.06em;
}}

.{p}-unknown {{
  padding: 0.75rem 1rem;
  border: 1px dashed var(--{p}-line);
  border-radius: var(--{p}-radius);
  color: var(--{p}-muted);
  font-size: 0.9375rem;
}}

@media (max-width: 720px) {{
  .{p}-gallery,
  .{p}-columns,
  .{p}-cards,
  .{p}-plans {{
    grid-template-columns: 1fr;
  }}
}}
"""

# Per-theme overrides: the parts of the shell that are the theme's own shape.
HEADER_CSS: dict[str, str] = {
    "corporate": "border-bottom: 1px solid var(--{p}-line);",
    "tech": "border-bottom: 1px solid var(--{p}-line); background: var(--{p}-surface);",
    "agency": "border-bottom: 0; padding-inline: 0;",
    "portfolio": "justify-content: center; flex-direction: column; gap: 0.25rem;",
    "magazine": "border-bottom: 0; padding-bottom: 0.75rem;",
    "commerce": "border-bottom: 0; padding-block: 0.75rem; font-size: 0.875rem;",
    "documentation": "position: sticky; top: 0; z-index: 10; background: var(--{p}-canvas); border-bottom: 1px solid var(--{p}-line);",
    "startup": "border-bottom: 0; justify-content: center;",
    "government": "border-bottom: 0; padding-bottom: 1rem;",
}

# Per-theme extras: the class names only this theme's markup uses.
EXTRA_CSS: dict[str, str] = {
    "corporate": """
/* The full-width band above the masthead: the identity a company page leads with. */
.{p}-band {{
  display: flex;
  width: 100%;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
  padding: 0.75rem 1.5rem;
  background: var(--{p}-accent);
  color: var(--{p}-canvas);
  font-size: 0.8125rem;
}}

.{p}-wordmark {{
  font-weight: 600;
  text-decoration: none;
  color: inherit;
}}

.{p}-band-tag {{
  opacity: 0.85;
  letter-spacing: 0.04em;
  text-transform: uppercase;
  font-size: 0.6875rem;
}}

.{p}-brand {{
  color: var(--{p}-ink);
  font-weight: 600;
  letter-spacing: -0.01em;
  text-decoration: none;
}}

.{p}-kind {{
  border: 1px solid var(--{p}-line);
  border-radius: 999px;
  padding: 0.125rem 0.625rem;
  font-size: 0.75rem;
  text-transform: uppercase;
  letter-spacing: 0.04em;
}}

.{p}-rule {{
  width: 100%;
  height: 2px;
  background: var(--{p}-ink);
}}
""",
    "tech": """
.{p}-brand {{
  display: inline-flex;
  align-items: center;
  gap: 0.5rem;
  color: var(--{p}-ink);
  font-weight: 700;
  letter-spacing: -0.02em;
  text-decoration: none;
}}

/* The mark is a shape rather than a letter, so it survives a wordmark of any length. */
.{p}-brand-dot {{
  width: 0.75rem;
  height: 0.75rem;
  border-radius: 3px;
  background: var(--{p}-accent);
}}

.{p}-pill {{
  padding: 0.4rem 0.9rem;
  border-radius: 999px;
  background: var(--{p}-accent);
  color: var(--{p}-canvas);
  font-size: 0.875rem;
  font-weight: 600;
  text-decoration: none;
}}

/* Raised, not outlined: a product site's cards sit above the canvas. */
.{p}-card {{
  border: 0;
  background: var(--{p}-surface);
  box-shadow: 0 1px 2px rgb(0 0 0 / 0.06), 0 8px 24px rgb(0 0 0 / 0.06);
}}
""",
    "agency": """
.{p}-header {{
  max-width: var(--{p}-container);
  margin-inline: auto;
}}

.{p}-brand {{
  color: var(--{p}-ink);
  font-weight: 800;
  font-size: 1.125rem;
  letter-spacing: -0.03em;
  text-decoration: none;
}}

.{p}-contact {{
  color: var(--{p}-ink);
  font-size: 0.875rem;
  font-weight: 600;
  text-decoration: none;
  border-bottom: 2px solid var(--{p}-accent);
  padding-bottom: 0.125rem;
}}

/* The card grid breaks the text column on purpose — a studio's work is not a footnote to
   its copy, and the break is what makes the page look composed rather than templated. */
.{p}-main .{p}-cards,
.{p}-main .{p}-gallery {{
  width: min(72rem, 100vw - 3rem);
  max-width: none;
  margin-left: 50%;
  transform: translateX(-50%);
}}

.{p}-card {{
  border: 2px solid var(--{p}-ink);
  background: transparent;
  border-radius: 0;
}}
""",
    "portfolio": """
.{p}-header {{
  flex-direction: column;
  gap: 0.25rem;
}}

.{p}-brand {{
  color: var(--{p}-ink);
  font-family: var(--{p}-font-title);
  font-size: 1.5rem;
  letter-spacing: -0.01em;
  text-decoration: none;
}}

.{p}-kind {{
  color: var(--{p}-muted);
  font-size: 0.75rem;
  letter-spacing: 0.12em;
  text-transform: uppercase;
}}

.{p}-card {{
  border: 1px solid var(--{p}-ink);
  background: transparent;
  border-radius: 0;
}}
""",
    "magazine": """
.{p}-brand {{
  font-family: var(--{p}-font-title);
  font-size: 1.75rem;
  font-weight: 700;
  letter-spacing: -0.02em;
  color: var(--{p}-ink);
  text-decoration: none;
}}

.{p}-issue {{
  color: var(--{p}-muted);
  font-size: 0.75rem;
  letter-spacing: 0.14em;
  text-transform: uppercase;
}}

/* The section rule: the editorial device that replaces a component. */
.{p}-rule {{
  width: 100%;
  height: 3px;
  margin-top: 0.5rem;
  background: var(--{p}-ink);
}}

.{p}-standfirst {{
  margin: 1.25rem 0 0;
  font-size: 1.25em;
  line-height: 1.45;
  font-style: italic;
  color: var(--{p}-muted);
}}

.{p}-heading {{
  padding-top: 1.5rem;
  border-top: 1px solid var(--{p}-line);
  font-size: 1.6em;
}}

.{p}-card {{
  border: 0;
  border-top: 1px solid var(--{p}-line);
  border-radius: 0;
  background: transparent;
  padding-inline: 0;
}}
""",
    "commerce": """
.{p}-brand {{
  color: var(--{p}-ink);
  font-weight: 700;
  letter-spacing: -0.01em;
  text-decoration: none;
}}

.{p}-utility {{
  display: flex;
  gap: 1.25rem;
}}

.{p}-utility a {{
  color: var(--{p}-ink);
  font-size: 0.875rem;
  text-decoration: none;
}}

.{p}-utility a:hover {{
  color: var(--{p}-accent);
}}

.{p}-rule {{
  width: 100%;
  height: 1px;
  background: var(--{p}-ink);
}}

/* The article runs the container's full width so a product grid has somewhere to go. */
.{p}-article {{
  max-width: var(--{p}-measure);
}}

.{p}-main {{
  max-width: 1200px;
}}

.{p}-card {{
  border: 1px solid var(--{p}-line);
  border-radius: var(--{p}-radius);
  background: var(--{p}-surface);
}}

/* Prices in a column have to line up. */
.{p}-price,
.{p}-card h3 + p {{
  font-variant-numeric: tabular-nums;
}}
""",
    "documentation": """
.{p}-brand {{
  color: var(--{p}-ink);
  font-weight: 700;
  text-decoration: none;
}}

.{p}-version {{
  padding: 0.125rem 0.5rem;
  border-radius: 4px;
  background: var(--{p}-accent-soft);
  color: var(--{p}-accent);
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 0.75rem;
  font-weight: 600;
}}

/* A long page needs a spine: the rail is sticky beside the article, not above it. */
.{p}-frame {{
  display: grid;
  gap: 2.5rem;
  width: 100%;
  max-width: var(--{p}-container);
  margin-inline: auto;
  padding: 2rem 1.5rem 4rem;
  grid-template-columns: 15rem minmax(0, 1fr);
}}

.{p}-toc {{
  position: sticky;
  top: 5rem;
  align-self: start;
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
  padding-right: 1.25rem;
  border-right: 1px solid var(--{p}-line);
  font-size: 0.9375rem;
}}

.{p}-toc-title {{
  margin: 0;
  color: var(--{p}-muted);
  font-size: 0.75rem;
  font-weight: 700;
  letter-spacing: 0.08em;
  text-transform: uppercase;
}}

.{p}-toc a {{
  color: var(--{p}-ink);
  text-decoration: none;
  font-weight: 600;
}}

.{p}-toc-kind {{
  color: var(--{p}-muted);
  font-size: 0.8125rem;
  text-transform: capitalize;
}}

.{p}-main {{
  padding: 0;
  max-width: none;
  min-width: 0;
}}

/* Docs are read, not skimmed. */
.{p}-body {{
  line-height: 1.75;
}}

@media (max-width: 900px) {{
  .{p}-frame {{
    grid-template-columns: 1fr;
  }}

  .{p}-toc {{
    position: static;
    border-right: 0;
    border-bottom: 1px solid var(--{p}-line);
    padding-bottom: 1rem;
  }}
}}
""",
    "startup": """
.{p}-header {{
  justify-content: center;
  padding-block: 2rem;
}}

.{p}-brand {{
  color: var(--{p}-ink);
  font-weight: 800;
  font-size: 1.25rem;
  letter-spacing: -0.03em;
  text-decoration: none;
}}

/* Everything centres: a launch page has one thing to say and one thing to press. */
.{p}-main,
.{p}-article {{
  text-align: center;
}}

.{p}-summary {{
  margin-inline: auto;
  max-width: 34rem;
}}

.{p}-actions {{
  display: flex;
  flex-wrap: wrap;
  justify-content: center;
  gap: 0.75rem;
  margin-top: 2rem;
}}

.{p}-action,
.{p}-action-ghost {{
  padding: 0.7rem 1.5rem;
  border-radius: 999px;
  font-weight: 600;
  text-decoration: none;
}}

.{p}-action {{
  background: var(--{p}-accent);
  color: var(--{p}-canvas);
}}

.{p}-action-ghost {{
  border: 1px solid var(--{p}-line);
  color: var(--{p}-ink);
}}

.{p}-meta {{
  margin-top: 2.5rem;
}}

.{p}-card {{
  border: 0;
  border-radius: var(--{p}-radius);
  background: linear-gradient(160deg, var(--{p}-accent-soft), var(--{p}-surface));
}}
""",
    "government": """
.{p}-utility {{
  display: flex;
  width: 100%;
  justify-content: space-between;
  gap: 1rem;
  padding: 0.5rem 1.5rem;
  background: var(--{p}-surface);
  color: var(--{p}-muted);
  font-size: 0.75rem;
  letter-spacing: 0.04em;
  text-transform: uppercase;
}}

.{p}-utility-right {{
  text-transform: capitalize;
}}

.{p}-brand {{
  color: var(--{p}-ink);
  font-weight: 700;
  font-size: 1.25rem;
  letter-spacing: -0.01em;
  text-decoration: none;
}}

.{p}-service {{
  padding: 0.4rem 0.9rem;
  border: 1px solid var(--{p}-accent);
  border-radius: var(--{p}-radius);
  color: var(--{p}-accent);
  font-size: 0.875rem;
  font-weight: 600;
  text-decoration: none;
}}

.{p}-rule {{
  width: 100%;
  height: 3px;
  background: var(--{p}-accent);
}}

/* Public-sector legibility: the focus ring is part of the design, not a browser default.
   The reset already draws one; this makes it wider still, which is the difference between
   "passes WCAG" and "a keyboard user can see where they are on a government page". */
html[data-theme="government"] :focus-visible {{
  outline: 3px solid var(--{p}-accent);
  outline-offset: 3px;
  border-radius: 2px;
}}

.{p}-card {{
  border: 1px solid var(--{p}-line);
  border-left: 4px solid var(--{p}-accent);
  border-radius: 2px;
  background: var(--{p}-surface);
}}

.{p}-main {{
  max-width: 1040px;
}}
""",
}


def stylesheet(theme: dict) -> str:
    p = class_prefix(theme["key"])
    key = theme["key"]
    tokens = "\n".join(
        f"  --{p}-{name}: {light};" for name, (light, _dark) in theme["tokens"].items()
    )
    dark_tokens = "\n".join(
        f"    --{p}-{name}: {dark};" for name, (_light, dark) in theme["tokens"].items()
    )
    head = f"""/**
 * {theme['name']} — {theme['description']}
 *
 * The tokens are the theme; everything below reads them. A settings revision overrides the
 * same custom property names, which is why they are declared once and never spelled into a
 * rule — two code paths computing "the effective colour" is how a preview ends up lying.
 *
 * Every element selector is scoped to `html[data-theme="{key}"]`. All ten bundled sheets are
 * loaded by the renderer's static import graph, so an unscoped `body {{ … }}` here would style
 * every theme's pages with this theme's palette. Block classes are already prefixed and need
 * no scoping; the resets, the focus ring and the media queries do.
 */
"""
    parts = [
        head,
        TOKENS_CSS.format(
            p=p,
            key=key,
            tokens=tokens,
            dark_tokens=dark_tokens,
            font_body=theme["font_body"],
            font_title=theme["font_title"],
            measure=theme["measure"],
            title_scale=theme["title_scale"],
            title_weight=theme["title_weight"],
            title_tracking=theme["title_tracking"],
            radius=theme["radius"],
            base_size=theme["base_size"],
        ),
        RESET_CSS.format(p=p, key=key),
        SHELL_CSS.format(p=p, header_css=HEADER_CSS[theme["key"]].format(p=p)),
        EXTRA_CSS[theme["key"]].format(p=p),
        BLOCKS_CSS.format(p=p),
    ]
    return "\n".join(parts)


def preview_svg(theme: dict) -> str:
    """A gallery card image. Drawn from the theme's own tokens, so it cannot drift from them."""
    p = class_prefix(theme["key"])
    canvas = theme["tokens"]["canvas"][0]
    surface = theme["tokens"]["surface"][0]
    ink = theme["tokens"]["ink"][0]
    muted = theme["tokens"]["muted"][0]
    accent = theme["tokens"]["accent"][0]
    line = theme["tokens"]["line"][0]
    weight = min(int(theme["title_weight"]) // 100, 8)
    return f"""<!-- Gallery card for the {theme['name']} theme. Generated from the theme's own
     tokens, so a palette change moves the card with it. -->
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 640 400" role="img" aria-label="{theme['name']} theme preview">
  <rect width="640" height="400" fill="{canvas}"/>
  <rect x="0" y="0" width="640" height="52" fill="{surface}"/>
  <rect x="36" y="20" width="108" height="12" rx="{theme['radius']}" fill="{ink}" opacity="0.85"/>
  <rect x="530" y="18" width="74" height="16" rx="999" fill="{accent}"/>
  <rect x="36" y="96" width="150" height="9" rx="4" fill="{accent}"/>
  <rect x="36" y="124" width="440" height="30" rx="{theme['radius']}" fill="{ink}" opacity="0.92"/>
  <rect x="36" y="162" width="360" height="30" rx="{theme['radius']}" fill="{ink}" opacity="0.92"/>
  <rect x="36" y="206" width="500" height="11" rx="5" fill="{muted}"/>
  <rect x="36" y="228" width="440" height="11" rx="5" fill="{muted}"/>
  <g>
    <rect x="36" y="272" width="176" height="84" rx="{theme['radius']}" fill="{surface}" stroke="{line}"/>
    <rect x="228" y="272" width="176" height="84" rx="{theme['radius']}" fill="{surface}" stroke="{line}"/>
    <rect x="420" y="272" width="176" height="84" rx="{theme['radius']}" fill="{accent}" opacity="0.14"/>
  </g>
  <text x="36" y="382" font-family="ui-sans-serif, system-ui, sans-serif" font-size="15" font-weight="700" fill="{ink}">{theme['name']}</text>
  <text x="604" y="382" text-anchor="end" font-family="ui-sans-serif, system-ui, sans-serif" font-size="12" fill="{muted}">weight {weight}</text>
</svg>
"""
