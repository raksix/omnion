"use client";

/**
 * Transformation presets: the named sizes a site may ask its images for (REQ-010, slice 3).
 *
 * A page asks for `?preset=card`; the platform answers with the pixels, built on the first
 * request and cached by a hash of its inputs. This screen is where the names come from, and the
 * parts that matter are the ones an operator cannot infer from a name:
 *
 * - **the example URL, copyable** — a preset nobody has pasted into a template is a preset that
 *   will never be used, and the URL is the whole contract;
 * - **what an edit does** — it changes the pixels the name produces *from now on*, and the
 *   already-generated ones stay exactly where they are. Saying so here is the difference between
 *   a surprising change and an understood one;
 * - **what a delete does** — it takes every derivative built from that preset with it, so it
 *   asks for a confirmation that names them.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { Check, Copy, Loader2, Pencil, Plus, Trash2, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  createMediaPreset,
  deleteMediaPreset,
  fetchMediaPresets,
  mediaRawUrl,
  updateMediaPreset,
  type MediaPresetInput,
} from "@/lib/api";
import { useSites } from "@/lib/sites";
import type { MediaPreset } from "@/lib/types";

/** The fits a preset can use, with the label each one gets. */
const FITS: [string, string][] = [
  ["cover", "Cover — fills the box and crops the overflow"],
  ["contain", "Contain — fits inside the box and pads the rest"],
  ["fill", "Fill — stretches to the box, ignoring the ratio"],
];

/** The formats a preset can emit. */
const FORMATS: [string, string][] = [
  ["webp", "WebP — smallest"],
  ["jpeg", "JPEG — universally supported"],
  ["png", "PNG — lossless, largest"],
];

/** The largest dimension a preset may ask for, matching the API's own ceiling. */
const MAX_DIMENSION = 8192;

/** One preset, as the editor holds it while it is being changed. */
type Draft = {
  name: string;
  width: string;
  height: string;
  fit: string;
  format: string;
  quality: string;
};

const EMPTY_DRAFT: Draft = {
  name: "",
  width: "",
  height: "",
  fit: "cover",
  format: "webp",
  quality: "80",
};

/** A preset as a fresh draft. */
function toDraft(preset: MediaPreset): Draft {
  return {
    name: preset.name,
    width: preset.width === null ? "" : String(preset.width),
    height: preset.height === null ? "" : String(preset.height),
    fit: preset.fit,
    format: preset.format,
    quality: String(preset.quality),
  };
}

