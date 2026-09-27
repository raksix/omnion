"use client";

/**
 * The block editor: `/pages/<id>/edit` (REQ-063, slice 1).
 *
 * Three panes — the outline of the page's blocks, the canvas, and the inspector of the
 * selected block — over a bottom bar that says what is unsaved and what is wrong. The editor
 * holds the working tree in memory and writes it with the page's own `PATCH`, so a block save
 * is an ordinary draft revision: the history, restore and publish rules the content model
 * already has keep holding for block content without a single new rule.
 *
 * Validation is the server's: every change asks `POST /api/v1/blocks/validate` what it would
 * say, and the answer drives the badges on the canvas, the messages in the inspector and
 * whether *Publish* is allowed. A second copy of those rules in the browser is exactly how a
 * panel ends up telling an author a page is fine when the API will not take it.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { BlockDefinition, BlockIssue, BlockRegistry, ContentBlock } from "@omnion/types";
import {
  ArrowDown,
  ArrowUp,
  Copy,
  Layers,
  Pencil,
  Plus,
  Rocket,
  Save,
  Trash2,
} from "lucide-react";
import Link from "next/link";
import { useParams, useRouter } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError, fetchBlockRegistry, fetchPage, publishPage, updatePage, validateBlocks } from "@/lib/api";
import type { Page } from "@/lib/types";
import { pageTitle } from "@/lib/types";
import { BlockCanvas } from "@/features/blocks/block-canvas";
import { BlockInspector, InsertPanel } from "@/features/blocks/block-inspector";
import { blockLabel, blockSummary, definitionFor } from "@/features/blocks/block-library";
import {
  MAX_COLUMNS,
  MAX_DEPTH,
  MIN_COLUMNS,
  addColumn,
  appendChild,
  blockAt,
  breadcrumb,
  cloneWithNewIds,
  duplicateBlock,
  insertAfter,
  insertColumns,
  moveBlock,
  newBlock,
  removeBlock,
  removeColumn,
  setProp,
  setSetting,
  walk,
  wordCount,
} from "@/features/blocks/block-tree";

/** The list a block at `path` lives in — its siblings, which is what it moves inside. */
function siblingList(blocks: ContentBlock[], path: number[]): ContentBlock[] {
  if (path.length === 0) {
    return blocks;
  }
  const parent = blockAt(blocks, path.slice(0, -1));
  return parent?.children ?? blocks;
}

/** Issues keyed by the block they belong to. */
function issuesByBlock(issues: BlockIssue[]): Map<string, BlockIssue[]> {
  const map = new Map<string, BlockIssue[]>();
  for (const issue of issues) {
    const existing = map.get(issue.block_id);
    if (existing) {
      existing.push(issue);
    } else {
      map.set(issue.block_id, [issue]);
    }
  }
  return map;
}

