"use client";

/**
 * `/forms/<id>/edit` — the builder (REQ-064, slice 2).
 *
 * The builder is three panes — palette, canvas, inspector — plus a settings drawer, and every
 * one of them is a *client* of the store's rules rather than a second authority:
 *
 * * **Validation messages are typed once and shown twice.** The inspector writes the rules; the
 *   preview runs the same `validate_answer` the public submit route will run, through the same
 *   endpoint. A preview that is more forgiving than the live form teaches an owner that a form
 *   works when it does not.
 * * **The publish button is refused locally, with the store's own reason.** Publishing a form
 *   with no fields is refused by the store; the button says so before the round trip rather than
 *   after it, and the same sentence appears when the server refuses.
 * * **Save keeps working state; Publish makes it live.** Those are two different promises, and a
 *   screen with one button makes one of them a lie.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { useRouter } from "next/navigation";
import {
  ArrowDown,
  ArrowUp,
  Eye,
  Loader2,
  Plus,
  RefreshCw,
  Save,
  Settings2,
  Trash2,
} from "lucide-react";

import { ApiError, fetchForm, saveFormFields, setFormStatus, updateForm } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { FormDetail, FormField } from "@/lib/types";

import { FieldInspector, FormPreview, FormSettings } from "./form-builder-panes";

/** The types the palette offers, in palette order. */
const PALETTE: { type: string; label: string; hint: string }[] = [
  { type: "text", label: "Text", hint: "One line" },
  { type: "textarea", label: "Textarea", hint: "A paragraph" },
  { type: "select", label: "Select", hint: "A dropdown" },
  { type: "radio", label: "Radio", hint: "One of many, visible" },
  { type: "checkbox", label: "Checkbox", hint: "One of many, compact" },
  { type: "date", label: "Date", hint: "A calendar" },
  { type: "file", label: "File", hint: "An attachment" },
  { type: "consent", label: "Consent", hint: "A box that has to be accepted" },
];

/** A field as the builder holds it while editing. Exported for the inspector and preview. */
export type Draft = {
  /** Stable client key, so React does not confuse two identical fields while dragging. */
  draftId: string;
  key: string;
  label: string;
  field_type: string;
  required: boolean;
  placeholder: string;
  help_text: string;
  width: string;
  rules: Record<string, unknown>;
  options: string[];
};

let draftCounter = 0;
function nextDraftId(): string {
  draftCounter += 1;
  return `draft-${draftCounter}`;
}

function toDraft(field: FormField): Draft {
  const raw = Array.isArray(field.options) ? field.options : [];
  return {
    draftId: field.id,
    key: field.key,
    label: field.label,
    field_type: field.field_type,
    required: field.required,
    placeholder: field.placeholder ?? "",
    help_text: field.help_text ?? "",
    width: field.width,
    rules: field.rules ?? {},
    // Both stored shapes read the same here: `{"value","label"}` objects and bare strings.
    options: raw
      .map((option) =>
        typeof option === "string"
          ? option
          : String(
              (option as { value?: unknown; label?: unknown })?.value ??
                (option as { label?: unknown })?.label ??
                "",
            ),
      )
      .filter((option) => option !== ""),
  };
}

/** The options a builder holds back into the store's shape. */
function toOptions(field: Draft): unknown {
  if (field.options.length === 0) return [];
  return field.options.map((option) => ({ value: option, label: option }));
}

/** A key derived from the label, so the common case is one field instead of three. */
function suggestKey(label: string): string {
  return label
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "_")
    .replace(/^_+|_+$/g, "")
    .replace(/^[0-9]+/, "f$&")
    .slice(0, 48);
}

/**
 * The error the builder refuses a *local* save with, checked before the round trip.
 *
 * Deliberately the same rules the store enforces, in the same order, so the two agree: a form
 * with two fields sharing a key is refused here with the same sentence the server would send.
 */
