/**
 * The custom pairs on the Metadata tab: the key/value rows an editor hangs on a file.
 *
 * ## Why this is a block and not four more fields
 *
 * The pairs are not facts about the *file* — they are facts about *this library's* copy of it.
 * The camera block reads `ISO 400` because the photograph says so; `campaign = spring-2026` says
 * so only because somebody typed it. Putting them in the same list as the dimensions would
 * claim they were extracted, and an editor who cannot tell which is which will trust both.
 *
 * ## Why the set is replaced, not merged
 *
 * The editor holds the whole set at all times and sends all of it. A row deleted here is deleted
 * in the row; there is no "and leave the others alone" button to get wrong. The alternative —
 * sending only the rows that changed — needs the server to merge, and a merge that loses a pair
 * on a conflict loses a licence number silently.
 *
 * ## What the server refuses, and what this shows
 *
 * A pair can be refused for its shape (a nested object), for its key (blank, punctuation-only,
 * over 60 bytes) or for its size (over 500 bytes, or over 40 pairs in total). Each refusal names
 * the offending key as `metadata.<key>`; `pairErrorFor` maps that back onto the row so the
 * message lands under the input that caused it, which is the same rule the scanning and retention
 * settings forms follow.
 */

import { useCallback, useState } from "react";
import { Plus, Save, Trash2 } from "lucide-react";

import { ApiError, updateMediaFile } from "@/lib/api";
import type { MediaFile } from "@/lib/types";

/** How many pairs the server accepts on one file. */
const MAX_PAIRS = 40;

/** The longest key, in bytes. */
const MAX_KEY_LENGTH = 60;

/** The longest value, in bytes. */
const MAX_VALUE_LENGTH = 500;

/** One row of the editor. */
type PairRow = {
  /** Stable across edits so React does not remount the input on every keystroke. */
  id: string;
  key: string;
  value: string;
};

/** Build the editor's rows from the pairs the file carries. */
function rowsOf(file: MediaFile): PairRow[] {
  const pairs = file.metadata ?? {};
  return Object.entries(pairs).map(([key, value]) => ({
    // The key is unique by the server's own rule, so it is the identity.
    id: `pair-${key}`,
    key,
    value: String(value),
  }));
}

/** A new row's id, unique against the ones already on screen. */
function nextId(rows: PairRow[]): string {
  return `pair-new-${rows.length}-${Math.random().toString(36).slice(2, 8)}`;
}

/**
 * The error message that belongs to one row, or `null`.
 *
 * The server's detail field is `metadata.<key>`, so a refusal about a licence number cannot land
 * under the campaign field beside it. A refusal about the *set* (`metadata`, with no key) is
 * shown once under the heading instead — there is no row it belongs to.
 */
function pairErrorFor(field: string | null, message: string | null): {
  row: string | null;
  set: string | null;
} {
  if (!field || !message) {
    return { row: null, set: null };
  }
  if (field === "metadata") {
    return { row: null, set: message };
  }
  const key = field.startsWith("metadata.") ? field.slice("metadata.".length) : field;
  return { row: key, set: null };
}

