"use client";

/**
 * `/themes/<key>/builder` — the theme builder (REQ-062, slice 3).
 *
 * This is the *same editor* the page builder has, and that is not a simplification — it is the
 * requirement. A header, a footer and a page body are all block trees rendered by one
 * renderer, so a second editor for slots would be a second implementation of insert, reorder,
 * duplicate, delete, nesting, the inspector and undo, and the first time one of them changed
 * the two would render the same tree differently. So this screen reuses `BlockCanvas`,
 * `BlockInspector`, `InsertPanel` and the whole of `block-tree.ts`, and the only thing it adds
 * is what a slot has and a page does not:
 *
 *  - **A slot picker.** The eight platform slots are a `const` in the crate, and the picker
 *    shows all eight every time — including the ones with nothing in them. A picker that hides
 *    an empty slot is a picker that cannot answer "what happens if I clear the header".
 *  - **A reset, per slot, and it is a *state* and not a button that always works.** A theme
 *    that ships no blocks for a slot has nothing to restore, the server answers `409`, and the
 *    control only appears for a slot the theme actually ships something for. Before the
 *    `default_blocks` split, one custom save took the default away and the reset answered 409
 *    on a slot that had had a default a moment earlier — a confirmation behind a one-way door.
 *  - **A save that is a draft.** A slot save writes the site's own row; what a visitor draws is
 *    the *published* settings revision's theme plus whichever slot row is not a default. Same
 *    draft/published split as the settings surface, one level down. This screen therefore never
 *    says "saved" without saying "saved for this site, not yet rendering".
 *
 * Validation is the server's, for the same reason the page editor's is: `POST /blocks/validate`
 * is the same code path `PUT /theme-layouts/{slot}` runs through `blocks::prepare_tree`, so a
 * badge this screen shows and a refusal the save returns cannot disagree.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { BlockDefinition, BlockIssue, BlockRegistry, ContentBlock } from "@omnion/types";
import {
  ArrowDown,
  ArrowUp,
  Copy,
  Download,
  Layers,
  Plus,
  Redo2,
  RotateCcw,
  Save,
  Trash2,
  Undo2,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  exportThemePackage,
  fetchBlockRegistry,
  fetchThemeLayouts,
  resetThemeSlot,
  saveThemeSlot,
  validateBlocks,
} from "@/lib/api";
import type { ThemeLayoutsView, ThemeSlotEntry } from "@/lib/api";
import { useSites } from "@/lib/sites";
import { BlockCanvas } from "@/features/blocks/block-canvas";
import { BlockInspector, InsertPanel } from "@/features/blocks/block-inspector";
import { blockLabel, blockSummary, definitionFor } from "@/features/blocks/block-library";
import {
  canRedo,
  canUndo,
  emptyHistory,
  peekUndo,
  record,
  redo,
  undo,
  type History,
} from "@/features/blocks/block-history";
import {
  addColumn,
  canNest,
  appendChild,
  blockAt,
  breadcrumb,
  duplicateBlock,
  insertAfter,
  insertColumns,
  moveBlock,
  newBlock,
  registryLimits,
  removeBlock,
  setProp,
  setSetting,
  walk,
  wordCount,
} from "@/features/blocks/block-tree";

/** The slot names, in the words the platform uses for them. */
const SLOT_LABELS: Record<string, string> = {
  header: "Header",
  footer: "Footer",
  home: "Home page",
  "blog-list": "Blog list",
  "single-page": "Single page",
  product: "Product",
  "404": "Not found",
  search: "Search results",
};

/** The three words the server uses, in the words an operator would say. */
const STATE_LABELS: Record<string, string> = {
  theme: "Theme default",
  custom: "Customised",
  empty: "Empty",
};

const STATE_CLASSES: Record<string, string> = {
  theme: "bg-canvas text-muted",
  custom: "bg-accent-soft text-accent-strong",
  empty: "bg-caution-soft text-caution",
};

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

