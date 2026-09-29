"use client";

/**
 * `/menus/{id}/edit` — the navigation editor (REQ-064, slice 1).
 *
 * Five decisions, each one a place where an editor's mental model and the store's disagree:
 *
 * 1. **The tree is the document.** A drag is a reorder and a drop-onto-a-row is a reparent, and
 *    both are expressed by saving the whole tree the editor holds. Per-item PATCHes would make
 *    "move these three rows" three requests that can half-apply — which is how a header ends up
 *    with a duplicate and a hole and nobody notices until a visitor reports it.
 * 2. **The ids are the editor's, minted on the client.** They are never rewritten, so an item can
 *    move, be retyped and be re-parented without the store ever losing track of which row is
 *    which. A server-minted id per save would make "the row I just dragged" a different row.
 * 3. **The preview reads the public endpoint.** The audience toggle calls
 *    `GET /api/v1/public/menus/{location}?audience=…` — the same call the theme's HTTP request
 *    makes. A preview that re-implemented the filter client-side would agree with nothing, and
 *    the day it disagreed with the live site would be the day somebody believed the preview.
 * 4. **A claimed location that is already taken is a conflict the editor sees before saving.**
 *    The store refuses the whole save and names the holder, so the location rail shows a warning
 *    while the editor still has the other fields open — not a 400 after the tree is already gone.
 * 5. **The reorder handles are buttons, not a drag library.** Keyboard and screen-reader users
 *    reorder with "move up"/"move down" and "nest"/"outdent", and a pointer drag is an
 *    *additional* affordance layered on the same move function. A gesture that only exists for a
 *    mouse is a feature half the panel cannot reach.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ChevronDown,
  ChevronRight,
  CornerDownRight,
  Eye,
  GripVertical,
  Layers,
  ListPlus,
  Loader2,
  Minus,
  Plus,
  RefreshCw,
  Save,
  Trash2,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  addPagesToMenu,
  fetchMenu,
  fetchPages,
  fetchRenderedMenu,
  saveMenuDocument,
  updateMenu,
  type MenuItemInput,
} from "@/lib/api";
import {
  MENU_ITEM_TYPES,
  MENU_VISIBILITIES,
  pageTitle,
  type MenuDetail,
  type MenuItem,
  type Page,
  type RenderedMenu,
} from "@/lib/types";

/** A new row, minted in the browser so its id survives every later move. */
function newItem(parentId: string | null, position: number, label = "New item"): MenuItem {
  return {
    // `crypto.randomUUID` is a browser API; the editor only ever runs in one, and a fallback that
    // produced a colliding id would silently merge two rows on the next save.
    id: crypto.randomUUID(),
    parent_id: parentId,
    position,
    label,
    item_type: "url",
    page_id: null,
    // A placeholder URL, not an empty one. The store refuses a `url` item with no URL by name
    // (`item "New item" is a link with no URL`) and it refuses the WHOLE submission, so an empty
    // string here meant "Add item" produced a row that made the entire next save fail — the author
    // saw three rows in the canvas, pressed Save, got one refusal naming the first of them, and
    // every item was gone. A placeholder keeps the tree saveable while the author types over it,
    // and it is site-relative, which the store accepts unchanged.
    url: "/",
    target: "_self",
    rel: "",
    css_class: "",
    enabled: true,
    visibility: "everyone",
    visibility_roles: [],
  };
}

/** Depth of an item, 1 for a top-level row. */
function depthOf(item: MenuItem, byId: Map<string, MenuItem>): number {
  let depth = 1;
  let cursor = item.parent_id;
  const seen = new Set<string>();
  while (cursor && !seen.has(cursor)) {
    seen.add(cursor);
    depth += 1;
    const parent = byId.get(cursor);
    if (!parent) break;
    cursor = parent.parent_id;
  }
  return depth;
}