/** The block editor of one page. */
export function BlockEditor() {
  const params = useParams<{ id: string }>();
  const router = useRouter();
  const pageId = params.id;

  const [registry, setRegistry] = useState<BlockRegistry | null>(null);
  const [page, setPage] = useState<Page | null>(null);
  const [blocks, setBlocks] = useState<ContentBlock[]>([]);
  const [selected, setSelected] = useState<number[] | null>(null);
  const [issues, setIssues] = useState<BlockIssue[]>([]);
  const [canPublish, setCanPublish] = useState(true);
  // "The API has not answered yet" is its own fact, not the absence of a blocking issue. Folding
  // the two together made a freshly opened editor claim the API was unreachable for the 250ms
  // before the first dry run landed.
  const [validated, setValidated] = useState(false);
  const [blockCount, setBlockCount] = useState(0);
  const [insertOpen, setInsertOpen] = useState(false);
  const [saving, setSaving] = useState(false);
  const [publishing, setPublishing] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [savedAt, setSavedAt] = useState<string | null>(null);
  const canvasRef = useRef<HTMLDivElement | null>(null);

  // The registry is the same document for every page, so it is fetched once per screen.
  useEffect(() => {
    let cancelled = false;
    fetchBlockRegistry()
      .then((document_) => {
        if (!cancelled) {
          setRegistry(document_);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setLoadError(
            cause instanceof ApiError
              ? cause.message
              : "The block registry could not be loaded.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // The page, and the working tree its draft carries.
  useEffect(() => {
    let cancelled = false;
    setPage(null);
    setLoadError(null);
    fetchPage(pageId)
      .then((loaded: Page) => {
        if (cancelled) {
          return;
        }
        setPage(loaded);
        const draft = loaded.draft ?? loaded.published;
        const tree = Array.isArray(draft?.blocks) ? (draft.blocks as ContentBlock[]) : [];
        setBlocks(tree);
        setSelected(tree.length > 0 ? [0] : null);
        setSavedAt(draft?.created_at ?? null);
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setLoadError(
            cause instanceof ApiError ? cause.message : "The page could not be loaded.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [pageId]);

  // Live validation: every change asks the API what it would say about the tree.
  const validationTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    if (blocks.length === 0 && !registry) {
      return;
    }
    if (validationTimer.current) {
      clearTimeout(validationTimer.current);
    }
    validationTimer.current = setTimeout(() => {
      validateBlocks(blocks)
        .then((result) => {
          setIssues(result.issues);
          setCanPublish(result.can_publish);
          setBlockCount(result.block_count);
          setValidated(true);
        })
        .catch(() => {
          // A dry run that cannot be reached must not silently report "fine": the page keeps its
          // last known answer, publishing is held back, and the bar says why.
          setCanPublish(false);
          setValidated(false);
        });
    }, 250);
    return () => {
      if (validationTimer.current) {
        clearTimeout(validationTimer.current);
      }
    };
  }, [blocks, registry]);

  const issueMap = useMemo(() => issuesByBlock(issues), [issues]);
  const selectedBlock = selected ? blockAt(blocks, selected) : undefined;
  const selectedIssues = selectedBlock ? (issueMap.get(selectedBlock.id) ?? []) : [];
  const blocking = issues.filter((issue) => issue.severity === "error");
  // The path of the first block that needs attention, so the summary in the bottom bar is a
  // way *into* the problem rather than a number the author has to go hunting for. Before this,
  // a blocking issue on a block that is not selected was counted in the bar and visible
  // nowhere else — "1 block needs attention" with no way to reach it is a dead end.
  const firstBlockingPath = (() => {
    const first = blocking[0];
    if (!first) {
      return null;
    }
    const entry = walk(blocks).find(({ block }) => block.id === first.block_id);
    return entry ? entry.path : null;
  })();
  const words = useMemo(() => wordCount(blocks), [blocks]);
  const crumbs = useMemo(
    () => (registry && selected ? breadcrumb(registry, blocks, selected) : []),
    [registry, blocks, selected],
  );

  const insert = useCallback(
    (definition: BlockDefinition) => {
      // A container's children have to fit inside the depth the platform allows; the check is
      // here so the author gets the sentence at the moment they press the button.
      const depth = selected ? selected.length : 0;
      if (definition.container && depth + 1 > MAX_DEPTH) {
        setActionError(
          `Blocks nest at most ${MAX_DEPTH} levels deep. Insert this one at the top level instead.`,
        );
        return;
      }
      // A Columns block is not a single node: inserting one has to bring the column wrappers
      // with it, because "two of these side by side" needs each column to be its own node. The
      // author lands inside the first column, which is the block they are about to fill.
      if (definition.key === "columns") {
        setBlocks((current) => {
          const placed = insertColumns(current, selected ?? [], definition);
          setSelected(placed.path);
          return placed.blocks;
        });
        setInsertOpen(false);
        setActionError(null);
        return;
      }
      const block = newBlock(definition);
      setBlocks((current) => {
        // The new block is selected as it lands. An author who pressed *Heading* is about to
        // type a heading, and an editor that makes them find the new row in the outline first
        // is an editor they will use once and then stop opening.
        if (!selected || selected.length === 0) {
          setSelected([current.length]);
          return [...current, block];
        }
        // Selecting a `column` puts the block inside that column rather than beside the
        // Columns block — the same rule as selecting any other container.
        const parent = blockAt(current, selected);
        if (parent?.type === "column" && !definition.container) {
          setSelected([...selected, (parent.children?.length ?? 0)]);
          return appendChild(current, selected, block);
        }
        if (parent?.children && !definition.container) {
          setSelected([...selected, parent.children.length]);
          return appendChild(current, selected, block);
        }
        setSelected([...selected.slice(0, -1), selected[selected.length - 1] + 1]);
        return insertAfter(current, selected, block);
      });
      setInsertOpen(false);
      setActionError(null);
    },
    [selected],
  );

  const save = async () => {
    if (saving) {
      return;
    }
    setSaving(true);
    setActionError(null);
    setNotice(null);
    try {
      const updated = await updatePage(pageId, { blocks });
      setPage(updated);
      setSavedAt(new Date().toISOString());
      setNotice(
        `Saved as draft revision ${updated.draft?.revision_no ?? "?"}. Publishing is a separate step.`,
      );
    } catch (cause: unknown) {
      setActionError(
        cause instanceof ApiError ? cause.message : "The page could not be saved.",
      );
    } finally {
      setSaving(false);
    }
  };

  const publish = async () => {
    if (publishing) {
      return;
    }
    if (blocking.length > 0) {
      setActionError(
        `Fix ${blocking.length} issue${blocking.length === 1 ? "" : "s"} before publishing — the first one is “${blocking[0].message}”.`,
      );
      return;
    }
    setPublishing(true);
    setActionError(null);
    setNotice(null);
    try {
      const updated = await publishPage(pageId);
      setPage(updated);
      setNotice(
        `Revision ${updated.published?.revision_no ?? "?"} is live at /${updated.slug}.`,
      );
    } catch (cause: unknown) {
      setActionError(
        cause instanceof ApiError ? cause.message : "The page could not be published.",
      );
    } finally {
      setPublishing(false);
    }
  };

  // Keyboard: the shortcuts the REQ names for the canvas, scoped to this screen and never
  // firing while the author is typing into the inspector.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement;
      if (typing || !selected) {
        return;
      }
      if (event.metaKey && event.key === "d") {
        event.preventDefault();
        setBlocks((current) => duplicateBlock(current, selected));
        return;
      }
      if (event.metaKey && event.altKey && (event.key === "ArrowUp" || event.key === "ArrowDown")) {
        event.preventDefault();
        const delta = event.key === "ArrowUp" ? -1 : 1;
        setBlocks((current) => moveBlock(current, selected, delta));
        return;
      }
      if (event.key === "Escape") {
        setSelected(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [selected]);

  if (loadError) {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="The page could not be opened"
          hint={loadError}
          action={
            <Link
              href="/pages"
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              Back to pages
            </Link>
          }
        />
      </div>
    );
  }

  if (!registry || !page) {
    return <LoadingTable columns={4} />;
  }

  // A block can move inside its own list only, so the buttons are disabled from the real
  // sibling count rather than guessed from the path.
  const siblings = selected ? (selected.length === 0 ? blocks : siblingList(blocks, selected)) : [];
  const position = selected ? (selected[selected.length - 1] ?? 0) : 0;
  const canMoveUp = selected ? position > 0 : false;
  const canMoveDown = selected ? position < siblings.length - 1 : false;

  // The column controls belong to the Columns block, not to a column: the count is the layout's
  // own property, so it is edited where the layout is selected and nowhere else. A column can
  // only be removed from the inside, which is why the delete button on a column checks the
  // count before it offers to take the blocks with it.
  const columnsPath = selectedBlock?.type === "columns" ? selected : null;
  const columnCount = columnsPath ? (blockAt(blocks, columnsPath)?.children?.length ?? 0) : 0;
  const canAddColumn = columnsPath !== null && columnCount < MAX_COLUMNS;
  const isColumn = selectedBlock?.type === "column";
  const columnParentPath = isColumn && selected ? selected.slice(0, -1) : null;
  const columnIndex = isColumn && selected ? (selected[selected.length - 1] ?? 0) : -1;
  const canRemoveColumn =
    columnParentPath !== null &&
    (blockAt(blocks, columnParentPath)?.children?.length ?? 0) > MIN_COLUMNS;

  return (
    <div className="flex flex-col gap-4" data-block-editor>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-baseline gap-2">
          <h2 className="text-[15px] font-medium">{pageTitle(page)}</h2>
          <span className="font-mono text-[12px] text-muted">/{page.slug}</span>
          <span className="text-[12px] text-muted">
            draft v{page.draft?.revision_no ?? 1}
            {page.published ? ` · live v${page.published.revision_no}` : " · not published"}
          </span>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Link
            href="/pages"
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            All pages
          </Link>
          <button
            type="button"
            data-block-insert-toggle
            onClick={() => setInsertOpen((open) => !open)}
            aria-expanded={insertOpen}
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            <Plus className="size-3.5" aria-hidden />
            Block
          </button>
          <button
            type="button"
            data-block-save
            onClick={save}
            disabled={saving}
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
          >
            <Save className="size-3.5" aria-hidden />
            {saving ? "Saving…" : "Save draft"}
          </button>
          <button
            type="button"
            data-block-publish
            onClick={publish}
            disabled={publishing || blocking.length > 0}
            title={
              blocking.length > 0
                ? "Fix the blocking issues before publishing"
                : "Publish the working draft"
            }
            className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-quiet-soft disabled:text-muted"
          >
            <Rocket className="size-3.5" aria-hidden />
            {publishing ? "Publishing…" : "Publish"}
          </button>
        </div>
      </div>

      {actionError ? (
        <p
          role="alert"
          className="rounded-xl border border-accent/40 bg-accent-soft px-4 py-3 text-[12.5px] text-accent-strong"
        >
          {actionError}
        </p>
      ) : null}
      {notice ? (
        <p className="rounded-xl border border-line bg-surface px-4 py-3 text-[12.5px] text-muted">
          {notice}
        </p>
      ) : null}

      {insertOpen ? (
        <InsertPanel
          registry={registry}
          onPick={insert}
          onClose={() => setInsertOpen(false)}
        />
      ) : null}

      <div className="grid gap-4 lg:grid-cols-[15rem_minmax(0,1fr)_19rem]">
        {/* Left: the outline of the page's blocks. */}
        <nav aria-label="Page blocks" className="rounded-xl border border-line bg-surface">
          <div className="flex items-center gap-1.5 border-b border-line px-3 py-2">
            <Layers className="size-3.5 text-muted" aria-hidden />
            <h2 className="text-[13px] font-medium">Outline</h2>
            <span className="ml-auto text-[11.5px] text-muted">{blockCount}</span>
          </div>
          <div className="max-h-[32rem] overflow-y-auto p-2">
            {blocks.length === 0 ? (
              <p className="px-1 py-4 text-center text-[12px] text-muted">
                No blocks yet. Add the first one above.
              </p>
            ) : (
              <ul className="flex flex-col gap-0.5">
                {walk(blocks).map(({ block, path, depth }) => (
                  <li key={block.id}>
                    <button
                      type="button"
                      data-block-outline-row
                      onClick={() => setSelected(path)}
                      aria-current={
                        selected?.join("-") === path.join("-") ? "true" : undefined
                      }
                      className={`flex w-full cursor-pointer flex-col rounded-md px-2 py-1.5 text-left transition ${
                        selected?.join("-") === path.join("-")
                          ? "bg-accent-soft"
                          : "hover:bg-canvas"
                      }`}
                      style={{ paddingLeft: `${8 + depth * 12}px` }}
                    >
                      <span className="truncate text-[12px] font-medium">
                        {blockLabel(registry, block)}
                      </span>
                      <span className="truncate text-[11px] text-muted">
                        {blockSummary(registry, block)}
                      </span>
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </nav>

        {/* Centre: the canvas. */}
        <div className="min-w-0">
          <div
            ref={canvasRef}
            data-block-canvas
            className="rounded-xl border border-line bg-surface p-4"
          >
            <BlockCanvas
              registry={registry}
              blocks={blocks}
              mode="edit"
              selected={selected}
              issues={issueMap}
              onSelect={setSelected}
            />
          </div>
        </div>

        {/* Right: the inspector of the selected block. */}
        <div className="min-w-0">
          {selectedBlock ? (
            <BlockInspector
              registry={registry}
              block={selectedBlock}
              issues={selectedIssues}
              onChange={(key, value) =>
                setBlocks((current) => (selected ? setProp(current, selected, key, value) : current))
              }
              onSetting={(key, value) =>
                setBlocks((current) =>
                  selected ? setSetting(current, selected, key, value) : current,
                )
              }
              breadcrumb={crumbs}
              onCrumb={setSelected}
              actions={
                <>
                  {columnsPath !== null ? (
                    <button
                      type="button"
                      data-block-add-column
                      onClick={() =>
                        setBlocks((current) => addColumn(current, columnsPath))
                      }
                      disabled={!canAddColumn}
                      aria-label="Add a column"
                      title={
                        canAddColumn
                          ? "Add a column"
                          : `A Columns block holds at most ${MAX_COLUMNS} columns`
                      }
                      className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-40"
                    >
                      <Plus className="size-3" aria-hidden />
                      Add column
                    </button>
                  ) : null}
                  {isColumn && columnParentPath !== null ? (
                    <button
                      type="button"
                      data-block-remove-column
                      onClick={() => {
                        if (!canRemoveColumn) {
                          setActionError(
                            `A Columns block keeps at least ${MIN_COLUMNS} columns. Add another one before removing this.`,
                          );
                          return;
                        }
                        const filled =
                          blockAt(blocks, columnParentPath)?.children?.[columnIndex]?.children
                            ?.length ?? 0;
                        if (
                          filled > 0 &&
                          !window.confirm(
                            `This column holds ${filled} block${filled === 1 ? "" : "s"}. Removing the column removes ${filled === 1 ? "it" : "them"} too.`,
                          )
                        ) {
                          return;
                        }
                        setActionError(null);
                        setBlocks((current) =>
                          removeColumn(current, columnParentPath, columnIndex),
                        );
                        setSelected(columnParentPath);
                      }}
                      aria-label="Remove this column"
                      title={
                        canRemoveColumn
                          ? "Remove this column and everything in it"
                          : `A Columns block keeps at least ${MIN_COLUMNS} columns`
                      }
                      className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px] text-accent-strong transition hover:bg-accent-soft"
                    >
                      <Trash2 className="size-3" aria-hidden />
                      Remove column
                    </button>
                  ) : null}
                  <button
                    type="button"
                    onClick={() =>
                      selected && setBlocks((current) => moveBlock(current, selected, -1))
                    }
                    disabled={!canMoveUp}
                    aria-label="Move block up"
                    title="Move up (⌘⌥↑)"
                    className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-40"
                  >
                    <ArrowUp className="size-3" aria-hidden />
                    Up
                  </button>
                  <button
                    type="button"
                    onClick={() =>
                      selected && setBlocks((current) => moveBlock(current, selected, 1))
                    }
                    disabled={!canMoveDown}
                    aria-label="Move block down"
                    title="Move down (⌘⌥↓)"
                    className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-40"
                  >
                    <ArrowDown className="size-3" aria-hidden />
                    Down
                  </button>
                  <button
                    type="button"
                    onClick={() =>
                      selected && setBlocks((current) => duplicateBlock(current, selected))
                    }
                    aria-label="Duplicate block"
                    title="Duplicate (⌘D)"
                    className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas"
                  >
                    <Copy className="size-3" aria-hidden />
                    Duplicate
                  </button>
                  <button
                    type="button"
                    onClick={() => {
                      if (!selected) {
                        return;
                      }
                      setBlocks((current) => removeBlock(current, selected));
                      setSelected(selected.length > 1 ? selected.slice(0, -1) : null);
                    }}
                    aria-label="Delete block"
                    title="Delete"
                    className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px] text-accent-strong transition hover:bg-accent-soft"
                  >
                    <Trash2 className="size-3" aria-hidden />
                    Delete
                  </button>
                </>
              }
            />
          ) : (
            <div className="rounded-xl border border-line bg-surface">
              <EmptyState
                title="No block selected"
                hint="Pick a block on the canvas or in the outline to edit its content, layout and visibility."
              />
            </div>
          )}
        </div>
      </div>

      {/* Bottom bar: what the page is made of, what is wrong with it, and when it was saved. */}
      <div
        data-block-status
        data-block-count={blockCount}
        data-block-errors={blocking.length}
        data-block-warnings={issues.length - blocking.length}
        className="flex flex-wrap items-center gap-x-4 gap-y-1 rounded-xl border border-line bg-surface px-4 py-2.5 text-[12px] text-muted"
      >
        <span className="flex items-center gap-1.5">
          <Pencil className="size-3" aria-hidden />
          {words} words · {blockCount} blocks
        </span>
        {blocking.length > 0 ? (
          <button
            type="button"
            data-block-first-issue
            onClick={() => setSelected(firstBlockingPath ?? selected)}
            title={firstBlockingPath ? "Go to the first block that needs attention" : undefined}
            className="font-medium text-accent-strong underline decoration-accent/40 underline-offset-2 transition hover:decoration-accent"
          >
            {blocking.length} block{blocking.length === 1 ? "" : "s"} need attention
            {firstBlockingPath ? " — show me" : ""}
          </button>
        ) : issues.length > 0 ? (
          <span className="text-caution">
            {issues.length} warning{issues.length === 1 ? "" : "s"}
          </span>
        ) : (
          <span className="text-positive">Ready to publish</span>
        )}
        <span className="ml-auto">
          {savedAt ? `Last saved ${new Date(savedAt).toLocaleTimeString()}` : "Not saved yet"}
          {validated
            ? ""
            : canPublish
              ? " · checking…"
              : " · the API could not be reached to confirm the tree"}
        </span>
        <button
          type="button"
          onClick={() => router.push("/pages")}
          className="rounded-md border border-line px-2 py-0.5 text-[11.5px] transition hover:bg-canvas"
        >
          Close
        </button>
      </div>
    </div>
  );
}
