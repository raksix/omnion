"use client";

/**
 * `/developer/graphql/settings` — the endpoint's limits (REQ-130, slice 2).
 *
 * ## Every number on this screen is READ by the endpoint
 *
 * This screen exists because of a defect the walk found last tick: `run_pre_execution` built its
 * limits from `Settings::default()`, so an operator who turned `persisted_only` on, or lowered
 * `max_depth`, watched the endpoint keep enforcing the shipped values while this page reported the
 * change as saved. One line wearing the costume of a default, and the fourth "documented but
 * unreachable" shape this request has produced.
 *
 * So the screen carries a **verdict per field** rather than a bare form: each number says whether
 * it is within the range the endpoint accepts, and the save is refused with the endpoint's own
 * field-level message rather than a generic "invalid settings". A settings form that accepts six
 * different bad numbers and says one sentence about all of them cannot be used.
 *
 * ## The playground toggle is a real gate, not a label
 *
 * `playground_enabled` is the switch that says the explorer is available to authenticated admins.
 * Turning it off does not disable the endpoint — a registered client still executes — so the
 * screen says exactly that in the field's own help text rather than implying the surface goes dark.
 *
 * Keyboard: `s` saves, `r` reloads, `Esc` abandons unsaved edits by reloading.
 */

import { useCallback, useEffect, useState } from "react";

import { AlertTriangle, Check, RefreshCw, Save } from "lucide-react";

import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchGraphqlSettings,
  saveGraphqlSettings,
  type GraphqlSettings,
} from "@/lib/graphql-api";

/** One field, with the range the endpoint accepts and why that number. */
type Field = {
  key: keyof Omit<GraphqlSettings, "persisted_only" | "playground_enabled">;
  label: string;
  detail: string;
  min: number;
  max: number;
  step: number;
};

const FIELDS: Field[] = [
  {
    key: "max_depth",
    label: "Maximum depth",
    detail: "Selection-set nesting. Refused with DEPTH_LIMIT before any resolver runs.",
    min: 1,
    max: 64,
    step: 1,
  },
  {
    key: "cost_budget",
    label: "Cost budget",
    detail: "Priced units one operation may cost. Refused with COST_LIMIT, naming the top contributors.",
    min: 1,
    max: 100_000,
    step: 10,
  },
  {
    key: "max_aliases",
    label: "Maximum aliases",
    detail: "Distinct selections in one operation. Alias spam is the cheapest way to multiply work.",
    min: 1,
    max: 200,
    step: 1,
  },
  {
    key: "max_fragments",
    label: "Maximum fragments",
    detail: "Fragment definitions plus spreads. A cyclic fragment is refused rather than expanded.",
    min: 1,
    max: 200,
    step: 1,
  },
  {
    key: "max_page_size",
    label: "Maximum page size",
    detail: "The largest `first`/`limit` any single argument may ask for. Refused with PAGE_SIZE_LIMIT.",
    min: 1,
    max: 1000,
    step: 10,
  },
  {
    key: "timeout_ms",
    label: "Request timeout (ms)",
    detail: "Wall-clock budget for one execution.",
    min: 100,
    max: 120_000,
    step: 500,
  },
];

function numberOf(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : Number.NaN;
}

/** The out-of-range fields, by their own message. Empty means the form may be sent. */
function offenders(draft: GraphqlSettings): Field[] {
  return FIELDS.filter((field) => {
    const value = numberOf(draft[field.key]);
    return !Number.isFinite(value) || value < field.min || value > field.max;
  });
}

