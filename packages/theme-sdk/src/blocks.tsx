/**
 * The block renderer every theme draws its content with (REQ-062 slice 4, REQ-063).
 *
 * A block is *content with a meaning* — a heading is a heading, a FAQ is a description list, a
 * testimonial is a figure with a caption — and that meaning belongs to the platform, not to any
 * one theme. Ten themes that each carried their own copy of the switch statement would produce
 * ten different `<dl>`s for one block type, which is the definition of a colour-swapped clone:
 * the difference between themes is meant to be layout, type and colour, and it cannot be "which
 * theme forgot that a gallery is a `figure`".
 *
 * So the switch lives here, once, and a theme supplies only what actually varies between
 * themes: the class-name prefix its stylesheet owns, and the couple of shapes where presentation
 * is the block's whole point (a magazine's card grid and a documentation theme's FAQ want
 * different wrappers around the same content).
 *
 * ## Why the markup is what it is
 *
 * Semantic elements are not a style choice. A screen reader navigating a FAQ expects a list of
 * questions and answers; a search engine indexing an article wants `article` and `h1`; a
 * browser's reader mode is useless against a wall of `div`s. The theme styles these elements —
 * it never replaces them.
 *
 * ## The prefix is a contract, not cosmetics
 *
 * Every class this module emits is `{prefix}-block`, `{prefix}-heading`, `{prefix}-figure` and so
 * on, and the stylesheet the theme ships styles exactly those. Two themes in one bundle
 * therefore cannot collide, which is why the prefix has to be unique per theme — see
 * `omnion create-theme`, which derives it from the key and now skips dashes so `non-profit` and
 * `nonprofit` do not share a namespace.
 */
import type { ContentBlock } from "@omnion/types";
import type { ComponentType } from "react";

/** The pieces of a theme the renderer needs. */
export interface BlockRendererOptions {
  /**
   * Class-name prefix the theme's stylesheet owns (`mn` for Minimal, `cg` for Corporate).
   *
   * Emitted verbatim, so it must be a valid CSS identifier fragment: letters, digits and
   * dashes, starting with a letter. The scaffolder's tests hold that rule; a hand-written theme
   * that breaks it produces class names no selector in its own stylesheet can match, and the
   * symptom is a page that renders with no styling at all rather than an error.
   */
  prefix: string;
  /**
   * How this theme draws a set of blocks. Defaults to a plain stack of `div`s the stylesheet
   * spaces out; a theme whose cards need a different element passes its own.
   */
  Container?: ComponentType<{ className: string; children: React.ReactNode }>;
}

/** What [`createBlockRenderer`] hands back. */
export interface BlockRenderer {
  /** Render a list of blocks. Returns `null` for an empty list, so it can be a child's only node. */
  BlockTree: ComponentType<{ blocks: ContentBlock[] }>;
  /** Render one block and, for a container, its children. */
  Block: ComponentType<{ block: ContentBlock }>;
  /** Split a plain-text body into paragraphs on blank lines. Every theme's body fallback needs it. */
  bodyParagraphs: (body: string) => string[];
  /** The theme's class prefix, for a stylesheet or a test that wants to assert on it. */
  prefix: string;
}

/** The text of a prop, or `""`. */
function text(block: ContentBlock, key: string): string {
  const value = block.props[key];
  return typeof value === "string" ? value : "";
}

/** A list-of-strings prop, empty when it is not one. */
function list(block: ContentBlock, key: string): string[] {
  const value = block.props[key];
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === "string")
    : [];
}

/** A number prop, with a fallback. */
function number(block: ContentBlock, key: string, fallback: number): number {
  const value = block.props[key];
  return typeof value === "number" ? value : fallback;
}

/** `?a|b|c` → `["a", "b", "c"]`; the editable list props are one entry per line. */
function parts(entry: string): string[] {
  return entry.split("|");
}

/**
 * Build a block renderer for one theme.
 *
 * A factory rather than a shared component with a `theme` prop: the ten themes are separate
 * packages that each import this, and passing the prefix as a prop would mean every block
 * re-renders whenever any theme's prefix changed, plus a runtime lookup where a closure will
 * do. More importantly, it keeps the guarantee that a theme's classes are known at author time —
 * there is no way for one theme to pass a prefix belonging to another.
 */
