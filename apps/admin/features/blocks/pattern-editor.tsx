"use client";

/**
 * The create/edit form for one pattern (REQ-063, slice 3).
 *
 * It is the *same* form for both, because a pattern's key is its identity: saving with an
 * existing key replaces that pattern, so there is nothing for a separate "create" endpoint to
 * decide and nothing for this form to know about which one it is.
 *
 * The blocks are edited as JSON, and that is a real decision rather than a shortcut. A pattern
 * is created *from a selection in the editor* — the editor already has a canvas, an inspector
 * and a validator, and rebuilding all three inside a dialog would give an author two places to
 * make the same mistake. The editor's "save as pattern" writes the selection here; this form
 * lets an author name it, categorise it, and see exactly what it holds before it is saved. The
 * JSON is validated by the *server* on save, and its refusal is shown verbatim — the same
 * message the editor would have shown, from the same validator.
 */
import { useMemo, useState } from "react";

import type { BlockRegistry, ContentPattern } from "@/lib/types";
import { Check, X } from "lucide-react";

import { ApiError, savePattern, updatePattern } from "@/lib/api";
import {
  countBlocks,
  describeTree,
  keyFromName,
  keyProblem,
} from "./block-tree-summary";
import type { PatternDraft } from "./pattern-library";

type Props = {
  /** The pattern being edited; absent for a create. */
  pattern?: ContentPattern;
  /** Pre-filled fields, for a duplicate. */
  draft?: PatternDraft | null;
  /** The block registry, for the outline line and the key hints. */
  registry: BlockRegistry | null;
  /** Close the form. */
  onCancel: () => void;
  /** The pattern was written. */
  onSaved: (pattern: ContentPattern) => void;
};

/** Blank and safe to edit: an empty list, not `null`. */
const EMPTY_JSON = "[]";

