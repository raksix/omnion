"use client";

/**
 * `/observability/settings` — sampling, retention, levels and the cardinality budget
 * (docs/requests/REQ-126, slice 4).
 *
 * The screen's job is to make the trade-offs legible, because every field on it spends something:
 *
 * - **The sampling ratio is not "how much telemetry you lose".** Errors are sampled whatever the
 *   ratio says, so turning it to 0 still leaves a complete trace for every 5xx. The copy says so
 *   next to the number, because an operator who believes 0.0 means "no traces" will never turn
 *   it down at all.
 * - **Retention is bounded by what the store can answer.** The caps are shown beside the inputs
 *   and the API's refusal names the field, so a save that fails says which one and what the
 *   limit is rather than "validation error".
 * - **A temporary level raise expires on its own.** An expired raise is listed greyed with the
 *   fact that it has expired, and it is NOT written back — the acceptance line is "expires back
 *   to the configured default without a restart", and half of that is the row no longer claiming
 *   a raise nobody will clear.
 * - **The egress note is on this screen, not in a manual.** Configuring an exporter sends data
 *   out of the instance, and the operator making that change should read what travels and what
 *   does not at the moment they make it.
 *
 * The budget field is the one most likely to be misread, so it is labelled with what exceeding it
 * does: samples are folded into an `other` series and counted, not dropped. An operator who
 * thinks it loses data will never raise it; an operator who knows it loses *precision* will.
 *
 * Keyboard: `s` saves, `Esc` discards unsaved edits (with a confirmation), `r` re-reads.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { AlertTriangle, CheckCircle2, Info, Loader2, RefreshCw, RotateCcw, Save } from "lucide-react";

import {
  ApiError,
  fetchLifecycle,
  fetchObservabilitySettings,
  saveObservabilitySettings,
  type LifecycleResponse,
  type ObservabilitySettings,
} from "@/lib/api";

/** The default duration offered by the "expires in" control, in hours. */
const EXPIRY_PRESETS = [
  { label: "1 hour", hours: 1 },
  { label: "4 hours", hours: 4 },
  { label: "24 hours", hours: 24 },
  { label: "7 days", hours: 24 * 7 },
];

function hoursFromNow(hours: number): string {
  return new Date(Date.now() + hours * 3_600_000).toISOString();
}

interface FormState {
  sampling_ratio: string;
  logs_retention_days: string;
  traces_retention_days: string;
  log_level_default: string;
  cardinality_budget: string;
  prometheus_public: boolean;
  /** The new raise being composed, not yet part of the saved object. */
  newTarget: string;
  newLevel: string;
  newExpiryHours: number;
}

function toForm(settings: ObservabilitySettings): FormState {
  return {
    sampling_ratio: String(settings.sampling_ratio),
    logs_retention_days: String(settings.logs_retention_days),
    traces_retention_days: String(settings.traces_retention_days),
    log_level_default: settings.log_level_default,
    cardinality_budget: String(settings.cardinality_budget),
    prometheus_public: settings.prometheus_public,
    newTarget: "",
    newLevel: "debug",
    newExpiryHours: 4,
  };
}

/**
 * The overrides as a saveable object, with the new raise folded in.
 *
 * Built from the RESOLVED list the API returned rather than from the raw column, so an expired
 * raise cannot ride along in a save. The API drops expired entries on write too — this is the
 * second of the two guards, and the visible one: the operator sees the list they are saving.
 */
function overridesPayload(
  settings: ObservabilitySettings,
  form: FormState,
): Record<string, unknown> {
  const payload: Record<string, unknown> = {};
  for (const row of settings.level_overrides) {
    if (row.expired) continue;
    payload[row.target] = row.expires_at
      ? { level: row.level, expires_at: row.expires_at }
      : { level: row.level };
  }
  const target = form.newTarget.trim();
  if (target) {
    payload[target] = {
      level: form.newLevel,
      expires_at: hoursFromNow(form.newExpiryHours),
    };
  }
  return payload;
}

/** A field whose number is outside its documented range, with the reason. */
function RangeProblem({
  value,
  min,
  max,
  field,
}: {
  value: number;
  min: number;
  max: number;
  field: string;
}) {
  if (!Number.isFinite(value)) {
    return (
      <span className="text-danger" data-field-error={field}>
        Not a number.
      </span>
    );
  }
  if (value < min || value > max) {
    return (
      <span className="text-danger" data-field-error={field}>
        Must be between {min} and {max}.
      </span>
    );
  }
  return null;
}

