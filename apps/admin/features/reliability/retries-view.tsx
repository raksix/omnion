"use client";

/**
 * `/settings/reliability/retries` — per-subsystem retry policies, the attempt ledger and the
 * dead-letter list (docs/requests/REQ-127, slice 3).
 *
 * Four things this screen refuses to blur, because each pair otherwise renders as the same tile
 * and each costs an operator an hour during an incident:
 *
 * - **The delay chart is the CEILING, not the schedule.** The server computes it with
 *   `draw = 1.0`, so every bar is the longest that attempt could ever wait. With `full` jitter
 *   the real delay is a random point at or below it. Drawing it as a schedule would promise
 *   timings the platform does not keep.
 * - **`stored: false` is not "unconfigured".** It means the shipped in-process default is in
 *   force, and it IS retrying — rendering it as an empty form is how an operator saves over a
 *   policy that was never written.
 * - **`retry now` does not fix anything.** The store appends an attempt and leaves the failure
 *   in the timeline, so the toast says "requeued" and the row stays flagged. A UI that greys
 *   the dead letter out after a retry is describing a history the database does not have.
 * - **A dead letter is not a permanent failure.** `failed_permanent` ends a sequence on attempt
 *   one with no retry and no dead letter; only an exhausted budget produces one.
 *
 * Keyboard: `/` filters, `n` opens the policy editor, `Esc` closes. Under `sm:` the tables
 * become cards and the numeric validation messages stay visible.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  CheckCircle2,
  Clock,
  Info,
  Loader2,
  RotateCw,
  Save,
  Search,
  Timer,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchReliabilityAttempts,
  fetchReliabilityRetryPolicies,
  retryNow,
  saveReliabilityRetryPolicy,
  type ReliabilityAttempt,
  type ReliabilityRetryPolicies,
  type ReliabilityRetryPolicy,
  type ReliabilityRetryPolicyInput,
} from "@/lib/api";

/** A blank form. Every field the API requires is present, so the form cannot post a hole. */
const BLANK: ReliabilityRetryPolicyInput = {
  provider_override: null,
  max_attempts: 5,
  base_delay_ms: 1000,
  factor: 2,
  jitter: "full",
  max_elapse_ms: 3_600_000,
  retry_on: ["5xx", "429"],
  enabled: true,
};

/** Milliseconds to a human string. `preview` values are ceilings, so `~` is the honest prefix. */
function humanMs(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return "—";
  if (ms <= 0) return "0 ms";
  if (ms < 1000) return `${ms} ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds % 1 === 0 ? seconds : seconds.toFixed(1)} s`;
  const minutes = seconds / 60;
  if (minutes < 60) return `${minutes % 1 === 0 ? minutes : minutes.toFixed(1)} min`;
  const hours = minutes / 60;
  return `${hours % 1 === 0 ? hours : hours.toFixed(1)} h`;
}

/** The outcome chip's label and colour class. Two outcomes must never share a chip. */
function outcomeKind(outcome: string): { label: string; tone: string } {
  switch (outcome) {
    case "succeeded":
      return { label: "Succeeded", tone: "text-emerald-700" };
    case "failed_retryable":
      return { label: "Retry scheduled", tone: "text-amber-700" };
    case "failed_permanent":
      return { label: "Permanent", tone: "text-muted" };
    case "exhausted":
      return { label: "Exhausted", tone: "text-rose-700" };
    default:
      return { label: outcome, tone: "text-muted" };
  }
}

/** The delay chart: one bar per attempt, height by the ceiling value. */
function DelayChart({ preview }: { preview: number[] }) {
  const peak = Math.max(...preview, 1);
  return (
    <div className="flex items-end gap-1" data-view="retry-delay-chart" aria-hidden="true">
      {preview.map((ms, index) => (
        <div key={index} className="flex flex-1 flex-col items-center gap-1">
          <div
            className="w-full rounded-t bg-quiet-soft"
            style={{ height: `${Math.max(2, Math.round((ms / peak) * 48))}px` }}
            title={`attempt ${index + 1}: up to ${humanMs(ms)}`}
          />
          <span className="text-[10px] text-muted">{index + 1}</span>
        </div>
      ))}
    </div>
  );
}

