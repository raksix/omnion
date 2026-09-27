"use client";

/**
 * The block canvas: a page's block tree drawn as the page itself (REQ-063).
 *
 * One renderer, two jobs: the editor's centre pane and the preview frame draw the same blocks
 * with the same component. That is the only way "the preview lies" cannot come back — a
 * second renderer for the editor would drift from the public one the first time either changed.
 *
 * `mode` decides what the canvas is allowed to be. In `edit` the block is a button (so the
 * outline and the canvas select the same thing, and the keyboard reaches every block), and the
 * block's own issues are drawn on it. In `render` it is plain content — the same markup the
 * public renderer emits, with no editor affordances in it at all.
 */
import type { BlockIssue, BlockRegistry, ContentBlock } from "@omnion/types";
import { AlertTriangle } from "lucide-react";

import { definitionFor, propValue } from "./block-library";

/** How the canvas is being used. */
export type CanvasMode = "edit" | "render";

type CanvasProps = {
  /** The registry the blocks are drawn against. */
  registry: BlockRegistry;
  /** The tree to draw. */
  blocks: ContentBlock[];
  /** `edit` makes blocks selectable, `render` draws them as content. */
  mode: CanvasMode;
  /** The selected block's path, in `edit` mode. */
  selected?: number[] | null;
  /** The live validation issues, keyed by block id. */
  issues?: Map<string, BlockIssue[]>;
  /** Selecting a block (edit mode). */
  onSelect?: (path: number[]) => void;
  /** The path prefix the block sits under, for selection bookkeeping. */
  basePath?: number[];
};

/** Issues of one block, in the order the API reports them. */
function issuesFor(
  issues: Map<string, BlockIssue[]> | undefined,
  block: ContentBlock,
): BlockIssue[] {
  return issues?.get(block.id) ?? [];
}

/** `true` when a path equals a selection. */
function samePath(a: number[] | null | undefined, b: number[]): boolean {
  if (!a || a.length !== b.length) {
    return false;
  }
  return a.every((index, position) => index === b[position]);
}

/** The canvas of one page (or of one column inside it). */
export function BlockCanvas({
  registry,
  blocks,
  mode,
  selected,
  issues,
  onSelect,
  basePath = [],
}: CanvasProps) {
  if (blocks.length === 0) {
    return mode === "edit" ? (
      <p className="rounded-lg border border-dashed border-line px-4 py-8 text-center text-[12.5px] text-muted">
        This page has no blocks yet. Use <strong>+ Block</strong> above to add the first one.
      </p>
    ) : null;
  }

  return (
    <div className="flex flex-col gap-3">
      {blocks.map((block, index) => (
        <CanvasBlock
          key={block.id}
          registry={registry}
          block={block}
          path={[...basePath, index]}
          mode={mode}
          selected={selected}
          issues={issues}
          onSelect={onSelect}
        />
      ))}
    </div>
  );
}

type CanvasBlockProps = {
  registry: BlockRegistry;
  block: ContentBlock;
  path: number[];
  mode: CanvasMode;
  selected?: number[] | null;
  issues?: Map<string, BlockIssue[]>;
  onSelect?: (path: number[]) => void;
};

