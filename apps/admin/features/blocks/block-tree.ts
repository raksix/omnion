"use client";

/**
 * The block tree the editor holds while a page is open (REQ-063, slice 1).
 *
 * Ids are generated on the client and never rewritten, which is the whole reason a reorder is
 * a move: the payload the API validates, stores and later diffs can point at the same block
 * before and after the change. Every operation here returns a *new* tree, so React's identity
 * check is a correct "did anything change" test and the undo stack (slice 4) can hold onto
 * previous trees without a copy dance.
 *
 * Depth is capped by the registry (`MAX_DEPTH` in `crates/content`) rather than here, so the
 * rule the server enforces and the rule the editor offers are the same one.
 */
import type { BlockDefinition, BlockRegistry, ContentBlock } from "@omnion/types";

/** Deepest nesting the editor offers. Mirrors the server's `MAX_DEPTH`. */
export const MAX_DEPTH = 3;

/** Most blocks the editor will insert into one page. Mirrors the server's `MAX_BLOCKS`. */
export const MAX_BLOCKS = 400;

/** A block id the editor mints. `crypto.randomUUID` is available in every browser the panel
 * supports, and a fallback keeps a private-mode context from silently losing every id. */
export function newBlockId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `b-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

/** A fresh block of a type, with the props its schema defaults to. */
export function newBlock(definition: BlockDefinition): ContentBlock {
  const props: Record<string, unknown> = {};
  for (const prop of definition.props) {
    // The schema's default is copied, never shared: a list default handed out by reference
    // would let one block's `items` array show up inside another block's editor.
    props[prop.key] = structuredCloneSafe(prop.default);
  }
  const block: ContentBlock = { id: newBlockId(), type: definition.key, props };
  if (definition.container) {
    block.children = [];
  }
  return block;
}

function structuredCloneSafe<T>(value: T): T {
  if (value === null || typeof value !== "object") {
    return value;
  }
  return JSON.parse(JSON.stringify(value)) as T;
}

/** Every block of a tree, depth first, with the path that reaches it. */
export function walk(
  blocks: ContentBlock[],
): Array<{ block: ContentBlock; path: number[]; depth: number }> {
  const out: Array<{ block: ContentBlock; path: number[]; depth: number }> = [];
  const visit = (nodes: ContentBlock[], trail: number[], depth: number) => {
    nodes.forEach((block, index) => {
      const path = [...trail, index];
      out.push({ block, path, depth });
      if (block.children) {
        visit(block.children, path, depth + 1);
      }
    });
  };
  visit(blocks, [], 0);
  return out;
}

/** The block at a path, or `undefined` when the path no longer exists. */
export function blockAt(
  blocks: ContentBlock[],
  path: number[],
): ContentBlock | undefined {
  let nodes: ContentBlock[] | undefined = blocks;
  let found: ContentBlock | undefined;
  for (const index of path) {
    found = nodes?.[index];
    if (!found) {
      return undefined;
    }
    nodes = found.children;
  }
  return found;
}

/** The array that holds a path's block, plus the index inside it. */
function locate(
  blocks: ContentBlock[],
  path: number[],
): { siblings: ContentBlock[]; index: number } | null {
  if (path.length === 0) {
    return null;
  }
  const parentPath = path.slice(0, -1);
  const siblings = parentPath.length === 0 ? blocks : blockAt(blocks, parentPath)?.children;
  if (!siblings) {
    return null;
  }
  return { siblings, index: path[path.length - 1] };
}

/** Replace one block, keeping every other block's identity and position. */
export function updateBlock(
  blocks: ContentBlock[],
  path: number[],
  change: (block: ContentBlock) => ContentBlock,
): ContentBlock[] {
  const at = locate(blocks, path);
  if (!at) {
    return blocks;
  }
  const next = blocks.slice();
  const parentPath = path.slice(0, -1);
  if (parentPath.length === 0) {
    next[at.index] = change(at.siblings[at.index]);
    return next;
  }
  const parent = blockAt(blocks, parentPath);
  if (!parent?.children) {
    return blocks;
  }
  const children = parent.children.slice();
  children[at.index] = change(children[at.index]);
  return updateBlock(blocks, parentPath, (found) => ({
    ...found,
    children,
  }));
}

/** Replace one prop of a block. */
export function setProp(
  blocks: ContentBlock[],
  path: number[],
  key: string,
  value: unknown,
): ContentBlock[] {
  return updateBlock(blocks, path, (block) => ({
    ...block,
    props: { ...block.props, [key]: value },
  }));
}

/** Remove the block at a path, with its subtree. */
export function removeBlock(
  blocks: ContentBlock[],
  path: number[],
): ContentBlock[] {
  const at = locate(blocks, path);
  if (!at) {
    return blocks;
  }
  const next = at.siblings.slice();
  next.splice(at.index, 1);
  return replaceSiblings(blocks, path, next);
}

/** Move a block one step up or down among its siblings; `false` when it cannot move. */
export function moveBlock(
  blocks: ContentBlock[],
  path: number[],
  delta: -1 | 1,
): ContentBlock[] {
  const at = locate(blocks, path);
  if (!at) {
    return blocks;
  }
  const target = at.index + delta;
  if (target < 0 || target >= at.siblings.length) {
    return blocks;
  }
  const next = at.siblings.slice();
  const [moved] = next.splice(at.index, 1);
  next.splice(target, 0, moved);
  return replaceSiblings(blocks, path, next);
}

/** Copy a block (with its subtree) and put the copy directly after it. */
export function duplicateBlock(
  blocks: ContentBlock[],
  path: number[],
): ContentBlock[] {
  const at = locate(blocks, path);
  if (!at) {
    return blocks;
  }
  const source = at.siblings[at.index];
  const copy = cloneWithNewIds(source);
  const next = at.siblings.slice();
  next.splice(at.index + 1, 0, copy);
  return replaceSiblings(blocks, path, next);
}

/**
 * Clone a block and everything under it with fresh ids.
 *
 * The subtree needs its own ids too: two cards sharing one id would make the inspector's
 * selection ambiguous and a diff unable to say which of them changed.
 */
export function cloneWithNewIds(block: ContentBlock): ContentBlock {
  return {
    id: newBlockId(),
    type: block.type,
    props: structuredCloneSafe(block.props),
    ...(block.children
      ? { children: block.children.map(cloneWithNewIds) }
      : {}),
  };
}

/** Insert a block after the given path; a path of `[]` inserts at the end of the top level. */
export function insertAfter(
  blocks: ContentBlock[],
  path: number[],
  block: ContentBlock,
): ContentBlock[] {
  if (path.length === 0) {
    return [...blocks, block];
  }
  const at = locate(blocks, path);
  if (!at) {
    return [...blocks, block];
  }
  const next = at.siblings.slice();
  next.splice(at.index + 1, 0, block);
  return replaceSiblings(blocks, path, next);
}

/** Insert a block as the last child of the container at `path`. */
export function appendChild(
  blocks: ContentBlock[],
  path: number[],
  block: ContentBlock,
): ContentBlock[] {
  return updateBlock(blocks, path, (parent) => ({
    ...parent,
    children: [...(parent.children ?? []), block],
  }));
}

/** Write a whole new sibling list back where it came from. */
function replaceSiblings(
  blocks: ContentBlock[],
  path: number[],
  siblings: ContentBlock[],
): ContentBlock[] {
  const parentPath = path.slice(0, -1);
  if (parentPath.length === 0) {
    return siblings;
  }
  return updateBlock(blocks, parentPath, (parent) => ({
    ...parent,
    children: siblings,
  }));
}

/** Fewest and most columns a Columns block holds. Mirrors the server's `MIN_COLUMNS`/`MAX_COLUMNS`. */
export const MIN_COLUMNS = 2;
export const MAX_COLUMNS = 4;

/**
 * Insert a Columns block with the column wrappers it needs, then leave the author inside the
 * first one.
 *
 * A Columns block whose children are plain blocks is not a layout, it is a list that happens to
 * be indented — "two of these side by side" is only expressible if each column is its own node
 * that holds blocks. So the editor builds the structure the validator requires instead of
 * letting the author create an invalid payload and discovering it at publish time.
 */
export function insertColumns(
  blocks: ContentBlock[],
  path: number[],
  definition: BlockDefinition,
  wanted = MIN_COLUMNS,
): { blocks: ContentBlock[]; path: number[] } {
  const count = Math.min(MAX_COLUMNS, Math.max(MIN_COLUMNS, wanted));
  const columns: ContentBlock[] = Array.from({ length: count }, () => ({
    id: newBlockId(),
    type: "column",
    props: { align: "left" },
    children: [],
  }));
  // `newBlock` already builds the schema defaults (and an empty `children` for a container);
  // the wrappers replace that empty list, so the only prop this sets is the count the layout
  // and the validator both read.
  const block = newBlock(definition);
  block.props.columns = count;
  block.children = columns;
  const index = path.length === 0 ? blocks.length : path[path.length - 1] + 1;
  return {
    blocks: insertAfter(blocks, path, block),
    path: [...path.slice(0, -1), index, 0],
  };
}

/**
 * Add a column to a Columns block.
 *
 * The new column is empty on purpose: a column with nothing in it is a gap the author can drop a
 * block into, and a column that copied the last one would silently duplicate content. The
 * `columns` prop moves with it, because that prop is the layout the renderer reads and a
 * disagreement between the two is the exact bug the validator would then report.
 */
export function addColumn(
  blocks: ContentBlock[],
  path: number[],
): ContentBlock[] {
  const container = blockAt(blocks, path);
  if (!container || container.type !== "columns") {
    return blocks;
  }
  if ((container.children?.length ?? 0) >= MAX_COLUMNS) {
    return blocks;
  }
  const column: ContentBlock = { id: newBlockId(), type: "column", props: { align: "left" }, children: [] };
  return updateBlock(blocks, path, (block) => ({
    ...block,
    props: { ...block.props, columns: (block.children?.length ?? 0) + 1 },
    children: [...(block.children ?? []), column],
  }));
}

/**
 * Remove a column from a Columns block, taking the blocks inside it with it.
 *
 * The REQ says two to four columns, so a Columns block never goes below two: the second-to-last
 * column is not removable and the control says so instead of returning a tree the API would
 * refuse. Content is never silently relocated — a column with blocks in it is a deliberate
 * deletion, and the confirm in the editor is where that decision belongs.
 */
export function removeColumn(
  blocks: ContentBlock[],
  path: number[],
  columnIndex: number,
): ContentBlock[] {
  const container = blockAt(blocks, path);
  if (!container || container.type !== "columns") {
    return blocks;
  }
  const count = container.children?.length ?? 0;
  if (count <= MIN_COLUMNS || columnIndex < 0 || columnIndex >= count) {
    return blocks;
  }
  const children = (container.children ?? []).filter((_, index) => index !== columnIndex);
  return updateBlock(blocks, path, (block) => ({
    ...block,
    props: { ...block.props, columns: children.length },
    children,
  }));
}

/** The blocks inside one column of a Columns block, for the editor's per-column drop target. */
export function columnBlocks(
  blocks: ContentBlock[],
  columnsPath: number[],
): ContentBlock[][] {
  const container = blockAt(blocks, columnsPath);
  if (!container?.children) {
    return [];
  }
  return container.children.map((column) => column.children ?? []);
}

/** The breadcrumb a selection produces: the chain of types down to the selected block. */
export function breadcrumb(
  registry: BlockRegistry,
  blocks: ContentBlock[],
  path: number[],
): Array<{ label: string; path: number[] }> {
  const trail: Array<{ label: string; path: number[] }> = [];
  let nodes = blocks;
  for (let depth = 0; depth < path.length; depth += 1) {
    const block = nodes?.[path[depth]];
    if (!block) {
      break;
    }
    const definition = registry.blocks.find((entry) => entry.key === block.type);
    trail.push({
      // A type name alone is not a position: four columns all read "Column", and a trail that
      // says Column / Column / Text cannot tell the author which of the four they are in — which
      // is the whole reason the breadcrumb exists. Numbering the siblings does.
      label:
        block.type === "column"
          ? `Column ${path[depth] + 1}`
          : (definition?.label ?? block.type),
      path: path.slice(0, depth + 1),
    });
    nodes = block.children ?? [];
  }
  return trail;
}

/** The words a page is made of, for the editor's bottom bar. */
export function wordCount(blocks: ContentBlock[]): number {
  let total = 0;
  for (const { block } of walk(blocks)) {
    for (const value of Object.values(block.props)) {
      if (typeof value === "string") {
        total += value.trim().split(/\s+/).filter(Boolean).length;
      }
    }
  }
  return total;
}
