"use client";

/**
 * Insert a pattern, and save the current selection as one (REQ-063, slice 3).
 *
 * Both halves of acceptance criterion 10 live here, because they are the same operation seen
 * from two ends: a pattern is a block group, "insert" copies it into the page at the insertion
 * point, and "save as pattern" copies it out of the page into the library.
 *
 * The insert path asks the **server** for the blocks rather than re-using the list payload.
 * The ids the page will store are the ones the server minted, and a browser that re-mints them
 * would be a second implementation of "copy this pattern" — the kind that disagrees with the
 * server the first time either one learns a rule the other does not have. The server also
 * answers with a count, so the editor can say what it inserted before saying it did.
 */
import { useCallback, useEffect, useState } from "react";

import type { BlockRegistry, ContentBlock, ContentPattern } from "@/lib/types";
import { Library, Plus } from "lucide-react";

import { ApiError, fetchPatternBlocks, fetchPatterns, savePattern } from "@/lib/api";
import { useContentTenant } from "@/lib/tenant";
import { countBlocks, describeTree, keyFromName } from "./block-tree-summary";

type Props = {
  /** The block registry, for naming the trees. */
  registry: BlockRegistry;
  /** The page's current block list, for the "save selection" form's count. */
  blocks: ContentBlock[];
  /** The selected block, when one is selected — the selection a pattern is cut from. */
  selectedPath: number[] | null;
  /** The blocks the pattern picker will insert. */
  onInsert: (blocks: ContentBlock[]) => void;
  /** Close the panel. */
  onClose: () => void;
};