export function createBlockRenderer(options: BlockRendererOptions): BlockRenderer {
  const { prefix: p } = options;
  const Container = options.Container ?? (({ className, children }) => (
    <div className={className}>{children}</div>
  ));

  /**
   * Alignment a block asks for.
   *
   * Only the two values the block registry offers beyond the default are mapped; an author who
   * typed something else gets the default rather than a class no stylesheet declares.
   */
  function alignClass(block: ContentBlock): string {
    const align = text(block, "align");
    return align === "center" || align === "right" ? ` ${p}-align-${align}` : "";
  }

  /**
   * The DOM attributes a block's own settings ask for.
   *
   * Only the *addressing* settings live here — `id` and `aria-label`. The class an author typed
   * deliberately does not: a spread lands after the element's own `className`, so a `className`
   * in the spread would replace the theme's layout classes instead of joining them, and an
   * author who typed one class name the theme does not know would lose the layout the theme
   * does. It is appended by [`blockClass`] instead.
   */
  function blockAttributes(block: ContentBlock): Record<string, string> {
    const attributes: Record<string, string> = {};
    const meta = block.meta ?? {};
    const id = typeof meta.id === "string" ? meta.id.trim() : "";
    const anchor = typeof meta.anchor === "string" ? meta.anchor.trim() : "";
    if (id || anchor) {
      attributes.id = id || anchor;
    }
    if (typeof meta.aria_label === "string" && meta.aria_label.trim()) {
      attributes["aria-label"] = meta.aria_label.trim();
    }
    return attributes;
  }

  /**
   * The author's own class names, to append to a block's theme classes.
   *
   * A block's class is a hint the active theme may style, so it joins the theme's names rather
   * than replacing them — and it is not a place to write arbitrary CSS, which the REQ keeps out
   * of blocks on purpose (theme tokens own styling).
   */
  function blockClass(block: ContentBlock): string {
    const extra = typeof block.meta?.class === "string" ? block.meta.class.trim() : "";
    return extra ? ` ${extra}` : "";
  }

  /** Render one block (and, for a container, its children). */
  function Block({ block }: { block: ContentBlock }) {
    const align = alignClass(block);
    const attributes = blockAttributes(block);

    switch (block.type) {
      case "heading": {
        const level = text(block, "level") || "h2";
        const Tag = (["h1", "h2", "h3", "h4", "h5", "h6"].includes(level) ? level : "h2") as
          | "h1"
          | "h2"
          | "h3"
          | "h4"
          | "h5"
          | "h6";
        return (
          <Tag className={`${p}-block ${p}-heading${align}${blockClass(block)}`} {...attributes}>
            {text(block, "text")}
          </Tag>
        );
      }
      case "text":
        return (
          <p className={`${p}-block ${p}-paragraph${align}${blockClass(block)}`} {...attributes}>
            {text(block, "text")}
          </p>
        );
      case "testimonial":
        return (
          <figure className={`${p}-block ${p}-testimonial${blockClass(block)}`} {...attributes}>
            <blockquote>{text(block, "quote")}</blockquote>
            <figcaption>
              {[text(block, "author"), text(block, "role")].filter(Boolean).join(", ")}
            </figcaption>
          </figure>
        );
      case "image":
        return (
          <figure className={`${p}-block ${p}-figure${blockClass(block)}`} {...attributes}>
            {/* The renderer serves the bytes itself: a public page must not depend on the panel
                session for its images, so a media id becomes the public path. */}
            <img src={publicMediaPath(text(block, "src"))} alt={text(block, "alt")} loading="lazy" />
            {text(block, "caption") ? <figcaption>{text(block, "caption")}</figcaption> : null}
          </figure>
        );
      case "gallery": {
        const images = list(block, "images");
        return (
          <figure className={`${p}-block ${p}-figure${blockClass(block)}`} {...attributes}>
            <div className={`${p}-gallery`} data-columns={number(block, "columns", 3)}>
              {images.map((src, index) => (
                <img key={`${src}-${index}`} src={publicMediaPath(src)} alt="" loading="lazy" />
              ))}
            </div>
            {images.length > 0 ? <figcaption>{images.length} images</figcaption> : null}
          </figure>
        );
      }
      case "video":
        return (
          <figure className={`${p}-block ${p}-figure${blockClass(block)}`} {...attributes}>
            <p className={`${p}-video`}>
              <a href={text(block, "src")}>{text(block, "title") || text(block, "src")}</a>
            </p>
          </figure>
        );
      case "cta":
        return (
          <section className={`${p}-block ${p}-cta${blockClass(block)}`} {...attributes}>
            <h2>{text(block, "title")}</h2>
            {text(block, "body") ? <p>{text(block, "body")}</p> : null}
            <a className={`${p}-cta-link`} href={text(block, "href")}>
              {text(block, "label")}
            </a>
          </section>
        );
      case "columns": {
        // The children of a Columns block are `column` wrappers, not content. A wrapper renders
        // as one cell of the grid; a slice-1 payload that put blocks straight under `columns`
        // still renders, because a child that is not a wrapper is drawn as a cell of its own —
        // that is what makes the validator's new rule an error the author can fix rather than a
        // page that disappears.
        const children = block.children ?? [];
        return (
          <section className={`${p}-block ${p}-columns`} data-columns={number(block, "columns", 2)}>
            {children.map((child) => (
              <div key={child.id} className={`${p}-column`}>
                {child.type === "column" ? (
                  <BlockTree blocks={child.children ?? []} />
                ) : (
                  <Block block={child} />
                )}
              </div>
            ))}
          </section>
        );
      }
      case "column":
        // A Column normally renders inside the Columns branch above. Reaching it directly means
        // the payload is the shape the validator reports as `block_column_orphan`; it still
        // renders its children rather than throwing, so a draft with that mistake is readable
        // instead of a page that 500s on an author who is mid-fix.
        return (
          <div className={`${p}-block ${p}-column`} {...attributes}>
            <BlockTree blocks={block.children ?? []} />
          </div>
        );
      case "card_grid": {
        const items = list(block, "items");
        return (
          <section className={`${p}-block ${p}-cards`} data-columns={number(block, "columns", 3)}>
            {items.map((item, index) => {
              const [title, body, href] = parts(item);
              return (
                <article key={`${item}-${index}`} className={`${p}-card`}>
                  <h3>{title}</h3>
                  {body ? <p>{body}</p> : null}
                  {href ? <a href={href}>Read more</a> : null}
                </article>
              );
            })}
          </section>
        );
      }
      case "pricing_table": {
        const plans = list(block, "plans");
        return (
          <section className={`${p}-block ${p}-plans`}>
            <ul>
              {plans.map((plan, index) => {
                const [name, price, ...features] = parts(plan);
                return (
                  <li key={`${plan}-${index}`} className={`${p}-plan`}>
                    <h3>{name}</h3>
                    <p className={`${p}-price`}>{price}</p>
                    <ul>
                      {features.map((feature, position) => (
                        <li key={`${feature}-${position}`}>{feature}</li>
                      ))}
                    </ul>
                  </li>
                );
              })}
            </ul>
            {text(block, "note") ? <p className={`${p}-note`}>{text(block, "note")}</p> : null}
          </section>
        );
      }
      case "faq": {
        const items = list(block, "items");
        return (
          <dl className={`${p}-block ${p}-faq`}>
            {items.map((item, index) => {
              const [question, answer] = parts(item);
              return (
                <div key={`${item}-${index}`}>
                  <dt>{question}</dt>
                  <dd>{answer}</dd>
                </div>
              );
            })}
          </dl>
        );
      }
      case "form":
        return (
          <section className={`${p}-block ${p}-form`} {...attributes}>
            <h2>{text(block, "title") || "Form"}</h2>
            <p>
              This page collects answers for the <strong>{text(block, "form_key")}</strong> form.
            </p>
          </section>
        );
      case "embed":
        return (
          <figure className={`${p}-block ${p}-figure${blockClass(block)}`} {...attributes}>
            <a href={text(block, "src")}>{text(block, "title") || text(block, "src")}</a>
          </figure>
        );
      case "raw_html":
        // The payload was sanitized server-side on save (REQ-063 slice 2 owns the allow-list);
        // the renderer draws what survived it and says so, rather than pretending it is markup
        // the platform vouched for.
        return (
          <div className={`${p}-block ${p}-raw`} {...attributes}>
            <p className={`${p}-raw-note`}>Author-written markup, sanitized on save</p>
            <pre>{text(block, "html")}</pre>
          </div>
        );
      case "product_grid":
      case "blog_list":
        return (
          <section className={`${p}-block ${p}-listing`} {...attributes}>
            <p>
              {text(block, "category")
                ? `${block.type === "blog_list" ? "Posts" : "Products"} in ${text(block, "category")}`
                : block.type === "blog_list"
                  ? "The most recent posts"
                  : "Products from the catalogue"}{" "}
              · up to {number(block, "limit", 3)}
            </p>
          </section>
        );
      default:
        // An unknown type is a payload written against a newer registry. It degrades to "one
        // block is missing", never to a blank page — which is what makes a theme safe to
        // install before the platform that renders it knows every block type.
        return (
          <p className={`${p}-block ${p}-unknown`} {...attributes}>
            A <code>{block.type}</code> block on this page is not supported by this theme.
          </p>
        );
    }
  }

  /**
   * Render a block tree.
   *
   * The tree that arrives is *already filtered* for the viewport: `GET /public/pages/{slug}`
   * drops the blocks the author hid from this screen before it sends the payload. Nothing here
   * hides anything with CSS, because a block that is merely invisible is still in the HTML the
   * device downloads and still in the accessibility tree, which is not what "hidden on phones"
   * means.
   */
  function BlockTree({ blocks }: { blocks: ContentBlock[] }) {
    if (blocks.length === 0) {
      return null;
    }
    return (
      <>
        {blocks.map((block) => (
          <Block key={block.id} block={block} />
        ))}
      </>
    );
  }

  return {
    BlockTree,
    Block,
    bodyParagraphs: (body: string) =>
      body
        .split(/\n\s*\n/)
        .map((block) => block.trim())
        .filter((block) => block.length > 0),
    prefix: p,
  };
}

/**
 * A media id becomes the public read path; anything else (an absolute URL) is left alone.
 *
 * The panel's own path needs a session, so a public page that used it would render broken
 * images for every visitor.
 */
export function publicMediaPath(src: string): string {
  const id = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(src)
    ? src
    : "";
  return id ? `/api/v1/public/media/${id}` : src;
}
