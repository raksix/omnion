"use client";

/**
 * `/patterns` — the pattern library (REQ-063, slice 3).
 *
 * A pattern is a block group an author cuts out of a page once and drops into the next forty.
 * The library is grouped by category, searchable, and every card carries the *outline* of what
 * it holds — read from the block registry the editor already holds, never from a label list of
 * its own, so a block type that ships tomorrow is named correctly here the day it lands.
 *
 * The screen is a library, not a page: nothing here is a live link into a page. "Insert" is the
 * editor's action (it needs a page open), and this screen links to it.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import type { BlockRegistry, ContentBlock, ContentPattern } from "@/lib/types";
import { Boxes, Plus, Search, Trash2 } from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError, deletePattern, fetchBlockRegistry, fetchPatterns } from "@/lib/api";
import { countBlocks, describeTree, keyProblem } from "./block-tree-summary";

/**
 * A registry-shaped value for when the registry request failed.
 *
 * The library is still usable without it — a card that cannot name its blocks shows the raw
 * type key, which is worse but not broken. What it must not do is throw on a render, because
 * a registry outage would otherwise take the screen down with it.
 */
const NO_REGISTRY: BlockRegistry = { version: "0", categories: [], blocks: [] };
import { PatternEditor } from "./pattern-editor";

/** One library row's local state: the editor's open/close plus a per-row error. */
type RowState = { editing: boolean; error: string | null };

const CLOSED: RowState = { editing: false, error: null };