/** The pair editor, with its own save so a pair change is never a silent caption edit. */
export function MetadataPairsEditor({
  file,
  onSaved,
}: {
  file: MediaFile;
  onSaved: (file: MediaFile) => void;
}) {
  const [rows, setRows] = useState<PairRow[]>(() => rowsOf(file));
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<{ field: string | null; message: string } | null>(null);

  // The editor follows the file: a restore that changes the row must repaint the pairs, or the
  // screen shows a version's campaign beside the current version's picture.
  const signature = JSON.stringify(file.metadata ?? {});
  const [seenSignature, setSeenSignature] = useState(signature);
  if (signature !== seenSignature) {
    setSeenSignature(signature);
    setRows(rowsOf(file));
  }

  const update = useCallback((id: string, patch: Partial<PairRow>) => {
    setRows((current) =>
      current.map((row) => (row.id === id ? { ...row, ...patch } : row)),
    );
  }, []);

  const add = useCallback(() => {
    setRows((current) => [...current, { id: nextId(current), key: "", value: "" }]);
  }, []);

  const remove = useCallback((id: string) => {
    setRows((current) => current.filter((row) => row.id !== id));
  }, []);

  const save = useCallback(async () => {
    setSaving(true);
    setError(null);
    setNotice(null);
    // A blank row is a row being typed, not a pair: sending `{"": ""}` would be refused by the
    // server for a reason the editor can see and fix, so it is dropped here instead.
    const pairs: Record<string, string> = {};
    for (const row of rows) {
      const key = row.key.trim();
      if (key === "" && row.value.trim() === "") {
        continue;
      }
      if (key === "") {
        setError({
          field: "metadata",
          message: "Every pair needs a key. Fill it in, or remove the row.",
        });
        setSaving(false);
        return;
      }
      pairs[key] = row.value;
    }

    try {
      const updated = await updateMediaFile(file.id, { metadata: pairs });
      onSaved(updated);
      setRows(rowsOf(updated));
      setNotice(
        Object.keys(pairs).length === 0
          ? "The pairs were cleared."
          : `Saved ${Object.keys(pairs).length} ${Object.keys(pairs).length === 1 ? "pair" : "pairs"}.`,
      );
    } catch (cause) {
      if (cause instanceof ApiError) {
        // `details` is `Record<string, unknown> | null`, so the field arrives as `unknown` and
        // has to be narrowed before it can index a row.
        const named = cause.details?.field;
        setError({
          field: typeof named === "string" ? named : null,
          message: cause.message,
        });
      } else {
        setError({ field: null, message: "The pairs were not saved." });
      }
    } finally {
      setSaving(false);
    }
  }, [file.id, rows, onSaved]);

  const placed = pairErrorFor(error?.field ?? null, error?.message ?? null);

  return (
    <section
      id="media-metadata-pairs"
      className="space-y-3 border-t border-line pt-3"
      aria-labelledby="media-metadata-pairs-heading"
    >
      <div className="flex items-baseline justify-between gap-3">
        <div>
          <h3
            id="media-metadata-pairs-heading"
            className="text-[12px] font-medium text-ink"
          >
            Custom pairs
          </h3>
          <p className="text-[11px] text-muted">
            Your own key/value notes — a campaign, a licence, a shoot. Searchable from the browser
            toolbar with <code className="font-mono">key=value</code>. Up to {MAX_PAIRS} pairs,{" "}
            {MAX_KEY_LENGTH}-byte keys and {MAX_VALUE_LENGTH}-byte values.
          </p>
        </div>
      </div>

      {rows.length === 0 ? (
        <p
          data-testid="media-pairs-empty"
          className="rounded-md border border-dashed border-line px-3 py-3 text-[12px] text-muted"
        >
          No custom pairs on this file. Add one to record the campaign, the licence or anything else
          the library should be able to filter by later.
        </p>
      ) : (
        <ul className="space-y-2">
          {rows.map((row) => {
            const rowError =
              placed.row !== null && placed.row === row.key.trim() ? placed.set : null;
            return (
              <li key={row.id} className="flex items-start gap-2">
                <div className="flex-1">
                  <label
                    htmlFor={`${row.id}-key`}
                    className="sr-only"
                  >
                    {`Pair ${row.key || "(new)"} key`}
                  </label>
                  <input
                    id={`${row.id}-key`}
                    data-testid="media-pair-key"
                    value={row.key}
                    placeholder="campaign"
                    onChange={(event) => update(row.id, { key: event.target.value })}
                    className="w-full rounded-md border border-line bg-panel px-2 py-1 font-mono text-[12px] text-ink outline-none transition-colors focus:border-accent-strong"
                  />
                  {rowError ? (
                    <p
                      role="alert"
                      className="mt-1 text-[11px] text-negative"
                    >
                      {rowError}
                    </p>
                  ) : null}
                </div>
                <div className="flex-1">
                  <label htmlFor={`${row.id}-value`} className="sr-only">
                    {`Pair ${row.key || "(new)"} value`}
                  </label>
                  <input
                    id={`${row.id}-value`}
                    data-testid="media-pair-value"
                    value={row.value}
                    placeholder="spring-2026"
                    onChange={(event) => update(row.id, { value: event.target.value })}
                    className="w-full rounded-md border border-line bg-panel px-2 py-1 text-[12px] text-ink outline-none transition-colors focus:border-accent-strong"
                  />
                </div>
                <button
                  type="button"
                  data-testid="media-pair-remove"
                  onClick={() => remove(row.id)}
                  aria-label={`Remove the pair ${row.key || "(new)"}`}
                  className="mt-0.5 shrink-0 rounded p-1 text-muted transition-colors hover:text-negative"
                >
                  <Trash2 className="h-3.5 w-3.5" aria-hidden />
                </button>
              </li>
            );
          })}
        </ul>
      )}

      {placed.row === null && placed.set ? (
        <p
          role="alert"
          data-testid="media-pairs-set-error"
          className="text-[11px] text-negative"
        >
          {placed.set}
        </p>
      ) : null}

      {notice ? (
        <p
          data-testid="media-pairs-notice"
          className="text-[11px] text-positive"
        >
          {notice}
        </p>
      ) : null}

      <div className="flex items-center gap-2">
        <button
          type="button"
          id="media-add-pair"
          onClick={add}
          disabled={rows.length >= MAX_PAIRS}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12px] font-medium text-ink transition-colors hover:bg-quiet-soft disabled:opacity-50"
        >
          <Plus className="h-3.5 w-3.5" aria-hidden />
          Add pair
        </button>
        <button
          type="button"
          id="media-save-pairs"
          disabled={saving}
          onClick={() => void save()}
          className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12px] font-medium text-inverted transition-opacity hover:opacity-90 disabled:opacity-50"
        >
          <Save className="h-3.5 w-3.5" aria-hidden />
          {saving ? "Saving…" : "Save pairs"}
        </button>
      </div>
      {rows.length >= MAX_PAIRS ? (
        <p className="text-[11px] text-muted">
          This file already carries the {MAX_PAIRS} pairs a file may hold. Remove one to add
          another.
        </p>
      ) : null}
    </section>
  );
}