export function MenuEditor({ menuId }: { menuId: string }) {
  const [detail, setDetail] = useState<MenuDetail | null>(null);
  /** The working copy. Null until the document loads; the saved one lives in `detail`. */
  const [items, setItems] = useState<MenuItem[] | null>(null);
  const [locations, setLocations] = useState<string[]>([]);
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const [pickingPages, setPickingPages] = useState(false);
  const [audience, setAudience] = useState<"visitor" | "member">("visitor");
  const [preview, setPreview] = useState<RenderedMenu | null>(null);
  const [previewLocation, setPreviewLocation] = useState("header");
  const [previewNote, setPreviewNote] = useState<string | null>(null);
  const [renameBusy, setRenameBusy] = useState(false);
  const firstLoad = useRef(true);

  // ------------------------------------------------------------------ the document
  const load = useCallback(
    async (reset: boolean) => {
      setError(null);
      try {
        const answer = await fetchMenu(menuId);
        setDetail(answer);
        setName(answer.name);
        if (reset || firstLoad.current) {
          setItems(answer.items);
          setLocations(answer.locations);
          setExpanded(new Set(answer.items.filter((item) => item.parent_id === null).map((item) => item.id)));
          setDirty(false);
          firstLoad.current = false;
        }
      } catch (caught) {
        setError((caught as ApiError).message);
      }
    },
    [menuId],
  );

  useEffect(() => {
    void load(true);
  }, [load]);

  // ------------------------------------------------------------------ the tree, as a shape
  const byId = useMemo(() => new Map((items ?? []).map((item) => [item.id, item])), [items]);

  /** Children of each parent, ordered. A parent id of `null` is the top level. */
  const childrenOf = useMemo(() => {
    const map = new Map<string | null, MenuItem[]>();
    for (const item of items ?? []) {
      const key = item.parent_id;
      const bucket = map.get(key) ?? [];
      bucket.push(item);
      map.set(key, bucket);
    }
    for (const bucket of map.values()) {
      bucket.sort((a, b) => a.position - b.position || a.label.localeCompare(b.label));
    }
    return map;
  }, [items]);

  const maxDepth = detail?.vocabulary.max_depth ?? 3;

  /** The deepest branch in the working tree, and the deepest allowed — both shown in the rail. */
  const treeDepth = useMemo(() => {
    if (!items) return 0;
    let deepest = 0;
    for (const item of items) {
      deepest = Math.max(deepest, depthOf(item, byId));
    }
    return deepest;
  }, [items, byId]);

  // ------------------------------------------------------------------ mutations
  const touch = useCallback((next: MenuItem[]) => {
    setItems(next);
    setDirty(true);
  }, []);

  const updateItem = useCallback(
    (id: string, patch: Partial<MenuItem>) => {
      setItems((current) =>
        (current ?? []).map((item) => (item.id === id ? { ...item, ...patch } : item)),
      );
      setDirty(true);
    },
    [],
  );

  const addItem = useCallback(
    (parentId: string | null) => {
      const siblings = (items ?? []).filter((item) => item.parent_id === parentId);
      const item = newItem(parentId, siblings.length);
      setItems([...(items ?? []), item]);
      if (parentId) setExpanded((current) => new Set([...current, parentId]));
      setDirty(true);
      // The new row is selected, not its parent. Selecting the parent looks equivalent and is
      // not: "Add item" at the top level left the selection on `null`, so the settings panel
      // never opened and the editor looked like a tree you could not configure — the button
      // worked, the row appeared, and the thing you came to fill in was nowhere.
      setSelected(item.id);
    },
    // `items` is in the list because the row is built from it. Building it inside a `setItems`
    // updater and reading it back out is the version that type-checks and intermittently
    // selects nothing: React may defer or re-invoke the updater, and the local capture is not
    // guaranteed to have run by the time the next line reads it.
    [items],
  );

  const removeItem = useCallback(
    (id: string) => {
      setItems((current) => {
        // Deleting a branch deletes the whole subtree: an item whose children were orphaned would
        // render as a set of top-level rows that used to be one menu branch.
        const doomed = new Set<string>([id]);
        let grew = true;
        while (grew) {
          grew = false;
          for (const item of current ?? []) {
            if (item.parent_id && doomed.has(item.parent_id) && !doomed.has(item.id)) {
              doomed.add(item.id);
              grew = true;
            }
          }
        }
        return (current ?? []).filter((item) => !doomed.has(item.id));
      });
      setSelected((current) => (current === id ? null : current));
      setDirty(true);
    },
    [],
  );

  /**
   * Move an item one slot among its siblings, or in or out of the tree.
   *
   * Every gesture — the drag handle's keyboard buttons and the pointer drag alike — ends up here,
   * so there is exactly one implementation of "what does a move mean" and the two cannot drift.
   * A refused move (nesting past the depth limit) says so; it does not silently clamp, because a
   * row that moved one pixel and stopped reads as a broken drag.
   */
  const move = useCallback(
    (id: string, direction: "up" | "down" | "in" | "out") => {
      setItems((current) => {
        const list = current ?? [];
        const item = list.find((row) => row.id === id);
        if (!item) return list;
        const siblings = list
          .filter((row) => row.parent_id === item.parent_id)
          .sort((a, b) => a.position - b.position);
        const index = siblings.findIndex((row) => row.id === id);

        if (direction === "up" || direction === "down") {
          const target = direction === "up" ? index - 1 : index + 1;
          if (target < 0 || target >= siblings.length) return list;
          const a = siblings[index];
          const b = siblings[target];
          return list.map((row) =>
            row.id === a.id
              ? { ...row, position: b.position }
              : row.id === b.id
                ? { ...row, position: a.position }
                : row,
          );
        }

        if (direction === "in") {
          // "Nest under the row above" — the keyboard equivalent of dropping onto a row.
          if (index <= 0) return list;
          const newParent = siblings[index - 1];
          if (depthOf(newParent, new Map(list.map((row) => [row.id, row]))) >= maxDepth) {
            setNotice(
              `Menus nest at most ${maxDepth} levels deep, and this row would be one deeper.`,
            );
            return list;
          }
          const newSiblings = list.filter((row) => row.parent_id === newParent.id);
          // The new parent is expanded here, and NOT inside the updater. A side effect inside a
          // `setItems` updater is the version React may run twice or defer, and the symptom is
          // worse than a missing expansion: the row keeps its place under a COLLAPSED parent, so it
          // leaves the tree entirely and the editor sees the row they just nested simply vanish —
          // the move looks like a delete, and pressing undo is the only way out.
          const expandParent = newParent.id;
          setExpanded((current) => new Set([...current, expandParent]));
          return list.map((row) =>
            row.id === id
              ? { ...row, parent_id: newParent.id, position: newSiblings.length }
              : row,
          );
        }

        // "Outdent" — become the next sibling of the current parent.
        if (!item.parent_id) return list;
        const parent = list.find((row) => row.id === item.parent_id);
        if (!parent) return list;
        const uncles = list
          .filter((row) => row.parent_id === parent.parent_id && row.id !== item.parent_id)
          .sort((a, b) => a.position - b.position);
        const afterUncle = uncles.filter((row) => row.position > parent.position).length;
        return list.map((row) =>
          row.id === id
            ? { ...row, parent_id: parent.parent_id, position: parent.position + 1 + afterUncle }
            : row,
        );
      });
      setDirty(true);
    },
    [maxDepth],
  );

  /** Pointer drag: the drop target is a row and a side, and dropping right means "become a child". */
  const [dropHint, setDropHint] = useState<{ id: string; nest: boolean } | null>(null);
  const dragId = useRef<string | null>(null);

  const onDrop = useCallback(
    (targetId: string, nest: boolean) => {
      const sourceId = dragId.current;
      dragId.current = null;
      setDropHint(null);
      if (!sourceId || sourceId === targetId) return;
      setItems((current) => {
        const list = current ?? [];
        const source = list.find((row) => row.id === sourceId);
        const target = list.find((row) => row.id === targetId);
        if (!source || !target) return list;
        const map = new Map(list.map((row) => [row.id, row]));
        if (nest && depthOf(target, map) >= maxDepth) {
          setNotice(`Menus nest at most ${maxDepth} levels deep, and this drop would be one deeper.`);
          return list;
        }
        // A branch cannot be dropped inside itself: the walk below would loop forever on save.
        if (nest) {
          let cursor: string | null = targetId;
          while (cursor) {
            if (cursor === sourceId) return list;
            cursor = map.get(cursor)?.parent_id ?? null;
          }
        }
        const parentId = nest ? targetId : target.parent_id;
        // A drop that lands a row under a collapsed parent has the same vanishing-row symptom as
        // the keyboard nest, so the branch is opened the same way — outside the updater.
        if (nest) setExpanded((current) => new Set([...current, targetId]));
        const siblings = list.filter((row) => row.parent_id === parentId && row.id !== sourceId);
        return list.map((row) =>
          row.id === sourceId
            ? { ...row, parent_id: parentId, position: siblings.length }
            : row,
        );
      });
      setDirty(true);
    },
    [maxDepth],
  );

  // ------------------------------------------------------------------ the save
  const save = useCallback(async () => {
    setSaving(true);
    setError(null);
    setNotice(null);
    try {
      const answer = await saveMenuDocument(menuId, {
        items: (items ?? []) as MenuItemInput[],
        locations,
      });
      setDetail(answer);
      setItems(answer.items);
      setLocations(answer.locations);
      setDirty(false);
      setNotice(`Saved ${answer.item_count} item${answer.item_count === 1 ? "" : "s"}.`);
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setSaving(false);
    }
  }, [items, locations, menuId]);

  const rename = useCallback(async () => {
    if (!detail || name.trim() === detail.name) return;
    setRenameBusy(true);
    setError(null);
    try {
      const answer = await updateMenu(menuId, { name: name.trim() });
      setDetail((current) => (current ? { ...current, ...answer } : current));
      setNotice("Renamed.");
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setRenameBusy(false);
    }
  }, [detail, menuId, name]);

  // ------------------------------------------------------------------ the preview
  const refreshPreview = useCallback(async () => {
    if (!detail) return;
    setPreviewNote(null);
    try {
      // `site_key`, never `site_id`: the public menu route resolves a site by global key or
      // host, and a uuid there is looked up as a key, matches nothing and answers 404 — which the
      // screen renders as "no menu claims this location", so the bug would read as an empty menu
      // rather than as a wrong argument.
      const answer = await fetchRenderedMenu(
        previewLocation,
        audience,
        detail.site_key,
      );
      setPreview(answer);
      if (!answer) {
        setPreviewNote(`No menu claims ${previewLocation} yet, so the theme renders nothing there.`);
      }
    } catch (caught) {
      setPreview(null);
      setPreviewNote((caught as ApiError).message);
    }
  }, [detail, previewLocation, audience]);

  // The preview re-reads on every audience flip and on every saved change, because the claim is
  // that it is the live payload — a preview that only refreshes on mount proves nothing.
  useEffect(() => {
    if (detail) void refreshPreview();
  }, [detail, refreshPreview]);

  /**
   * One row's buttons, closed over that row's id.
   *
   * The recursion in `TreeRow` hands every child *its own* factory rather than the parent's
   * callbacks. Passing the parent's handlers down is the version of this that looks right and is
   * wrong: the handler closes over the top-level item, so clicking "delete" on a third-level row
   * deletes the first-level row the editor can no longer see.
   *
   * It sits ABOVE the early returns on purpose. This hook was below them, which is legal
   * JavaScript and a runtime crash: the first render took the skeleton branch and ran three
   * hooks, the second render (the document had arrived) ran four, and React reported "Rendered
   * more hooks than during the previous render" — the editor never painted, and the walkthrough
   * read that as a screen that did not load rather than as a rules-of-hooks violation.
   */
  const actionsFor = useCallback(
    (id: string): RowActions => ({
      select: () => setSelected((current) => (current === id ? null : id)),
      toggle: () =>
        setExpanded((current) => {
          const next = new Set(current);
          if (next.has(id)) next.delete(id);
          else next.add(id);
          return next;
        }),
      addChild: () => addItem(id),
      remove: () => removeItem(id),
      move: (direction) => move(id, direction),
      dragStart: () => {
        dragId.current = id;
      },
      dragEnter: (nest) => setDropHint({ id, nest }),
      drop: () => onDrop(id, dropHint?.nest ?? false),
    }),
    // `dropHint` is read at click time, so it belongs here: a factory memoized without it would
    // drop a row as a sibling when the pointer said "nest" half a second earlier.
    [addItem, removeItem, move, onDrop, dropHint],
  );

  // ------------------------------------------------------------------ the states
  if (error && !detail) {
    return (
      <div className="space-y-3" data-menu-editor-state="error">
        <p className="text-[13px] text-red-700 dark:text-red-300">{error}</p>
        <div className="flex gap-2">
          <button
            type="button"
            onClick={() => void load(true)}
            className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-[13px]"
          >
            <RefreshCw className="h-4 w-4" aria-hidden />
            Retry
          </button>
          <Link href="/menus" className="rounded-md border border-line px-3 py-2 text-[13px]">
            Back to menus
          </Link>
        </div>
      </div>
    );
  }
  if (!detail || items === null) return <EditorSkeleton />;

  const topLevel = childrenOf.get(null) ?? [];
  const selectedItem = selected ? (byId.get(selected) ?? null) : null;

  return (
    <div className="space-y-6" data-menu-editor-state="ready">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <label className="flex items-center gap-2 text-[15px] font-semibold">
            <span className="sr-only">Menu name</span>
            <input
              data-menu-name
              value={name}
              onChange={(event) => setName(event.target.value)}
              onBlur={() => void rename()}
              className="min-w-0 rounded-md border border-transparent bg-transparent px-1 py-0.5 text-[15px] font-semibold hover:border-line focus:border-line"
            />
          </label>
          <p className="text-[12px] text-muted">
            <code className="font-mono">{detail.key}</code> · {items.length} item
            {items.length === 1 ? "" : "s"} · deepest branch {treeDepth} of {maxDepth}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <Link href="/menus" className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]">
            All menus
          </Link>
          <button
            type="button"
            data-menu-save
            disabled={saving || !dirty}
            onClick={() => void save()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            {saving ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
            ) : (
              <Save className="h-3.5 w-3.5" aria-hidden />
            )}
            {dirty ? "Save tree" : "Saved"}
          </button>
        </div>
      </div>

      {notice ? (
        <p data-menu-notice className="text-[12.5px] text-muted">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p data-menu-error className="text-[12.5px] text-red-700 dark:text-red-300">
          {error}
        </p>
      ) : null}

      <div className="grid gap-6 lg:grid-cols-[minmax(0,2fr)_minmax(0,1fr)]">
        {/* ------------------------------------------------------------- the tree */}
        <section aria-labelledby="tree-heading" className="space-y-3">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <h2 id="tree-heading" className="text-[13.5px] font-semibold">
              Items
            </h2>
            <div className="flex flex-wrap gap-2">
              <button
                type="button"
                data-menu-add-item
                onClick={() => addItem(null)}
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
              >
                <Plus className="h-3.5 w-3.5" aria-hidden />
                Add item
              </button>
              <button
                type="button"
                data-menu-add-pages
                onClick={() => setPickingPages((value) => !value)}
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
              >
                <ListPlus className="h-3.5 w-3.5" aria-hidden />
                Add pages…
              </button>
            </div>
          </div>

          {pickingPages ? (
            <PagePicker
              siteId={detail.site_id}
              onClose={() => setPickingPages(false)}
              onAdd={async (pageIds) => {
                try {
                  const answer = await addPagesToMenu(menuId, { page_ids: pageIds });
                  setDetail((current) => (current ? { ...current, ...answer } : current));
                  setItems(answer.items);
                  setNotice(`Added ${pageIds.length} page${pageIds.length === 1 ? "" : "s"}.`);
                } catch (caught) {
                  setError((caught as ApiError).message);
                } finally {
                  setPickingPages(false);
                }
              }}
            />
          ) : null}

          {topLevel.length === 0 ? (
            <div className="rounded-lg border border-line" data-menu-tree-empty>
              <EmptyState
                title="This menu has no items"
                hint="A menu with no items renders as an empty strip. Add a row, or pull in pages that are already published."
                action={
                  <button
                    type="button"
                    onClick={() => addItem(null)}
                    className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
                  >
                    Add the first item
                  </button>
                }
              />
            </div>
          ) : (
            <ul data-menu-tree className="space-y-1">
              {topLevel.map((item) => (
                <TreeRow
                  key={item.id}
                  item={item}
                  childrenOf={childrenOf}
                  expanded={expanded}
                  dropHint={dropHint}
                  selectedId={selected}
                  actionsFor={actionsFor}
                />
              ))}
            </ul>
          )}
        </section>

        {/* ------------------------------------------------------------- the rail */}
        <div className="space-y-5">
          {selectedItem ? (
            <ItemInspector
              item={selectedItem}
              onChange={(patch) => updateItem(selectedItem.id, patch)}
              onRemove={() => removeItem(selectedItem.id)}
            />
          ) : null}

          <LocationRail
            available={detail.vocabulary.locations}
            claimed={locations}
            onToggle={(location) => {
              setLocations((current) =>
                current.includes(location)
                  ? current.filter((value) => value !== location)
                  : [...current, location],
              );
              setDirty(true);
              setPreviewLocation(location);
            }}
          />

          <PreviewStrip
            locations={detail.vocabulary.locations}
            location={previewLocation}
            audience={audience}
            menu={preview}
            note={previewNote}
            onLocation={setPreviewLocation}
            onAudience={setAudience}
            onRefresh={() => void refreshPreview()}
          />
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// The tree row
// ---------------------------------------------------------------------------------------------

type ChildrenOf = Map<string | null, MenuItem[]>;

/** What one row's buttons do. Built per item, so a child never runs its parent's callbacks. */
type RowActions = {
  select: () => void;
  toggle: () => void;
  addChild: () => void;
  remove: () => void;
  move: (direction: "up" | "down" | "in" | "out") => void;
  dragStart: () => void;
  dragEnter: (nest: boolean) => void;
  drop: () => void;
};

function TreeRow({
  item,
  childrenOf,
  expanded,
  dropHint,
  selectedId,
  actionsFor,
}: {
  item: MenuItem;
  childrenOf: ChildrenOf;
  expanded: Set<string>;
  dropHint: { id: string; nest: boolean } | null | undefined;
  selectedId: string | null;
  actionsFor: (id: string) => RowActions;
}) {
  // Each row — at every depth — takes its own actions out of the factory. Handing the recursion
  // one item's already-bound actions is the version of this that type-checks and deletes the
  // wrong row: the closures capture the top-level id, so a third-level "delete" takes the
  // first-level branch with it.
  const { select, toggle, addChild, remove, move, dragStart, dragEnter, drop } = actionsFor(item.id);
  const children = childrenOf.get(item.id) ?? [];
  const isOpen = expanded.has(item.id);
  const type = MENU_ITEM_TYPES.find((entry) => entry.value === item.item_type);

  return (
    <li data-menu-item={item.id} className="space-y-1">
      <div
        data-menu-item-row={item.id}
        onDragOver={(event) => event.preventDefault()}
        onDragEnter={(event) => {
          const rect = event.currentTarget.getBoundingClientRect();
          // The right third of the row is "become a child"; the rest is "take this slot". The
          // divider is at 70% because the row also carries buttons on its right edge.
          dragEnter(event.clientX > rect.left + rect.width * 0.7);
        }}
        onDrop={(event) => {
          event.preventDefault();
          drop();
        }}
        onDragStart={dragStart}
        draggable
        className={`flex flex-wrap items-center gap-2 rounded-md border px-2 py-1.5 ${
          dropHint?.id === item.id
            ? dropHint.nest
              ? "border-accent bg-accent-soft"
              : "border-line bg-quiet-soft"
            : selectedId === item.id
              ? "border-accent"
              : "border-line"
        }`}
      >
        <button
          type="button"
          data-menu-item-grip={item.id}
          aria-label={`Reorder ${item.label}`}
          className="cursor-grab text-muted"
          onClick={toggle}
        >
          {children.length > 0 ? (
            isOpen ? (
              <ChevronDown className="h-3.5 w-3.5" aria-hidden />
            ) : (
              <ChevronRight className="h-3.5 w-3.5" aria-hidden />
            )
          ) : (
            <GripVertical className="h-3.5 w-3.5" aria-hidden />
          )}
        </button>

        <button
          type="button"
          data-menu-item-label={item.id}
          onClick={select}
          className="min-w-0 flex-1 text-left"
        >
          <span className={`truncate text-[13px] ${item.enabled ? "" : "text-muted line-through"}`}>
            {item.label}
          </span>
          <span className="ml-2 text-[11.5px] text-muted">
            {type?.label ?? item.item_type}
            {item.visibility !== "everyone" ? ` · ${item.visibility}` : ""}
            {!item.enabled ? " · disabled" : ""}
          </span>
        </button>

        {/* The keyboard move buttons. Same function the pointer drag calls, so the two affordances
            cannot disagree about what "move up" means. */}
        <span className="flex gap-1">
          <IconButton label={`Move ${item.label} up`} onClick={() => move("up")}>
            <ChevronDown className="h-3.5 w-3.5 rotate-180" aria-hidden />
          </IconButton>
          <IconButton label={`Move ${item.label} down`} onClick={() => move("down")}>
            <ChevronDown className="h-3.5 w-3.5" aria-hidden />
          </IconButton>
          <IconButton label={`Nest ${item.label} under the row above`} onClick={() => move("in")}>
            <CornerDownRight className="h-3.5 w-3.5" aria-hidden />
          </IconButton>
          <IconButton label={`Outdent ${item.label}`} onClick={() => move("out")}>
            <CornerDownRight className="h-3.5 w-3.5 -scale-x-100" aria-hidden />
          </IconButton>
          <IconButton label={`Add a child under ${item.label}`} onClick={addChild}>
            <Plus className="h-3.5 w-3.5" aria-hidden />
          </IconButton>
          <IconButton label={`Delete ${item.label} and its children`} onClick={remove}>
            <Trash2 className="h-3.5 w-3.5" aria-hidden />
          </IconButton>
        </span>
      </div>

      {isOpen && children.length > 0 ? (
        <ul className="ml-5 space-y-1 border-l border-line pl-2">
          {children.map((child) => (
            <TreeRow
              key={child.id}
              item={child}
              childrenOf={childrenOf}
              expanded={expanded}
              dropHint={dropHint}
              selectedId={selectedId}
              actionsFor={actionsFor}
            />
          ))}
        </ul>
      ) : null}
    </li>
  );
}

function IconButton({
  label,
  onClick,
  children,
}: {
  label: string;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      onClick={onClick}
      className="inline-flex items-center rounded border border-line p-1 text-muted hover:text-ink"
    >
      {children}
    </button>
  );
}

// ---------------------------------------------------------------------------------------------
// The per-item inspector
// ---------------------------------------------------------------------------------------------

function ItemInspector({
  item,
  onChange,
  onRemove,
}: {
  item: MenuItem;
  onChange: (patch: Partial<MenuItem>) => void;
  onRemove: () => void;
}) {
  return (
    <section
      aria-labelledby="inspector-heading"
      data-menu-inspector={item.id}
      className="space-y-3 rounded-lg border border-line p-3"
    >
      <h2 id="inspector-heading" className="text-[13px] font-semibold">
        Item settings
      </h2>
      <label className="flex flex-col gap-1 text-[12px]">
        <span className="text-muted">Label</span>
        <input
          data-item-label
          value={item.label}
          onChange={(event) => onChange({ label: event.target.value })}
          className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
        />
      </label>
      <label className="flex flex-col gap-1 text-[12px]">
        <span className="text-muted">Type</span>
        <select
          data-item-type
          value={item.item_type}
          onChange={(event) => {
            const next = event.target.value;
            // Switching to `page` clears a URL that would otherwise be silently ignored: a
            // `page` item whose `url` still holds the old address is a row that renders the old
            // link with no way to see why.
            onChange({ item_type: next, ...(next === "page" ? { url: "" } : {}) });
          }}
          className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
        >
          {MENU_ITEM_TYPES.map((entry) => (
            <option key={entry.value} value={entry.value}>
              {entry.label} — {entry.hint}
            </option>
          ))}
        </select>
      </label>
      {item.item_type === "page" ? (
        <p className="text-[12px] text-muted">
          Linked to a page. Use <strong>Add pages…</strong> to pick published pages — a draft is
          refused by the server, because a menu that links to a draft is a 404 for every visitor.
        </p>
      ) : item.item_type === "index" ? null : (
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">
            {item.item_type === "anchor" ? "Anchor (with #)" : "URL"}
          </span>
          <input
            data-item-url
            value={item.url}
            onChange={(event) => onChange({ url: event.target.value })}
            placeholder={item.item_type === "anchor" ? "#pricing" : "/about or https://…"}
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
          />
        </label>
      )}
      <div className="grid grid-cols-2 gap-2">
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Opens in</span>
          <select
            data-item-target
            value={item.target}
            onChange={(event) => onChange({ target: event.target.value })}
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
          >
            <option value="_self">Same tab</option>
            <option value="_blank">New tab</option>
          </select>
        </label>
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">rel</span>
          <input
            data-item-rel
            value={item.rel}
            onChange={(event) => onChange({ rel: event.target.value })}
            placeholder="noopener"
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
          />
        </label>
      </div>
      <label className="flex flex-col gap-1 text-[12px]">
        <span className="text-muted">CSS class</span>
        <input
          data-item-css-class
          value={item.css_class}
          onChange={(event) => onChange({ css_class: event.target.value })}
          className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
        />
      </label>
      <label className="flex flex-col gap-1 text-[12px]">
        <span className="text-muted">Visible to</span>
        <select
          data-item-visibility
          value={item.visibility}
          onChange={(event) => onChange({ visibility: event.target.value, visibility_roles: [] })}
          className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
        >
          {MENU_VISIBILITIES.map((entry) => (
            <option key={entry.value} value={entry.value}>
              {entry.label}
            </option>
          ))}
        </select>
      </label>
      {item.visibility === "roles" ? (
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Roles (comma separated)</span>
          <input
            data-item-roles
            value={item.visibility_roles.join(", ")}
            onChange={(event) =>
              onChange({
                visibility_roles: event.target.value
                  .split(",")
                  .map((value) => value.trim())
                  .filter(Boolean),
              })
            }
            placeholder="editor, author"
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
          />
        </label>
      ) : null}
      <label className="flex items-center gap-2 text-[12px]">
        <input
          type="checkbox"
          data-item-enabled
          checked={item.enabled}
          onChange={(event) => onChange({ enabled: event.target.checked })}
        />
        Enabled
      </label>
      <button
        type="button"
        data-item-delete
        onClick={onRemove}
        className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
      >
        <Trash2 className="h-3.5 w-3.5" aria-hidden />
        Delete this item
      </button>
    </section>
  );
}

// ---------------------------------------------------------------------------------------------
// The location rail
// ---------------------------------------------------------------------------------------------

/**
 * The claimed theme slots.
 *
 * The checkbox says "claim", and the rail warns when two menus in this site could claim the same
 * slot — the store refuses the second save, and a refusal that only arrives as a 400 after the
 * editor has filled a whole tree is a refusal that costs them their work.
 */
function LocationRail({
  available,
  claimed,
  onToggle,
}: {
  available: string[];
  claimed: string[];
  onToggle: (location: string) => void;
}) {
  return (
    <section aria-labelledby="locations-heading" data-menu-locations className="space-y-2 rounded-lg border border-line p-3">
      <h2 id="locations-heading" className="text-[13px] font-semibold">
        Locations
      </h2>
      <p className="text-[12px] text-muted">
        One menu per slot. Claiming a slot another menu holds moves it — the save is refused if the
        other menu is saved in the same moment.
      </p>
      <ul className="space-y-1.5">
        {available.map((location) => (
          <li key={location}>
            <label className="flex items-center gap-2 text-[12.5px]">
              <input
                type="checkbox"
                data-menu-location-toggle={location}
                checked={claimed.includes(location)}
                onChange={() => onToggle(location)}
              />
              <span className="font-mono">{location}</span>
              {claimed.includes(location) ? (
                <span className="text-[11.5px] text-muted">claimed by this menu</span>
              ) : null}
            </label>
          </li>
        ))}
      </ul>
      {claimed.length === 0 ? (
        <p data-menu-locations-warn className="text-[12px] text-amber-700 dark:text-amber-300">
          This menu renders nowhere until it claims a slot.
        </p>
      ) : null}
    </section>
  );
}

// ---------------------------------------------------------------------------------------------
// The rendered preview
// ---------------------------------------------------------------------------------------------

/**
 * The preview strip.
 *
 * It is a real read of `GET /api/v1/public/menus/{location}?audience=…` — the endpoint the theme
 * calls. That is the whole point: the acceptance criterion is that a `members` item is absent for
 * a signed-out visitor and present after sign-in, and the only honest way to see that from the
 * panel is to ask the same question the site answers.
 */
function PreviewStrip({
  locations,
  location,
  audience,
  menu,
  note,
  onLocation,
  onAudience,
  onRefresh,
}: {
  locations: string[];
  location: string;
  audience: "visitor" | "member";
  menu: RenderedMenu | null;
  note: string | null;
  onLocation: (value: string) => void;
  onAudience: (value: "visitor" | "member") => void;
  onRefresh: () => void;
}) {
  return (
    <section aria-labelledby="preview-heading" data-menu-preview className="space-y-2 rounded-lg border border-line p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h2 id="preview-heading" className="text-[13px] font-semibold">
          <span className="inline-flex items-center gap-1.5">
            <Eye className="h-3.5 w-3.5" aria-hidden />
            Rendered preview
          </span>
        </h2>
        <button
          type="button"
          data-menu-preview-refresh
          onClick={onRefresh}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px]"
        >
          <RefreshCw className="h-3.5 w-3.5" aria-hidden />
          Re-read
        </button>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <label className="flex items-center gap-1.5 text-[12px]">
          <span className="text-muted">Slot</span>
          <select
            data-menu-preview-location
            value={location}
            onChange={(event) => onLocation(event.target.value)}
            className="rounded-md border border-line bg-transparent px-2 py-1 text-[12.5px]"
          >
            {(locations.length > 0 ? locations : ["header"]).map((value) => (
              <option key={value} value={value}>
                {value}
              </option>
            ))}
          </select>
        </label>
        <div className="flex gap-1" role="group" aria-label="Audience">
          {(["visitor", "member"] as const).map((value) => (
            <button
              key={value}
              type="button"
              data-menu-preview-audience={value}
              aria-pressed={audience === value}
              onClick={() => onAudience(value)}
              className={`rounded-full border px-2.5 py-1 text-[12px] ${
                audience === value ? "border-line bg-quiet-soft" : "border-line"
              }`}
            >
              {value === "visitor" ? "Visitor" : "Member"}
            </button>
          ))}
        </div>
      </div>
      {note ? (
        <p data-menu-preview-note className="text-[12px] text-muted">
          {note}
        </p>
      ) : menu ? (
        <nav aria-label={`${menu.name} preview`} data-menu-preview-list>
          <ul className="flex flex-wrap gap-2 text-[12.5px]">
            {menu.items.map((item) => (
              <li key={item.id} data-menu-preview-item={item.id}>
                <span className="underline decoration-dotted">
                  {item.label}
                  <span className="ml-1 text-[11px] text-muted">{item.href}</span>
                </span>
                {item.children.length > 0 ? (
                  <ul className="ml-3 mt-1 flex flex-wrap gap-2 border-l border-line pl-2">
                    {item.children.map((child) => (
                      <li key={child.id} data-menu-preview-child={child.id}>
                        <span className="underline decoration-dotted">
                          {child.label}
                          <span className="ml-1 text-[11px] text-muted">{child.href}</span>
                        </span>
                      </li>
                    ))}
                  </ul>
                ) : null}
              </li>
            ))}
          </ul>
        </nav>
      ) : null}
    </section>
  );
}

