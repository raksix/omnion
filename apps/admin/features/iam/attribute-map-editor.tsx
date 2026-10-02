"use client";

/**
 * The attribute map editor (REQ-065, slice 2) — the wizard's third step.
 *
 * *Which claim is the email* is the question every enterprise sign-in setup answers once and then
 * forgets, and it is the question whose wrong answer is invisible until somebody cannot sign in.
 * So this is a real editor: rows, drag order, six transforms, a required flag, and a **preview
 * against a pasted payload** that runs the same code the sign-in callback runs.
 *
 * Three decisions the screen makes on purpose:
 *
 * * **The pickers come from the server.** `target_fields` and `transforms` arrive with the map,
 *   so an option the platform does not accept is never offered in the first place. A hard-coded
 *   list here would be a second source of truth that goes stale quietly.
 * * **Save is one PUT of every row.** The server replaces the map in a transaction, so a
 *   half-written map — the state where an email row is missing and real sign-ins are refused for
 *   a reason nobody can see — cannot exist even for a moment.
 * * **The preview refuses the same way a sign-in refuses.** When a required field is missing the
 *   preview says so by name and produces no values, rather than showing a cheerful partial table
 *   that would go on to create a half account.
 */

import { useCallback, useEffect, useMemo, useState } from "react";

import {
  AlertTriangle,
  CheckCircle2,
  GripVertical,
  Loader2,
  PlayCircle,
  Plus,
  Save,
  Trash2,
  XCircle,
} from "lucide-react";

import {
  ApiError,
  fetchIamAttributeMap,
  previewIamAttributeMap,
  saveIamAttributeMap,
  type IamAttributeMapping,
  type IamAttributePreview,
  type IamTransform,
} from "@/lib/api";

/** Panel-field labels, because `employment_type` is not a label. */
const FIELD_LABELS: Record<string, string> = {
  email: "Email address",
  username: "Username",
  display_name: "Display name",
  phone: "Phone",
  department: "Department",
  title: "Job title",
  employment_type: "Employment type",
  employee_id: "Employee id",
};

/** A blank row. The email row starts `required` because the server insists and the form should
 * not make an operator discover that by being refused. */
function blankRow(position: number, targetField = "email"): IamAttributeMapping {
  return {
    source_attr: "",
    target_field: targetField,
    transform: "none",
    needs_argument: false,
    transform_arg: null,
    required: targetField === "email",
    position,
  };
}