/** Create or edit one pattern. */
export function PatternEditor({ pattern, draft, registry, onCancel, onSaved }: Props) {
  const [name, setName] = useState(pattern?.name ?? draft?.name ?? "");
  const [key, setKey] = useState(pattern?.key ?? draft?.key ?? "");
  const [keyTouched, setKeyTouched] = useState(Boolean(pattern?.key));
  const [category, setCategory] = useState(
    pattern?.category ?? draft?.category ?? "general",
  );
  const [description, setDescription] = useState(
    pattern?.description ?? draft?.description ?? "",
  );
  const [json, setJson] = useState(
    pattern || draft
      ? JSON.stringify(pattern?.blocks ?? draft?.blocks ?? [], null, 2)
      : EMPTY_JSON,
  );
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  // The key is derived from the name until the author edits it. A pattern whose key followed
  // its name would change its identity on every rename — and the key is what the database
  // enforces as unique, so a rename would collide with whatever took the old key.
  const effectiveKey = keyTouched ? key : keyFromName(name);
  const keyError = effectiveKey === "" ? null : keyProblem(effectiveKey);
  const nameError = name.trim() === "" ? "A name is required." : null;

  const parsed = useMemo(() => {
    try {
      const value: unknown = JSON.parse(json);
      if (!Array.isArray(value)) {
        return { blocks: null, error: "A pattern's blocks must be a JSON array." };
      }
      return { blocks: value, error: null as string | null };
    } catch (cause) {
      return {
        blocks: null,
        error: `That is not valid JSON: ${(cause as Error).message}`,
      };
    }
  }, [json]);

  const ready =
    nameError === null && keyError === null && parsed.error === null && !saving;

  const onSave = async () => {
    if (!ready || parsed.blocks === null) {
      return;
    }
    setSaving(true);
    setError(null);
    try {
      const saved = pattern
        ? await updatePattern(pattern.id, {
            name: name.trim(),
            category: category.trim() || "general",
            description,
            blocks: parsed.blocks as never,
          })
        : await savePattern({
            key: effectiveKey,
            name: name.trim(),
            category: category.trim() || "general",
            description,
            blocks: parsed.blocks as never,
          });
      onSaved(saved);
    } catch (cause: unknown) {
      // The server's message is the validator's own, so an author sees the same wording the
      // editor would have shown for the same tree. Replacing it with "could not save" would
      // throw away the only sentence that says which block is wrong.
      setError(
        cause instanceof ApiError
          ? cause.message
          : "The pattern could not be saved.",
      );
      setSaving(false);
    }
  };

  const blocks = parsed.blocks;

  return (
    <section
      data-pattern-editor={pattern ? pattern.key : "new"}
      className="flex flex-col gap-3 rounded-xl border border-accent/40 bg-surface p-4"
    >
      <header className="flex items-baseline justify-between gap-3">
        <h3 className="text-[13.5px] font-medium">
          {pattern ? `Edit ${pattern.name}` : "New pattern"}
        </h3>
        <button
          type="button"
          onClick={onCancel}
          aria-label="Close the pattern form"
          className="cursor-pointer rounded-lg border border-line p-1 transition hover:bg-canvas"
        >
          <X className="size-3.5" aria-hidden />
        </button>
      </header>

      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1">
          <span className="text-[12px] font-medium">Name</span>
          <input
            id="pattern-name"
            name="pattern-name"
            value={name}
            onChange={(event) => {
              setName(event.target.value);
              if (!keyTouched) {
                setKey(keyFromName(event.target.value));
              }
            }}
            aria-invalid={nameError !== null}
            aria-describedby={nameError ? "pattern-name-error" : undefined}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
          {nameError ? (
            <span id="pattern-name-error" className="text-[11.5px] text-caution">
              {nameError}
            </span>
          ) : null}
        </label>

        <label className="flex flex-col gap-1">
          <span className="text-[12px] font-medium">Key</span>
          <input
            id="pattern-key"
            name="pattern-key"
            value={keyTouched ? key : keyFromName(name)}
            onChange={(event) => {
              setKeyTouched(true);
              setKey(event.target.value);
            }}
            aria-invalid={keyError !== null}
            aria-describedby={keyError ? "pattern-key-error" : "pattern-key-hint"}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
          {keyError ? (
            <span id="pattern-key-error" className="text-[11.5px] text-caution">
              {keyError}
            </span>
          ) : (
            <span id="pattern-key-hint" className="text-[11.5px] text-muted">
              The key is the pattern&apos;s identity. Saving with an existing key replaces it.
            </span>
          )}
        </label>

        <label className="flex flex-col gap-1">
          <span className="text-[12px] font-medium">Category</span>
          <input
            id="pattern-category-field"
            name="pattern-category"
            value={category}
            onChange={(event) => setCategory(event.target.value)}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>

        <label className="flex flex-col gap-1">
          <span className="text-[12px] font-medium">Description</span>
          <input
            id="pattern-description"
            name="pattern-description"
            value={description}
            onChange={(event) => setDescription(event.target.value)}
            placeholder="One line: what this group is for"
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
      </div>

      <label className="flex flex-col gap-1">
        <span className="flex items-baseline justify-between text-[12px] font-medium">
          <span>Blocks</span>
          {blocks ? (
            <span className="font-normal text-muted">
              {countBlocks(blocks as never)} block
              {countBlocks(blocks as never) === 1 ? "" : "s"}
            </span>
          ) : null}
        </span>
        <textarea
          id="pattern-blocks"
          name="pattern-blocks"
          value={json}
          onChange={(event) => setJson(event.target.value)}
          rows={8}
          spellCheck={false}
          aria-invalid={parsed.error !== null}
          aria-describedby={parsed.error ? "pattern-blocks-error" : undefined}
          className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[11.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
        />
        {parsed.error ? (
          <span id="pattern-blocks-error" className="text-[11.5px] text-caution">
            {parsed.error}
          </span>
        ) : registry && blocks ? (
          <span className="text-[11.5px] text-muted">
            {describeTree(blocks as never, registry, 8)}
          </span>
        ) : null}
      </label>

      {error ? (
        <p
          role="alert"
          data-pattern-error
          className="rounded-lg border border-caution/40 bg-caution-soft px-2.5 py-2 text-[12px] text-caution"
        >
          {error}
        </p>
      ) : null}

      <div className="flex items-center gap-2">
        <button
          type="button"
          data-action="save-pattern"
          disabled={!ready}
          onClick={() => {
            onSave();
          }}
          className="inline-flex cursor-pointer items-center gap-1.5 rounded-lg bg-accent px-2.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:cursor-not-allowed disabled:opacity-50"
        >
          <Check className="size-3.5" aria-hidden />
          {saving ? "Saving…" : pattern ? "Save pattern" : "Create pattern"}
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="cursor-pointer rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] transition hover:bg-canvas"
        >
          Cancel
        </button>
      </div>
    </section>
  );
}