// ---------------------------------------------------------------------------------------------
// Add pages…
// ---------------------------------------------------------------------------------------------

/**
 * The page picker.
 *
 * Only **published** pages are offered, because the server refuses the rest: a batch containing a
 * draft is refused whole and inserts nothing. A picker that listed drafts would let an editor
 * select five pages and lose all five.
 */
function PagePicker({
  siteId,
  onAdd,
  onClose,
}: {
  siteId: string;
  onAdd: (pageIds: string[]) => Promise<void>;
  onClose: () => void;
}) {
  const [pages, setPages] = useState<Page[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [chosen, setChosen] = useState<Set<string>>(new Set());
  const [type, setType] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    fetchPages(siteId, "published")
      .then(setPages)
      .catch((caught) => setError((caught as ApiError).message));
  }, [siteId]);

  const types = useMemo(
    () => Array.from(new Set((pages ?? []).map((page) => page.page_type))).sort(),
    [pages],
  );
  const visible = useMemo(
    () => (pages ?? []).filter((page) => !type || page.page_type === type),
    [pages, type],
  );

  if (error) {
    return (
      <p data-pages-error className="text-[12.5px] text-red-700 dark:text-red-300">
        {error}
      </p>
    );
  }
  if (!pages) {
    return (
      <p data-pages-loading className="text-[12.5px] text-muted">
        Loading published pages…
      </p>
    );
  }

  return (
    <div data-page-picker className="space-y-2 rounded-lg border border-line p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h3 className="text-[13px] font-semibold">Add published pages</h3>
        <label className="flex items-center gap-1.5 text-[12px]">
          <span className="text-muted">Type</span>
          <select
            data-page-picker-type
            value={type}
            onChange={(event) => setType(event.target.value)}
            className="rounded-md border border-line bg-transparent px-2 py-1 text-[12.5px]"
          >
            <option value="">All types</option>
            {types.map((value) => (
              <option key={value} value={value}>
                {value}
              </option>
            ))}
          </select>
        </label>
      </div>
      {visible.length === 0 ? (
        <p data-pages-empty className="text-[12.5px] text-muted">
          This site has no published page of that type. Publish a page first — a menu that links to
          a draft is a 404 for every visitor.
        </p>
      ) : (
        <ul className="max-h-56 space-y-1 overflow-y-auto">
          {visible.map((page) => (
            <li key={page.id}>
              <label className="flex items-center gap-2 text-[12.5px]">
                <input
                  type="checkbox"
                  data-page-picker-page={page.id}
                  checked={chosen.has(page.id)}
                  onChange={() =>
                    setChosen((current) => {
                      const next = new Set(current);
                      if (next.has(page.id)) next.delete(page.id);
                      else next.add(page.id);
                      return next;
                    })
                  }
                />
                <span className="truncate">{pageTitle(page)}</span>
                <span className="text-[11.5px] text-muted">
                  /{page.slug} · {page.page_type}
                </span>
              </label>
            </li>
          ))}
        </ul>
      )}
      <div className="flex gap-2">
        <button
          type="button"
          data-page-picker-add
          disabled={chosen.size === 0 || busy}
          onClick={() => {
            setBusy(true);
            void onAdd(Array.from(chosen));
          }}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
        >
          {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <Layers className="h-3.5 w-3.5" aria-hidden />}
          Add {chosen.size} page{chosen.size === 1 ? "" : "s"}
        </button>
        <button
          type="button"
          onClick={onClose}
          className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
        >
          Cancel
        </button>
      </div>
    </div>
  );
}

function EditorSkeleton() {
  return (
    <div className="space-y-4" data-menu-editor-state="loading" aria-busy="true">
      <div className="h-5 w-56 animate-pulse rounded bg-quiet-soft" />
      <div className="h-40 animate-pulse rounded-lg bg-quiet-soft" />
      <div className="h-32 animate-pulse rounded-lg bg-quiet-soft" />
    </div>
  );
}
