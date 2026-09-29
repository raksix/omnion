"use client";

/**
 * `/security/rate-limits` — the limiter's five scopes and the tester (REQ-012, slice 3).
 *
 * The screen exists for the 3am question: *a client is being refused and I do not know why.*
 * Four rules decide what it shows, and each one exists because the naive version of it lies:
 *
 * 1. **The tester is a server call, never a client computation.** It posts the request to
 *    `POST /security/rate-limits/test`, which calls the same `decide` the middleware calls. A
 *    tester reimplemented in TypeScript would agree with the platform on the day it was written
 *    and drift the first time somebody tuned a limit — which is the day somebody relies on it.
 * 2. **The verdict shows the arithmetic, not a word.** "Allowed" over "would be allowed" hides
 *    `3 of 11 requests in the window` — and the number is what tells an operator whether to
 *    raise the limit, wait for the window, or find a client that is looping. The refusal's
 *    `Retry-After` is the same number the wire carries, and the counter key is shown so the
 *    claim is checkable against a Redis dump rather than merely believable.
 * 3. **A disabled scope is visibly off, not absent.** `enabled: false` is a policy an operator
 *    chose, and a table where it simply disappears looks identical to a scope the platform has
 *    never heard of — the exact confusion the server-side default merge exists to prevent.
 * 4. **The unsaved-changes guard is the same compare-and-swap the server enforces.** The form
 *    remembers the document it was opened with; if somebody else saved in the meantime the save
 *    is **refused with their message** rather than overwriting a policy that is refusing traffic.
 *
 * `burst` gets its own column and tooltip rather than being folded into `limit`, because
 * `burst` is *headroom inside the window*, not a second window: `ceiling = limit + burst` is
 * computed by the server and shown, so nobody has to do the addition on a row and get one wrong.
 *
 * Keyboard: `Ctrl/Cmd+S` saves, every control is reachable by `Tab` in render order, and the
 * tester's result is announced through an `aria-live` region so the verdict is not a purely
 * visual answer to a keyboard user's question. Mobile: the table becomes stacked cards — five
 * scopes with four numbers each is unreadable at 360px, and a horizontally scrolling table hides
 * the values that matter behind a scroll nobody discovers.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { FlaskConical, Loader2, RotateCcw, Save } from "lucide-react";

import {
  fetchRateLimits,
  saveRateLimits,
  testRateLimit,
  type ApiError,
} from "@/lib/api";
import { SecurityTabs } from "@/features/security/security-tabs";
import type {
  RateLimitsDocument,
  RateLimitTestResponse,
} from "@/lib/types";

/** One scope's editable state, kept apart from the document so "dirty" is computable. */
type DraftRow = {
  window_seconds: string;
  limit: string;
  burst: string;
  enabled: boolean;
};

/** The drafts, keyed by scope name. */
type Draft = Record<string, DraftRow>;

/**
 * The form's own copy of the document, as strings.
 *
 * Strings, deliberately: a number input whose value is a `number` cannot represent "the operator
 * cleared this field", and an empty field silently becomes `0` — which for a limit is a scope
 * that refuses everything, and for a window is a division by zero in the key's bucket. The text
 * is parsed on save and the *server* decides whether it is in range; the client only refuses to
 * send something that is not a number at all, because that has no server-side meaning to report.
 */
function draftFrom(document: RateLimitsDocument): Draft {
  const draft: Draft = {};
  for (const row of document.scopes) {
    draft[row.scope] = {
      window_seconds: String(row.window_seconds),
      limit: String(row.limit),
      burst: String(row.burst),
      enabled: row.enabled,
    };
  }
  return draft;
}

/** The document exactly as it arrived, which is the CAS key a save must present. */
function documentOf(document: RateLimitsDocument): unknown {
  return {
    scopes: document.scopes.map((row) => ({
      scope: row.scope,
      window_seconds: row.window_seconds,
      limit: row.limit,
      burst: row.burst,
      enabled: row.enabled,
    })),
  };
}

/** A scope's name as a person reads it. */
function scopeLabel(scope: string): string {
  return scope.replace(/_/g, " ").replace(/^./, (c) => c.toUpperCase());
}

/** Seconds as a duration a person can check against a clock. */
function humanSeconds(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
  if (seconds < 86_400) return `${Math.round(seconds / 3600)}h`;
  return `${Math.round(seconds / 86_400)}d`;
}

