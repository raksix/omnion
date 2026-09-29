"use client";

/**
 * `/security/headers` — the response header policy (REQ-012, slice 2).
 *
 * This is the one screen in the platform whose output does not stay inside it: every line saved
 * here lands on every response, in every browser the platform serves. So the design is built
 * around the one question that matters on such a screen — *is what I am about to save the same
 * as what will actually be sent?* — and six rules answer it:
 *
 * 1. **The preview is the server's rendering, never the form's.** The `rendered` column comes
 *    from `HeaderPolicy::render`, the same function the response middleware applies. While the
 *    form is dirty the preview switches to a **client-side draft rendering that is explicitly
 *    labelled as a draft**, because the stored one is stale the moment a field changes — and a
 *    preview that keeps showing the saved policy next to an edited form is the "the header I
 *    configured is not the header I get" bug in its most convincing form.
 * 2. **An off header is a row with a strike, not a missing row.** `value: null` means
 *    "configured off" and it renders visibly. A header an operator turned off and cannot see is
 *    a header they will not know is off.
 * 3. **`report_only` and `enforce` are a radio pair, never two checkboxes.** They are mutually
 *    exclusive on the wire — report-only sends *no* enforcing header at all — and a form that
 *    offered both as independent switches would let somebody believe a policy is being
 *    reported when it is in fact being enforced, or the reverse.
 * 4. **The server owns validation, and the refusal is shown by name.** The client checks exactly
 *    one thing (an empty directive name, which would otherwise be an un-submittable row) and
 *    lets `HeaderPolicy::new` decide the rest; a second validator that disagreed with it would
 *    be a second place to be wrong.
 * 5. **A `max-age` a browser would ignore is a warning on the field, not a silent save.** Below
 *    15768000s the browser drops the whole header, so 3600 is not "a shorter policy", it is no
 *    policy at all — and an operator deserves to hear that before saving, not from a scan.
 * 6. **Unsaved changes are guarded and visible.** A dirty form says so in the header, and a save
 *    is refused with the server's compare-and-swap message rather than overwriting whatever
 *    landed in the meantime.
 *
 * Keyboard: `Tab` reaches every control in render order, the mode radios are a real radio group
 * so the arrow keys move between them, and `Ctrl/Cmd+S` saves. Mobile: one column, the preview
 * scrolls horizontally **inside its own container** rather than the page — a 400-character CSP on
 * a 360px screen is unreadable either way, but only one of them is navigable.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Link from "next/link";
import { AlertTriangle, Eye, Loader2, Plus, RefreshCw, Save, Trash2 } from "lucide-react";

import {
  fetchHeaderPolicy,
  saveHeaderPolicy,
  type ApiError,
} from "@/lib/api";
import { SecurityTabs } from "@/features/security/security-tabs";
import {
  CSP_DIRECTIVE_NAMES,
  MIN_HSTS_MAX_AGE,
  REFERRER_POLICIES,
  type CspDirective,
  type CspMode,
  type HeaderLine,
  type HeaderPolicyDocument,
} from "@/lib/types";

/** The editable form's state. Kept apart from the server document so "dirty" is computable. */
type Draft = {
  csp_mode: CspMode;
  csp: CspDirective[];
  hsts_max_age_seconds: number | null;
  hsts_include_subdomains: boolean;
  hsts_preload: boolean;
  content_type_options: boolean;
  referrer_policy: string;
  permissions_policy: string[];
};

function draftFrom(document: HeaderPolicyDocument): Draft {
  return {
    csp_mode: document.csp_mode,
    // Cloned, because editing a row must not mutate the object the compare-and-swap key is
    // built from — an in-place edit would make `expected_document` describe the edit.
    csp: document.csp.map((row) => ({ directive: row.directive, values: [...row.values] })),
    hsts_max_age_seconds: document.hsts.max_age_seconds,
    hsts_include_subdomains: document.hsts.include_subdomains,
    hsts_preload: document.hsts.preload,
    content_type_options: document.content_type_options,
    referrer_policy: document.referrer_policy ?? "",
    permissions_policy: [...document.permissions_policy],
  };
}