function CanvasBlock({
  registry,
  block,
  path,
  mode,
  selected,
  issues,
  onSelect,
}: CanvasBlockProps) {
  const definition = definitionFor(registry, block.type);
  const blockIssues = issuesFor(issues, block);
  const blocking = blockIssues.find((issue) => issue.severity === "error");
  const warning = blockIssues.find((issue) => issue.severity === "warning");
  const body = definition
    ? renderProps(definition.key, block)
    : (
        <p className="text-[12.5px] text-caution">
          Unknown block type <code className="font-mono">{block.type}</code> — nothing renders
          for it until the platform ships that type.
        </p>
      );

  const active = samePath(selected, path);
  const heading = (
    <span className="mb-1 flex items-center gap-2 text-[11px] tracking-wide text-muted uppercase">
      {definition?.label ?? block.type}
      {blocking ? (
        <span className="inline-flex items-center gap-1 rounded-full bg-accent-soft px-1.5 py-0.5 text-[10.5px] text-accent-strong normal-case">
          <AlertTriangle className="size-2.5" aria-hidden />
          Needs attention
        </span>
      ) : null}
      {!blocking && warning ? (
        <span className="inline-flex items-center gap-1 rounded-full bg-caution-soft px-1.5 py-0.5 text-[10.5px] text-caution normal-case">
          <AlertTriangle className="size-2.5" aria-hidden />
          {warning.message}
        </span>
      ) : null}
    </span>
  );

  // A container is its own block *and* the frame its children are drawn in. Rendering only the
  // container's props would leave an empty box on the canvas, which is the placeholder the
  // REQ's "no placeholder boxes" rule is about.
  //
  // A `columns` block is the one container whose children are *not* content: each is a `column`
  // wrapper, and the blocks live one level deeper. Drawing the wrappers as a flat row of blocks
  // is exactly the "the canvas lies about the layout" bug, so the column frame gets its own
  // branch: a grid of per-column slots, each labelled and each with a drop target.
  const frame =
    block.type === "columns" && block.children ? (
      <div
        data-block-columns
        data-block-column-count={block.children.length}
        className="mt-2 flex flex-col gap-2"
      >
        {block.children.map((column, columnIndex) => (
          <div
            key={column.id}
            data-block-column
            data-block-column-index={columnIndex}
            className="rounded-md border border-dashed border-line p-1.5"
          >
            <p className="mb-1 px-1 text-[10.5px] tracking-wide text-muted uppercase">
              Column {columnIndex + 1}
            </p>
            {column.children && column.children.length > 0 ? (
              <div className="flex flex-col gap-1.5">
                {column.children.map((child, childIndex) => (
                  <CanvasBlock
                    key={child.id}
                    registry={registry}
                    block={child}
                    path={[...path, columnIndex, childIndex]}
                    mode={mode}
                    selected={selected}
                    issues={issues}
                    onSelect={onSelect}
                  />
                ))}
              </div>
            ) : (
              <button
                type="button"
                data-block-column-empty={columnIndex}
                onClick={() => onSelect?.([...path, columnIndex])}
                className="w-full cursor-pointer rounded border border-dashed border-line px-2 py-3 text-center text-[11.5px] text-muted transition hover:border-accent/50 hover:text-ink"
              >
                Empty — select this column, then use + Block
              </button>
            )}
          </div>
        ))}
      </div>
    ) : definition?.container && block.children ? (
      <div
        className={`mt-2 grid gap-2 ${
          (block.children?.length ?? 0) > 2 ? "sm:grid-cols-3" : "sm:grid-cols-2"
        }`}
      >
        {block.children.map((child, childIndex) => (
          <CanvasBlock
            key={child.id}
            registry={registry}
            block={child}
            path={[...path, childIndex]}
            mode={mode}
            selected={selected}
            issues={issues}
            onSelect={onSelect}
          />
        ))}
      </div>
    ) : null;

  if (mode === "render") {
    return (
      <div className="bl-canvas-block">
        {body}
        {frame}
      </div>
    );
  }

  return (
    <div
      data-block-canvas-block={block.type}
      data-block-has-error={blocking ? "true" : "false"}
      className={`relative rounded-lg border transition ${
        active ? "border-accent ring-2 ring-accent/15" : "border-line hover:border-accent/40"
      } ${blocking ? "border-accent-strong" : ""}`}
    >
      <button
        type="button"
        onClick={() => onSelect?.(path)}
        aria-pressed={active}
        aria-label={`Select ${definition?.label ?? block.type} block`}
        className="block w-full cursor-pointer rounded-lg px-3 py-2.5 text-left"
      >
        {heading}
        <span className="block">{body}</span>
      </button>
      {frame}
    </div>
  );
}