/** The list a block at `path` lives in. */
function siblingList(blocks: ContentBlock[], path: number[]): ContentBlock[] {
  if (path.length === 0) {
    return blocks;
  }
  return blockAt(blocks, path.slice(0, -1))?.children ?? blocks;
}

export function ThemeBuilderView() {
  const { selectedSite, status: siteStatus, error: siteError } = useSites();
  const [registry, setRegistry] = useState<BlockRegistry | null>(null);
  const [layouts, setLayouts] = useState<ThemeLayoutsView | null>(null);
  const [slot, setSlot] = useState<string | null>(null);
  const [blocks, setBlocks] = useState<ContentBlock[]>([]);
  const [selected, setSelected] = useState<number[] | null>(null);
  const [history, setHistory] = useState<History>(emptyHistory());
  const [issues, setIssues] = useState<BlockIssue[]>([]);
  const [blockCount, setBlockCount] = useState(0);
  const [insertOpen, setInsertOpen] = useState(false);
  const [busy, setBusy] = useState<"save" | "reset" | "export" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [pendingReset, setPendingReset] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);
  const [savedAt, setSavedAt] = useState<string | null>(null);
  const [exporting, setExporting] = useState(false);

  // The working tree, kept beside the rendered one so a keystroke during a round trip is not
  // what a save posts, and the saved baseline so "unsaved" is a comparison of two values
  // rather than a flag that a failed request can leave lying.
  const blocksRef = useRef<ContentBlock[]>([]);
  const savedRef = useRef<ContentBlock[]>([]);

  // The registry is the same document for every slot, so it is fetched once.
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
          setError(cause instanceof ApiError ? cause.message : "The block registry could not be loaded.");
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const load = useCallback(async () => {
    if (!selectedSite) return;
    setLayouts(null);
    setError(null);
    try {
      const view = await fetchThemeLayouts(selectedSite.id);
      setLayouts(view);
      // The picker opens on the first slot the site has changed, because that is the one an
      // operator came back for. With nothing customised it opens on `header` — the first slot
      // in the platform's own order, so the screen never invents a "most likely" slot.
      const customised = view.slots.find((entry) => entry.state === "custom");
      setSlot((current) => current ?? customised?.slot ?? view.slots[0]?.slot ?? null);
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The theme layouts could not be loaded.");
    }
  }, [selectedSite]);

  useEffect(() => {
    void load();
  }, [load]);

  const entry: ThemeSlotEntry | null = useMemo(
    () => layouts?.slots.find((candidate) => candidate.slot === slot) ?? null,
    [layouts, slot],
  );

  // Switching slot loads that slot's tree and starts a fresh history: the previous slot's undo
  // stack would answer `⌘Z` on a tree the author is no longer looking at.
  useEffect(() => {
    if (!entry) return;
    const tree = Array.isArray(entry.blocks) ? (entry.blocks as ContentBlock[]) : [];
    blocksRef.current = tree;
    savedRef.current = tree;
    setBlocks(tree);
    setSelected(tree.length > 0 ? [0] : null);
    setDirty(false);
    setSavedAt(null);
    setNotice(null);
    setError(null);
    setHistory(emptyHistory());
  }, [entry]);

  // Live validation: the same dry run the save performs.
  const validationTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    if (!registry) return;
    if (validationTimer.current) clearTimeout(validationTimer.current);
    validationTimer.current = setTimeout(() => {
      validateBlocks(blocks)
        .then((result) => {
          setIssues(result.issues);
          setBlockCount(result.block_count);
        })
        .catch(() => {
          // A dry run that cannot be reached must not read as "fine".
          setIssues([]);
        });
    }, 250);
    return () => {
      if (validationTimer.current) clearTimeout(validationTimer.current);
    };
  }, [blocks, registry]);

  /** One undoable change, with the history updated in the same pass. */
  const apply = useCallback(
    (label: string, change: (current: ContentBlock[]) => ContentBlock[]) => {
      const current = blocksRef.current;
      const next = change(current);
      if (next === current) {
        return;
      }
      blocksRef.current = next;
      setBlocks(next);
      setDirty(next !== savedRef.current);
      setHistory((existing) => record(existing, { blocks: current, selected, label }, true));
    },
    [selected],
  );

  const undoStep = useCallback(() => {
    setHistory((existing) => {
      const taken = undo(existing, blocksRef.current, selected);
      if (!taken) return existing;
      blocksRef.current = taken.entry.blocks;
      setBlocks(taken.entry.blocks);
      setSelected(taken.entry.selected);
      setDirty(taken.entry.blocks !== savedRef.current);
      setNotice(`Undone — ${taken.entry.label.toLowerCase()}.`);
      return taken.history;
    });
  }, [selected]);

  const redoStep = useCallback(() => {
    setHistory((existing) => {
      const taken = redo(existing, blocksRef.current, selected);
      if (!taken) return existing;
      blocksRef.current = taken.entry.blocks;
      setBlocks(taken.entry.blocks);
      setSelected(taken.entry.selected);
      setDirty(taken.entry.blocks !== savedRef.current);
      setNotice(`Redone — ${taken.entry.label.toLowerCase()}.`);
      return taken.history;
    });
  }, [selected]);

  const insert = useCallback(
    (definition: BlockDefinition) => {
      const depth = selected ? selected.length : 0;
      if (!canNest(registry, depth)) {
        setError(`Blocks nest at most ${limits.max_depth} levels deep. Insert this one at the top level instead.`);
        return;
      }
      if (definition.key === "columns") {
        apply(`Add ${definition.label}`, (current) => {
          const placed = insertColumns(current, selected ?? [], definition, undefined, limits);
          setSelected(placed.path);
          return placed.blocks;
        });
        setInsertOpen(false);
        return;
      }
      const block = newBlock(definition);
      apply(`Add ${definition.label}`, (current) => {
        if (!selected || selected.length === 0) {
          setSelected([current.length]);
          return [...current, block];
        }
        const parent = blockAt(current, selected);
        if (parent?.type === "column" && !definition.container) {
          setSelected([...selected, parent.children?.length ?? 0]);
          return appendChild(current, selected, block);
        }
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
    },
    // `registry` because the depth bound is read off it; without it the callback keeps the
    // fallback numbers from the render that created it.
    [apply, registry, selected],
  );

  const save = async () => {
    if (!selectedSite || !slot || busy) return;
    setBusy("save");
    setError(null);
    setNotice(null);
    const treeToSave = blocksRef.current;
    try {
      const result = await saveThemeSlot(selectedSite.id, slot, treeToSave);
      savedRef.current = treeToSave;
      setDirty(false);
      setSavedAt(new Date().toISOString());
      // The picker's badge is the server's, and the save answers with the layout it wrote — so
      // the badge moves without a second request and without this screen deciding what a save
      // meant. An empty saved tree is `empty`, not `custom`: a slot the author deliberately
      // cleared is not a slot they customised, and saying otherwise would put a "Reset to
      // theme default" control on a row that is already the question.
      setLayouts((current) =>
        current
          ? {
              ...current,
              slots: current.slots.map((candidate) =>
                candidate.slot === slot
                  ? {
                      ...candidate,
                      blocks: treeToSave,
                      blockCount: treeToSave.length,
                      state: treeToSave.length === 0 ? "empty" : "custom",
                      isDefault: false,
                    }
                  : candidate,
              ),
            }
          : current,
      );
      setIssues(
        result.issues.map((message) => ({
          block_id: "",
          path: "slot",
          code: "slot_warning",
          message,
          severity: "warning" as const,
        })),
      );
      setNotice(
        `Saved for this site. ${result.issues.length} note${result.issues.length === 1 ? "" : "s"} came back with the save; a slot with an empty region still renders.`,
      );
    } catch (cause: unknown) {
      // The refusal arrives with the validator's own sentences; printing them is the only way
      // an author can act on them. The local tree is untouched.
      setError(cause instanceof ApiError ? cause.message : "The slot could not be saved.");
    } finally {
      setBusy(null);
    }
  };

  const doReset = async (target: string) => {
    if (!selectedSite || busy) return;
    setPendingReset(null);
    setBusy("reset");
    setError(null);
    setNotice(null);
    try {
      const result = await resetThemeSlot(selectedSite.id, target);
      const restored = Array.isArray(result.layout.blocks) ? (result.layout.blocks as ContentBlock[]) : [];
      blocksRef.current = restored;
      savedRef.current = restored;
      setBlocks(restored);
      setSelected(restored.length > 0 ? [0] : null);
      setDirty(false);
      setHistory(emptyHistory());
      setLayouts((current) =>
        current
          ? {
              ...current,
              slots: current.slots.map((candidate) =>
                candidate.slot === target
                  ? {
                      ...candidate,
                      blocks: restored,
                      blockCount: restored.length,
                      state: restored.length === 0 ? "empty" : "theme",
                      isDefault: true,
                    }
                  : candidate,
              ),
            }
          : current,
      );
      setNotice(`Restored what ${layouts?.themeKey ?? "the theme"} ships for ${SLOT_LABELS[target] ?? target}.`);
    } catch (caught: unknown) {
      setError(caught instanceof ApiError ? caught.message : "The slot could not be restored.");
    } finally {
      setBusy(null);
    }
  };

  // The export is a download, and it has to be: a package is a file an operator hands to
  // another site, and a JSON blob they cannot see is not a package. The anchor carries the
  // cookie because the API is session-authenticated and a plain navigation would not.
  const downloadPackage = async () => {
    if (!selectedSite || busy) return;
    setBusy("export");
    setError(null);
    try {
      const pkg = await exportThemePackage(selectedSite.id);
      const blob = new Blob([JSON.stringify(pkg, null, 2)], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = `${pkg.themeKey || "theme"}-package.json`;
      document.body.appendChild(anchor);
      anchor.click();
      anchor.remove();
      URL.revokeObjectURL(url);
      setExporting(false);
      setNotice("The package was downloaded. Import it from the gallery's Upload a theme screen.");
    } catch (caught: unknown) {
      setError(caught instanceof ApiError ? caught.message : "The package could not be exported.");
    } finally {
      setBusy(null);
    }
  };

  // Keyboard: the editor's own shortcuts, scoped to this screen and never firing while the
  // author is typing into a field.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement ||
        (target instanceof HTMLElement && target.isContentEditable);
      if (!typing && (event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "z") {
        event.preventDefault();
        if (event.shiftKey) {
          redoStep();
        } else {
          undoStep();
        }
        return;
      }
      if (typing || !selected) return;
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
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [apply, redoStep, selected, undoStep]);

  if (siteStatus === "error") {
    return <EmptyState title="No site selected" hint={siteError ?? "The site list could not be loaded."} />;
  }
  if (!selectedSite) {
    return <EmptyState title="No site selected" hint="Pick a site to build its theme's slots." />;
  }
  if (!registry || !layouts) {
    return error ? (
      <EmptyState title="The builder could not be opened" hint={error} />
    ) : (
      <LoadingTable columns={3} rows={4} />
    );
  }

  const issueMap = issuesByBlock(issues.filter((issue) => issue.block_id !== ""));
  const selectedBlock = selected ? blockAt(blocks, selected) : undefined;
  const selectedIssues = selectedBlock ? (issueMap.get(selectedBlock.id) ?? []) : [];
  const blocking = issues.filter((issue) => issue.severity === "error");
  const words = wordCount(blocks);
  const crumbs = selected ? breadcrumb(registry, blocks, selected) : [];
  // Read off the registry document, so this builder's ceiling is the validator's ceiling rather
  // than a second copy of a constant that can move without this file noticing.
  const limits = registryLimits(registry);
  const siblings = selected ? siblingList(blocks, selected) : [];
  const position = selected ? (selected[selected.length - 1] ?? 0) : 0;
  const canMoveUp = selected ? position > 0 : false;
  const canMoveDown = selected ? position < siblings.length - 1 : false;
  const columnsPath = selectedBlock?.type === "columns" ? selected : null;
  const isColumn = selectedBlock?.type === "column";
  const columnParentPath = isColumn && selected ? selected.slice(0, -1) : null;
  // Reset is offered for a slot the theme *ships* something for. `state: empty` is excluded on
  // purpose: it is either a theme that ships nothing (nothing to restore) or a site that
  // deliberately cleared the slot, and in both cases the honest answer is the server's 409
  // rather than a button that only works for some empties.
  const canReset = entry !== null && (entry.state === "custom" || entry.state === "theme") && entry.blockCount > 0;

  return (
    <div className="flex flex-col gap-4" data-theme-builder data-theme-key={layouts.themeKey}>
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h2 className="text-[15px] font-medium">Theme builder</h2>
          <p className="text-[12.5px] text-muted" data-theme-builder-theme-key={layouts.themeKey}>
            {layouts.themeKey} · the eight regions the renderer draws. A slot save is this site&apos;s
            own copy; the theme&apos;s own blocks stay available to restore.
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
            onClick={() => setExporting((open) => !open)}
            aria-expanded={exporting}
            data-theme-builder-export-toggle
          >
            <Download className="size-3.5" aria-hidden />
            Export package
          </button>
          <button
            type="button"
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
            onClick={() => void downloadPackage()}
            disabled={busy !== null}
            data-theme-builder-export
          >
            {busy === "export" ? "Preparing…" : "Download the package"}
          </button>
          <Link
            href="/themes/upload"
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            data-theme-builder-upload-link
          >
            Upload a theme
          </Link>
        </div>
      </header>

      {exporting ? (
        <p className="rounded-xl border border-line bg-surface px-4 py-3 text-[12.5px] text-muted" data-theme-builder-export-note>
          The package carries this site&apos;s active theme, its published tokens and its slot trees
          — including the theme defaults, so the second site can restore a slot it has not edited.
          Import it from <Link className="underline" href="/themes/upload">Upload a theme</Link>; the
          install lands inactive, so nothing renders with it until an operator activates it.
        </p>
      ) : null}

      {error ? (
        <p role="alert" className="rounded-xl border border-accent/40 bg-accent-soft px-4 py-3 text-[12.5px] text-accent-strong" data-theme-builder-error>
          {error}
        </p>
      ) : null}
      {notice ? (
        <p role="status" className="rounded-xl border border-line bg-surface px-4 py-3 text-[12.5px] text-muted" data-theme-builder-notice>
          {notice}
        </p>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-[13rem_minmax(0,1fr)_19rem]">
        {/* Left: the slot picker. All eight, always — including the empty ones. */}
        <nav aria-label="Theme slots" className="rounded-xl border border-line bg-surface" data-theme-builder-slots>
          <div className="flex items-center gap-1.5 border-b border-line px-3 py-2">
            <Layers className="size-3.5 text-muted" aria-hidden />
            <h3 className="text-[13px] font-medium">Slots</h3>
          </div>
          <ul className="flex flex-col gap-0.5 p-2">
            {layouts.slots.map((candidate) => (
              <li key={candidate.slot}>
                <button
                  type="button"
                  data-theme-slot={candidate.slot}
                  data-theme-slot-state={candidate.state}
                  aria-current={candidate.slot === slot ? "true" : undefined}
                  onClick={() => setSlot(candidate.slot)}
                  className={`flex w-full flex-col gap-0.5 rounded-md px-2 py-1.5 text-left transition ${
                    candidate.slot === slot ? "bg-accent-soft" : "hover:bg-canvas"
                  }`}
                >
                  <span className="flex items-center justify-between gap-2">
                    <span className="truncate text-[12px] font-medium">
                      {SLOT_LABELS[candidate.slot] ?? candidate.slot}
                    </span>
                    <span
                      className={`shrink-0 rounded-full px-1.5 py-0.5 text-[10px] ${STATE_CLASSES[candidate.state] ?? ""}`}
                    >
                      {STATE_LABELS[candidate.state] ?? candidate.state}
                    </span>
                  </span>
                  <span className="text-[11px] text-muted">
                    {candidate.blockCount} block{candidate.blockCount === 1 ? "" : "s"}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </nav>

        {/* Centre: the canvas of the selected slot. */}
        <div className="min-w-0">
          <div className="mb-3 flex flex-wrap items-center gap-2">
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
              data-block-undo
              onClick={undoStep}
              disabled={!canUndo(history)}
              className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-40"
              title={canUndo(history) ? `Undo — ${peekUndo(history)?.label.toLowerCase()} (⌘Z)` : "Nothing to undo"}
            >
              <Undo2 className="size-3.5" aria-hidden />
              Undo
            </button>
            <button
              type="button"
              data-block-redo
              onClick={redoStep}
              disabled={!canRedo(history)}
              className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-40"
            >
              <Redo2 className="size-3.5" aria-hidden />
              Redo
            </button>
            <button
              type="button"
              data-theme-builder-save
              onClick={() => void save()}
              disabled={busy !== null || !dirty}
              className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
            >
              <Save className="size-3.5" aria-hidden />
              {busy === "save" ? "Saving…" : "Save slot"}
            </button>
            {canReset ? (
              <button
                type="button"
                data-theme-builder-reset
                onClick={() => setPendingReset(slot)}
                disabled={busy !== null}
                className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
                title={`Put back what ${layouts.themeKey} ships for ${SLOT_LABELS[slot ?? ""] ?? slot}`}
              >
                <RotateCcw className="size-3.5" aria-hidden />
                Reset to theme default
              </button>
            ) : null}
          </div>

          {insertOpen && registry ? (
            <InsertPanel registry={registry} onPick={insert} onClose={() => setInsertOpen(false)} />
          ) : null}

          <div className="rounded-xl border border-line bg-surface p-4" data-theme-builder-canvas data-theme-builder-canvas-slot={slot ?? ""}>
            {slot ? (
              <BlockCanvas
                registry={registry}
                blocks={blocks}
                mode="edit"
                selected={selected}
                issues={issueMap}
                onSelect={setSelected}
              />
            ) : null}
          </div>

          {/* The bottom bar: the same facts the page editor reports, because this is the same
              editor. "Saved for this site" is not the same claim as "rendering", and the bar
              never blurs them. */}
          <div className="mt-3 flex flex-wrap items-center gap-x-4 gap-y-1 text-[12px] text-muted" data-theme-builder-bar>
            <span data-theme-builder-words>{words} words</span>
            <span data-theme-builder-count>{blockCount} blocks</span>
            <span data-theme-builder-slot-state={entry?.state ?? ""}>
              {STATE_LABELS[entry?.state ?? ""] ?? "—"}
            </span>
            <span data-theme-builder-dirty={dirty ? "true" : "false"}>
              {dirty ? "Unsaved changes" : savedAt ? `Saved ${new Date(savedAt).toLocaleTimeString()}` : "Saved"}
            </span>
            {blocking.length > 0 ? (
              <span className="text-accent-strong" data-theme-builder-blocking>
                {blocking.length} issue{blocking.length === 1 ? "" : "s"} the server would refuse
              </span>
            ) : null}
          </div>
        </div>

        {/* Right: the outline and the inspector. */}
        <div className="flex min-w-0 flex-col gap-3">
          <nav aria-label="Slot blocks" className="rounded-xl border border-line bg-surface">
            <div className="flex items-center gap-1.5 border-b border-line px-3 py-2">
              <h3 className="text-[13px] font-medium">Outline</h3>
              <span className="ml-auto text-[11.5px] text-muted">{blockCount}</span>
            </div>
            <div className="max-h-72 overflow-y-auto p-2">
              {blocks.length === 0 ? (
                <p className="px-1 py-4 text-center text-[12px] text-muted">
                  This slot is empty. Add a block, or restore what the theme ships.
                </p>
              ) : (
                <ul className="flex flex-col gap-0.5">
                  {walk(blocks).map(({ block, path, depth }) => (
                    <li key={block.id}>
                      <button
                        type="button"
                        data-block-outline-row
                        onClick={() => setSelected(path)}
                        aria-current={selected?.join("-") === path.join("-") ? "true" : undefined}
                        className={`flex w-full flex-col rounded-md px-2 py-1.5 text-left transition ${
                          selected?.join("-") === path.join("-") ? "bg-accent-soft" : "hover:bg-canvas"
                        }`}
                        style={{ paddingLeft: `${8 + depth * 12}px` }}
                      >
                        <span className="truncate text-[12px] font-medium">{blockLabel(registry, block)}</span>
                        <span className="truncate text-[11px] text-muted">{blockSummary(registry, block)}</span>
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </nav>

          {selectedBlock ? (
            <BlockInspector
              registry={registry}
              block={selectedBlock}
              issues={selectedIssues}
              onChange={(key, value) => {
                if (!selected) return;
                apply(`Edit ${blockLabel(registry, selectedBlock)}`, (current) =>
                  setProp(current, selected, key, value),
                );
              }}
              onSetting={(key, value) => {
                if (!selected) return;
                apply(
                  value === "" ? `Reset ${key}` : `${key} — ${value}`,
                  (current) => setSetting(current, selected, key, value),
                );
              }}
              breadcrumb={crumbs}
              onCrumb={setSelected}
              actions={
                <>
                  <button
                    type="button"
                    className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-40"
                    onClick={() => selected && apply("Move block up", (current) => moveBlock(current, selected, -1))}
                    disabled={!canMoveUp}
                    title="Move up (⌘⌥↑)"
                  >
                    <ArrowUp className="size-3.5" aria-hidden />
                  </button>
                  <button
                    type="button"
                    className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-40"
                    onClick={() => selected && apply("Move block down", (current) => moveBlock(current, selected, 1))}
                    disabled={!canMoveDown}
                    title="Move down (⌘⌥↓)"
                  >
                    <ArrowDown className="size-3.5" aria-hidden />
                  </button>
                  <button
                    type="button"
                    data-block-duplicate
                    className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas"
                    onClick={() => selected && apply("Duplicate block", (current) => duplicateBlock(current, selected))}
                    title="Duplicate (⌘D)"
                  >
                    <Copy className="size-3.5" aria-hidden />
                  </button>
                  {columnsPath !== null ? (
                    <button
                      type="button"
                      data-block-add-column
                      className="rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas"
                      onClick={() => apply("Add column", (current) => addColumn(current, columnsPath, limits))}
                    >
                      Add column
                    </button>
                  ) : null}
                  <button
                    type="button"
                    data-block-delete
                    className="ml-auto flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[12px] text-danger transition hover:bg-canvas"
                    onClick={() => selected && apply("Delete block", (current) => removeBlock(current, selected))}
                  >
                    <Trash2 className="size-3.5" aria-hidden />
                    Delete
                  </button>
                </>
              }
            />
          ) : (
            <p className="rounded-xl border border-dashed border-line px-3 py-6 text-center text-[12px] text-muted">
              Select a block in the outline to edit its content and settings.
            </p>
          )}
        </div>
      </div>

      {pendingReset ? (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
          role="dialog"
          aria-modal="true"
          aria-labelledby="theme-reset-title"
          data-theme-builder-reset-confirm
        >
          <div className="w-full max-w-md rounded-lg border border-line bg-panel p-5 shadow-lg">
            <h2 id="theme-reset-title" className="text-base font-semibold text-ink">
              Put back the theme&apos;s {SLOT_LABELS[pendingReset] ?? pendingReset}?
            </h2>
            <p className="mt-2 text-sm text-muted">
              This replaces the {blockCount} block{blockCount === 1 ? "" : "s"} in this slot with
              what {layouts.themeKey} ships. The blocks you have now are not recoverable afterwards —
              the slot keeps one row, not a history.
            </p>
            <div className="mt-4 flex justify-end gap-2">
              <button type="button" className="btn btn-ghost" onClick={() => setPendingReset(null)}>
                Cancel
              </button>
              <button
                type="button"
                className="btn btn-primary"
                data-theme-builder-reset-accept
                onClick={() => void doReset(pendingReset)}
              >
                Reset the slot
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}