/** The document as it went over the wire, which is the CAS key a save must present. */
function documentOf(draft: Draft, saved: HeaderPolicyDocument): unknown {
  return {
    csp_mode: draft.csp_mode,
    csp: draft.csp,
    hsts: {
      max_age_seconds: draft.hsts_max_age_seconds,
      include_subdomains: draft.hsts_include_subdomains,
      preload: draft.hsts_preload,
    },
    content_type_options: draft.content_type_options,
    referrer_policy: draft.referrer_policy === "" ? null : draft.referrer_policy,
    permissions_policy: draft.permissions_policy,
  };
  // `saved` is unused on purpose but kept in the signature: the call site passes the document
  // it was opened with, and making that explicit documents where the key comes from.
  void saved;
}

/**
 * The draft's own header lines, for the preview while the form is dirty.
 *
 * Deliberately a **copy** of the server's assembly order and rules, and deliberately the *only*
 * client-side rendering in the app: it is right because it is small, and it is labelled as a
 * draft everywhere it appears, because a second assembly in the client is exactly how the two
 * would drift. `enforce` and `report_only` are mutually exclusive here for the same reason they
 * are on the server.
 */
function renderDraft(draft: Draft): HeaderLine[] {
  const lines: HeaderLine[] = [];
  const csp = draft.csp
    .filter((row) => row.directive.trim() !== "")
    .map((row) =>
      row.values.length === 0
        ? row.directive.trim()
        : `${row.directive.trim()} ${row.values.join(" ")}`,
    )
    .join("; ");

  if (csp) {
    lines.push({
      name:
        draft.csp_mode === "enforce"
          ? "Content-Security-Policy"
          : "Content-Security-Policy-Report-Only",
      value: csp,
    });
  }

  lines.push({
    name: "Strict-Transport-Security",
    value:
      draft.hsts_max_age_seconds === null
        ? null
        : [
            `max-age=${draft.hsts_max_age_seconds}`,
            ...(draft.hsts_include_subdomains ? ["includeSubDomains"] : []),
            ...(draft.hsts_preload ? ["preload"] : []),
          ].join("; "),
  });
  lines.push({
    name: "X-Content-Type-Options",
    value: draft.content_type_options ? "nosniff" : null,
  });
  lines.push({
    name: "Referrer-Policy",
    value: draft.referrer_policy === "" ? null : draft.referrer_policy,
  });

  const permissions = draft.permissions_policy
    .map((entry) => entry.trim())
    .filter((entry) => entry !== "");
  lines.push({
    name: "Permissions-Policy",
    value: permissions.length > 0 ? permissions.join(", ") : null,
  });

  return lines;
}

/** The compare-and-swap key: the JSON text, so field order cannot make a false difference. */
function stableKey(value: unknown): string {
  return JSON.stringify(value, (_key, entry) => {
    if (entry && typeof entry === "object" && !Array.isArray(entry)) {
      return Object.fromEntries(
        Object.entries(entry as Record<string, unknown>).sort(([a], [b]) =>
          a < b ? -1 : a > b ? 1 : 0,
        ),
      );
    }
    return entry;
  });
}

function when(iso: string | null): string {
  if (!iso) return "never";
  const parsed = new Date(iso);
  return Number.isNaN(parsed.getTime()) ? iso : parsed.toLocaleString();
}

/** The one client-side check: a row with no name could never be saved. */
function emptyDirectiveNames(draft: Draft): number[] {
  return draft.csp
    .map((row, index) => (row.directive.trim() === "" ? index : -1))
    .filter((index) => index >= 0);
}