export function SettingsView() {
  const [draft, setDraft] = useState<GraphqlSettings | null>(null);
  const [stored, setStored] = useState<GraphqlSettings | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const load = useCallback(async () => {
    setError(null);
    try {
      const settings = await fetchGraphqlSettings();
      setStored(settings);
      setDraft(settings);
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The settings could not be read.");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const save = useCallback(async () => {
    if (!draft) return;
    const bad = offenders(draft);
    if (bad.length > 0) {
      setNotice(
        `${bad.length} field${bad.length === 1 ? " is" : "s are"} out of range: ${bad
          .map((field) => `${field.label} (${field.min}–${field.max})`)
          .join(", ")}. Nothing was sent.`,
      );
      return;
    }
    setSaving(true);
    setNotice(null);
    try {
      const saved = await saveGraphqlSettings(draft);
      setStored(saved);
      setDraft(saved);
      setNotice(
        saved.persisted_only !== stored?.persisted_only
          ? `Saved. Persisted-only mode is now ${saved.persisted_only ? "on" : "off"} — the next ad-hoc document is ${saved.persisted_only ? "refused" : "accepted"}.`
          : "Saved. The endpoint reads these numbers on the next request.",
      );
    } catch (caught) {
      setNotice(
        caught instanceof ApiError ? caught.message : "The settings could not be saved.",
      );
    } finally {
      setSaving(false);
    }
  }, [draft, stored]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" || target?.tagName === "TEXTAREA" || target?.tagName === "SELECT";
      if (typing) return;
      if (event.key === "s") {
        event.preventDefault();
        void save();
      } else if (event.key === "r" || event.key === "Escape") {
        event.preventDefault();
        void load();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [save, load]);

  if (loading || !draft) return <LoadingTable columns={2} rows={4} />;

  const bad = offenders(draft);
  const changed = stored !== null && JSON.stringify(stored) !== JSON.stringify(draft);

  return (
    <div className="flex flex-col gap-5" data-view="graphql-settings">
      {error ? (
        <div
          role="alert"
          className="flex items-start gap-2 rounded-md border border-red-300 bg-red-50 px-3 py-2.5 text-[13px] text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200"
        >
          <AlertTriangle size={16} className="mt-0.5 shrink-0" aria-hidden />
          <span>
            {error}{" "}
            <button type="button" onClick={() => void load()} className="inline-flex underline">
              Try again
            </button>
          </span>
        </div>
      ) : null}

      {notice ? (
        <p role="status" className="rounded-md border border-line bg-surface px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}

      <form
        onSubmit={(event) => {
          event.preventDefault();
          void save();
        }}
        className="flex flex-col gap-5"
      >
        <fieldset className="flex flex-col gap-3">
          <legend className="text-[13px] font-medium">Limits</legend>
          <div className="grid gap-3 sm:grid-cols-2">
            {FIELDS.map((field) => {
              const value = numberOf(draft[field.key]);
              const invalid = !Number.isFinite(value) || value < field.min || value > field.max;
              return (
                <label key={field.key} className="block">
                  <span className="text-[12px] font-medium">{field.label}</span>
                  <input
                    data-graphql-setting={field.key}
                    type="number"
                    inputMode="numeric"
                    min={field.min}
                    max={field.max}
                    step={field.step}
                    value={Number.isFinite(value) ? value : ""}
                    onChange={(event) =>
                      setDraft({
                        ...draft,
                        [field.key]: event.target.value === "" ? Number.NaN : Number(event.target.value),
                      })
                    }
                    aria-describedby={`${field.key}-help`}
                    aria-invalid={invalid}
                    className={`mt-1 w-full rounded-md border bg-background px-3 py-2 text-[13px] outline-none focus:border-accent ${
                      invalid ? "border-red-400 dark:border-red-700" : "border-line"
                    }`}
                  />
                  <span
                    id={`${field.key}-help`}
                    className={`mt-1 block text-[11.5px] ${invalid ? "text-red-700 dark:text-red-300" : "text-muted"}`}
                  >
                    {invalid
                      ? `Out of range — the endpoint accepts ${field.min} to ${field.max}.`
                      : field.detail}
                  </span>
                </label>
              );
            })}
          </div>
        </fieldset>

        <fieldset className="flex flex-col gap-3">
          <legend className="text-[13px] font-medium">Policy</legend>

          <label className="flex items-start gap-2.5">
            <input
              type="checkbox"
              checked={draft.persisted_only}
              onChange={(event) => setDraft({ ...draft, persisted_only: event.target.checked })}
              className="mt-0.5"
            />
            <span className="text-[12.5px]">
              <span className="font-medium">Persisted-only mode</span>
              <span className="mt-0.5 block text-[11.5px] text-muted">
                Refuses any document sent as text, with{" "}
                <code className="font-mono">PERSISTED_QUERY_NOT_FOUND</code>, before it is parsed
                or priced. A registered document still executes by id or hash — that is the point of
                the flag, so a leaked read scope cannot run arbitrary queries.
              </span>
            </span>
          </label>

          <label className="flex items-start gap-2.5">
            <input
              type="checkbox"
              checked={draft.playground_enabled}
              onChange={(event) =>
                setDraft({ ...draft, playground_enabled: event.target.checked })
              }
              className="mt-0.5"
            />
            <span className="text-[12.5px]">
              <span className="font-medium">Playground available to administrators</span>
              <span className="mt-0.5 block text-[11.5px] text-muted">
                Hides the explorer screen from the panel navigation. It does not disable the
                endpoint: a registered client keeps executing either way, and the toggle governs
                who gets a console.
              </span>
            </span>
          </label>
        </fieldset>

        <div className="flex flex-wrap items-center gap-2">
          <button
            type="submit"
            disabled={saving || bad.length > 0}
            className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3 py-2 text-[13px] text-white disabled:opacity-60"
          >
            {saving ? <Check size={13} aria-hidden /> : <Save size={13} aria-hidden />}
            {saving ? "Saving…" : "Save"} <kbd className="text-[10.5px]">s</kbd>
          </button>
          <button
            type="button"
            onClick={() => void load()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-2 text-[13px]"
          >
            <RefreshCw size={13} aria-hidden /> Reload <kbd className="text-[10.5px]">r</kbd>
          </button>
          {changed ? (
            <span className="text-[12px] text-muted">
              Unsaved changes — reloading discards them.
            </span>
          ) : null}
        </div>
      </form>
    </div>
  );
}