export function ReliabilityRetriesView() {
  const [policies, setPolicies] = useState<ReliabilityRetryPolicies | null>(null);
  const [attempts, setAttempts] = useState<ReliabilityAttempt[]>([]);
  const [deadLetters, setDeadLetters] = useState<ReliabilityAttempt[]>([]);
  const [dueNow, setDueNow] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [editing, setEditing] = useState<ReliabilityRetryPolicy | null>(null);
  const [form, setForm] = useState<ReliabilityRetryPolicyInput>(BLANK);
  const [formError, setFormError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [busyAttempt, setBusyAttempt] = useState<number | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [policyBody, attemptBody] = await Promise.all([
        fetchReliabilityRetryPolicies(),
        fetchReliabilityAttempts(50),
      ]);
      setPolicies(policyBody);
      setAttempts(attemptBody.attempts);
      setDeadLetters(attemptBody.dead_letters);
      setDueNow(attemptBody.due_now);
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // Keyboard: `/` focuses the filter, `n` opens the editor, `Esc` closes it.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target &&
        (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable);
      if (event.key === "/" && !typing) {
        event.preventDefault();
        searchRef.current?.focus();
        return;
      }
      if (event.key === "Escape" && editing) {
        setEditing(null);
        setFormError(null);
        return;
      }
      if (event.key.toLowerCase() === "n" && !typing && !editing) {
        event.preventDefault();
        const base = policies?.policies.find((p) => !p.provider_override);
        setEditing({
          subsystem: base?.subsystem ?? policies?.subsystems[0] ?? "webhook",
          provider_override: null,
          max_attempts: base?.max_attempts ?? BLANK.max_attempts,
          base_delay_ms: base?.base_delay_ms ?? BLANK.base_delay_ms,
          factor: base?.factor ?? BLANK.factor,
          jitter: base?.jitter ?? BLANK.jitter,
          max_elapse_ms: base?.max_elapse_ms ?? BLANK.max_elapse_ms,
          retry_on: base?.retry_on ?? BLANK.retry_on ?? [],
          enabled: base?.enabled ?? true,
          stored: false,
          delay_preview: base?.delay_preview ?? [],
          exceeds_budget: false,
        });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [editing, policies]);

  const visible = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    if (!needle) return attempts;
    return attempts.filter((a) =>
      [a.subsystem, a.subject_kind, a.subject_id, a.outcome, a.error_class]
        .filter(Boolean)
        .some((field) => String(field).toLowerCase().includes(needle)),
    );
  }, [attempts, filter]);

  const openEditor = (policy: ReliabilityRetryPolicy) => {
    setEditing(policy);
    setFormError(null);
    setForm({
      provider_override: policy.provider_override,
      max_attempts: policy.max_attempts,
      base_delay_ms: policy.base_delay_ms,
      factor: policy.factor,
      jitter: policy.jitter,
      max_elapse_ms: policy.max_elapse_ms,
      retry_on: policy.retry_on,
      enabled: policy.enabled,
    });
  };

  const save = async () => {
    if (!editing) return;
    setSaving(true);
    setFormError(null);
    try {
      await saveReliabilityRetryPolicy(editing.subsystem, form);
      setEditing(null);
      setNotice(
        `Saved the ${editing.subsystem} policy — in force from the next attempt, no deploy needed.`,
      );
      await load();
    } catch (caught) {
      setFormError(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setSaving(false);
    }
  };

  const requeue = async (attempt: ReliabilityAttempt) => {
    setBusyAttempt(attempt.id);
    try {
      await retryNow(attempt.id);
      setNotice(
        `Requeued attempt ${attempt.attempt + 1}. The original failure stays in the timeline on purpose.`,
      );
      await load();
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setBusyAttempt(null);
    }
  };

  if (loading && !policies) {
    return (
      <div data-view="reliability-retries">
        <LoadingTable columns={5} />
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-6" data-view="reliability-retries">
      {error ? (
        <div
          role="alert"
          className="flex items-start gap-2 rounded-lg border border-rose-200 bg-rose-50 px-4 py-3 text-[13px] text-rose-800"
        >
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />
          <span>{error}</span>
        </div>
      ) : null}
      {notice ? (
        <div
          role="status"
          className="flex items-start gap-2 rounded-lg border border-line bg-quiet-soft px-4 py-3 text-[13px]"
        >
          <CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0 text-emerald-700" aria-hidden="true" />
          <span>{notice}</span>
        </div>
      ) : null}

      {/* Backlog: the scheduler's own predicate, so this number and the worklist are one question. */}
      <div className="grid gap-3 sm:grid-cols-3">
        <div className="rounded-lg border border-line px-4 py-3">
          <p className="text-[12px] text-muted">Due now</p>
          <p className="text-[22px] font-semibold tabular-nums" data-view="retry-due-now">
            {dueNow}
          </p>
          <p className="text-[11.5px] text-muted">sequences owed an attempt</p>
        </div>
        <div className="rounded-lg border border-line px-4 py-3">
          <p className="text-[12px] text-muted">Dead letters</p>
          <p className="text-[22px] font-semibold tabular-nums" data-view="retry-dead-count">
            {deadLetters.length}
          </p>
          <p className="text-[11.5px] text-muted">exhausted budgets, each with retry now</p>
        </div>
        <div className="rounded-lg border border-line px-4 py-3">
          <p className="text-[12px] text-muted">Attempt rows</p>
          <p className="text-[22px] font-semibold tabular-nums">{attempts.length}</p>
          <p className="text-[11.5px] text-muted">newest first, every subsystem</p>
        </div>
      </div>

      {/* Policies. */}
      <section className="rounded-lg border border-line">
        <header className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-3">
          <h2 className="text-[14px] font-medium">Retry policies</h2>
          <p className="text-[12px] text-muted">
            The bars are the <strong>ceiling</strong> — the longest each attempt could wait. Real
            delays with full jitter land at or below them.
          </p>
        </header>
        {policies && policies.policies.length === 0 ? (
          <EmptyState
            title="No retry policies"
            hint="Every subsystem ships a default; if this list is empty the store has not been read yet."
            action={
              <button
                type="button"
                onClick={load}
                className="rounded-md border border-line px-3 py-1.5 text-[13px]"
              >
                Reload
              </button>
            }
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead className="text-[12px] text-muted">
                <tr>
                  <th className="px-4 py-2.5 font-medium">Subsystem</th>
                  <th className="px-4 py-2.5 font-medium">Attempts</th>
                  <th className="px-4 py-2.5 font-medium">Base / factor</th>
                  <th className="px-4 py-2.5 font-medium">Jitter</th>
                  <th className="px-4 py-2.5 font-medium">Delay ceiling</th>
                  <th className="px-4 py-2.5 font-medium">Budget</th>
                  <th className="px-4 py-2.5" />
                </tr>
              </thead>
              <tbody>
                {(policies?.policies ?? []).map((policy) => (
                  <tr
                    key={`${policy.subsystem}:${policy.provider_override ?? ""}`}
                    className="border-t border-line"
                    data-policy={`${policy.subsystem}:${policy.provider_override ?? ""}`}
                  >
                    <td className="px-4 py-3">
                      <span className="font-medium">{policy.subsystem}</span>
                      {policy.provider_override ? (
                        <span className="ml-2 text-[12px] text-muted">
                          {policy.provider_override}
                        </span>
                      ) : null}
                      {!policy.stored ? (
                        <span className="ml-2 rounded border border-line px-1.5 py-0.5 text-[11px] text-muted">
                          shipped default
                        </span>
                      ) : null}
                      {!policy.enabled ? (
                        <span className="ml-2 rounded border border-line px-1.5 py-0.5 text-[11px] text-muted">
                          disabled
                        </span>
                      ) : null}
                    </td>
                    <td className="px-4 py-3 tabular-nums">{policy.max_attempts}</td>
                    <td className="px-4 py-3 tabular-nums">
                      {humanMs(policy.base_delay_ms)} · ×{policy.factor}
                    </td>
                    <td className="px-4 py-3">{policy.jitter}</td>
                    <td className="w-40 px-4 py-3">
                      <DelayChart preview={policy.delay_preview} />
                    </td>
                    <td className="px-4 py-3">
                      <span className="tabular-nums">{humanMs(policy.max_elapse_ms)}</span>
                      {policy.exceeds_budget ? (
                        <span className="mt-1 flex items-center gap-1 text-[11.5px] text-amber-700">
                          <AlertTriangle className="h-3.5 w-3.5" aria-hidden="true" />
                          curve runs past the budget
                        </span>
                      ) : null}
                    </td>
                    <td className="px-4 py-3 text-right">
                      <button
                        type="button"
                        onClick={() => openEditor(policy)}
                        className="rounded-md border border-line px-2.5 py-1 text-[12.5px]"
                      >
                        Edit
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>

      {/* Dead letters. */}
      <section className="rounded-lg border border-line" data-view="retry-dead-letters">
        <header className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-3">
          <h2 className="text-[14px] font-medium">Dead letters</h2>
          <p className="text-[12px] text-muted">
            Retrying appends an attempt and leaves the failure here — the timeline is the evidence.
          </p>
        </header>
        {deadLetters.length === 0 ? (
          <EmptyState
            title="Nothing is dead-lettered"
            hint="A sequence appears here only when its budget is exhausted, which is different from a permanent failure — that one ends on attempt one and never retries."
          />
        ) : (
          <ul className="divide-y divide-line">
            {deadLetters.map((attempt) => (
              <li key={attempt.id} className="flex flex-wrap items-center gap-3 px-4 py-3">
                <div className="min-w-0 flex-1">
                  <p className="text-[13px] font-medium">
                    {attempt.subsystem} · attempt {attempt.attempt}
                    {attempt.error_class ? (
                      <span className="ml-2 text-[12px] text-muted">{attempt.error_class}</span>
                    ) : null}
                  </p>
                  <p className="truncate text-[12px] text-muted">
                    {attempt.subject_kind}
                    {attempt.subject_id ? ` · ${attempt.subject_id}` : ""}
                  </p>
                </div>
                <button
                  type="button"
                  disabled={busyAttempt === attempt.id}
                  onClick={() => void requeue(attempt)}
                  className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1 text-[12.5px] disabled:opacity-60"
                >
                  {busyAttempt === attempt.id ? (
                    <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
                  ) : (
                    <RotateCw className="h-3.5 w-3.5" aria-hidden="true" />
                  )}
                  Retry now
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>

      {/* The ledger. */}
      <section className="rounded-lg border border-line">
        <header className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-3">
          <h2 className="text-[14px] font-medium">Attempt ledger</h2>
          <label className="flex items-center gap-2 text-[12.5px] text-muted">
            <Search className="h-3.5 w-3.5" aria-hidden="true" />
            <input
              ref={searchRef}
              value={filter}
              onChange={(event) => setFilter(event.target.value)}
              placeholder="Filter (press /)"
              className="w-52 rounded-md border border-line px-2.5 py-1.5 text-[13px] text-ink outline-none focus:border-ink-soft"
            />
          </label>
        </header>
        {visible.length === 0 ? (
          <EmptyState
            title={attempts.length === 0 ? "No attempts recorded yet" : "Nothing matches that filter"}
            hint={
              attempts.length === 0
                ? "Every retryable delivery writes a row here, whether it succeeded on the first try or not."
                : "Clear the filter to see the whole ledger."
            }
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead className="text-[12px] text-muted">
                <tr>
                  <th className="px-4 py-2.5 font-medium">Subsystem</th>
                  <th className="px-4 py-2.5 font-medium">Attempt</th>
                  <th className="px-4 py-2.5 font-medium">Outcome</th>
                  <th className="px-4 py-2.5 font-medium">Next attempt</th>
                </tr>
              </thead>
              <tbody>
                {visible.map((attempt) => {
                  const kind = outcomeKind(attempt.outcome);
                  return (
                    <tr key={attempt.id} className="border-t border-line">
                      <td className="px-4 py-3">
                        {attempt.subsystem}
                        <span className="ml-2 text-[12px] text-muted">
                          {attempt.subject_id ?? attempt.subject_kind}
                        </span>
                      </td>
                      <td className="px-4 py-3 tabular-nums">{attempt.attempt}</td>
                      <td className={`px-4 py-3 ${kind.tone}`}>
                        {kind.label}
                        {attempt.error_class ? (
                          <span className="ml-2 text-[12px] text-muted">{attempt.error_class}</span>
                        ) : null}
                      </td>
                      <td className="px-4 py-3">
                        {attempt.next_attempt_at ? (
                          <span className="inline-flex items-center gap-1.5">
                            <Timer className="h-3.5 w-3.5" aria-hidden="true" />
                            {new Date(attempt.next_attempt_at).toLocaleString()}
                          </span>
                        ) : (
                          <span className="inline-flex items-center gap-1.5 text-muted">
                            <Clock className="h-3.5 w-3.5" aria-hidden="true" />
                            nothing owed
                          </span>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </section>

      {/* The editor. */}
      {editing ? (
        <div className="fixed inset-0 z-50 flex items-end justify-center bg-black/40 p-0 sm:items-center sm:p-6">
          <div
            role="dialog"
            aria-label="Edit retry policy"
            className="max-h-[90vh] w-full max-w-lg overflow-y-auto rounded-t-xl border border-line bg-panel p-5 sm:rounded-xl"
          >
            <div className="flex items-start justify-between gap-3">
              <div>
                <h3 className="text-[15px] font-medium">Edit retry policy</h3>
                <p className="text-[12.5px] text-muted">
                  {editing.subsystem}
                  {editing.provider_override ? ` · ${editing.provider_override}` : ""}
                  {editing.stored ? "" : " · creating a row for a shipped default"}
                </p>
              </div>
              <button
                type="button"
                onClick={() => setEditing(null)}
                aria-label="Close"
                className="rounded-md border border-line p-1"
              >
                <X className="h-4 w-4" aria-hidden="true" />
              </button>
            </div>

            <div className="mt-4 grid gap-3 sm:grid-cols-2">
              <label className="text-[12.5px] text-muted">
                Max attempts
                <input
                  type="number"
                  min={1}
                  max={20}
                  value={form.max_attempts}
                  onChange={(event) =>
                    setForm({ ...form, max_attempts: Number(event.target.value) })
                  }
                  className="mt-1 w-full rounded-md border border-line px-2.5 py-1.5 text-[13px] text-ink"
                />
              </label>
              <label className="text-[12.5px] text-muted">
                Base delay (ms)
                <input
                  type="number"
                  min={1}
                  value={form.base_delay_ms}
                  onChange={(event) =>
                    setForm({ ...form, base_delay_ms: Number(event.target.value) })
                  }
                  className="mt-1 w-full rounded-md border border-line px-2.5 py-1.5 text-[13px] text-ink"
                />
              </label>
              <label className="text-[12.5px] text-muted">
                Factor
                <input
                  type="number"
                  min={1}
                  max={10}
                  step={0.1}
                  value={form.factor}
                  onChange={(event) => setForm({ ...form, factor: Number(event.target.value) })}
                  className="mt-1 w-full rounded-md border border-line px-2.5 py-1.5 text-[13px] text-ink"
                />
              </label>
              <label className="text-[12.5px] text-muted">
                Jitter mode
                <select
                  value={form.jitter}
                  onChange={(event) => setForm({ ...form, jitter: event.target.value })}
                  className="mt-1 w-full rounded-md border border-line px-2.5 py-1.5 text-[13px] text-ink"
                >
                  {(policies?.jitter_modes ?? ["none", "equal", "full"]).map((mode) => (
                    <option key={mode} value={mode}>
                      {mode}
                    </option>
                  ))}
                </select>
              </label>
              <label className="text-[12.5px] text-muted sm:col-span-2">
                Total elapsed budget (ms)
                <input
                  type="number"
                  min={1000}
                  value={form.max_elapse_ms}
                  onChange={(event) =>
                    setForm({ ...form, max_elapse_ms: Number(event.target.value) })
                  }
                  className="mt-1 w-full rounded-md border border-line px-2.5 py-1.5 text-[13px] text-ink"
                />
              </label>
              <label className="flex items-center gap-2 text-[12.5px] sm:col-span-2">
                <input
                  type="checkbox"
                  checked={form.enabled ?? true}
                  onChange={(event) => setForm({ ...form, enabled: event.target.checked })}
                />
                Retrying is enabled for this subsystem
              </label>
            </div>

            <p className="mt-3 flex items-start gap-1.5 text-[12px] text-muted">
              <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden="true" />
              <span>
                <strong>full</strong> jitter draws a random point between zero and the ceiling, which
                is what stops a fleet of workers retrying in lockstep after an outage. <strong>
                none
                </strong> is deterministic and therefore synchronised — useful in a test, dangerous
                in production.
              </span>
            </p>

            {formError ? (
              <p role="alert" className="mt-3 text-[12.5px] text-rose-700">
                {formError}
              </p>
            ) : null}

            <div className="mt-5 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setEditing(null)}
                className="rounded-md border border-line px-3 py-1.5 text-[13px]"
              >
                Cancel
              </button>
              <button
                type="button"
                disabled={saving}
                onClick={() => void save()}
                className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[13px] text-canvas disabled:opacity-60"
              >
                {saving ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
                ) : (
                  <Save className="h-3.5 w-3.5" aria-hidden="true" />
                )}
                Save policy
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}
