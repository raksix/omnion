/**
 * The block renderer of the Minimal theme (REQ-063).
 *
 * The theme draws the blocks the API hands it; a page whose block tree is empty falls back to
 * the plain body, which is what keeps every revision written before the block system rendering
 * exactly as it did. Semantic markup is the point of the whole exercise — a heading is an
 * `h2`, an FAQ is a `dl`, an image is a `figure` with a `figcaption` — because a page builder
 * that emits a wall of `div`s has thrown away the accessibility it was supposed to add.
 */
import type { ContentBlock } from "@omnion/types";

/** The text of a prop, or `""`. */
function text(block: ContentBlock, key: string): string {
  const value = block.props[key];
  return typeof value === "string" ? value : "";
}

/** A list-of-strings prop, empty when it is not one. */
function list(block: ContentBlock, key: string): string[] {
  const value = block.props[key];
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
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

/** Alignment a block asks for, mapped onto a class the stylesheet owns. */
function alignClass(block: ContentBlock): string {
  const align = text(block, "align");
  return align === "center" || align === "right" ? `mn-align-${align}` : "";
}

/**
 * Render a block tree.
 *
 * An unknown type renders a quiet notice instead of throwing: a payload written against a
 * newer registry must degrade to "one block is missing", never to a blank page.
 *
 * The tree that arrives is *already filtered* for the viewport: `GET /public/pages/{slug}`
 * drops the blocks the author hid from this screen before it sends the payload. Nothing here
 * hides anything with CSS, because a block that is merely invisible is still in the HTML the
 * device downloads and still in the accessibility tree, which is not what "hidden on phones"
 * means.
 */
export function BlockTree({ blocks }: { blocks: ContentBlock[] }) {
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

/**
 * The DOM attributes a block's own settings ask for.
 *
 * Only the *addressing* settings live here — `id` and `aria-label`. The class an author typed
 * deliberately does not: a spread lands after the element's own `className`, so a `className` in
 * the spread would replace the theme's layout classes instead of joining them, and an author who
 * typed one class name the theme does not know would lose the layout the theme does. It is
 * appended by [`blockClass`] instead.
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
 * than replacing them — and it is not a place to write arbitrary CSS, which the REQ keeps out of
 * blocks on purpose (theme tokens own styling).
 */
function blockClass(block: ContentBlock): string {
  const extra = typeof block.meta?.class === "string" ? block.meta.class.trim() : "";
  return extra ? ` ${extra}` : "";
}

/** Render one block (and, for a container, its children). */
export function Block({ block }: { block: ContentBlock }) {
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
        <Tag className={`mn-block mn-heading ${align}${blockClass(block)}`} {...attributes}>
          {text(block, "text")}
        </Tag>
      );
    }
    case "text":
      return (
        <p className={`mn-block mn-paragraph ${align}${blockClass(block)}`} {...attributes}>
          {text(block, "text")}
        </p>
      );
    case "testimonial":
      return (
        <figure className={`mn-block mn-testimonial${blockClass(block)}`} {...attributes}>
          <blockquote>{text(block, "quote")}</blockquote>
          <figcaption>
            {[text(block, "author"), text(block, "role")].filter(Boolean).join(", ")}
          </figcaption>
        </figure>
      );
    case "image":
      return (
        <figure className={`mn-block mn-figure${blockClass(block)}`} {...attributes}>
          {/* The renderer serves the bytes itself: a public page must not depend on the panel
              session for its images, so a media id becomes the public path. */}
          <img
            src={publicMediaPath(text(block, "src"))}
            alt={text(block, "alt")}
            loading="lazy"
          />
          {text(block, "caption") ? <figcaption>{text(block, "caption")}</figcaption> : null}
        </figure>
      );
    case "gallery": {
      const images = list(block, "images");
      return (
        <figure className="mn-block mn-figure">
          <div className="mn-gallery" data-columns={number(block, "columns", 3)}>
            {images.map((src, index) => (
              <img key={`${src}-${index}`} src={publicMediaPath(src)} alt="" loading="lazy" />
            ))}
          </div>
          <figcaption>{images.length} images</figcaption>
        </figure>
      );
    }
    case "video":
      return (
        <figure className="mn-block mn-figure">
          <p className="mn-video">
            <a href={text(block, "src")}>{text(block, "title") || text(block, "src")}</a>
          </p>
        </figure>
      );
    case "cta":
      return (
        <section className={`mn-block mn-cta${blockClass(block)}`} {...attributes}>
          <h2>{text(block, "title")}</h2>
          {text(block, "body") ? <p>{text(block, "body")}</p> : null}
          <a className="mn-cta-link" href={text(block, "href")}>
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
        <section className="mn-block mn-columns" data-columns={number(block, "columns", 2)}>
          {children.map((child) => (
            <div key={child.id} className="mn-column">
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
        <div className="mn-block mn-column">
          <BlockTree blocks={block.children ?? []} />
        </div>
      );
    case "card_grid": {
      const items = list(block, "items");
      return (
        <section className="mn-block mn-cards" data-columns={number(block, "columns", 3)}>
          {items.map((item, index) => {
            const [title, body, href] = parts(item);
            return (
              <article key={`${item}-${index}`} className="mn-card">
                <h3>{title}</h3>
                {body ? <p>{body}</p> : null}
                {href ? (
                  <a href={href}>Read more</a>
                ) : null}
              </article>
            );
          })}
        </section>
      );
    }
    case "pricing_table": {
      const plans = list(block, "plans");
      return (
        <section className="mn-block mn-plans">
          <ul>
            {plans.map((plan, index) => {
              const [name, price, ...features] = parts(plan);
              return (
                <li key={`${plan}-${index}`} className="mn-plan">
                  <h3>{name}</h3>
                  <p className="mn-price">{price}</p>
                  <ul>
                    {features.map((feature, position) => (
                      <li key={`${feature}-${position}`}>{feature}</li>
                    ))}
                  </ul>
                </li>
              );
            })}
          </ul>
          {text(block, "note") ? <p className="mn-note">{text(block, "note")}</p> : null}
        </section>
      );
    }
    case "faq": {
      const items = list(block, "items");
      return (
        <dl className="mn-block mn-faq">
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
        <section className="mn-block mn-form">
          <h2>{text(block, "title") || "Form"}</h2>
          <p>
            This page collects answers for the <strong>{text(block, "form_key")}</strong> form.
          </p>
        </section>
      );
    case "embed":
      return (
        <figure className="mn-block mn-figure">
          <a href={text(block, "src")}>{text(block, "title") || text(block, "src")}</a>
        </figure>
      );
    case "raw_html":
      // The payload was sanitized server-side on save (REQ-063 slice 2 owns the allow-list);
      // the renderer draws what survived it and says so, rather than pretending it is markup
      // the platform vouched for.
      return (
        <div className="mn-block mn-raw">
          <p className="mn-raw-note">Author-written markup, sanitized on save</p>
          <pre>{text(block, "html")}</pre>
        </div>
      );
    case "product_grid":
    case "blog_list":
      return (
        <section className="mn-block mn-listing">
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
      return (
        <p className="mn-block mn-unknown">
          A <code>{block.type}</code> block on this page is not supported by this theme.
        </p>
      );
  }
}

/**
 * A media id becomes the public read path; anything else (an absolute URL) is left alone.
 *
 * The panel's own path needs a session, so a public page that used it would render broken
 * images for every visitor.
 */
function publicMediaPath(src: string): string {
  const id = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(src)
    ? src
    : "";
  return id ? `/api/v1/public/media/${id}` : src;
}