/** The transformation presets of the selected site. */
export function MediaSettingsView() {
  const { selectedSite, status: sitesStatus } = useSites();
  const [presets, setPresets] = useState<MediaPreset[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [editing, setEditing] = useState<string | "new" | null>(null);
  const [draft, setDraft] = useState<Draft>(EMPTY_DRAFT);
  const [reloadToken, setReloadToken] = useState(0);

  const siteId = selectedSite?.id ?? null;
  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    if (!siteId) {
      setPresets(null);
      return;
    }
    let cancelled = false;
    setPresets(null);
    setError(null);
    fetchMediaPresets(siteId)
      .then((answer) => {
        if (!cancelled) {
          setPresets(answer.presets);
        }
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setError(
          cause instanceof ApiError ? cause.message : "The presets could not be loaded.",
        );
      });
    return () => {
      cancelled = true;
    };
  }, [siteId, reloadToken]);

  const startNew = () => {
    setEditing("new");
    setDraft(EMPTY_DRAFT);
    setFieldError(null);
    setNotice(null);
  };

  const startEdit = (preset: MediaPreset) => {
    setEditing(preset.id);
    setDraft(toDraft(preset));
    setFieldError(null);
    setNotice(null);
  };

  const cancel = () => {
    setEditing(null);
    setFieldError(null);
  };

  /** Turn the draft into the request body, or explain which input refused it. */
  const build = (): MediaPresetInput | { field: string; message: string } => {
    const name = draft.name.trim().toLowerCase();
    if (name === "") {
      return { field: "name", message: "The preset needs a name — that name is the URL." };
    }
    if (!/^[a-z0-9][a-z0-9._-]*$/.test(name)) {
      return {
        field: "name",
        message: "A name starts with a letter or digit, and may then contain . _ and -",
      };
    }

    const width = draft.width.trim() === "" ? null : Number(draft.width);
    const height = draft.height.trim() === "" ? null : Number(draft.height);
    if (width === null && height === null) {
      return {
        field: "width",
        message: "A preset needs a width or a height — a preset with neither is the original file.",
      };
    }
    for (const [label, value] of [
      ["width", width],
      ["height", height],
    ] as const) {
      if (value !== null && (!Number.isInteger(value) || value < 1 || value > MAX_DIMENSION)) {
        return { field: label, message: `The ${label} must be between 1 and ${MAX_DIMENSION}.` };
      }
    }

    const quality = Number(draft.quality);
    if (!Number.isInteger(quality) || quality < 1 || quality > 100) {
      return { field: "quality", message: "The quality must be between 1 and 100." };
    }

    return { name, width, height, fit: draft.fit, format: draft.format, quality };
  };

  const save = async () => {
    if (!siteId || editing === null) {
      return;
    }
    const built = build();
    if ("field" in built) {
      setFieldError({ field: built.field, message: built.message });
      return;
    }
    setFieldError(null);
    setBusy(true);
    setError(null);
    try {
      if (editing === "new") {
        const created = await createMediaPreset(siteId, built);
        setNotice(`“${created.name}” created. Pages can now ask for ${created.example_query}.`);
      } else {
        const updated = await updateMediaPreset(siteId, editing, built);
        setNotice(
          `“${updated.name}” saved. Images already generated keep the pixels they were built from; ` +
            "new requests use the new definition.",
        );
      }
      setEditing(null);
      reload();
    } catch (cause) {
      if (cause instanceof ApiError) {
        // The API names the field it refused. Putting the message under that input is the whole
        // reason the error carries one; a banner above a six-field form tells the operator
        // nothing about which input to look at.
        const field = typeof cause.details?.field === "string" ? cause.details.field : "";
        setFieldError({ field, message: cause.message });
        if (field === "") {
          setError(cause.message);
        }
      } else {
        setError("The preset could not be saved.");
      }
    } finally {
      setBusy(false);
    }
  };

  const remove = async (preset: MediaPreset) => {
    if (!siteId) {
      return;
    }
    // Named, because the consequence is not obvious from the button: every derivative built from
    // this preset goes with it, and any page still asking for it falls back to the original.
    const confirmed = window.confirm(
      `Delete the “${preset.name}” preset?\n\n` +
        "Every image already generated with it is deleted too, and any page still asking for it " +
        "will fall back to the original file.",
    );
    if (!confirmed) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await deleteMediaPreset(siteId, preset.id);
      setNotice(`“${preset.name}” deleted, with the images it had generated.`);
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The preset could not be deleted.");
    } finally {
      setBusy(false);
    }
  };

  const errorFor = (field: string) =>
    fieldError && fieldError.field === field ? fieldError.message : null;

  if (sitesStatus === "error") {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="The site list could not be loaded"
          hint="Presets belong to a site, so the screen needs the sites first."
        />
      </div>
    );
  }

  if (sitesStatus === "ready" && !selectedSite) {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState title="No sites yet" hint="A transformation preset belongs to a site." />
      </div>
    );
  }

  return (
    <div className="overflow-hidden rounded-xl border border-line bg-surface">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-3">
        <div className="flex items-baseline gap-2">
          <h2 className="text-[13.5px] font-medium">Transformation presets</h2>
          {presets ? (
            <span className="text-[12px] text-muted">
              {presets.length} {presets.length === 1 ? "preset" : "presets"}
            </span>
          ) : null}
        </div>
        <button
          type="button"
          onClick={startNew}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
        >
          <Plus className="size-3.5" aria-hidden />
          New preset
        </button>
      </div>

      <p className="border-b border-line bg-canvas/60 px-4 py-2 text-[12px] text-muted">
        A page asks for an image by name — <code className="font-mono">?preset=card</code> — and the
        answer is the same pixels every time. The first request builds them; later ones are served
        from the cache. A request larger than the original is refused rather than enlarged, so
        serve the original instead of asking for more pixels than it has.
      </p>

      {notice ? (
        <p className="border-b border-line bg-canvas/60 px-4 py-2 text-[12px] text-muted">
          {notice}
        </p>
      ) : null}

      {error ? (
        <div
          role="alert"
          className="flex flex-col items-center gap-3 border-b border-line px-6 py-6 text-center"
        >
          <p className="text-[12.5px] text-accent-strong">{error}</p>
          <button
            type="button"
            onClick={reload}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Try again
          </button>
        </div>
      ) : null}

      {presets === null && !error ? (
        <LoadingTable columns={4} />
      ) : (
        <PresetList
          presets={presets ?? []}
          busy={busy}
          onEdit={startEdit}
          onDelete={remove}
        />
      )}

      {editing !== null ? (
        <PresetEditor
          draft={draft}
          setDraft={setDraft}
          busy={busy}
          saving={editing === "new" ? "Create preset" : "Save changes"}
          errorFor={errorFor}
          onSave={() => void save()}
          onCancel={cancel}
        />
      ) : null}
    </div>
  );
}