export function AttributeMapEditor({ providerId }: { providerId: string }) {
  const [rows, setRows] = useState<IamAttributeMapping[]>([]);
  const [fields, setFields] = useState<string[]>([]);
  const [transforms, setTransforms] = useState<IamTransform[]>([]);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [savedAt, setSavedAt] = useState<string | null>(null);

  const [sample, setSample] = useState("");
  const [preview, setPreview] = useState<IamAttributePreview | null>(null);
  const [previewing, setPreviewing] = useState(false);
  const [previewError, setPreviewError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const map = await fetchIamAttributeMap(providerId);
      setRows(map.mappings);
      setFields(map.target_fields);
      setTransforms(map.transforms);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "the attribute map could not be read");
    } finally {
      setLoading(false);
    }
  }, [providerId]);

  useEffect(() => {
    void load();
  }, [load]);

  // The rows are renumbered on every change rather than on save: the position is what the
  // preview and the audit read, and a stale index that only gets fixed on save is a preview of
  // a map that does not exist yet.
  const commit = useCallback((next: IamAttributeMapping[]) => {
    setSavedAt(null);
    setRows(next.map((row, index) => ({ ...row, position: index })));
  }, []);

  const update = useCallback(
    (index: number, patch: Partial<IamAttributeMapping>) => {
      commit(rows.map((row, at) => (at === index ? { ...row, ...patch } : row)));
    },
    [rows, commit],
  );

  const move = useCallback(
    (index: number, to: number) => {
      if (to < 0 || to >= rows.length) return;
      const next = [...rows];
      const [row] = next.splice(index, 1);
      next.splice(to, 0, row);
      commit(next);
    },
    [rows, commit],
  );

  // Keyboard reordering. The grab handle is a button, so the same move is reachable without a
  // pointer — and the answer a screen gives to "reorder without a mouse" is otherwise nothing.
  const onRowKey = (event: React.KeyboardEvent, index: number) => {
    if (event.key === "ArrowUp" && (event.altKey || event.metaKey)) {
      event.preventDefault();
      move(index, index - 1);
    }
    if (event.key === "ArrowDown" && (event.altKey || event.metaKey)) {
      event.preventDefault();
      move(index, index + 1);
    }
  };

  const save = useCallback(async () => {
    setSaving(true);
    setError(null);
    try {
      const map = await saveIamAttributeMap(providerId, rows);
      setRows(map.mappings);
      setFields(map.target_fields);
      setTransforms(map.transforms);
      setSavedAt(new Date().toLocaleTimeString());
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "the map could not be saved");
    } finally {
      setSaving(false);
    }
  }, [providerId, rows]);

  const runPreview = useCallback(async () => {
    setPreviewing(true);
    setPreviewError(null);
    setPreview(null);
    let parsed: unknown;
    try {
      parsed = JSON.parse(sample);
    } catch {
      setPreviewError("the sample must be valid JSON — paste the claims object your provider sends");
      setPreviewing(false);
      return;
    }
    try {
      setPreview(await previewIamAttributeMap(providerId, parsed));
    } catch (cause) {
      setPreviewError(
        cause instanceof ApiError ? cause.message : "the sample could not be read",
      );
    } finally {
      setPreviewing(false);
    }
  }, [providerId, sample]);

  // The fields still unmapped, so "add row" offers what is left rather than what already exists.
  const available = useMemo(
    () => fields.filter((field) => !rows.some((row) => row.target_field === field)),
    [fields, rows],
  );

  const needsArgument = (row: IamAttributeMapping) =>
    transforms.find((item) => item.name === row.transform)?.needs_argument ?? row.needs_argument;

  if (loading) {
    return (
      <div data-attribute-map-loading className="flex items-center gap-2 text-[12px] text-muted">
        <Loader2 className="size-3.5 animate-spin" aria-hidden />
        reading the attribute map…
      </div>
    );
  }

  return (
    <section data-attribute-map className="flex flex-col gap-4">
      <header className="flex flex-wrap items-start justify-between gap-2">
        <div>
          <h3 className="text-[13px] font-semibold">Attribute mapping</h3>
          <p className="text-[12px] text-muted">
            Which claim from the provider fills which panel field. The email is required — without
            one, a sign-in cannot create an account.
          </p>
        </div>
        <div className="flex items-center gap-2">
          {savedAt ? (
            <span data-attribute-map-saved className="text-[11.5px] text-emerald-700">
              saved at {savedAt}
            </span>
          ) : null}
          <button
            type="button"
            data-attribute-map-add
            disabled={available.length === 0}
            onClick={() => commit([...rows, blankRow(rows.length, available[0] ?? "username")])}
            className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px] text-ink disabled:opacity-40"
          >
            <Plus className="size-3.5" aria-hidden />
            Add row
          </button>
          <button
            type="button"
            data-attribute-map-save
            disabled={saving || rows.length === 0}
            onClick={() => void save()}
            className="flex h-8 items-center gap-1.5 rounded-lg border border-accent bg-accent px-2.5 text-[12px] text-white disabled:opacity-40"
          >
            {saving ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <Save className="size-3.5" aria-hidden />
            )}
            Save map
          </button>
        </div>
      </header>

      {error ? (
        <p
          data-attribute-map-error
          role="alert"
          className="flex items-start gap-1.5 rounded-lg border border-danger/50 bg-danger/10 px-2.5 py-2 text-[12px] text-caution"
        >
          <XCircle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          {error}
        </p>
      ) : null}

      {rows.length === 0 ? (
        <p
          data-attribute-map-empty
          className="rounded-lg border border-dashed border-line px-3 py-6 text-center text-[12px] text-muted"
        >
          No attributes are mapped yet. Sign-ins can connect, but an account cannot be created
          without a mapped email — add the row and map the claim your provider sends it in.
        </p>
      ) : (
        <ul className="flex flex-col gap-2">
          {rows.map((row, index) => (
            <li
              key={`${row.target_field}-${index}`}
              data-attribute-row={row.target_field}
              onKeyDown={(event) => onRowKey(event, index)}
              className="flex flex-col gap-2 rounded-lg border border-line bg-panel px-2.5 py-2 lg:flex-row lg:items-end"
            >
              <span className="flex items-center gap-1 lg:w-14">
                <button
                  type="button"
                  data-attribute-move-up={index}
                  disabled={index === 0}
                  aria-label={`Move ${FIELD_LABELS[row.target_field] ?? row.target_field} up`}
                  onClick={() => move(index, index - 1)}
                  className="flex h-8 w-6 items-center justify-center rounded text-muted disabled:opacity-30"
                >
                  <GripVertical className="size-3.5" aria-hidden />
                </button>
                <span className="text-[11px] text-muted">{index + 1}</span>
              </span>

              <label className="flex flex-1 flex-col gap-1 text-[11.5px]">
                <span className="text-muted">Claim / attribute</span>
                <input
                  data-attribute-source={index}
                  value={row.source_attr}
                  placeholder="mail"
                  onChange={(event) => update(index, { source_attr: event.target.value })}
                  className="h-8 rounded-lg border border-line bg-surface px-2 font-mono text-[12px] text-ink"
                />
              </label>

              <label className="flex flex-1 flex-col gap-1 text-[11.5px]">
                <span className="text-muted">Panel field</span>
                <select
                  data-attribute-target={index}
                  value={row.target_field}
                  onChange={(event) => {
                    const target = event.target.value;
                    update(index, {
                      target_field: target,
                      // The email is never optional, so choosing it re-arms the required flag
                      // rather than letting the form save a row the server will refuse.
                      required: target === "email" ? true : row.required,
                    });
                  }}
                  className="h-8 rounded-lg border border-line bg-surface px-2 text-[12px] text-ink"
                >
                  {fields.map((field) => (
                    <option key={field} value={field}>
                      {FIELD_LABELS[field] ?? field}
                    </option>
                  ))}
                </select>
              </label>

              <label className="flex flex-1 flex-col gap-1 text-[11.5px]">
                <span className="text-muted">Transform</span>
                <select
                  data-attribute-transform={index}
                  value={row.transform}
                  onChange={(event) => update(index, { transform: event.target.value })}
                  className="h-8 rounded-lg border border-line bg-surface px-2 text-[12px] text-ink"
                >
                  {transforms.map((item) => (
                    <option key={item.name} value={item.name} title={item.hint}>
                      {item.name}
                    </option>
                  ))}
                </select>
              </label>

              {needsArgument(row) ? (
                <label className="flex flex-1 flex-col gap-1 text-[11.5px]">
                  <span className="text-muted">Argument</span>
                  <input
                    data-attribute-arg={index}
                    value={row.transform_arg ?? ""}
                    placeholder={row.transform === "split" ? "," : "value"}
                    onChange={(event) =>
                      update(index, { transform_arg: event.target.value || null })
                    }
                    className="h-8 rounded-lg border border-line bg-surface px-2 font-mono text-[12px] text-ink"
                  />
                </label>
              ) : null}

              <label className="flex items-center gap-1.5 pb-2 text-[11.5px] text-muted lg:pb-0">
                <input
                  type="checkbox"
                  data-attribute-required={index}
                  checked={row.required}
                  disabled={row.target_field === "email"}
                  onChange={(event) => update(index, { required: event.target.checked })}
                  className="size-3.5"
                />
                Required
              </label>

              <button
                type="button"
                data-attribute-remove={index}
                aria-label={`Remove the ${FIELD_LABELS[row.target_field] ?? row.target_field} row`}
                onClick={() => commit(rows.filter((_, at) => at !== index))}
                className="flex h-8 w-8 items-center justify-center rounded-lg border border-line text-ink lg:mb-0"
              >
                <Trash2 className="size-3.5" aria-hidden />
              </button>
            </li>
          ))}
        </ul>
      )}

      {/* The preview. It is the same projection the callback runs, which is the only reason to
          trust it — a display-only "what would happen" is a second implementation of the mapping
          rules and it is guaranteed to agree with reality right up until the day it does not. */}
      <div className="flex flex-col gap-2 rounded-lg border border-line px-2.5 py-2">
        <label className="flex flex-col gap-1 text-[11.5px]">
          <span className="font-medium">Preview against a sample payload</span>
          <textarea
            data-attribute-sample
            value={sample}
            rows={4}
            spellCheck={false}
            placeholder={'{\n  "sub": "2481",\n  "mail": "ferkan@example.com"\n}'}
            onChange={(event) => {
              setSample(event.target.value);
              setPreview(null);
              setPreviewError(null);
            }}
            className="rounded-lg border border-line bg-surface px-2 py-1.5 font-mono text-[11.5px] text-ink"
          />
        </label>
        <div className="flex items-center gap-2">
          <button
            type="button"
            data-attribute-preview
            disabled={previewing || sample.trim().length === 0}
            onClick={() => void runPreview()}
            className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px] text-ink disabled:opacity-40"
          >
            {previewing ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <PlayCircle className="size-3.5" aria-hidden />
            )}
            Preview
          </button>
          <span className="text-[11.5px] text-muted">
            Runs the real mapping. Nothing is written and no account is created.
          </span>
        </div>

        {previewError ? (
          <p
            data-attribute-preview-error
            role="alert"
            className="flex items-start gap-1.5 text-[12px] text-caution"
          >
            <XCircle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
            {previewError}
          </p>
        ) : null}

        {preview ? (
          <div data-attribute-preview-result={preview.ok ? "ok" : "refused"} className="flex flex-col gap-2">
            <p
              className={`flex items-start gap-1.5 text-[12px] ${
                preview.ok ? "text-emerald-700" : "text-caution"
              }`}
            >
              {preview.ok ? (
                <CheckCircle2 className="mt-0.5 size-3.5 shrink-0" aria-hidden />
              ) : (
                <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
              )}
              {preview.ok
                ? `A sign-in with this payload would create or match an account with ${preview.values.length} mapped field(s).`
                : `A sign-in with this payload would be refused: ${preview.missing.join(", ")} is missing. The field is named so the mapping can be fixed.`}
            </p>

            <div className="overflow-x-auto">
              <table className="w-full min-w-[420px] text-left text-[12px]">
                <thead>
                  <tr className="border-b border-line text-[11px] text-muted">
                    <th className="py-1 pr-2 font-medium">Source</th>
                    <th className="py-1 pr-2 font-medium">Field</th>
                    <th className="py-1 pr-2 font-medium">Raw</th>
                    <th className="py-1 font-medium">Result</th>
                  </tr>
                </thead>
                <tbody>
                  {preview.rows.map((row) => (
                    <tr
                      key={row.target_field}
                      data-attribute-preview-row={row.target_field}
                      className="border-b border-line/50"
                    >
                      <td className="py-1 pr-2 font-mono text-[11.5px]">{row.source_attr}</td>
                      <td className="py-1 pr-2">{FIELD_LABELS[row.target_field] ?? row.target_field}</td>
                      <td className="py-1 pr-2 font-mono text-[11.5px] text-muted">
                        {row.raw ?? "—"}
                      </td>
                      <td
                        className={`py-1 font-mono text-[11.5px] ${row.value ? "text-ink" : "text-caution"}`}
                      >
                        {row.value ?? "missing"}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>

            {preview.unused.length > 0 ? (
              <p data-attribute-unused className="text-[11.5px] text-muted">
                Not in this sample: {preview.unused.join(", ")}. Usually a claim this provider only
                sometimes sends, or a typo in the attribute name.
              </p>
            ) : null}
          </div>
        ) : null}
      </div>
    </section>
  );
}
