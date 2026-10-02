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
  Eye,
  History as HistoryIcon,
  Layers,
  Library,
  Pencil,
  Plus,
  Redo2,
  Rocket,
  Save,
  Trash2,
  Undo2,
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
import { PatternTools } from "@/features/blocks/pattern-tools";
import {
  canRedo,
  canUndo,
  emptyHistory,
  isTypingStep,
  peekUndo,
  record,
  redo,
  undo,
  type History,
  type StepIdentity,
} from "@/features/blocks/block-history";
import {
  addColumn,
  appendChild,
  blockAt,
  breadcrumb,
  canNest,
  limitProblem,
  cloneWithNewIds,
  duplicateBlock,
  insertAfter,
  insertColumns,
  insertGroup,
  moveBlock,
  newBlock,
  registryLimits,
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

/**
 * A setting in the words an author used to find it.
 *
 * The undo label is the only place a step says what it did, and "set hide_on" is a database key
 * rather than an action. "Hide on mobile" is the phrase on the control, so it is the phrase in
 * the history — which is also what makes the two directions readable: pressing *everywhere*
 * stores absence, and the label says it reset rather than claiming it set something.
 */
function settingLabel(key: string): string {
  switch (key) {
    case "hide_on":
      return "Visibility";
    case "align":
      return "Alignment";
    case "anchor":
      return "Anchor";
    case "id":
      return "DOM id";
    case "class":
      return "CSS class";
    case "aria_label":
      return "Accessible name";
    default:
      return key;
  }
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
  const [patternToolsOpen, setPatternToolsOpen] = useState(false);
  const [saving, setSaving] = useState(false);
  const [publishing, setPublishing] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [savedAt, setSavedAt] = useState<string | null>(null);
  // The undo/redo stack (acceptance 9). It holds trees, not diffs: every helper in
  // `block-tree.ts` is pure and returns a new array, so a snapshot is a value that cannot be
  // mutated later, and undoing is an assignment rather than an inverse operation that would
  // have to know what "inverse" means for each of the nine tree helpers.
  const [history, setHistory] = useState<History>(emptyHistory);
  // The tree as it is RIGHT NOW, outside React's render cycle. Recording a step needs the
  // "before" tree at the moment the author pressed the button, and a `setBlocks` updater runs
  // later — by then the closure may be stale. The ref is written by `apply` and by undo/redo,
  // which are the only things that change the working tree.
  const blocksRef = useRef<ContentBlock[]>([]);
  // The tree the server last acknowledged. Identity is the whole comparison: `blocksRef` is
  // this same array until the author changes something, so `dirty` is a pointer check and not
  // a deep-equal of 400 blocks on every keystroke.
  const savedTreeRef = useRef<ContentBlock[]>([]);
  const [dirty, setDirty] = useState(false);
  // Whether the viewport is wide enough to *edit* on, per the REQ's mobile rule. The answer is
  // measured, not guessed from a CSS class: the editor is three panes, and at 390 px the third
  // one is a stack of 19rem columns the author cannot drag between, so the honest answer there
  // is "read the page, edit it on a wider screen, preview it here" rather than an editor whose
  // inspector is off-screen. `matchMedia` and not a resize listener, because the question is
  // a breakpoint and the browser answers it from the same source the stylesheet does.
  // `false` until the first measure lands, so a server-rendered pass and the first paint agree.
  const [wideEnough, setWideEnough] = useState(false);
  const [viewportKnown, setViewportKnown] = useState(false);
  useEffect(() => {
    const query = window.matchMedia("(min-width: 901px)");
    const measure = () => {
      setWideEnough(query.matches);
      setViewportKnown(true);
    };
    measure();
    query.addEventListener("change", measure);
    return () => query.removeEventListener("change", measure);
  }, []);
  // The newest step's identity. Two prop edits in a row on the same block collapse into one
  // undoable step: without this, a paragraph of typing evicts every structural edit the author
  // made before it from a 100-step history, and undo starts feeling broken.
  const lastStep = useRef<StepIdentity | null>(null);
  const canvasRef = useRef<HTMLDivElement | null>(null);

  /**
   * The one way the working tree changes.
   *
   * Every structural edit and every prop edit goes through here, so the history cannot be
   * bypassed: a mutation applied with a bare `setBlocks` is invisible to undo, which is exactly
   * what an author reports as "undo is broken" and cannot reproduce.
   */
  const apply = useCallback(
    (
      label: string,
      change: (current: ContentBlock[]) => ContentBlock[],
      step: StepIdentity = { blockId: null, kind: label },
    ) => {
      const current = blocksRef.current;
      const next = change(current);
      if (next === current) {
        // The helper refused the operation (the first block cannot move up, a path no longer
        // exists). Recording it would make undo a button that eats a press and changes
        // nothing.
        return false;
      }
      blocksRef.current = next;
      setBlocks(next);
      // Undo can walk the tree back to exactly the saved one, so "dirty" is recomputed from
      // identity here rather than set to true — otherwise undoing back to the saved state
      // still reads as unsaved work.
      setDirty(next !== savedTreeRef.current);
      // A run of typing is ONE step. The snapshot the first keystroke took is already the
      // tree from before the run began, so a later keystroke must not push another one.
      const merge = isTypingStep(lastStep.current, step);
      if (!merge) {
        setHistory((existing) => record(existing, { blocks: current, selected, label }, true));
      }
      lastStep.current = step;
      return true;
    },
    [selected],
  );

  /** Undo one step, and say which one in the status bar. */
  const undoStep = useCallback(() => {
    setHistory((existing) => {
      const taken = undo(existing, blocksRef.current, selected);
      if (!taken) {
        return existing;
      }
      blocksRef.current = taken.entry.blocks;
      setBlocks(taken.entry.blocks);
      setSelected(taken.entry.selected);
      setDirty(taken.entry.blocks !== savedTreeRef.current);
      setNotice(`Undone — ${taken.entry.label.toLowerCase()}.`);
      // The next step starts a new run, so the next prop edit is its own entry rather than a
      // continuation of the one just taken back.
      lastStep.current = null;
      return taken.history;
    });
  }, [selected]);

  /** Redo one step. */
  const redoStep = useCallback(() => {
    setHistory((existing) => {
      const taken = redo(existing, blocksRef.current, selected);
      if (!taken) {
        return existing;
      }
      blocksRef.current = taken.entry.blocks;
      setBlocks(taken.entry.blocks);
      setSelected(taken.entry.selected);
      setDirty(taken.entry.blocks !== savedTreeRef.current);
      setNotice(`Redone — ${taken.entry.label.toLowerCase()}.`);
      lastStep.current = null;
      return taken.history;
    });
  }, [selected]);

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
        blocksRef.current = tree;
        savedTreeRef.current = tree;
        setDirty(false);
        setBlocks(tree);
        setSelected(tree.length > 0 ? [0] : null);
        setSavedAt(draft?.created_at ?? null);
        // A newly opened page starts with nothing to undo: the draft it loaded IS the
        // baseline, and an undo that reached back past it would restore a tree this page
        // never had.
        setHistory(emptyHistory());
        lastStep.current = null;
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
  // A warning never blocks a publish, and that is exactly why it needs its own way into the
  // block carrying it: nothing in the flow forces the author toward a block that is merely
  // worth a look. Counted apart from `blocking` so the bar can say which kind it is reporting
  // without the reader having to subtract one from the other.
  const warnings = issues.filter((issue) => issue.severity !== "error");
  // The same "way into the problem" the blocking branch has, for the same reason.
  const firstWarningPath = (() => {
    const first = warnings[0];
    if (!first) {
      return null;
    }
    const entry = walk(blocks).find(({ block }) => block.id === first.block_id);
    return entry ? entry.path : null;
  })();
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
  // The bounds come from the registry document, so the depth the editor refuses and the depth
  // the validator refuses are the same number read once.
  const limits = useMemo(() => registryLimits(registry), [registry]);
  // Checked here, against the tree the author is holding, rather than only after a dry run:
  // the server already refuses these payloads, and an author who builds past a bound should be
  // told by the editor instead of by a 422 that names a block id.
  const overLimit = useMemo(() => limitProblem(blocks, limits), [blocks, limits]);

  const insert = useCallback(
    (definition: BlockDefinition) => {
      // A container's children have to fit inside the depth the platform allows; the check is
      // here so the author gets the sentence at the moment they press the button.
      const depth = selected ? selected.length : 0;
      if (!canNest(registry, depth)) {
        setActionError(
          `Blocks nest at most ${limits.max_depth} levels deep. Insert this one at the top level instead.`,
        );
        return;
      }
      // A Columns block is not a single node: inserting one has to bring the column wrappers
      // with it, because "two of these side by side" needs each column to be its own node. The
      // author lands inside the first column, which is the block they are about to fill.
      if (definition.key === "columns") {
        apply(`Add ${definition.label}`, (current) => {
          const placed = insertColumns(current, selected ?? [], definition, undefined, limits);
          setSelected(placed.path);
          return placed.blocks;
        });
        setInsertOpen(false);
        setActionError(null);
        return;
      }
      const block = newBlock(definition);
      // A *nesting* insert (into a column, into any container) is the case the criterion names
      // explicitly, and it arrives here through the same two lines as a top-level one — so
      // undo cannot tell them apart, which is right: the author pressed one button either way.
      apply(`Add ${definition.label}`, (current) => {
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
        // A Columns block holds Column blocks and nothing else, and the API says so
        // (`block_child_not_allowed`). Appending a heading as a direct child made a tree the
        // server refuses, which an author can only discover by trying to publish — the editor
        // accepted a structure the renderer cannot draw. `column` is a `structure_only` type, so
        // it is never in the insert panel; the only honest move is to put the block after the
        // Columns block, at the level the author is actually working on.
        if (parent?.type === "columns") {
          setSelected([...selected.slice(0, -1), selected[selected.length - 1] + 1]);
          return insertAfter(current, selected, block);
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
    // `registry` is in the deps because the depth bound is read off it: the callback is created
    // on the first render, when the document has not landed yet, so a stale closure would keep
    // answering with the fallback numbers for the whole session. That is the mirror this tick
    // removed, reintroduced one level up.
    [apply, registry, selected],
  );

  const save = async () => {
    if (saving) {
      return;
    }
    // BEFORE `setSaving(true)`: a guard placed after it returns without ever clearing the flag,
    // so the button keeps its spinner forever and the author has no way to try again.
    if (overLimit) {
      setActionError(overLimit);
      return;
    }
    setSaving(true);
    setActionError(null);
    setNotice(null);
    // The tree the server is about to be given. Captured before the await because a keystroke
    // during the round trip must not be what gets saved, and `blocks` in this closure is the
    // tree as of the click.
    const treeToSave = blocksRef.current;
    try {
      const updated = await updatePage(pageId, { blocks: treeToSave });
      setPage(updated);
      setSavedAt(new Date().toISOString());
      setNotice(
        `Saved as draft revision ${updated.draft?.revision_no ?? "?"}. Publishing is a separate step.`,
      );
      // A save does NOT clear the history and does NOT become a step of its own — it is the
      // boundary between "what the server holds" and "what the author is doing". Clearing the
      // history would satisfy the criterion for exactly one press and then lose everything
      // the author did before it; recording the save AS a step is just as wrong, because the
      // tree at the moment of the save is the tree that is already on screen, so the first
      // `⌘Z` after a save would restore an identical tree and appear to do nothing.
      //
      // What the author needs instead is to know the server is now AHEAD of the editor once
      // they undo, which is what `savedTree` is for: the bar says the draft still holds the
      // saved revision and that the change is unsaved again.
      savedTreeRef.current = treeToSave;
      setDirty(false);
      lastStep.current = null;
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
        target instanceof HTMLSelectElement ||
        (target instanceof HTMLElement && target.isContentEditable);
      // Undo/redo come FIRST and are the only shortcuts that work with nothing selected: an
      // author who has just deleted the block they were looking at has no selection, and
      // `⌘Z` is exactly what they press. Inside a text field the browser's own undo is the
      // better one — it undoes the keystroke, not the whole field edit — so it is left alone.
      if (!typing && (event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "z") {
        event.preventDefault();
        if (event.shiftKey) {
          redoStep();
        } else {
          undoStep();
        }
        return;
      }
      if (typing || !selected) {
        return;
      }
      if (event.metaKey && event.key === "d") {
        event.preventDefault();
        apply("Duplicate block", (current) => duplicateBlock(current, selected));
        return;
      }
      if (event.metaKey && event.altKey && (event.key === "ArrowUp" || event.key === "ArrowDown")) {
        event.preventDefault();
        const delta = event.key === "ArrowUp" ? -1 : 1;
        apply(delta < 0 ? "Move block up" : "Move block down", (current) =>
          moveBlock(current, selected, delta),
        );
        return;
      }
      if (event.key === "Escape") {
        setSelected(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [apply, redoStep, selected, undoStep]);

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
  const canAddColumn = columnsPath !== null && columnCount < limits.max_columns;
  const isColumn = selectedBlock?.type === "column";
  const columnParentPath = isColumn && selected ? selected.slice(0, -1) : null;
  const columnIndex = isColumn && selected ? (selected[selected.length - 1] ?? 0) : -1;
  const canRemoveColumn =
    columnParentPath !== null &&
    (blockAt(blocks, columnParentPath)?.children?.length ?? 0) > limits.min_columns;
  // Narrow means "measured, and too narrow to edit". An unmeasured viewport is treated as wide
  // for one frame, because the alternative is a phone that flashes a read-only notice before the
  // author has seen an editor — and a desktop that does the same on a slow first paint.
  const narrow = viewportKnown && !wideEnough;

  return (
    <div
      className="flex flex-col gap-4"
      data-block-editor
      // The narrow answer, readable from the DOM rather than from a screenshot: a screen the
      // author cannot edit on says so with these three attributes, and the pass asserts them
      // instead of asserting that a button happens to be greyed out.
      data-block-editor-narrow={narrow ? "true" : "false"}
      data-block-editor-editable={narrow ? "false" : "true"}
    >
      {narrow ? (
        <div
          role="status"
          data-block-editor-narrow-notice
          className="flex flex-col gap-2 rounded-xl border border-line bg-surface px-4 py-3"
        >
          <p className="text-[13px] font-medium">Editing needs a wider screen</p>
          <p className="max-w-prose text-[12.5px] text-muted">
            This editor arranges a page in three panes side by side, which a phone has no room
            for. The page below still reads exactly as it will be published, and the preview is
            fully usable here — open it, switch to the phone, and edit the text in place. Save
            draft and Publish need the wider screen.
          </p>
          <div className="flex flex-wrap items-center gap-2">
            <Link
              href={`/pages/${pageId}/preview`}
              data-block-narrow-preview
              className="flex w-fit items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              <Eye className="size-3.5" aria-hidden />
              Open the preview
            </Link>
            <span className="text-[12px] text-muted">
              {blockCount} block{blockCount === 1 ? "" : "s"} · {words} words
            </span>
          </div>
        </div>
      ) : null}
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
            href={`/pages/${pageId}/preview`}
            data-block-preview-link
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            <Eye className="size-3.5" aria-hidden />
            Preview
          </Link>
          <Link
            href="/pages"
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            All pages
          </Link>
          {!narrow ? (
            <>
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
            data-pattern-tools-toggle
            onClick={() => setPatternToolsOpen((open) => !open)}
            aria-expanded={patternToolsOpen}
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            <Library className="size-3.5" aria-hidden />
            Patterns
          </button>
          <button
            type="button"
            data-block-undo
            onClick={undoStep}
            disabled={!canUndo(history)}
            aria-label="Undo"
            title={
              canUndo(history)
                ? `Undo — ${peekUndo(history)?.label.toLowerCase()} (⌘Z)`
                : "Nothing to undo"
            }
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-40"
          >
            <Undo2 className="size-3.5" aria-hidden />
            Undo
          </button>
          <button
            type="button"
            data-block-redo
            onClick={redoStep}
            disabled={!canRedo(history)}
            aria-label="Redo"
            title={canRedo(history) ? "Redo (⇧⌘Z)" : "Nothing to redo"}
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-40"
          >
            <Redo2 className="size-3.5" aria-hidden />
            Redo
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
            </>
          ) : null}
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

      {patternToolsOpen ? (
        <PatternTools
          registry={registry}
          blocks={blocks}
          selectedPath={selected}
          onInsert={(patternBlocks) => {
            // A pattern lands where the author is: inside a container they just made, after the
            // block they just selected, or — with nothing selected — at the end of the page.
            // `insertGroup` owns those three cases; the reason for each lives there.
            apply(
              `Insert pattern (${patternBlocks.length} block${patternBlocks.length === 1 ? "" : "s"})`,
              (current) => insertGroup(current, selected, patternBlocks),
            );
            setPatternToolsOpen(false);
          }}
          onClose={() => setPatternToolsOpen(false)}
        />
      ) : null}

      {/* The three panes are one column on a narrow screen, and the outline and the inspector
          are *removed* there rather than stacked: a selection that scrolls away while the author
          types, and an inspector below a canvas nobody can see, are two ways to make the phone
          look like a broken editor. `narrow` is the measured question, not a Tailwind class, so
          the DOM the pass reads and the layout the browser draws are the same fact. */}
      <div
        className={`grid gap-4 ${narrow ? "grid-cols-1" : "lg:grid-cols-[15rem_minmax(0,1fr)_19rem]"}`}
      >
        {!narrow ? (
          <>
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
          </>
        ) : null}

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
              // `render` on a phone, and it is the same component the public renderer draws the
              // page with: a read-only editor that showed a *different* preview of the same tree
              // would be a third implementation of "what the page looks like", which is the class
              // of bug this REQ has been fighting since slice 1.
              mode={narrow ? "render" : "edit"}
              selected={selected}
              issues={issueMap}
              onSelect={setSelected}
            />
          </div>
        </div>

        {!narrow ? (
          <>
        {/* Right: the inspector of the selected block. */}
        <div className="min-w-0">
          {selectedBlock ? (
            <BlockInspector
              registry={registry}
              block={selectedBlock}
              issues={selectedIssues}
              onChange={(key, value) => {
                if (!selected || !selectedBlock) {
                  return;
                }
                // A prop edit fires per keystroke, so it is keyed by BLOCK id: consecutive
                // edits to the same block collapse into one undoable step, and moving to a
                // different block starts a new one.
                const label = `Edit ${blockLabel(registry, selectedBlock)}`;
                apply(
                  label,
                  (current) => setProp(current, selected, key, value),
                  { blockId: selectedBlock.id, kind: "prop" },
                );
              }}
              onSetting={(key, value) => {
                if (!selected || !selectedBlock) {
                  return;
                }
                // A setting is one press of a `<select>`, never a keystroke, so it is its own
                // step and never merges with the prop edit before it.
                const label =
                  value === ""
                    ? `Reset ${settingLabel(key)}`
                    : `${settingLabel(key)} — ${value}`;
                apply(label, (current) => setSetting(current, selected, key, value));
              }}
              breadcrumb={crumbs}
              onCrumb={setSelected}
              actions={
                <>
                  {columnsPath !== null ? (
                    <button
                      type="button"
                      data-block-add-column
                      onClick={() =>
                        apply("Add column", (current) => addColumn(current, columnsPath, limits))
                      }
                      disabled={!canAddColumn}
                      aria-label="Add a column"
                      title={
                        canAddColumn
                          ? "Add a column"
                          : `A Columns block holds at most ${limits.max_columns} columns`
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
                            `A Columns block keeps at least ${limits.min_columns} columns. Add another one before removing this.`,
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
                        apply("Remove column", (current) =>
                          removeColumn(current, columnParentPath, columnIndex, limits),
                        );
                        setSelected(columnParentPath);
                      }}
                      aria-label="Remove this column"
                      title={
                        canRemoveColumn
                          ? "Remove this column and everything in it"
                          : `A Columns block keeps at least ${limits.min_columns} columns`
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
                      selected && apply("Move block up", (current) => moveBlock(current, selected, -1))
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
                      selected && apply("Move block down", (current) => moveBlock(current, selected, 1))
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
                      selected &&
                      apply("Duplicate block", (current) => duplicateBlock(current, selected))
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
                      if (!selected || !selectedBlock) {
                        return;
                      }
                      // Deleting the selection clears it, which is why the criterion's
                      // "`⌘Z` after a save" scenario needs an undo that works with nothing
                      // selected — the author is already in that state here.
                      apply(`Delete ${blockLabel(registry, selectedBlock)}`, (current) =>
                        removeBlock(current, selected),
                      );
                      setSelected(selected.length > 1 ? selected.slice(0, -1) : null);
                    }}
                    aria-label="Delete block"
                    title="Delete"
                    data-block-delete
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
          </>
        ) : null}
      </div>

      {/* Bottom bar: what the page is made of, what is wrong with it, and when it was saved. */}
      <div
        data-block-status
        data-block-count={blockCount}
        data-block-errors={blocking.length}
        data-block-warnings={warnings.length}
        data-block-undo-depth={history.past.length}
        data-block-redo-depth={history.future.length}
        data-block-dirty={dirty ? "true" : "false"}
        className="flex flex-wrap items-center gap-x-4 gap-y-1 rounded-xl border border-line bg-surface px-4 py-2.5 text-[12px] text-muted"
      >
        <span className="flex items-center gap-1.5">
          <Pencil className="size-3" aria-hidden />
          {words} words · {blockCount} blocks
        </span>
        {overLimit ? (
          <span data-block-over-limit className="font-medium text-caution">
            {overLimit}
          </span>
        ) : null}
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
        ) : warnings.length > 0 ? (
          /* A warning is advisory, so it never blocks a publish — which used to make it
             unreachable as well. The issue list itself lives in the inspector, so a warning on a
             block the author is not looking at was counted in this bar and visible nowhere else,
             and the one thing the author could not do was read it. The blocking branch above
             solved exactly this with a "show me"; a warning is the same dead end with a softer
             voice, so it gets the same way in. */
          <button
            type="button"
            data-block-first-warning
            onClick={() => setSelected(firstWarningPath ?? selected)}
            title={firstWarningPath ? "Go to the first block with a warning" : undefined}
            className="text-caution underline decoration-caution/40 underline-offset-2 transition hover:decoration-caution"
          >
            {warnings.length} warning{warnings.length === 1 ? "" : "s"}
            {firstWarningPath ? " — show me" : ""}
          </button>
        ) : (
          <span className="text-positive">Ready to publish</span>
        )}
        {/* The history depth, in words. A number alone says nothing about whether the button
            does anything, and the "can I undo" question is the one an author asks when they
            reach for ⌘Z. */}
        <span data-block-history className="flex items-center gap-1.5">
          <HistoryIcon className="size-3" aria-hidden />
          {history.past.length} undoable
          {canRedo(history) ? ` · ${history.future.length} redoable` : ""}
        </span>
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