/** The preset table, with a real empty state rather than a blank area. */
function PresetList({
  presets,
  busy,
  onEdit,
  onDelete,
}: {
  presets: MediaPreset[];
  busy: boolean;
  onEdit: (preset: MediaPreset) => void;
  onDelete: (preset: MediaPreset) => void;
}) {
  if (presets.length === 0) {
    return (
      <EmptyState
        title="No presets yet"
        hint="A preset is a named size — a card, a thumbnail, a social image. Create one and any page can ask for it by name."
      />
    );
  }

  return (
    <ul className="divide-y divide-line">
      {presets.map((preset) => (
        <li key={preset.id} className="flex flex-wrap items-center gap-3 px-4 py-3">
          <div className="min-w-[180px] flex-1">
            <p className="font-mono text-[13px] font-medium">{preset.name}</p>
            <p className="text-[12px] text-muted">{preset.summary}</p>
          </div>
          <PresetExample preset={preset} />
          <div className="flex items-center gap-1">
            <button
              type="button"
              onClick={() => onEdit(preset)}
              disabled={busy}
              aria-label={`Edit the ${preset.name} preset`}
              className="rounded p-1.5 text-muted transition hover:text-ink disabled:cursor-not-allowed"
            >
              <Pencil className="size-3.5" aria-hidden />
            </button>
            <button
              type="button"
              onClick={() => onDelete(preset)}
              disabled={busy}
              aria-label={`Delete the ${preset.name} preset`}
              className="rounded p-1.5 text-muted transition hover:text-accent-strong disabled:cursor-not-allowed"
            >
              <Trash2 className="size-3.5" aria-hidden />
            </button>
          </div>
        </li>
      ))}
    </ul>
  );
}

/** The URL a page uses, with a copy button that says whether it worked. */
function PresetExample({ preset }: { preset: MediaPreset }) {
  const [copied, setCopied] = useState(false);
  const example = useMemo(() => `${mediaRawUrl("<file-id>")}${preset.example_query}`, [preset]);

  return (
    <div className="flex items-center gap-1.5">
      <code className="max-w-[280px] truncate rounded bg-canvas px-2 py-1 font-mono text-[11.5px] text-muted">
        {example}
      </code>
      <button
        type="button"
        onClick={() => {
          void navigator.clipboard?.writeText(example).then(() => {
            setCopied(true);
            window.setTimeout(() => setCopied(false), 2000);
          });
        }}
        aria-label={`Copy the example URL for ${preset.name}`}
        className="rounded p-1.5 text-muted transition hover:text-ink"
      >
        {copied ? (
          <Check className="size-3.5" aria-hidden />
        ) : (
          <Copy className="size-3.5" aria-hidden />
        )}
      </button>
    </div>
  );
}