function Preview({ lines, draft }: { lines: HeaderLine[]; draft: boolean }) {
  return (
    <div
      className="rounded border border-line"
      data-header-preview={draft ? "draft" : "saved"}
      aria-label="Rendered response headers"
    >
      <div className="flex flex-wrap items-center gap-2 border-b border-line px-3 py-2">
        <Eye aria-hidden className="h-3.5 w-3.5 text-muted" />
        <h3 className="text-[12px] font-medium text-ink">What a response carries</h3>
        {draft ? (
          <span
            className="rounded-full bg-amber-100 px-2 py-0.5 text-[11px] font-medium text-amber-900 dark:bg-amber-950 dark:text-amber-200"
            data-header-preview-draft
          >
            Draft — not saved yet
          </span>
        ) : null}
      </div>
      {/* The horizontal scroll lives on this container, never the page: a 400-character CSP
          value has to be scrollable somewhere, and if it is the body the whole panel moves
          sideways on a phone. */}
      <div className="max-w-full overflow-x-auto">
        <dl className="min-w-full divide-y divide-line text-[12px]">
          {lines.length === 0 ? (
            <p className="px-3 py-2 text-muted">This policy sends no headers at all.</p>
          ) : (
            lines.map((line) => (
              <div key={line.name} className="flex flex-col gap-0.5 px-3 py-2 sm:flex-row sm:gap-3">
                <dt className="shrink-0 font-mono text-[11px] text-ink sm:w-64">{line.name}</dt>
                <dd
                  className={`min-w-0 whitespace-pre-wrap break-all font-mono text-[11px] ${
                    line.value === null ? "text-muted line-through" : "text-muted"
                  }`}
                  data-header-line={line.name}
                  data-header-off={line.value === null ? "true" : "false"}
                >
                  {line.value === null ? "not sent" : line.value}
                </dd>
              </div>
            ))
          )}
        </dl>
      </div>
    </div>
  );
}