/** The pattern picker and the "save this as a pattern" form. */
export function PatternTools({ registry, blocks, selectedPath, onInsert, onClose }: Props) {
  const [patterns, setPatterns] = useState<ContentPattern[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

    // See the pattern library: a tenant-addressed read has to name its tenant, and the platform
    // Owner — the account every installation starts with — has none of its own.
  const tenant = useContentTenant();
  const organizationId = tenant.organizationId ?? undefined;

const load = useCallback(() => {
    setError(null);
    if (!organizationId) {
      setPatterns([]);
      return;
    }
    fetchPatterns(undefined, organizationId)
      .then(setPatterns)
      .catch((cause: unknown) => {
        setPatterns([]);
        setError(
          cause instanceof ApiError
            ? cause.message
            : "The pattern library could not be loaded.",
        );
      });
  }, [organizationId]);

  useEffect(load, [load]);

  const selection = selectedPath
    ? blocks.slice(0, 0).concat(subtree(blocks, selectedPath))
    : null;

  const insert = async (pattern: ContentPattern) => {
    setBusyId(pattern.id);
    setError(null);
    try {
      const answer = await fetchPatternBlocks(pattern.id, organizationId);
      onInsert(answer.blocks as ContentBlock[]);
      setBusyId(null);
    } catch (cause: unknown) {
      setError(
        cause instanceof ApiError
          ? cause.message
          : "That pattern could not be inserted.",
      );
      setBusyId(null);
    }
  };

  return (
    <section
      data-pattern-tools
      className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
    >
      <header className="flex items-baseline justify-between gap-3">
        <h2 className="flex items-center gap-1.5 text-[13.5px] font-medium">
          <Library className="size-4 text-muted" aria-hidden />
          Patterns
        </h2>
        <button
          type="button"
          onClick={onClose}
          className="cursor-pointer rounded-lg border border-line px-2 py-1 text-[12px] transition hover:bg-canvas"
        >
          Close
        </button>
      </header>

      {error ? (
        <p
          role="alert"
          data-pattern-tools-error
          className="rounded-lg border border-caution/40 bg-caution-soft px-2.5 py-2 text-[12px] text-caution"
        >
          {error}
        </p>
      ) : null}

      {patterns === null ? (
        <p className="text-[12px] text-muted">Loading the pattern library…</p>
      ) : patterns.length === 0 ? (
        <p className="text-[12px] text-muted">
          The library is empty. Build a page, select a block, and save it below — or create a
          pattern on the <code className="font-mono">/patterns</code> screen.
        </p>
      ) : (
        <ul className="flex flex-col gap-1.5">
          {patterns.map((pattern) => (
            <li
              key={pattern.id}
              data-pattern-option={pattern.key}
              className="flex items-center gap-2 rounded-lg border border-line px-2.5 py-2"
            >
              <span className="flex min-w-0 flex-1 flex-col">
                <span className="truncate text-[12.5px] font-medium">{pattern.name}</span>
                <span className="truncate font-mono text-[11px] text-muted">
                  {describeTree(pattern.blocks as ContentBlock[], registry, 4)}
                </span>
              </span>
              <button
                type="button"
                data-action={`insert-pattern-${pattern.key}`}
                disabled={busyId !== null}
                onClick={() => {
                  insert(pattern);
                }}
                className="inline-flex shrink-0 cursor-pointer items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-50"
              >
                <Plus className="size-3.5" aria-hidden />
                {busyId === pattern.id ? "Inserting…" : "Insert"}
              </button>
            </li>
          ))}
        </ul>
      )}

      <SaveSelectionForm
        registry={registry}
        selection={selection}
        saving={saving}
        onSaving={setSaving}
        onSaved={() => {
          setSaving(false);
          load();
        }}
        onError={(message) => {
          setSaving(false);
          setError(message);
        }}
      />
    </section>
  );
}

/** The blocks a path reaches, in order — the "selection" a pattern is cut from. */
function subtree(blocks: ContentBlock[], path: number[]): ContentBlock[] {
  const [index, ...rest] = path;
  const block = blocks[index];
  if (!block) {
    return [];
  }
  if (rest.length === 0) {
    return [block];
  }
  return subtree(block.children ?? [], rest);
}

// ---------------------------------------------------------------------------------------------

type SaveProps = {
  registry: BlockRegistry;
  selection: ContentBlock[] | null;
  saving: boolean;
  onSaving: (saving: boolean) => void;
  onSaved: () => void;
  onError: (message: string) => void;
};

/**
 * "New pattern from selection".
 *
 * It saves the **whole page's blocks** when nothing is selected, and the **selected subtree**
 * when something is — and it says which one it is about to do, because those are very
 * different patterns and an author who meant the second and got the first would only find out
 * by inserting it elsewhere.
 */
function SaveSelectionForm({
  registry,
  selection,
  saving,
  onSaving,
  onSaved,
  onError,
}: SaveProps) {
  // The hook lives *here* rather than in `PatternTools`, and that placement is the whole bug
  // this fixes. The parent needed it for the library list; the save form is a separate component
  // several hundred lines below, and the value never reached it. So the screen listed patterns
  // and refused to save one — the two halves of one rule kept in two places, and only the half
  // that had been noticed was correct.
  const tenant = useContentTenant();
  const organizationId = tenant.organizationId ?? undefined;
  const [name, setName] = useState("");
  const [key, setKey] = useState("");
  const [keyTouched, setKeyTouched] = useState(false);
  const [category, setCategory] = useState("general");
  const [description, setDescription] = useState("");

  const effectiveKey = keyTouched ? key : keyFromName(name);
  const source = selection ?? [];
  const ready = name.trim() !== "" && effectiveKey !== "" && !saving;

  const onSave = async () => {
    if (!ready) {
      return;
    }
    onSaving(true);
    try {
      await savePattern({
        key: effectiveKey,
        name: name.trim(),
        category: category.trim() || "general",
        description,
        blocks: source,
        // The fourth copy of the tenant rule, and the one the walkthrough still caught. The hook
        // was already in this file for the *read* above; the create beside it was never given
        // the value, so "save as pattern" answered 400 while the list beside it rendered fine.
        organizationId,
      });
      setName("");
      setKeyTouched(false);
      setDescription("");
      onSaved();
    } catch (cause: unknown) {
      onError(
        cause instanceof ApiError ? cause.message : "The pattern could not be saved.",
      );
    }
  };

  return (
    <div
      data-save-pattern-form
      className="flex flex-col gap-2 border-t border-line pt-3"
    >
      <p className="text-[12px] text-muted">
        {selection
          ? `New pattern from the selected block (${countBlocks(source)} block${countBlocks(source) === 1 ? "" : "s"}).`
          : "Nothing is selected, so this saves the whole page as one pattern."}
      </p>
      <div className="grid gap-2 sm:grid-cols-2">
        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] font-medium">Name</span>
          <input
            id="save-pattern-name"
            name="save-pattern-name"
            value={name}
            onChange={(event) => setName(event.target.value)}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] font-medium">Key</span>
          <input
            id="save-pattern-key"
            name="save-pattern-key"
            value={keyTouched ? key : keyFromName(name)}
            onChange={(event) => {
              setKeyTouched(true);
              setKey(event.target.value);
            }}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[12px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] font-medium">Category</span>
          <input
            id="save-pattern-category"
            name="save-pattern-category"
            value={category}
            onChange={(event) => setCategory(event.target.value)}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] font-medium">Description</span>
          <input
            id="save-pattern-description"
            name="save-pattern-description"
            value={description}
            onChange={(event) => setDescription(event.target.value)}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
      </div>
      {source.length > 0 ? (
        <p className="font-mono text-[11px] text-muted">
          {describeTree(source, registry, 6)}
        </p>
      ) : null}
      <button
        type="button"
        data-action="save-selection-as-pattern"
        disabled={!ready}
        onClick={() => {
          onSave();
        }}
        className="w-fit cursor-pointer rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-50"
      >
        {saving ? "Saving…" : "Save as pattern"}
      </button>
    </div>
  );
}