function localProblems(fields: Draft[]): Record<string, string> {
  const problems: Record<string, string> = {};
  if (fields.length === 0) {
    problems.__form = "a form needs at least one field";
    return problems;
  }
  const seen = new Map<string, number>();
  for (const field of fields) {
    if (!field.key.trim()) {
      problems[field.draftId] = "this field has no key — the answer would have no name";
      continue;
    }
    if (!/^[a-z][a-z0-9_]*$/.test(field.key)) {
      problems[field.draftId] =
        "the key must start with a letter and use only lowercase letters, digits and underscores";
      continue;
    }
    const count = (seen.get(field.key) ?? 0) + 1;
    seen.set(field.key, count);
    if (count > 1) {
      problems[field.draftId] = `two fields share the key "${field.key}" — answers are stored by key`;
      continue;
    }
    if (!field.label.trim()) {
      problems[field.draftId] = "this field has no label";
      continue;
    }
    if (
      ["select", "radio"].includes(field.field_type) &&
      field.options.every((option) => option.trim() === "")
    ) {
      problems[field.draftId] = `a ${field.field_type} with no options to choose from`;
    }
  }
  return problems;
}

export function FormBuilder({ formId }: { formId: string }) {
  const router = useRouter();
  const [detail, setDetail] = useState<FormDetail | null>(null);
  const [fields, setFields] = useState<Draft[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [showSettings, setShowSettings] = useState(false);
  const [showPreview, setShowPreview] = useState(false);

  const load = useCallback(async () => {
    setError(null);
    try {
      const document = await fetchForm(formId);
      setDetail(document);
      const drafts = document.fields.map(toDraft);
      setFields(drafts);
      setSelected(drafts[0]?.draftId ?? null);
    } catch (caught) {
      setError((caught as ApiError).message);
    }
  }, [formId]);

  useEffect(() => {
    void load();
  }, [load]);

  const problems = useMemo(() => (fields ? localProblems(fields) : {}), [fields]);
  const active = fields?.find((field) => field.draftId === selected) ?? null;

  const mutate = useCallback((draftId: string, change: Partial<Draft>) => {
    setFields((current) =>
      current?.map((field) =>
        field.draftId === draftId
          ? {
              ...field,
              ...change,
              // The key follows the label until the editor edits the key themselves — the same
              // rule the create form uses, and the reason renaming a label does not orphan the
              // answers already stored under the old key *until they choose to*.
              ...(change.label !== undefined && field.key === suggestKey(field.label)
                ? { key: suggestKey(change.label) }
                : {}),
            }
          : field,
      ) ?? null,
    );
  }, []);

  const addField = useCallback((fieldType: string) => {
    const label = fieldType === "consent" ? "I agree to the privacy policy" : "New field";
    const draft: Draft = {
      draftId: nextDraftId(),
      key: suggestKey(label) || "field",
      label,
      field_type: fieldType,
      required: fieldType !== "consent",
      placeholder: "",
      help_text: "",
      width: "full",
      rules: {},
      options: ["select", "radio"].includes(fieldType) ? ["First option"] : [],
    };
    setFields((current) => [...(current ?? []), draft]);
    setSelected(draft.draftId);
    setNotice(null);
  }, []);

  const removeField = useCallback((draftId: string) => {
    setFields((current) => {
      const next = (current ?? []).filter((field) => field.draftId !== draftId);
      setSelected((was) => (was === draftId ? (next[0]?.draftId ?? null) : was));
      return next;
    });
  }, []);

  const move = useCallback((draftId: string, delta: number) => {
    setFields((current) => {
      const list = [...(current ?? [])];
      const index = list.findIndex((field) => field.draftId === draftId);
      const target = index + delta;
      if (index < 0 || target < 0 || target >= list.length) return list;
      const [moved] = list.splice(index, 1);
      list.splice(target, 0, moved);
      return list;
    });
  }, []);

  const save = useCallback(async () => {
    if (!fields) return;
    const refusals = localProblems(fields);
    if (Object.keys(refusals).length > 0) {
      setError(
        refusals.__form ?? "fix the fields marked below before saving — the store would refuse them.",
      );
      return;
    }
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const saved = await saveFormFields(
        formId,
        fields.map((field) => ({
          key: field.key,
          label: field.label,
          field_type: field.field_type,
          required: field.required,
          placeholder: field.placeholder || null,
          help_text: field.help_text || null,
          width: field.width,
          rules: field.rules,
          options: toOptions(field),
        })),
      );
      setDetail(saved);
      const drafts = saved.fields.map(toDraft);
      setFields(drafts);
      setSelected(drafts[0]?.draftId ?? null);
      setNotice("Saved. The form is still a draft until you publish it.");
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  }, [fields, formId]);

  const publish = useCallback(async () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      // Refused locally first, with the store's own sentence: a round trip to be told "this form
      // has no fields" is a worse experience than a disabled button with the reason beside it.
      if (fields && fields.length === 0) {
        throw new ApiError(400, "invalid_form", "this form has no fields yet, so there is nothing to publish");
      }
      const next = detail?.status === "published" ? "draft" : "published";
      const saved = await setFormStatus(formId, next);
      setDetail(saved);
      setNotice(
        next === "published"
          ? "Published. The form now accepts submissions at its public address."
          : "Back to draft. It accepts nothing while it is a draft.",
      );
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  }, [detail?.status, fields, formId]);

  if (error && detail === null && fields === null) {
    return (
      <div className="space-y-3" data-form-builder-state="error">
        <p className="text-[13px] text-red-700 dark:text-red-300">{error}</p>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-[13px]"
        >
          <RefreshCw className="h-4 w-4" aria-hidden />
          Retry
        </button>
      </div>
    );
  }
  if (!detail || fields === null) {
    return (
      <div className="space-y-3" data-form-builder-state="loading" aria-busy="true">
        <div className="h-4 w-48 animate-pulse rounded bg-quiet-soft" />
        <div className="h-64 animate-pulse rounded-lg bg-quiet-soft" />
      </div>
    );
  }

  const live = detail.status === "published";

  return (
    <div className="space-y-5" data-form-builder={detail.key} data-form-status={detail.status}>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <p className="text-[13.5px] font-medium">
            {detail.name}
            <span className="ml-2 font-mono text-[12px] text-muted">/{detail.key}</span>
          </p>
          <p className="text-[12px] text-muted">
            {fields.length} field{fields.length === 1 ? "" : "s"} · changed{" "}
            {formatTimestamp(detail.updated_at)}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            data-form-preview-toggle
            onClick={() => setShowPreview((value) => !value)}
            aria-pressed={showPreview}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <Eye className="h-3.5 w-3.5" aria-hidden />
            {showPreview ? "Hide preview" : "Preview"}
          </button>
          <button
            type="button"
            data-form-settings-toggle
            onClick={() => setShowSettings((value) => !value)}
            aria-pressed={showSettings}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <Settings2 className="h-3.5 w-3.5" aria-hidden />
            Settings
          </button>
          <button
            type="button"
            data-form-save
            disabled={busy}
            onClick={() => void save()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <Save className="h-3.5 w-3.5" aria-hidden />}
            Save
          </button>
          <button
            type="button"
            data-form-publish
            disabled={busy || fields.length === 0}
            title={fields.length === 0 ? "A form with no fields accepts nothing, so there is nothing to publish" : undefined}
            onClick={() => void publish()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            {live ? "Unpublish" : "Publish"}
          </button>
        </div>
      </div>

      {fields.length === 0 ? (
        /* The one refusal the builder makes *before* the round trip, with the reason visible:
           the store refuses a form with no fields, and a disabled Publish with no explanation
           would read as a broken button. */
        <p data-form-publish-refusal className="text-[12.5px] text-amber-700 dark:text-amber-300">
          This form has no fields yet, so there is nothing to publish. Add one from the palette.
        </p>
      ) : null}

      {notice ? (
        <p data-form-notice className="text-[12.5px] text-muted">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p data-form-error className="text-[12.5px] text-red-700 dark:text-red-300">
          {error}
        </p>
      ) : null}

      {showSettings ? <FormSettings detail={detail} onSaved={setDetail} /> : null}

      <div className="grid gap-4 lg:grid-cols-[180px_minmax(0,1fr)_260px]">
        {/* The palette. On mobile it becomes a horizontal strip, because a 180px rail on a
            390px screen is a column of buttons beside nothing. */}
        <aside className="rounded-lg border border-line p-3" data-form-palette>
          <p className="text-[12px] font-medium">Fields</p>
          <div className="mt-2 flex flex-wrap gap-1.5 lg:flex-col">
            {PALETTE.map((entry) => (
              <button
                key={entry.type}
                type="button"
                data-form-add={entry.type}
                onClick={() => addField(entry.type)}
                title={entry.hint}
                className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1.5 text-left text-[12px]"
              >
                <Plus className="h-3 w-3" aria-hidden />
                {entry.label}
              </button>
            ))}
          </div>
        </aside>

        {/* The canvas. */}
        <section className="space-y-2" data-form-canvas>
          {fields.map((field, index) => (
            <div
              key={field.draftId}
              data-form-field={field.key}
              data-form-field-selected={field.draftId === selected}
              className={`rounded-lg border px-3 py-2.5 ${
                field.draftId === selected ? "border-line" : "border-line/60"
              }`}
            >
              <div className="flex flex-wrap items-start justify-between gap-2">
                <button
                  type="button"
                  data-form-field-select={field.key}
                  onClick={() => setSelected(field.draftId)}
                  className="min-w-0 flex-1 text-left"
                >
                  <span className="block text-[13px]">
                    {field.label || "(no label)"}
                    {field.required ? <span aria-hidden> *</span> : null}
                  </span>
                  <span className="block text-[11.5px] text-muted">
                    {PALETTE.find((entry) => entry.type === field.field_type)?.label ?? field.field_type}
                    {" · "}
                    <code className="font-mono">{field.key}</code>
                  </span>
                </button>
                <div className="flex gap-1">
                  <button
                    type="button"
                    data-form-field-up={field.key}
                    aria-label={`Move ${field.label} up`}
                    disabled={index === 0}
                    onClick={() => move(field.draftId, -1)}
                    className="rounded-md border border-line p-1 disabled:opacity-40"
                  >
                    <ArrowUp className="h-3 w-3" aria-hidden />
                  </button>
                  <button
                    type="button"
                    data-form-field-down={field.key}
                    aria-label={`Move ${field.label} down`}
                    disabled={index === fields.length - 1}
                    onClick={() => move(field.draftId, 1)}
                    className="rounded-md border border-line p-1 disabled:opacity-40"
                  >
                    <ArrowDown className="h-3 w-3" aria-hidden />
                  </button>
                  <button
                    type="button"
                    data-form-field-remove={field.key}
                    aria-label={`Remove ${field.label}`}
                    onClick={() => removeField(field.draftId)}
                    className="rounded-md border border-line p-1"
                  >
                    <Trash2 className="h-3 w-3" aria-hidden />
                  </button>
                </div>
              </div>
              {problems[field.draftId] ? (
                <p data-form-field-error={field.key} className="mt-1 text-[11.5px] text-red-700 dark:text-red-300">
                  {problems[field.draftId]}
                </p>
              ) : null}
            </div>
          ))}
        </section>

        {/* The inspector. */}
        <aside className="rounded-lg border border-line p-3" data-form-inspector>
          {active ? (
            <FieldInspector
              field={active}
              problem={problems[active.draftId]}
              onChange={(change: Partial<Draft>) => mutate(active.draftId, change)}
            />
          ) : (
            <p className="text-[12px] text-muted">Pick a field to edit it.</p>
          )}
        </aside>
      </div>

      {showPreview ? (
        <FormPreview detail={detail} fields={fields} />
      ) : null}

      <p className="text-[11.5px] text-muted">
        <button
          type="button"
          onClick={() => router.push("/forms")}
          className="underline underline-offset-2"
        >
          Back to forms
        </button>
      </p>
    </div>
  );
}