/** The create/edit form, with every field labelled and its error under it. */
function PresetEditor({
  draft,
  setDraft,
  busy,
  saving,
  errorFor,
  onSave,
  onCancel,
}: {
  draft: Draft;
  setDraft: (draft: Draft) => void;
  busy: boolean;
  saving: string;
  errorFor: (field: string) => string | null;
  onSave: () => void;
  onCancel: () => void;
}) {
  const set = (key: keyof Draft) => (value: string) => setDraft({ ...draft, [key]: value });

  return (
    <div className="border-t border-line bg-canvas/40">
      <div className="flex items-center justify-between px-4 pt-3">
        <h3 className="text-[13px] font-medium">{saving === "Create preset" ? "New preset" : "Edit preset"}</h3>
        <button
          type="button"
          onClick={onCancel}
          aria-label="Close the preset editor"
          className="rounded p-1.5 text-muted transition hover:text-ink"
        >
          <X className="size-3.5" aria-hidden />
        </button>
      </div>

      <div className="grid gap-3 px-4 py-3 sm:grid-cols-2">
        <Field label="Name" hint="Lowercase; it appears in the URL as ?preset=…" error={errorFor("name")}>
          <input
            value={draft.name}
            onChange={(event) => set("name")(event.target.value)}
            placeholder="card"
            className={inputClass(errorFor("name"))}
          />
        </Field>

        <Field label="Size" hint="At least one edge. The other follows the image's ratio.">
          <div className="flex items-center gap-2">
            <input
              value={draft.width}
              onChange={(event) => set("width")(event.target.value)}
              inputMode="numeric"
              placeholder="1200"
              aria-label="Width in pixels"
              className={inputClass(errorFor("width"))}
            />
            <span className="text-[12px] text-muted">×</span>
            <input
              value={draft.height}
              onChange={(event) => set("height")(event.target.value)}
              inputMode="numeric"
              placeholder="630"
              aria-label="Height in pixels"
              className={inputClass(errorFor("height"))}
            />
          </div>
          {errorFor("width") ? <FieldError message={errorFor("width")!} /> : null}
        </Field>

        <Field label="Fit" hint="How the image fills the box.">
          <select
            value={draft.fit}
            onChange={(event) => set("fit")(event.target.value)}
            className={inputClass(null)}
          >
            {FITS.map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
        </Field>

        <Field label="Format" hint="What the answer is encoded as.">
          <select
            value={draft.format}
            onChange={(event) => set("format")(event.target.value)}
            className={inputClass(null)}
          >
            {FORMATS.map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
        </Field>

        <Field label="Quality" hint="1–100. A lower number is a smaller file." error={errorFor("quality")}>
          <input
            value={draft.quality}
            onChange={(event) => set("quality")(event.target.value)}
            inputMode="numeric"
            className={inputClass(errorFor("quality"))}
          />
        </Field>
      </div>

      <div className="flex items-center gap-2 px-4 pb-4">
        <button
          type="button"
          onClick={onSave}
          disabled={busy}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:cursor-not-allowed"
        >
          {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
          {saving}
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-lg px-3 py-1.5 text-[12.5px] text-muted transition hover:text-ink"
        >
          Cancel
        </button>
      </div>
    </div>
  );
}

/** One labelled form field with its hint and error. */
function Field({
  label,
  hint,
  error,
  children,
}: {
  label: string;
  hint: string;
  error?: string | null;
  children: React.ReactNode;
}) {
  return (
    <label className="block">
      <span className="text-[12.5px] font-medium">{label}</span>
      <div className="mt-1">{children}</div>
      {error ? (
        <FieldError message={error} />
      ) : (
        <span className="mt-1 block text-[11.5px] text-muted">{hint}</span>
      )}
    </label>
  );
}

/** A field-level error, rendered where the field is rather than in a banner. */
function FieldError({ message }: { message: string }) {
  return (
    <span className="mt-1 block text-[11.5px] text-accent-strong" role="alert">
      {message}
    </span>
  );
}

/** The input class, tinted when the field carries an error. */
function inputClass(error: string | null | undefined): string {
  return [
    "w-full rounded-lg border bg-surface px-2.5 py-1.5 text-[13px] outline-none transition",
    "focus:border-accent focus:ring-2 focus:ring-accent/20",
    error ? "border-accent-strong" : "border-line",
  ].join(" ");
}