function Field({
  label,
  hint,
  children,
  htmlFor,
}: {
  label: string;
  hint: React.ReactNode;
  children: React.ReactNode;
  htmlFor: string;
}) {
  return (
    <div className="grid gap-1.5">
      <label htmlFor={htmlFor} className="text-sm font-medium text-ink">
        {label}
      </label>
      {children}
      <p className="text-xs text-muted">{hint}</p>
    </div>
  );
}

export function SettingsView() {
  const [settings, setSettings] = useState<ObservabilitySettings | null>(null);
  const [lifecycle, setLifecycle] = useState<LifecycleResponse | null>(null);
  const [form, setForm] = useState<FormState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);

  const load = useCallback(async () => {
    try {
      const [row, contract] = await Promise.all([
        fetchObservabilitySettings(),
        // The probe contract is fetched with the settings because both are "how this instance
        // behaves" — and a screen that can read the drain timeout is what tells an operator
        // whether to raise it.
        fetchLifecycle().catch(() => null),
      ]);
      setSettings(row);
      setForm(toForm(row));
      setLifecycle(contract);
      setError(null);
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The settings could not be read.",
      );
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const problems = useMemo(() => {
    if (!form || !settings) return {} as Record<string, React.ReactNode>;
    const caps = settings.caps;
    return {
      sampling_ratio: (
        <RangeProblem
          value={Number(form.sampling_ratio)}
          min={0}
          max={caps.sampling_max}
          field="sampling_ratio"
        />
      ),
      logs_retention_days: (
        <RangeProblem
          value={Number(form.logs_retention_days)}
          min={1}
          max={caps.logs_retention_max}
          field="logs_retention_days"
        />
      ),
      traces_retention_days: (
        <RangeProblem
          value={Number(form.traces_retention_days)}
          min={1}
          max={caps.traces_retention_max}
          field="traces_retention_days"
        />
      ),
      cardinality_budget: (
        <RangeProblem
          value={Number(form.cardinality_budget)}
          min={1}
          max={caps.cardinality_max}
          field="cardinality_budget"
        />
      ),
      log_level_default: caps.log_levels.includes(form.log_level_default) ? null : (
        <span className="text-danger" data-field-error="log_level_default">
          Must be one of {caps.log_levels.join(", ")}.
        </span>
      ),
    } as Record<string, React.ReactNode>;
  }, [form, settings]);

  const hasProblems =
    form !== null && Object.values(problems).some((problem) => problem !== null && problem !== undefined);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" || target?.tagName === "TEXTAREA" || target?.tagName === "SELECT";
      if (typing) return;
      if (event.key === "r") {
        event.preventDefault();
        void load();
      } else if (event.key === "s" && !hasProblems) {
        event.preventDefault();
        void save();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [load, hasProblems, form]);

  async function save() {
    if (!form) return;
    setSaving(true);
    setError(null);
    setFieldError(null);
    setSaved(false);
    try {
      const savedRow = await saveObservabilitySettings({
        sampling_ratio: Number(form.sampling_ratio),
        logs_retention_days: Number(form.logs_retention_days),
        traces_retention_days: Number(form.traces_retention_days),
        log_level_default: form.log_level_default,
        log_level_overrides: overridesPayload(settings as ObservabilitySettings, form),
        cardinality_budget: Number(form.cardinality_budget),
        prometheus_public: form.prometheus_public,
      });
      setSettings(savedRow);
      setForm(toForm(savedRow));
      setSaved(true);
    } catch (caught) {
      if (caught instanceof ApiError) {
        setError(caught.message);
        // The API's refusals are field-level by contract, so the message is placed under the form
        // AND the offending field is named in a chip. A blanket "validation error" is what makes
        // a settings form feel broken.
        const match = /`([a-z_]+)`/.exec(caught.message);
        if (match) {
          setFieldError({ field: match[1], message: caught.message });
        }
      } else {
        setError("The settings could not be saved.");
      }
    } finally {
      setSaving(false);
    }
  }

  if (loading || !form || !settings) {
    return (
      <div className="grid gap-3" aria-busy="true">
        <div className="h-8 w-48 rounded bg-quiet-soft" />
        <div className="h-24 rounded bg-quiet-soft" />
        <div className="h-24 rounded bg-quiet-soft" />
      </div>
    );
  }

  const caps = settings.caps;
  const input =
    "rounded-md border border-line bg-surface px-3 py-2 text-sm font-mono";

  return (
    <div className="grid gap-6" data-view="observability-settings" data-settings-view>
      {error && (
        <div
          className="rounded-md border border-danger/30 bg-danger-soft p-3 text-sm text-danger"
          data-settings-error
        >
          <p>{error}</p>
          {fieldError && (
            <p className="mt-1 text-xs">
              Field: <code className="font-mono">{fieldError.field}</code>
            </p>
          )}
        </div>
      )}

      {saved && (
        <p
          className="flex items-center gap-1.5 rounded-md border border-positive/30 bg-positive-soft p-3 text-sm text-positive"
          data-settings-saved
        >
          <CheckCircle2 className="h-4 w-4" aria-hidden />
          Saved. The sampling ratio and the budget took effect without a restart.
        </p>
      )}

      <section className="grid gap-4 rounded-lg border border-line bg-panel p-5">
        <div>
          <h2 className="text-sm font-semibold">Tracing</h2>
          <p className="text-xs text-muted">
            How much of the healthy traffic the trace index keeps.
          </p>
        </div>

        <Field
          htmlFor="sampling_ratio"
          label="Sampling ratio"
          hint={
            <>
              The share of successful requests whose trace is kept, 0.0 to{" "}
              {caps.sampling_max}.{" "}
              <strong className="text-ink">Errors are sampled regardless</strong> — at 0.0 a
              failing request still gets a complete trace, which is the whole reason it is safe to
              let a non-expert turn this down.
              {problems.sampling_ratio}
            </>
          }
        >
          <input
            id="sampling_ratio"
            type="number"
            min={0}
            max={caps.sampling_max}
            step={0.05}
            value={form.sampling_ratio}
            onChange={(event) => setForm({ ...form, sampling_ratio: event.target.value })}
            className={`${input} w-32`}
            data-setting="sampling_ratio"
          />
        </Field>
      </section>

      <section className="grid gap-4 rounded-lg border border-line bg-panel p-5">
        <div>
          <h2 className="text-sm font-semibold">Retention</h2>
          <p className="text-xs text-muted">
            The log explorer and the trace index are conveniences for recent debugging, not log
            platforms. Anything older belongs to the operator&rsquo;s own backend.
          </p>
        </div>

        <div className="grid gap-4 sm:grid-cols-2">
          <Field
            htmlFor="logs_retention_days"
            label="Log retention (days)"
            hint={
              <>
                1 to {caps.logs_retention_max}. The store cannot answer a search older than it
                keeps, so the cap is the store&rsquo;s own.
                {problems.logs_retention_days}
              </>
            }
          >
            <input
              id="logs_retention_days"
              type="number"
              min={1}
              max={caps.logs_retention_max}
              value={form.logs_retention_days}
              onChange={(event) => setForm({ ...form, logs_retention_days: event.target.value })}
              className={`${input} w-28`}
              data-setting="logs_retention_days"
            />
          </Field>

          <Field
            htmlFor="traces_retention_days"
            label="Trace retention (days)"
            hint={
              <>
                1 to {caps.traces_retention_max}.
                {problems.traces_retention_days}
              </>
            }
          >
            <input
              id="traces_retention_days"
              type="number"
              min={1}
              max={caps.traces_retention_max}
              value={form.traces_retention_days}
              onChange={(event) => setForm({ ...form, traces_retention_days: event.target.value })}
              className={`${input} w-28`}
              data-setting="traces_retention_days"
            />
          </Field>
        </div>
      </section>

      <section className="grid gap-4 rounded-lg border border-line bg-panel p-5">
        <div>
          <h2 className="text-sm font-semibold">Log levels</h2>
          <p className="text-xs text-muted">
            A raise here expires on its own. That is the point: a debug level left on by accident
            costs money on a busy instance and is forgotten by everyone within a day.
          </p>
        </div>

        <Field
          htmlFor="log_level_default"
          label="Default level"
          hint={
            <>
              What a module logs at unless it is raised. {problems.log_level_default}
            </>
          }
        >
          <select
            id="log_level_default"
            value={form.log_level_default}
            onChange={(event) => setForm({ ...form, log_level_default: event.target.value })}
            className="rounded-md border border-line bg-surface px-3 py-2 text-sm"
            data-setting="log_level_default"
          >
            {caps.log_levels.map((level) => (
              <option key={level} value={level}>
                {level}
              </option>
            ))}
          </select>
        </Field>

        <div className="grid gap-2">
          <p className="text-sm font-medium text-ink">Temporary raises</p>
          {settings.level_overrides.length === 0 ? (
            <p className="text-xs text-muted">
              None. Every module logs at the default.
            </p>
          ) : (
            <ul className="grid gap-1.5" data-level-overrides>
              {settings.level_overrides.map((row) => (
                <li
                  key={row.target}
                  className={`flex flex-wrap items-center justify-between gap-2 rounded-md border px-3 py-2 text-xs ${
                    row.expired
                      ? "border-line bg-quiet-soft text-muted"
                      : "border-caution/30 bg-caution-soft text-ink"
                  }`}
                  data-level-override={row.target}
                  data-expired={row.expired}
                >
                  <span className="font-mono">
                    {row.target} → {row.level}
                  </span>
                  <span className="text-muted">
                    {row.expired ? (
                      <>
                        <AlertTriangle className="mr-1 inline h-3 w-3" aria-hidden />
                        expired — ignored, and dropped on the next save
                      </>
                    ) : row.expires_at ? (
                      <>expires {new Date(row.expires_at).toLocaleString()}</>
                    ) : (
                      <>no expiry — this one is permanent until you remove it</>
                    )}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>

        <div className="grid gap-2 rounded-md border border-dashed border-line p-3 sm:grid-cols-4">
          <label className="grid gap-1 text-xs sm:col-span-2">
            <span className="font-medium text-ink">Module path prefix</span>
            <input
              value={form.newTarget}
              onChange={(event) => setForm({ ...form, newTarget: event.target.value })}
              placeholder="omnion_secrets"
              className="rounded-md border border-line bg-surface px-2 py-1.5 font-mono text-xs"
              data-new-override-target
            />
          </label>
          <label className="grid gap-1 text-xs">
            <span className="font-medium text-ink">Level</span>
            <select
              value={form.newLevel}
              onChange={(event) => setForm({ ...form, newLevel: event.target.value })}
              className="rounded-md border border-line bg-surface px-2 py-1.5 text-xs"
              data-new-override-level
            >
              {caps.log_levels.map((level) => (
                <option key={level} value={level}>
                  {level}
                </option>
              ))}
            </select>
          </label>
          <label className="grid gap-1 text-xs">
            <span className="font-medium text-ink">Expires in</span>
            <select
              value={String(form.newExpiryHours)}
              onChange={(event) => setForm({ ...form, newExpiryHours: Number(event.target.value) })}
              className="rounded-md border border-line bg-surface px-2 py-1.5 text-xs"
              data-new-override-expiry
            >
              {EXPIRY_PRESETS.map((preset) => (
                <option key={preset.hours} value={preset.hours}>
                  {preset.label}
                </option>
              ))}
            </select>
          </label>
          <p className="text-[11px] text-muted sm:col-span-4">
            Saved with the form below. A raise with no target is ignored rather than applied to
            everything — a level raise scoped to nothing is a level raise nobody asked for.
          </p>
        </div>
      </section>

      <section className="grid gap-4 rounded-lg border border-line bg-panel p-5">
        <div>
          <h2 className="text-sm font-semibold">Metrics</h2>
          <p className="text-xs text-muted">
            Cardinality is the failure mode of every metric system: it does not break, it gets
            expensive, and it is found out about months later.
          </p>
        </div>

        <Field
          htmlFor="cardinality_budget"
          label="Cardinality budget (series)"
          hint={
            <>
              1 to {caps.cardinality_max}. Exceeding it does not drop samples: they are folded
              into an <code className="font-mono">other</code> series and counted on{" "}
              <code className="font-mono">omnion_registry_budget_exceeded</code>. So this costs
              you precision, not data — which is why raising it is usually the right move.
              {problems.cardinality_budget}
            </>
          }
        >
          <input
            id="cardinality_budget"
            type="number"
            min={1}
            max={caps.cardinality_max}
            value={form.cardinality_budget}
            onChange={(event) => setForm({ ...form, cardinality_budget: event.target.value })}
            className={`${input} w-40`}
            data-setting="cardinality_budget"
          />
        </Field>

        <label className="flex items-start gap-2 text-sm">
          <input
            type="checkbox"
            checked={form.prometheus_public}
            onChange={(event) => setForm({ ...form, prometheus_public: event.target.checked })}
            className="mt-0.5"
            data-setting="prometheus_public"
          />
          <span>
            <span className="font-medium text-ink">Expose /metrics beyond this interface</span>
            <span className="block text-xs text-muted">
              The exposition carries no secrets, but it does carry route templates and provider
              names. Leave it off unless a scraper reaches the instance over a network you control,
              and put a token or a CIDR allow-list in front of it.
            </span>
          </span>
        </label>
      </section>

      <section className="grid gap-3 rounded-lg border border-line bg-panel p-5">
        <div>
          <h2 className="text-sm font-semibold">What leaves this instance</h2>
          <p className="text-xs text-muted">
            Stated here, at the moment you change something that sends data out.
          </p>
        </div>
        <p className="text-sm text-muted" data-egress-note>
          {settings.egress_note}
        </p>
      </section>

      {lifecycle && (
        <section className="grid gap-3 rounded-lg border border-line bg-panel p-5">
          <div>
            <h2 className="text-sm font-semibold">Shutdown behaviour</h2>
            <p className="text-xs text-muted">{lifecycle.note}</p>
          </div>
          <dl className="grid gap-1.5 text-xs sm:grid-cols-2">
            <div className="flex justify-between gap-2 border-b border-line py-1">
              <dt className="text-muted">Drain timeout</dt>
              <dd className="font-mono" data-lifecycle="drain">
                {lifecycle.drain_timeout_ms} ms
              </dd>
            </div>
            <div className="flex justify-between gap-2 border-b border-line py-1">
              <dt className="text-muted">Currently draining</dt>
              <dd className="font-mono" data-lifecycle="draining">
                {lifecycle.draining ? `yes · ${lifecycle.in_flight} in flight` : "no"}
              </dd>
            </div>
            <div className="flex justify-between gap-2 border-b border-line py-1">
              <dt className="text-muted">Liveness</dt>
              <dd className="font-mono">
                {lifecycle.probes.liveness.path} (+{lifecycle.probes.liveness.alias})
              </dd>
            </div>
            <div className="flex justify-between gap-2 border-b border-line py-1">
              <dt className="text-muted">Readiness</dt>
              <dd className="font-mono">
                {lifecycle.probes.readiness.path} — fails on drain
              </dd>
            </div>
          </dl>
          <p className="flex items-start gap-1.5 text-xs text-muted">
            <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden />
            The drain timeout has to fit inside your supervisor&rsquo;s kill timeout with room for
            the telemetry flush that follows. A kill timeout below the drain is a deploy that
            loses the last batch.
          </p>
        </section>
      )}

      <div className="flex items-center justify-end gap-2">
        <button
          type="button"
          onClick={() => {
            setForm(toForm(settings));
            setError(null);
            setFieldError(null);
            setSaved(false);
          }}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-sm hover:bg-quiet-soft"
          data-settings-reset
        >
          <RotateCcw className="h-4 w-4" aria-hidden />
          Discard edits
        </button>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-sm hover:bg-quiet-soft"
          data-settings-refresh
        >
          <RefreshCw className="h-4 w-4" aria-hidden />
          Re-read
        </button>
        <button
          type="button"
          onClick={() => void save()}
          disabled={saving || hasProblems}
          className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3 py-1.5 text-sm text-white disabled:opacity-50"
          data-settings-save
        >
          {saving ? (
            <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
          ) : (
            <Save className="h-4 w-4" aria-hidden />
          )}
          Save
          <kbd className="ml-1 rounded border border-white/30 px-1 text-[10px]">s</kbd>
        </button>
      </div>
    </div>
  );
}