export function HeaderPolicyScreen() {
  const [document, setDocument] = useState<HeaderPolicyDocument | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [baseline, setBaseline] = useState<string>("");
  const [error, setError] = useState<ApiError | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<ApiError | null>(null);
  const [savedAt, setSavedAt] = useState<string | null>(null);
  const permissionsInput = useRef<HTMLTextAreaElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const next = await fetchHeaderPolicy();
      const nextDraft = draftFrom(next);
      setDocument(next);
      setDraft(nextDraft);
      setBaseline(stableKey(documentOf(nextDraft, next)));
    } catch (caught) {
      setError(caught as ApiError);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const dirty = useMemo(
    () => (draft ? stableKey(documentOf(draft, document as HeaderPolicyDocument)) !== baseline : false),
    [draft, document, baseline],
  );

  const blocked = useMemo(() => (draft ? emptyDirectiveNames(draft) : []), [draft]);

  /** `Ctrl/Cmd+S` saves, from anywhere on the form. */
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "s") {
        event.preventDefault();
        if (dirty && draft && !saving) void save();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const save = useCallback(async () => {
    if (!draft) return;
    setSaving(true);
    setSaveError(null);
    try {
      const saved = await saveHeaderPolicy({
        csp_mode: draft.csp_mode,
        csp: draft.csp
          .filter((row) => row.directive.trim() !== "")
          .map((row) => ({
            directive: row.directive.trim().toLowerCase(),
            values: row.values.map((value) => value.trim()).filter((value) => value !== ""),
          })),
        hsts_max_age_seconds: draft.hsts_max_age_seconds,
        hsts_include_subdomains: draft.hsts_include_subdomains,
        hsts_preload: draft.hsts_preload,
        content_type_options: draft.content_type_options,
        referrer_policy: draft.referrer_policy === "" ? null : draft.referrer_policy,
        permissions_policy: draft.permissions_policy
          .map((entry) => entry.trim())
          .filter((entry) => entry !== ""),
        // The document the form was *opened* with, not the one being saved: that is the whole
        // point of the compare-and-swap.
        expected_document: document ? documentOf(draftFrom(document), document) : null,
      });
      const nextDraft = draftFrom(saved);
      setDocument(saved);
      setDraft(nextDraft);
      setBaseline(stableKey(documentOf(nextDraft, saved)));
      setSavedAt(new Date().toISOString());
    } catch (caught) {
      setSaveError(caught as ApiError);
    } finally {
      setSaving(false);
    }
  }, [draft, document]);

  if (loading && !draft) {
    return (
      <p className="flex items-center gap-2 text-[13px] text-muted" data-header-loading>
        <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" />
        Reading the header policy…
      </p>
    );
  }

  if (error && !draft) {
    return (
      <div className="space-y-3" data-header-error>
        <p className="text-[13px] text-red-700 dark:text-red-300">
          The header policy could not be read: {error.message}
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-1.5 rounded border border-line px-3 py-1.5 text-[13px] text-ink hover:bg-surface"
        >
          <RefreshCw aria-hidden className="h-3.5 w-3.5" />
          Try again
        </button>
      </div>
    );
  }

  if (!draft || !document) return null;

  const preview = dirty ? renderDraft(draft) : document.rendered;
  const hstsTooShort =
    draft.hsts_max_age_seconds !== null && draft.hsts_max_age_seconds < MIN_HSTS_MAX_AGE;

  const setRow = (index: number, patch: Partial<CspDirective>) => {
    setDraft((current) => {
      if (!current) return current;
      return {
        ...current,
        csp: current.csp.map((row, at) => (at === index ? { ...row, ...patch } : row)),
      };
    });
  };

  return (
    <div className="space-y-6" data-header-policy>
      <SecurityTabs current="headers" />
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div className="text-[12px] text-muted">
          {document.saved ? (
            <p data-header-saved>
              Saved {when(document.updated_at)}.
              {savedAt ? ` Your change was written ${when(savedAt)}.` : null}
            </p>
          ) : (
            <p data-header-unsaved>
              Nobody has saved a policy yet. What you see is the platform&apos;s baseline — it is
              already being sent, and saving writes the same thing explicitly.
            </p>
          )}
          {dirty ? (
            <p className="mt-1 font-medium text-amber-700 dark:text-amber-300" data-header-dirty>
              Unsaved changes. The preview shows the draft; the response still carries the saved
              policy.
            </p>
          ) : null}
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {dirty ? (
            <button
              type="button"
              onClick={() => {
                const reverted = draftFrom(document);
                setDraft(reverted);
                setSaveError(null);
              }}
              className="rounded border border-line px-3 py-1.5 text-[13px] text-ink hover:bg-surface"
              data-header-revert
            >
              Discard
            </button>
          ) : null}
          <button
            type="button"
            onClick={() => void save()}
            disabled={!dirty || saving || blocked.length > 0}
            className="inline-flex items-center gap-1.5 rounded bg-ink px-3 py-1.5 text-[13px] text-canvas disabled:opacity-50"
            data-header-save
            title="Ctrl/Cmd+S"
          >
            {saving ? (
              <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <Save aria-hidden className="h-3.5 w-3.5" />
            )}
            {saving ? "Saving…" : "Save policy"}
          </button>
        </div>
      </header>

      {saveError ? (
        <div
          className="flex items-start gap-2 rounded border border-red-300 bg-red-50 p-3 text-[13px] text-red-900 dark:border-red-900 dark:bg-red-950 dark:text-red-200"
          data-header-save-error
          role="alert"
        >
          <AlertTriangle aria-hidden className="mt-0.5 h-4 w-4 shrink-0" />
          <div>
            <p className="font-medium">The policy was not saved: {saveError.message}</p>
            {saveError.code === "security_settings_conflict" ? (
              <p className="mt-1">
                Somebody saved this policy while the form was open. Reload to see their version
                before saving yours.
              </p>
            ) : null}
          </div>
        </div>
      ) : null}

      <div className="grid gap-6 lg:grid-cols-[1fr_minmax(0,26rem)]">
        {/* ---------------------------------------------------------------- the form --------- */}
        <div className="space-y-6">
          <fieldset>
            <legend className="text-[13px] font-medium text-ink">Content Security Policy mode</legend>
            <p className="mt-1 text-[12px] text-muted">
              Report-only records violations and blocks nothing. Enforce actually blocks. They
              are exclusive: report-only sends no enforcing header at all.
            </p>
            <div className="mt-2 flex flex-wrap gap-4">
              {(
                [
                  ["report_only", "Report only"],
                  ["enforce", "Enforce"],
                ] as const
              ).map(([value, label]) => (
                <label key={value} className="flex items-center gap-2 text-[13px] text-ink">
                  <input
                    type="radio"
                    name="csp-mode"
                    value={value}
                    checked={draft.csp_mode === value}
                    onChange={() => setDraft({ ...draft, csp_mode: value })}
                    className="h-3.5 w-3.5"
                    data-header-mode={value}
                  />
                  {label}
                </label>
              ))}
            </div>
          </fieldset>

          <section aria-label="Content Security Policy directives">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <h3 className="text-[13px] font-medium text-ink">Directives</h3>
              <button
                type="button"
                onClick={() =>
                  setDraft({
                    ...draft,
                    csp: [...draft.csp, { directive: "", values: [] }],
                  })
                }
                className="inline-flex items-center gap-1.5 rounded border border-line px-2 py-1 text-[12px] text-ink hover:bg-surface"
                data-header-add-directive
              >
                <Plus aria-hidden className="h-3.5 w-3.5" />
                Add directive
              </button>
            </div>

            {blocked.length > 0 ? (
              <p className="mt-2 text-[12px] text-red-700 dark:text-red-300" data-header-empty-name>
                A directive with no name cannot be saved. Fill it in or remove the row.
              </p>
            ) : null}

            <ul className="mt-2 space-y-2">
              {draft.csp.map((row, index) => (
                <li
                  key={index}
                  className="grid gap-2 rounded border border-line p-2 sm:grid-cols-[minmax(0,14rem)_1fr_auto] sm:items-start"
                >
                  <label className="block">
                    <span className="sr-only">Directive {index + 1} name</span>
                    <input
                      list="csp-directive-names"
                      value={row.directive}
                      onChange={(event) => setRow(index, { directive: event.target.value })}
                      placeholder="default-src"
                      aria-label={`Directive ${index + 1} name`}
                      className="w-full rounded border border-line bg-canvas px-2 py-1 font-mono text-[12px] text-ink"
                      data-header-directive-name={index}
                    />
                  </label>
                  <label className="block">
                    <span className="sr-only">Directive {index + 1} sources</span>
                    <input
                      value={row.values.join(" ")}
                      onChange={(event) =>
                        setRow(index, {
                          values: event.target.value.split(/\s+/).filter((value) => value !== ""),
                        })
                      }
                      placeholder="'self' https://cdn.example.com"
                      aria-label={`Directive ${index + 1} sources, space separated`}
                      className="w-full rounded border border-line bg-canvas px-2 py-1 font-mono text-[12px] text-ink"
                      data-header-directive-values={index}
                    />
                  </label>
                  <button
                    type="button"
                    onClick={() =>
                      setDraft({
                        ...draft,
                        csp: draft.csp.filter((_, at) => at !== index),
                      })
                    }
                    aria-label={`Remove directive ${row.directive || index + 1}`}
                    className="inline-flex h-7 w-7 shrink-0 items-center justify-center rounded border border-line text-muted hover:text-red-700 dark:hover:text-red-300"
                    data-header-remove-directive={index}
                  >
                    <Trash2 aria-hidden className="h-3.5 w-3.5" />
                  </button>
                </li>
              ))}
            </ul>
            <datalist id="csp-directive-names">
              {CSP_DIRECTIVE_NAMES.map((name) => (
                <option key={name} value={name} />
              ))}
            </datalist>
          </section>

          <section aria-label="Strict Transport Security" className="space-y-2">
            <h3 className="text-[13px] font-medium text-ink">Strict Transport Security</h3>
            <label className="flex flex-wrap items-center gap-2 text-[13px] text-ink">
              <input
                type="checkbox"
                checked={draft.hsts_max_age_seconds !== null}
                onChange={(event) =>
                  setDraft({
                    ...draft,
                    hsts_max_age_seconds: event.target.checked ? 31_536_000 : null,
                  })
                }
                className="h-3.5 w-3.5"
                data-header-hsts-enabled
              />
              Send Strict-Transport-Security
            </label>
            {draft.hsts_max_age_seconds !== null ? (
              <div className="flex flex-wrap items-center gap-4">
                <label className="text-[13px] text-ink">
                  max-age (seconds)
                  <input
                    type="number"
                    min={0}
                    value={draft.hsts_max_age_seconds}
                    onChange={(event) =>
                      setDraft({
                        ...draft,
                        hsts_max_age_seconds: Number(event.target.value) || 0,
                      })
                    }
                    className="ml-2 w-44 rounded border border-line bg-canvas px-2 py-1 font-mono text-[12px] text-ink"
                    data-header-hsts-max-age
                  />
                </label>
                <label className="flex items-center gap-2 text-[13px] text-ink">
                  <input
                    type="checkbox"
                    checked={draft.hsts_include_subdomains}
                    onChange={(event) =>
                      setDraft({ ...draft, hsts_include_subdomains: event.target.checked })
                    }
                    className="h-3.5 w-3.5"
                    data-header-hsts-subdomains
                  />
                  includeSubDomains
                </label>
                <label className="flex items-center gap-2 text-[13px] text-ink">
                  <input
                    type="checkbox"
                    checked={draft.hsts_preload}
                    onChange={(event) => setDraft({ ...draft, hsts_preload: event.target.checked })}
                    className="h-3.5 w-3.5"
                    data-header-hsts-preload
                  />
                  preload
                </label>
              </div>
            ) : null}
            {hstsTooShort ? (
              <p
                className="flex items-start gap-2 text-[12px] text-amber-700 dark:text-amber-300"
                data-header-hsts-warning
              >
                <AlertTriangle aria-hidden className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                Browsers ignore the whole header below {MIN_HSTS_MAX_AGE.toLocaleString()} seconds.
                Saving this still works, but it sends no protection.
              </p>
            ) : null}
          </section>

          <section aria-label="Other headers" className="space-y-3">
            <h3 className="text-[13px] font-medium text-ink">Other headers</h3>
            <label className="flex items-center gap-2 text-[13px] text-ink">
              <input
                type="checkbox"
                checked={draft.content_type_options}
                onChange={(event) =>
                  setDraft({ ...draft, content_type_options: event.target.checked })
                }
                className="h-3.5 w-3.5"
                data-header-nosniff
              />
              Send X-Content-Type-Options: nosniff
            </label>
            <label className="block text-[13px] text-ink">
              Referrer-Policy
              <select
                value={draft.referrer_policy}
                onChange={(event) => setDraft({ ...draft, referrer_policy: event.target.value })}
                className="ml-2 rounded border border-line bg-canvas px-2 py-1 text-[12px] text-ink"
                data-header-referrer
              >
                <option value="">send no header</option>
                {REFERRER_POLICIES.map((policy) => (
                  <option key={policy} value={policy}>
                    {policy}
                  </option>
                ))}
              </select>
            </label>
            <label className="block text-[13px] text-ink">
              Permissions-Policy (one entry per line)
              <textarea
                ref={permissionsInput}
                value={draft.permissions_policy.join("\n")}
                onChange={(event) =>
                  setDraft({ ...draft, permissions_policy: event.target.value.split("\n") })
                }
                rows={3}
                placeholder="camera=()"
                className="mt-1 w-full rounded border border-line bg-canvas px-2 py-1 font-mono text-[12px] text-ink"
                data-header-permissions
              />
            </label>
          </section>
        </div>

        {/* -------------------------------------------------------------- the preview -------- */}
        <div className="space-y-3 lg:sticky lg:top-4 lg:self-start">
          <Preview lines={preview} draft={dirty} />
          <p className="text-[12px] text-muted">
            A header struck through is configured off and is not sent. The saved policy applies to
            the next response, not at the next restart.
          </p>
          <Link
            href="/security"
            className="inline-block text-[12px] text-ink underline underline-offset-2"
          >
            Back to the posture overview
          </Link>
        </div>
      </div>
    </div>
  );
}