/**
 * Draw one block's props as its content.
 *
 * A block's *rendered* shape is a small, closed vocabulary: a heading really is a heading, a
 * gallery really is a list of figures, an FAQ really is a description list. Rendering it any
 * other way is what turns a page builder into a CMS full of `<div>` soup that screen readers
 * have to guess their way through.
 */
function renderProps(kind: string, block: ContentBlock) {
  const props = block.props;
  const text = (key: string) => {
    const value = props[key];
    return typeof value === "string" ? value : "";
  };
  const list = (key: string) => {
    const value = props[key];
    return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
  };

  switch (kind) {
    case "heading": {
      // The level is a real h1–h6 so the document outline is real; `text` is the label.
      const level = text("level") || "h2";
      const Tag = (["h1", "h2", "h3", "h4", "h5", "h6"].includes(level)
        ? level
        : "h2") as "h1";
      return <Tag className="text-[17px] font-semibold">{text("text") || "Untitled heading"}</Tag>;
    }
    case "text":
      return (
        <p className="text-[13px] leading-relaxed whitespace-pre-line">
          {text("text") || "Empty text block — write something in the inspector."}
        </p>
      );
    case "testimonial":
      return (
        <figure className="border-l-2 border-accent pl-3">
          <blockquote className="text-[13px] italic">“{text("quote") || "…"}”</blockquote>
          <figcaption className="mt-1 text-[11.5px] text-muted">
            {[text("author"), text("role")].filter(Boolean).join(", ") || "Unattributed"}
          </figcaption>
        </figure>
      );
    case "image":
      return (
        <figure>
          <div className="flex h-28 items-center justify-center rounded-md bg-quiet-soft text-[11.5px] text-muted">
            {text("src") || "No image URL"}
          </div>
          <figcaption className="mt-1 text-[11.5px] text-muted">
            {text("alt") ? `Alt: ${text("alt")}` : "No alternative text yet"}
            {text("caption") ? ` · ${text("caption")}` : ""}
          </figcaption>
        </figure>
      );
    case "gallery": {
      const images = list("images");
      return (
        <figure>
          <div className="grid grid-cols-3 gap-2">
            {images.length === 0 ? (
              <span className="col-span-3 rounded-md bg-quiet-soft px-3 py-6 text-center text-[11.5px] text-muted">
                No images yet
              </span>
            ) : (
              images.map((src, index) => (
                <span
                  key={`${src}-${index}`}
                  className="flex h-16 items-center justify-center truncate rounded-md bg-quiet-soft px-1 text-[10.5px] text-muted"
                >
                  {src}
                </span>
              ))
            )}
          </div>
          <figcaption className="mt-1 text-[11.5px] text-muted">
            {images.length} image{images.length === 1 ? "" : "s"}
          </figcaption>
        </figure>
      );
    }
    case "video":
      return (
        <figure className="rounded-md border border-line">
          <div className="flex h-28 items-center justify-center bg-quiet-soft text-[11.5px] text-muted">
            {text("src") || "No video URL"}
          </div>
          {text("title") ? (
            <figcaption className="px-2 py-1 text-[11.5px] text-muted">{text("title")}</figcaption>
          ) : null}
        </figure>
      );
    case "cta":
      return (
        <section className="rounded-lg bg-accent-soft px-3 py-3">
          <p className="text-[14px] font-semibold text-accent-strong">
            {text("title") || "Call to action"}
          </p>
          {text("body") ? <p className="mt-0.5 text-[12.5px]">{text("body")}</p> : null}
          <span className="mt-2 inline-block rounded-md bg-accent px-2.5 py-1 text-[12px] font-medium text-white">
            {text("label") || "Button"}
          </span>
        </section>
      );
    case "card_grid": {
      const items = list("items");
      return (
        <div className="grid grid-cols-2 gap-2">
          {items.length === 0 ? (
            <span className="col-span-2 rounded-md bg-quiet-soft px-3 py-6 text-center text-[11.5px] text-muted">
              No cards yet
            </span>
          ) : (
            items.map((item, index) => (
              <span
                key={`${item}-${index}`}
                className="rounded-md border border-line px-2.5 py-2 text-[12px]"
              >
                {item || "Untitled card"}
              </span>
            ))
          )}
        </div>
      );
    }
    case "pricing_table": {
      const plans = list("plans");
      return (
        <div>
          <ul className="flex flex-col gap-1.5">
            {plans.length === 0 ? (
              <li className="rounded-md bg-quiet-soft px-3 py-4 text-center text-[11.5px] text-muted">
                No plans yet
              </li>
            ) : (
              plans.map((plan, index) => (
                <li key={`${plan}-${index}`} className="rounded-md border border-line px-2.5 py-2 text-[12px]">
                  {plan}
                </li>
              ))
            )}
          </ul>
          {text("note") ? <p className="mt-1 text-[11.5px] text-muted">{text("note")}</p> : null}
        </div>
      );
    }
    case "faq": {
      const items = list("items");
      return (
        <dl className="flex flex-col gap-1">
          {items.length === 0 ? (
            <p className="rounded-md bg-quiet-soft px-3 py-4 text-center text-[11.5px] text-muted">
              No questions yet
            </p>
          ) : (
            items.map((item, index) => (
              <div key={`${item}-${index}`} className="rounded-md border border-line px-2.5 py-2">
                <dt className="text-[12.5px] font-medium">{item.split("|")[0] || "Question"}</dt>
                <dd className="text-[12px] text-muted">{item.split("|")[1] || ""}</dd>
              </div>
            ))
          )}
        </dl>
      );
    }
    case "form":
      return (
        <section className="rounded-lg border border-dashed border-line px-3 py-4 text-center">
          <p className="text-[12.5px] font-medium">{text("title") || "Form"}</p>
          <p className="mt-0.5 text-[11.5px] text-muted">
            Bound to form <code className="font-mono">{text("form_key") || "…"}</code>
          </p>
        </section>
      );
    case "embed":
      return (
        <figure className="rounded-md border border-line">
          <div className="flex h-24 items-center justify-center bg-quiet-soft text-[11.5px] text-muted">
            {text("src") || "No embed URL"}
          </div>
          {text("title") ? (
            <figcaption className="px-2 py-1 text-[11.5px] text-muted">{text("title")}</figcaption>
          ) : null}
        </figure>
      );
    case "raw_html":
      return (
        <div className="rounded-md border border-caution/40 bg-caution-soft px-2.5 py-2">
          <p className="mb-1 text-[10.5px] tracking-wide text-caution uppercase">
            Untrusted content — sanitized on save
          </p>
          <pre className="overflow-x-auto text-[11.5px] whitespace-pre-wrap">{text("html") || ""}</pre>
        </div>
      );
    case "product_grid":
    case "blog_list":
      return (
        <section className="rounded-md border border-dashed border-line px-3 py-4 text-center text-[11.5px] text-muted">
          {text("category")
            ? `${kind === "blog_list" ? "Blog posts" : "Products"} in ${text("category")}`
            : kind === "blog_list"
              ? "The most recent posts"
              : "Products from the catalogue"}{" "}
          · up to {typeof props.limit === "number" ? props.limit : 3}
        </section>
      );
    case "columns": {
      // The container's own line; its children are drawn in the frame beside it, so a
      // `columns` block reads as the columns an author actually put in it.
      const count = (block.children ?? []).length;
      const filled = (block.children ?? []).filter((column) => (column.children ?? []).length > 0)
        .length;
      return (
        <p className="text-[12px] text-muted">
          {count} columns · {filled} filled
        </p>
      );
    }
    case "column":
      // A column is a slot, not content: on the canvas its own line says how much is in it, and
      // the blocks themselves are drawn by the Columns frame around it.
      return (
        <p className="text-[12px] text-muted">
          {(block.children ?? []).length === 0
            ? "Empty column"
            : `${(block.children ?? []).length} block${(block.children ?? []).length === 1 ? "" : "s"}`}
        </p>
      );
    default:
      return <p className="text-[12.5px] text-muted">Nothing renders for this block type.</p>;
  }
}
