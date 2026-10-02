"use client";

/**
 * Reading a block tree as a line of text (REQ-063, slice 3).
 *
 * The pattern library and the template gallery both have to answer "what is in this?" on a
 * card, and both would otherwise grow their own little walker. A card that says "12 blocks" is
 * a number; a card that says "Hero · Text · Two columns" is an outline the author can recognise
 * before clicking. So the description is derived from the *registry* the editor already holds,
 * never from a hard-coded label list — a block type that ships tomorrow shows up here with its
 * own name, without a line in this file.
 */
import type { BlockDefinition, BlockRegistry, ContentBlock } from "@omnion/types";

/** The first text-typed prop a block carries, in registry order — the same rule the diff uses. */
export function blockHeadline(
  block: ContentBlock,
  registry: BlockRegistry,
): string | null {
  const definition = registry.blocks.find((entry) => entry.key === block.type);
  if (!definition) {
    return null;
  }
  for (const prop of definition.props) {
    if (prop.type !== "text" && prop.type !== "string") {
      continue;
    }
    const value = block.props[prop.key];
    if (typeof value === "string" && value.trim() !== "") {
      return value.trim().replace(/\s+/g, " ").slice(0, 60);
    }
  }
  return null;
}

/** The label the registry gives a block type, or the raw key when the type is unknown. */
export function blockLabel(
  block: ContentBlock,
  registry: BlockRegistry,
): string {
  return (
    registry.blocks.find((entry: BlockDefinition) => entry.key === block.type)?.label ??
    block.type
  );
}

/**
 * One line describing a tree, in reading order, nesting marked with an indent rather than a
 * bracket — a card has one line of height, and a tree printed as JSON tells an author nothing
 * about the shape they are about to insert.
 */
export function describeTree(
  blocks: ContentBlock[],
  registry: BlockRegistry,
  limit = 6,
): string {
  const parts: string[] = [];
  const visit = (nodes: ContentBlock[], depth: number) => {
    for (const block of nodes) {
      if (parts.length >= limit) {
        return;
      }
      const indent = depth > 0 ? `${"· ".repeat(depth)}` : "";
      const headline = blockHeadline(block, registry);
      parts.push(`${indent}${blockLabel(block, registry)}${headline ? ` — ${headline}` : ""}`);
      if (block.children?.length) {
        visit(block.children, depth + 1);
      }
    }
  };
  visit(blocks, 0);
  const rest = countBlocks(blocks) - parts.length;
  if (rest > 0) {
    parts.push(`+${rest} more`);
  }
  return parts.join(" · ");
}

/** Blocks in a tree, nested included. */
export function countBlocks(blocks: ContentBlock[]): number {
  return blocks.reduce(
    (total, block) => total + 1 + countBlocks(block.children ?? []),
    0,
  );
}

/** The block types a tree uses, unique, in first-seen order — a card's "what's inside" chips. */
export function treeTypes(blocks: ContentBlock[]): string[] {
  const seen: string[] = [];
  const visit = (nodes: ContentBlock[]) => {
    for (const block of nodes) {
      if (!seen.includes(block.type)) {
        seen.push(block.type);
      }
      if (block.children?.length) {
        visit(block.children);
      }
    }
  };
  visit(blocks);
  return seen;
}

/** The longest accepted key, shared with the API's own rule so the form can say so first. */
export const MAX_KEY_LENGTH = 63;

/**
 * A readable key from a name, for the "new pattern" form's key field.
 *
 * The key is the pattern's identity in the database and in the library's filter, so it has to
 * survive a rename without the author editing it by hand — and it has to be *stable*, which is
 * why this runs once on the name the author typed rather than on every keystroke of it.
 */
export function keyFromName(name: string): string {
  return name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, MAX_KEY_LENGTH)
    .replace(/-+$/, "");
}

/** The key shape the API enforces, for the form's own message. */
export function keyProblem(key: string): string | null {
  if (key.trim() === "") {
    return "A key is required.";
  }
  if (key !== key.toLowerCase()) {
    return "Use lowercase letters, digits and dashes.";
  }
  if (!/^[a-z0-9]/.test(key)) {
    return "Start with a letter or a digit.";
  }
  if (!/[a-z0-9]$/.test(key)) {
    return "End with a letter or a digit.";
  }
  if (/[^a-z0-9-]/.test(key)) {
    return "Use lowercase letters, digits and dashes only.";
  }
  if (key.length > MAX_KEY_LENGTH) {
    return `Keep the key under ${MAX_KEY_LENGTH} characters.`;
  }
  return null;
}