/** The pattern library: browse, search, create, edit, duplicate and delete. */
export function PatternLibrary() {
  const [patterns, setPatterns] = useState<ContentPattern[] | null>(null);
  const [registry, setRegistry] = useState<BlockRegistry | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [category, setCategory] = useState<string>("");
  const [rows, setRows] = useState<Record<string, RowState>>({});
  const [creating, setCreating] = useState(false);

  // The registry is what turns a tree into words. The library is still usable without it — the
  // cards fall back to the raw block type — so a registry failure is not the screen's failure.
  useEffect(() => {
    let cancelled = false;
    fetchBlockRegistry()
      .then((document_) => {
        if (!cancelled) {
          setRegistry(document_);
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, []);

  const load = useCallback(() => {
    setError(null);
    fetchPatterns()
      .then((listed) => setPatterns(listed))
      .catch((cause: unknown) => {
        setPatterns([]);
        setError(
          cause instanceof ApiError
            ? cause.message
            : "The pattern library could not be loaded.",
        );
      });
  }, []);

  useEffect(load, [load]);

  const categories = useMemo(() => {
    const seen = new Set<string>();
    for (const pattern of patterns ?? []) {
      seen.add(pattern.category);
    }
    return [...seen].sort();
  }, [patterns]);

  const visible = useMemo(() => {
    if (!patterns) {
      return [];
    }
    const needle = query.trim().toLowerCase();
    return patterns.filter((pattern) => {
      if (category !== "" && pattern.category !== category) {
        return false;
      }
      if (needle === "") {
        return true;
      }
      return (
        pattern.name.toLowerCase().includes(needle) ||
        pattern.key.includes(needle) ||
        (pattern.description ?? "").toLowerCase().includes(needle)
      );
    });
  }, [patterns, query, category]);

  if (error) {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="The pattern library could not be loaded"
          hint={`${error} The library is read with content.blocks.read — an account without that key cannot author against it.`}
        />
      </div>
    );
  }

  if (!patterns) {
    return <LoadingTable columns={3} />;
  }

  const setRow = (id: string, next: RowState) =>
    setRows((current) => ({ ...current, [id]: next }));

  const onSaved = (saved: ContentPattern) => {
    setPatterns((current) => {
      const rest = (current ?? []).filter((entry) => entry.id !== saved.id);
      return [saved, ...rest];
    });
    setCreating(false);
  };

  const onDuplicate = (source: ContentPattern) => {
    // Duplication is a create with a *new* key. The blocks are copied as they are — the ids
    // travel, and the copy is stored under a key of its own, so a later "insert" mints fresh
    // ones for the page anyway. Cloning the ids here would only matter if a pattern's ids were
    // ever read on their own, and nothing does that.
    setCreating(true);
    setPatterns((current) => current);
    pendingDuplicate.current = {
      key: `${source.key}-copy`,
      name: `${source.name} (copy)`,
      category: source.category,
      description: source.description ?? undefined,
      blocks: source.blocks,
    };
  };

  const onDelete = async (pattern: ContentPattern) => {
    setRow(pattern.id, { editing: false, error: null });
    try {
      await deletePattern(pattern.id);
      setPatterns((current) =>
        (current ?? []).filter((entry) => entry.id !== pattern.id),
      );
    } catch (cause: unknown) {
      setRow(pattern.id, {
        editing: false,
        error:
          cause instanceof ApiError
            ? cause.message
            : "The pattern could not be deleted.",
      });
    }
  };

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3 rounded-xl border border-line bg-surface px-4 py-3">
        <div className="flex items-baseline gap-2">
          <Boxes className="size-4 text-muted" aria-hidden />
          <h2 className="text-[13.5px] font-medium">Patterns</h2>
          <span className="text-[12px] text-muted">
            {patterns.length} reusable block group{patterns.length === 1 ? "" : "s"}
          </span>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {categories.length > 1 ? (
            <label className="flex items-center gap-2">
              <span className="sr-only">Filter by category</span>
              <select
                id="pattern-category"
                name="pattern-category"
                value={category}
                onChange={(event) => setCategory(event.target.value)}
                className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              >
                <option value="">All categories</option>
                {categories.map((entry) => (
                  <option key={entry} value={entry}>
                    {entry}
                  </option>
                ))}
              </select>
            </label>
          ) : null}
          <label className="flex items-center gap-2">
            <span className="sr-only">Search the pattern library</span>
            <Search className="size-3.5 text-muted" aria-hidden />
            <input
              id="pattern-search"
              name="pattern-search"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Search patterns…"
              className="w-52 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
          </label>
          <button
            type="button"
            data-action="new-pattern"
            onClick={() => {
              pendingDuplicate.current = null;
              setCreating(true);
            }}
            className="inline-flex cursor-pointer items-center gap-1.5 rounded-lg bg-accent px-2.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
          >
            <Plus className="size-3.5" aria-hidden />
            New pattern
          </button>
        </div>
      </div>

      {creating ? (
        <PatternEditor
          draft={pendingDuplicate.current}
          registry={registry}
          onCancel={() => {
            pendingDuplicate.current = null;
            setCreating(false);
          }}
          onSaved={onSaved}
        />
      ) : null}

      {visible.length === 0 ? (
        <div className="rounded-xl border border-line bg-surface">
          <EmptyState
            title={
              patterns.length === 0
                ? "No patterns yet"
                : "Nothing matches that filter"
            }
            hint={
              patterns.length === 0
                ? "A pattern is a block group you can reuse. Build a page, select the blocks worth keeping, and save them here."
                : "Clear the search or the category filter to see the rest of the library."
            }
            action={
              patterns.length === 0 ? (
                <button
                  type="button"
                  data-action="new-pattern-empty"
                  onClick={() => {
                    pendingDuplicate.current = null;
                    setCreating(true);
                  }}
                  className="inline-flex cursor-pointer items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] font-medium transition hover:bg-canvas"
                >
                  <Plus className="size-3.5" aria-hidden />
                  New pattern
                </button>
              ) : null
            }
          />
        </div>
      ) : (
        <ul className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
          {visible.map((pattern) => {
            const state = rows[pattern.id] ?? CLOSED;
            return (
              <li
                key={pattern.id}
                data-pattern-card={pattern.key}
                className="flex flex-col gap-2 rounded-xl border border-line bg-surface p-3.5"
              >
                <div className="flex items-baseline gap-2">
                  <span className="text-[13px] font-medium">{pattern.name}</span>
                  <code className="font-mono text-[11px] text-muted">{pattern.key}</code>
                </div>
                {pattern.description ? (
                  <p className="text-[12px] text-muted">{pattern.description}</p>
                ) : null}
                <p className="text-[11.5px] text-muted">
                  {pattern.block_count} block
                  {pattern.block_count === 1 ? "" : "s"} · {pattern.category}
                </p>
                <p className="line-clamp-2 font-mono text-[11px] text-muted/90">
                  {describeTree(pattern.blocks as ContentBlock[], registry ?? NO_REGISTRY, 5)}
                </p>

                {state.error ? (
                  <p
                    role="alert"
                    className="rounded-lg border border-caution/40 bg-caution-soft px-2 py-1.5 text-[11.5px] text-caution"
                  >
                    {state.error}
                  </p>
                ) : null}

                {state.editing ? (
                  <PatternEditor
                    pattern={pattern}
                    registry={registry}
                    onCancel={() => setRow(pattern.id, CLOSED)}
                    onSaved={onSaved}
                  />
                ) : (
                  <div className="mt-auto flex flex-wrap items-center gap-1.5 pt-1">
                    <Link
                      href="/pages"
                      className="rounded-lg border border-line px-2 py-1 text-[12px] transition hover:bg-canvas"
                    >
                      Insert in a page
                    </Link>
                    <button
                      type="button"
                      data-action={`edit-pattern-${pattern.key}`}
                      onClick={() => setRow(pattern.id, { editing: true, error: null })}
                      className="cursor-pointer rounded-lg border border-line px-2 py-1 text-[12px] transition hover:bg-canvas"
                    >
                      Edit
                    </button>
                    <button
                      type="button"
                      data-action={`duplicate-pattern-${pattern.key}`}
                      onClick={() => onDuplicate(pattern)}
                      className="cursor-pointer rounded-lg border border-line px-2 py-1 text-[12px] transition hover:bg-canvas"
                    >
                      Duplicate
                    </button>
                    <button
                      type="button"
                      data-action={`delete-pattern-${pattern.key}`}
                      onClick={() => {
                        onDelete(pattern);
                      }}
                      className="ml-auto inline-flex cursor-pointer items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12px] text-caution transition hover:bg-caution-soft"
                    >
                      <Trash2 className="size-3.5" aria-hidden />
                      Delete
                    </button>
                  </div>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

/** What the editor opens with: a blank form, or a duplicate's fields. */
export interface PatternDraft {
  key: string;
  name: string;
  category: string;
  description?: string;
  blocks: ContentBlock[];
}

/**
 * The duplicate the author asked for, held between the click and the editor's own submit.
 *
 * Module scope rather than state because it is one value that only two handlers touch, and
 * putting it in this component's state would mean re-rendering every card on the library to
 * carry a draft the editor reads once.
 */
const pendingDuplicate: { current: PatternDraft | null } = { current: null };

export { keyProblem, countBlocks };