export function RateLimitsScreen() {
  const [document_, setLoaded] = useState<RateLimitsDocument | null>(null);
  const [draft, setDraft] = useState<Draft>({});
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<ApiError | null>(null);
  const [savedAt, setSavedAt] = useState<string | null>(null);

  const [probeMethod, setProbeMethod] = useState("POST");
  const [probePath, setProbePath] = useState("/api/v1/auth/login");
  const [probeIp, setProbeIp] = useState("203.0.113.7");
  const [probeCount, setProbeCount] = useState("");
  const [probeBusy, setProbeBusy] = useState(false);
  const [probeResult, setProbeResult] = useState<RateLimitTestResponse | null>(null);
  const [probeError, setProbeError] = useState<string | null>(null);

  const probeRegion = useRef<HTMLDivElement | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    try {
      const next = await fetchRateLimits();
      setLoaded(next);
      setDraft(draftFrom(next));
    } catch (error) {
      setLoadError(error instanceof Error ? error.message : "the limiter could not be read");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  /** True when the form holds something the server document does not. */
  const dirty = useMemo(() => {
    if (!document_) return false;
    return JSON.stringify(draftFrom(document_)) !== JSON.stringify(draft);
  }, [document_, draft]);

  /**
   * The refusal to send, computed before the request.
   *
   * Exactly two rules, and both are about *meaninglessness* rather than ranges: a field that is
   * not a number at all, and a ceiling that is zero. Every range question is the server's — it
   * owns the numbers, and a client copy of them would drift the first time a constant changed.
   */
  const localRefusal = useMemo(() => {
    if (!document_) return null;
    for (const row of document_.scopes) {
      const field = draft[row.scope];
      if (!field) return `the ${scopeLabel(row.scope)} row is missing from the form`;
      for (const [key, value] of [
        ["window", field.window_seconds],
        ["limit", field.limit],
        ["burst", field.burst],
      ] as const) {
        if (value.trim() === "" || !Number.isInteger(Number(value))) {
          return `${scopeLabel(row.scope)}: the ${key} must be a whole number`;
        }
      }
    }
    return null;
  }, [document_, draft]);

  const setField = (scope: string, key: keyof Omit<DraftRow, "enabled">, value: string) => {
    setDraft((current) => ({ ...current, [scope]: { ...current[scope], [key]: value } }));
    setSavedAt(null);
  };

  const setEnabled = (scope: string, enabled: boolean) => {
    setDraft((current) => ({ ...current, [scope]: { ...current[scope], enabled } }));
    setSavedAt(null);
  };

  const save = useCallback(async () => {
    if (!document_ || localRefusal) return;
    setSaving(true);
    setSaveError(null);
    setSavedAt(null);
    try {
      const saved = await saveRateLimits({
        scopes: document_.scopes.map((row) => ({
          scope: row.scope,
          window_seconds: Number(draft[row.scope].window_seconds),
          limit: Number(draft[row.scope].limit),
          burst: Number(draft[row.scope].burst),
          enabled: draft[row.scope].enabled,
        })),
        expected_scopes: documentOf(document_),
      });
      // Replace rather than merge: the server's answer is the truth about what is now in force,
      // and keeping a local row would show a limit the platform no longer enforces.
      setLoaded(saved);
      setDraft(draftFrom(saved));
      setSavedAt(new Date().toISOString());
    } catch (error) {
      setSaveError(error as ApiError);
    } finally {
      setSaving(false);
    }
  }, [document_, draft, localRefusal]);

  // `Ctrl/Cmd+S` saves, matching the header policy screen: these are policy documents, and a
  // person who has just typed five numbers wants the same shortcut on both.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "s") {
        event.preventDefault();
        if (dirty && !localRefusal && !saving) void save();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [dirty, localRefusal, saving, save]);

  const runProbe = useCallback(async () => {
    setProbeBusy(true);
    setProbeError(null);
    setProbeResult(null);
    try {
      setProbeResult(
        await testRateLimit({
          method: probeMethod,
          path: probePath,
          client_ip: probeIp.trim() || null,
          count: probeCount.trim() === "" ? null : Number(probeCount),
          machine_key: false,
        }),
      );
      // The result is the point of the screen, so focus moves to it for a keyboard user; a
      // verdict that only appears in a column is a verdict a screen reader never reads.
      probeRegion.current?.focus();
    } catch (error) {
      setProbeError(error instanceof Error ? error.message : "the tester refused the request");
    } finally {
      setProbeBusy(false);
    }
  }, [probeMethod, probePath, probeIp, probeCount]);

  if (loading) {
    return (
      <div className="space-y-3" data-rate-limits="loading" aria-busy="true">
        <div className="h-5 w-48 animate-pulse rounded bg-quiet-soft" />
        <div className="h-40 animate-pulse rounded-lg bg-quiet-soft" />
        <p className="flex items-center gap-2 text-[12.5px] text-muted">
          <Loader2 className="size-3.5 animate-spin" aria-hidden="true" />
          Reading the limiter
        </p>
      </div>
    );
  }

  if (loadError || !document_) {
    return (
      <div className="space-y-3" data-rate-limits="error">
        <p role="alert" className="text-[13px] text-danger">
          {loadError}
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-quiet-soft"
        >
          <RotateCcw className="size-3.5" aria-hidden="true" />
          Try again
        </button>
      </div>
    );
  }

  return (
    <div className="space-y-6" data-rate-limits="ready">
      <SecurityTabs current="rate-limits" />

      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-[15px] font-medium text-ink">Rate limits</h2>
          <p className="mt-0.5 text-[12.5px] text-muted">
            {document_.is_saved
              ? `Last saved ${document_.updated_at ?? "recently"}`
              : "The platform baseline — nobody has saved a policy on this deployment yet"}
          </p>
        </div>
        <div className="flex items-center gap-2">
          {dirty ? (
            <span
              data-rate-limits-dirty
              className="rounded border border-warn-soft bg-warn-soft px-2 py-1 text-[12px] text-warn"
            >
              Unsaved changes
            </span>
          ) : null}
          <button
            type="button"
            onClick={() => void save()}
            disabled={!dirty || saving || Boolean(localRefusal)}
            data-rate-limits-save
            className="inline-flex items-center gap-1.5 rounded-md bg-accent px-2.5 py-1.5 text-[12.5px] font-medium text-accent-ink disabled:opacity-40"
          >
            {saving ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden="true" />
            ) : (
              <Save className="size-3.5" aria-hidden="true" />
            )}
            Save limits
          </button>
        </div>
      </header>

      {localRefusal ? (
        <p
          role="alert"
          data-rate-limits-local-error
          className="rounded-md border border-danger-soft bg-danger-soft px-3 py-2 text-[12.5px] text-danger"
        >
          {localRefusal}
        </p>
      ) : null}

      {saveError ? (
        <p
          role="alert"
          data-rate-limits-save-error
          className="rounded-md border border-danger-soft bg-danger-soft px-3 py-2 text-[12.5px] text-danger"
        >
          {saveError.message}
        </p>
      ) : null}

      {savedAt ? (
        <p data-rate-limits-saved className="text-[12.5px] text-ok">
          Saved. These limits apply to the next request.
        </p>
      ) : null}

      {/* Desktop: the table. `aria-label` on the caption rather than a visible heading cell so
          the table is self-describing to a screen reader. */}
      <div className="hidden overflow-x-auto md:block">
        <table className="w-full border-collapse text-[12.5px]">
          <caption className="sr-only">
            Rate limits per scope, with the window, the limit, the burst headroom and the state
          </caption>
          <thead>
            <tr className="border-b border-line text-left text-muted">
              <th scope="col" className="py-2 pr-3 font-medium">Scope</th>
              <th scope="col" className="py-2 pr-3 font-medium">Window (s)</th>
              <th scope="col" className="py-2 pr-3 font-medium">Limit</th>
              <th scope="col" className="py-2 pr-3 font-medium">Burst</th>
              <th scope="col" className="py-2 pr-3 font-medium">Admits</th>
              <th scope="col" className="py-2 pr-3 font-medium">State</th>
            </tr>
          </thead>
          <tbody>
            {document_.scopes.map((row) => {
              const field = draft[row.scope];
              return (
                <tr
                  key={row.scope}
                  data-rate-limit-row={row.scope}
                  className="border-b border-line/60 last:border-0"
                >
                  <th scope="row" className="py-2 pr-3 text-left font-normal text-ink">
                    {scopeLabel(row.scope)}
                    {row.scope === "sign_in" ? (
                      <span className="ml-1.5 text-[11px] text-muted">unauthenticated</span>
                    ) : null}
                  </th>
                  {(["window_seconds", "limit", "burst"] as const).map((key) => (
                    <td key={key} className="py-2 pr-3">
                      <input
                        type="number"
                        inputMode="numeric"
                        aria-label={`${scopeLabel(row.scope)} ${key.replace(/_/g, " ")}`}
                        data-rate-limit-input={`${row.scope}:${key}`}
                        value={field[key]}
                        disabled={!field.enabled}
                        onChange={(event) => setField(row.scope, key, event.target.value)}
                        className="w-20 rounded border border-line bg-surface px-1.5 py-1 text-[12.5px] text-ink disabled:opacity-50"
                      />
                    </td>
                  ))}
                  <td className="py-2 pr-3 tabular-nums text-muted">{row.ceiling}</td>
                  <td className="py-2 pr-3">
                    <label className="inline-flex items-center gap-1.5 text-[12.5px]">
                      <input
                        type="checkbox"
                        data-rate-limit-enabled={row.scope}
                        checked={field.enabled}
                        onChange={(event) => setEnabled(row.scope, event.target.checked)}
                        className="accent-[var(--accent)]"
                      />
                      <span className={field.enabled ? "text-ink" : "text-muted"}>
                        {field.enabled ? "Enforced" : "Off"}
                      </span>
                    </label>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      {/* Mobile: the same five scopes as cards. Not a scrolling table — four numbers a row at
          360px pushes the state toggle off the screen, and the state is the field a phone user
          is most likely to be checking. */}
      <ul className="space-y-2 md:hidden" data-rate-limits-cards>
        {document_.scopes.map((row) => {
          const field = draft[row.scope];
          return (
            <li
              key={row.scope}
              data-rate-limit-card={row.scope}
              className="rounded-lg border border-line p-3"
            >
              <div className="flex items-center justify-between">
                <span className="text-[13px] font-medium text-ink">{scopeLabel(row.scope)}</span>
                <label className="inline-flex items-center gap-1.5 text-[12px]">
                  <input
                    type="checkbox"
                    data-rate-limit-enabled={row.scope}
                    checked={field.enabled}
                    onChange={(event) => setEnabled(row.scope, event.target.checked)}
                    className="accent-[var(--accent)]"
                  />
                  <span className={field.enabled ? "text-ink" : "text-muted"}>
                    {field.enabled ? "Enforced" : "Off"}
                  </span>
                </label>
              </div>
              <div className="mt-2 grid grid-cols-3 gap-2">
                {(["window_seconds", "limit", "burst"] as const).map((key) => (
                  <label key={key} className="text-[11px] text-muted">
                    {key.replace(/_/g, " ")}
                    <input
                      type="number"
                      inputMode="numeric"
                      aria-label={`${scopeLabel(row.scope)} ${key.replace(/_/g, " ")}`}
                      data-rate-limit-input={`${row.scope}:${key}`}
                      value={field[key]}
                      disabled={!field.enabled}
                      onChange={(event) => setField(row.scope, key, event.target.value)}
                      className="mt-0.5 w-full rounded border border-line bg-surface px-1.5 py-1 text-[12.5px] text-ink disabled:opacity-50"
                    />
                  </label>
                ))}
              </div>
              <p className="mt-1.5 text-[11.5px] text-muted">
                Admits {row.ceiling} per window · window frees in{" "}
                {humanSeconds(row.window_remaining_seconds)}
              </p>
            </li>
          );
        })}
      </ul>

      {/* The tester. */}
      <section
        aria-labelledby="rate-limits-tester"
        className="rounded-lg border border-line p-4"
        data-rate-limits-tester
      >
        <h3
          id="rate-limits-tester"
          className="flex items-center gap-1.5 text-[13.5px] font-medium text-ink"
        >
          <FlaskConical className="size-3.5 text-muted" aria-hidden="true" />
          Would this be limited?
        </h3>
        <p className="mt-0.5 text-[12px] text-muted">
          The same decision the request path makes, with the same function — not a second copy of
          the arithmetic that would drift the first time a limit is tuned.
        </p>

        <div className="mt-3 grid gap-2 sm:grid-cols-2 lg:grid-cols-5">
          <label className="text-[11.5px] text-muted">
            Method
            <select
              value={probeMethod}
              data-rate-limit-probe-method
              onChange={(event) => setProbeMethod(event.target.value)}
              className="mt-0.5 w-full rounded border border-line bg-surface px-1.5 py-1 text-[12.5px] text-ink"
            >
              {["GET", "POST", "PUT", "PATCH", "DELETE"].map((method) => (
                <option key={method} value={method}>
                  {method}
                </option>
              ))}
            </select>
          </label>
          <label className="text-[11.5px] text-muted lg:col-span-2">
            Path
            <input
              type="text"
              value={probePath}
              data-rate-limit-probe-path
              onChange={(event) => setProbePath(event.target.value)}
              className="mt-0.5 w-full rounded border border-line bg-surface px-1.5 py-1 text-[12.5px] text-ink"
            />
          </label>
          <label className="text-[11.5px] text-muted">
            Client IP
            <input
              type="text"
              value={probeIp}
              data-rate-limit-probe-ip
              onChange={(event) => setProbeIp(event.target.value)}
              placeholder="leave blank to use yours"
              className="mt-0.5 w-full rounded border border-line bg-surface px-1.5 py-1 text-[12.5px] text-ink"
            />
          </label>
          <label className="text-[11.5px] text-muted">
            Counter already at
            <input
              type="number"
              inputMode="numeric"
              value={probeCount}
              data-rate-limit-probe-count
              onChange={(event) => setProbeCount(event.target.value)}
              placeholder="1"
              className="mt-0.5 w-full rounded border border-line bg-surface px-1.5 py-1 text-[12.5px] text-ink"
            />
          </label>
        </div>

        <div className="mt-3 flex items-center gap-2">
          <button
            type="button"
            onClick={() => void runProbe()}
            disabled={probeBusy || probePath.trim() === ""}
            data-rate-limit-probe-run
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-quiet-soft disabled:opacity-40"
          >
            {probeBusy ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden="true" />
            ) : (
              <FlaskConical className="size-3.5" aria-hidden="true" />
            )}
            Test this request
          </button>
        </div>

        {probeError ? (
          <p role="alert" data-rate-limit-probe-error className="mt-2 text-[12.5px] text-danger">
            {probeError}
          </p>
        ) : null}

        {/* The verdict. `tabIndex={-1}` + focus on completion so the answer reaches a keyboard
            user, and `aria-live` so it is announced rather than merely drawn. */}
        <div
          ref={probeRegion}
          tabIndex={-1}
          aria-live="polite"
          data-rate-limit-probe-result={probeResult ? (probeResult.verdict.limited ? "limited" : "allowed") : "none"}
          className="mt-3 rounded-md border border-line bg-quiet-soft/40 p-3 outline-none"
        >
          {probeResult ? (
            <>
              <p
                data-rate-limit-verdict
                className={`text-[13px] font-medium ${
                  probeResult.verdict.limited ? "text-danger" : "text-ok"
                }`}
              >
                {probeResult.verdict.limited ? "Would be limited" : "Would be allowed"}
                <span className="ml-1.5 font-normal text-muted">
                  · scope {scopeLabel(probeResult.verdict.scope)}
                </span>
              </p>
              <p data-rate-limit-reason className="mt-1 text-[12.5px] text-ink">
                {probeResult.verdict.reason}
              </p>
              <dl className="mt-2 grid gap-x-4 gap-y-1 text-[12px] sm:grid-cols-2">
                <div className="flex gap-2">
                  <dt className="text-muted">Counted against</dt>
                  <dd data-rate-limit-identity className="font-mono text-ink">
                    {probeResult.counter_identity}
                  </dd>
                </div>
                <div className="flex gap-2">
                  <dt className="text-muted">Counter key</dt>
                  <dd
                    data-rate-limit-key
                    className="truncate font-mono text-ink"
                    title={probeResult.counter_key}
                  >
                    {probeResult.counter_key}
                  </dd>
                </div>
                <div className="flex gap-2">
                  <dt className="text-muted">Requests</dt>
                  <dd className="tabular-nums text-ink">
                    {probeResult.verdict.count} of {probeResult.verdict.ceiling}
                  </dd>
                </div>
                <div className="flex gap-2">
                  <dt className="text-muted">Retry after</dt>
                  <dd data-rate-limit-retry className="tabular-nums text-ink">
                    {probeResult.verdict.retry_after === null
                      ? "—"
                      : humanSeconds(probeResult.verdict.retry_after)}
                  </dd>
                </div>
              </dl>
            </>
          ) : (
            <p data-rate-limit-probe-hint className="text-[12.5px] text-muted">
              No request tested yet. The counter field is what makes this useful during an
              incident: set it to what the client has already spent and the answer explains the
              refusal they are seeing.
            </p>
          )}
        </div>
      </section>
    </div>
  );
}
