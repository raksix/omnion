import type { BlockDefinition, BlockRegistry } from "@omnion/types";

/**
 * The block library of the panel (REQ-063).
 *
 * Every list, every field and every default in the editor comes from the registry document the
 * API answers with (`GET /api/v1/blocks`). Nothing here names a block type, a prop or an
 * option: adding a block to the platform is a code change in `crates/content`, and this file
 * picks it up on the next fetch without a panel release.
 */

/** The registry, as the panel holds it. */
export type Registry = BlockRegistry;

/** One definition looked up by key. */
export type Definition = BlockDefinition;

/** The labels the category headers carry, in the order the API declares the categories. */
const CATEGORY_LABELS: Record<string, string> = {
  layout: "Layout",
  text: "Text",
  media: "Media",
  marketing: "Marketing",
  content: "Content",
};

/** The label a category header shows. */
export function categoryLabel(category: string): string {
  return CATEGORY_LABELS[category] ?? category;
}

/** The definitions of one category, in registry order. */
export function definitionsIn(
  registry: Registry,
  category: string,
): Definition[] {
  return registry.blocks.filter((entry) => entry.category === category);
}

/** Look one definition up; `undefined` for a type this registry does not ship. */
export function definitionFor(
  registry: Registry,
  key: string,
): Definition | undefined {
  return registry.blocks.find((entry) => entry.key === key);
}

/**
 * The label a block shows in the outline: its own name plus, for a heading, the words it holds.
 *
 * An outline row that said "Heading" sixteen times tells an author nothing, and a page is mostly
 * headings — so the text comes with the type, truncated to what a row has room for.
 */
export function blockLabel(
  registry: Registry,
  block: { type: string; props: Record<string, unknown> },
): string {
  const definition = definitionFor(registry, block.type);
  if (!definition) {
    return block.type;
  }
  const text = block.props.text ?? block.props.quote ?? block.props.title;
  if (typeof text !== "string" || text.trim() === "") {
    return definition.label;
  }
  const trimmed = text.trim();
  return trimmed.length > 40 ? `${trimmed.slice(0, 40)}…` : trimmed;
}

/** The prop value of a block, with the schema's default standing in for an absent one. */
export function propValue(
  definition: Definition,
  props: Record<string, unknown>,
  key: string,
): unknown {
  return props[key] ?? definition.props.find((prop) => prop.key === key)?.default;
}

/**
 * A short one-line summary of a block's content for the outline's second line — the value of
 * the prop the author is most likely to be looking for, never the whole payload.
 */
export function blockSummary(
  registry: Registry,
  block: { type: string; props: Record<string, unknown>; children?: unknown[] },
): string {
  const definition = definitionFor(registry, block.type);
  if (!definition) {
    return "Unknown block type";
  }
  if (definition.container) {
    const count = block.children?.length ?? 0;
    return count === 1 ? "1 nested block" : `${count} nested blocks`;
  }
  for (const candidate of ["text", "quote", "title", "src", "form_key", "note"]) {
    const value = propValue(definition, block.props, candidate);
    if (typeof value === "string" && value.trim() !== "") {
      const trimmed = value.trim();
      return trimmed.length > 60 ? `${trimmed.slice(0, 60)}…` : trimmed;
    }
  }
  if (Array.isArray(block.props.items) && block.props.items.length > 0) {
    return `${block.props.items.length} entries`;
  }
  return definition.description;
}
