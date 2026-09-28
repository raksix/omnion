"use client";

/**
 * `/observability/alerts` — the rules, their live state, the timeline, and the silences
 * (docs/requests/REQ-126, slice 4).
 *
 * The screen's job is to make four distinctions visible, each of which otherwise renders as the
 * same green or grey chip:
 *
 * - **`pending` is not `firing`.** A rule that has crossed its threshold but is still inside its
 *   dwell is *pending*, and the row says how long it has held and how much longer it must. A
 *   screen that collapsed the two would page an operator before the rule has proven it is real.
 * - **`no data` is not `under the threshold`.** A preview of a family that has never recorded
 *   says so. Both read as "not breaching", and only one of them is the operator's to fix — the
 *   other is a metric system that is not running.
 * - **A rule whose expression stopped parsing is not a healthy rule.** A family renamed in a
 *   later release leaves the row saying `configured` with a body that can never fire, so an
 *   invalid expression is a red row with the refusal under it, not a silent one.
 * - **A silence is not a resolution.** A silenced rule keeps its `firing` state and its history;
 *   the silence suppresses the notification, not the fact. Silencing during an incident is how an
 *   operator says "I know", and the timeline has to keep telling the truth afterwards.
 *
 * The form validates through the API rather than in the browser: the expression grammar lives in
 * the evaluator, and a form-side copy of it is a second grammar that drifts. So `Preview` is what
 * tells the operator whether their expression works, and a save is refused with a `422` naming the
 * family or the position.
 *
 * Keyboard: `/` focuses the filter, `n` opens the new form, `p` previews the expression being
 * typed, `Esc` closes the form. Under `sm:` the table becomes cards.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  BellOff,
  CheckCircle2,
  Eye,
  Info,
  Loader2,
  Plus,
  RefreshCw,
  Save,
  Search,
  Trash2,
  X,
  XCircle,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  createAlertRule,
  createSilence,
  deleteAlertRule,
  deleteSilence,
  fetchAlertRules,
  fetchAlerts,
  previewAlertRule,
  updateAlertRule,
  type AlertEventRow,
  type AlertPreview,
  type AlertRuleRow,
  type AlertRulesResponse,
  type AlertsResponse,
  type SilenceRow,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The severity chip. The three are distinguishable without relying on colour alone. */
const SEVERITY: Record<string, { tone: string; icon: typeof Info; meaning: string }> = {
  critical: {
    tone: "bg-danger-soft text-danger",
    icon: XCircle,
    meaning: "Pages immediately.",
  },
  warning: {
    tone: "bg-caution-soft text-caution",
    icon: AlertTriangle,
    meaning: "Wants attention during working hours.",
  },
  info: {
    tone: "bg-quiet-soft text-muted",
    icon: Info,
    meaning: "A note, not a page.",
  },
};

function severityOf(name: string) {
  return (
    SEVERITY[name] ?? {
      tone: "bg-quiet-soft text-muted",
      icon: Info,
      meaning: "No rule matches this severity.",
    }
  );
}

/** The state chip, and the sentence that goes with it. */
const STATE: Record<
  string,
  { tone: string; icon: typeof CheckCircle2; meaning: string }
> = {
  firing: {
    tone: "bg-danger-soft text-danger",
    icon: XCircle,
    meaning: "Over its threshold, past its dwell. Someone has been told.",
  },
  pending: {
    tone: "bg-caution-soft text-caution",
    icon: Loader2,
    meaning: "Over its threshold, still inside its dwell. Nobody has been told yet.",
  },
  resolved: {
    tone: "bg-positive-soft text-positive",
    icon: CheckCircle2,
    meaning: "Back under its threshold.",
  },
};

function stateOf(name: string | null) {
  if (name === null) {
    return {
      tone: "bg-quiet-soft text-muted",
      icon: CheckCircle2,
      meaning: "Under its threshold. The rule is being evaluated and is not complaining.",
    };
  }
  return (
    STATE[name] ?? {
      tone: "bg-quiet-soft text-muted",
      icon: Info,
      meaning: "No state rule matches this value.",
    }
  );
}

/** `1h 5m 3s` from seconds, for the dwell countdown. Never `0s` for a live countdown. */
function humanDuration(seconds: number): string {
  if (seconds < 60) return `${Math.max(0, Math.round(seconds))}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${Math.round(seconds % 60)}s`;
  return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
}

/** The instant `minutes` from now, as the RFC 3339 the API wants. */
function inMinutes(minutes: number): string {
  return new Date(Date.now() + minutes * 60_000).toISOString();
}

interface FormState {
  name: string;
  expr: string;
  severity: string;
  for_seconds: string;
  summary: string;
  runbook_url: string;
}

const BLANK: FormState = {
  name: "",
  expr: "",
  severity: "warning",
  for_seconds: "300",
  summary: "",
  runbook_url: "",
};

function toForm(row: AlertRuleRow): FormState {
  return {
    name: row.name,
    expr: row.expr,
    severity: row.severity,
    for_seconds: String(row.for_seconds),
    summary: row.summary,
    runbook_url: row.runbook_url ?? "",
  };
}

/** What the preview found, rendered as a verdict rather than as a number. */
function PreviewVerdict({ preview }: { preview: AlertPreview }) {
  if (preview.no_data) {
    // The distinction the module comment is about. A chip that said "not breaching" here would be
    // indistinguishable from a healthy system, and the operator's next hour would be spent
    // looking for a problem in the metrics rather than in the recorder.
    return (
      <div
        className="rounded-md border border-caution/30 bg-caution-soft p-3 text-sm text-caution"
        data-preview="no-data"
      >
        <p className="flex items-center gap-1.5 font-medium">
          <Info className="h-4 w-4" aria-hidden />
          No samples yet
        </p>
        <p className="mt-1 text-muted">
          <code className="font-mono">{preview.family}</code> has recorded nothing in this
          process. A rule on it will never fire — not because it is healthy, but because nothing
          is being measured. Check the request path before trusting a quiet dashboard.
        </p>
      </div>
    );
  }
  return (
    <div
      className={`rounded-md border p-3 text-sm ${
        preview.breaching
          ? "border-danger/30 bg-danger-soft text-danger"
          : "border-positive/30 bg-positive-soft text-positive"
      }`}
      data-preview={preview.breaching ? "breaching" : "ok"}
    >
      <p className="flex items-center gap-1.5 font-medium">
        {preview.breaching ? (
          <XCircle className="h-4 w-4" aria-hidden />
        ) : (
          <CheckCircle2 className="h-4 w-4" aria-hidden />
        )}
        {preview.breaching ? "Firing now" : "Not breaching now"}
      </p>
      <p className="mt-1 font-mono text-xs text-muted">
        {preview.rendered} → {preview.value ?? "—"} across {preview.series} series
      </p>
    </div>
  );
}

function AlertRuleForm({
  initial,
  catalogue,
  maxForSeconds,
  saving,
  error,
  onSave,
  onClose,
}: {
  initial: FormState;
  catalogue: AlertRulesResponse;
  maxForSeconds: number;
  saving: boolean;
  error: string | null;
  onSave: (form: FormState) => void;
  onClose: () => void;
}) {
  const [form, setForm] = useState<FormState>(initial);
  const [preview, setPreview] = useState<AlertPreview | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [previewing, setPreviewing] = useState(false);
  const nameRef = useRef<HTMLInputElement>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);

  useEffect(() => {
    nameRef.current?.focus();
  }, []);

  // `p` previews from anywhere in the form, and `Escape` closes it. Bound to the dialog, not the
  // document, so the shortcut does not fire while the operator is typing in the settings screen
  // behind this modal.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        onClose();
        return;
      }
      const target = event.target as HTMLElement | null;
      const typing = target?.tagName === "TEXTAREA";
      if (event.key === "p" && !typing && !(target?.tagName === "INPUT")) {
        event.preventDefault();
        void runPreview();
      }
    };
    // Declared before `runPreview` on purpose: the handler closes over the CURRENT form, so it
    // has to be re-bound whenever the expression changes, or `p` previews the expression from
    // when the form opened.
    void runPreview;
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [form.expr]);

  const set = <K extends keyof FormState>(key: K, value: FormState[K]) => {
    setForm((current) => ({ ...current, [key]: value }));
    // Any edit invalidates the previous verdict. Leaving it up would let the operator save a
    // rule on the strength of a preview that described a different expression.
    setPreview(null);
    setPreviewError(null);
  };

  async function runPreview() {
    if (!form.expr.trim()) {
      setPreviewError("Write an expression first — `<family> {labels} <op> <number>`.");
      return;
    }
    setPreviewing(true);
    setPreviewError(null);
    try {
      setPreview(await previewAlertRule(form.expr));
    } catch (caught) {
      setPreview(null);
      setPreviewError(
        caught instanceof ApiError
          ? caught.message
          : "The preview could not be run. Check the expression and try again.",
      );
    } finally {
      setPreviewing(false);
    }
  }

  const dwell = Number(form.for_seconds);
  const dwellInvalid = !Number.isFinite(dwell) || dwell < 0 || dwell > maxForSeconds;

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4"
      role="dialog"
      aria-modal="true"
      aria-labelledby="alert-rule-form-title"
      data-alert-rule-form
    >
      <div className="w-full max-w-2xl rounded-lg border border-line bg-panel p-5 shadow-xl">
        <div className="flex items-start justify-between gap-4">
          <h2 id="alert-rule-form-title" ref={headingRef} className="text-base font-semibold">
            {initial.name ? "Edit alert rule" : "New alert rule"}
          </h2>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="rounded p-1 text-muted hover:bg-quiet-soft"
          >
            <X className="h-4 w-4" aria-hidden />
          </button>
        </div>

        <div className="mt-4 grid gap-4">
          <label className="grid gap-1.5 text-sm">
            <span className="font-medium text-ink">Name</span>
            <input
              ref={nameRef}
              value={form.name}
              onChange={(event) => set("name", event.target.value)}
              placeholder="HighErrorRate"
              className="rounded-md border border-line bg-surface px-3 py-2 font-mono text-sm"
              data-alert-rule-name
            />
            <span className="text-xs text-muted">
              How a notification attributes itself. Names are unique.
            </span>
          </label>

          <label className="grid gap-1.5 text-sm">
            <span className="font-medium text-ink">Expression</span>
            <textarea
              value={form.expr}
              onChange={(event) => set("expr", event.target.value)}
              rows={2}
              spellCheck={false}
              placeholder='omnion_http_requests_total{status="5xx"} > 0.05'
              className="rounded-md border border-line bg-surface px-3 py-2 font-mono text-sm"
              data-alert-rule-expr
            />
            <span className="text-xs text-muted">
              A family from the catalogue, optional label matchers, and one comparison. This is a
              closed grammar, not PromQL — the evaluator has to be able to explain every rule the
              panel shows.
            </span>
          </label>

          <div className="flex flex-wrap items-center gap-2">
            <button
              type="button"
              onClick={() => void runPreview()}
              disabled={previewing}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-sm hover:bg-quiet-soft disabled:opacity-50"
              data-alert-rule-preview
            >
              {previewing ? (
                <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
              ) : (
                <Eye className="h-4 w-4" aria-hidden />
              )}
              Preview
              <kbd className="ml-1 rounded border border-line px-1 text-[10px] text-muted">p</kbd>
            </button>
            {preview && <PreviewVerdict preview={preview} />}
          </div>
          {previewError && (
            <p className="text-sm text-danger" data-preview-error>
              {previewError}
            </p>
          )}

          <div className="grid gap-4 sm:grid-cols-2">
            <label className="grid gap-1.5 text-sm">
              <span className="font-medium text-ink">Severity</span>
              <select
                value={form.severity}
                onChange={(event) => set("severity", event.target.value)}
                className="rounded-md border border-line bg-surface px-3 py-2"
                data-alert-rule-severity
              >
                {catalogue.severities.map((value) => (
                  <option key={value} value={value}>
                    {value}
                  </option>
                ))}
              </select>
              <span className="text-xs text-muted">{severityOf(form.severity).meaning}</span>
            </label>

            <label className="grid gap-1.5 text-sm">
              <span className="font-medium text-ink">For (seconds)</span>
              <input
                value={form.for_seconds}
                onChange={(event) => set("for_seconds", event.target.value)}
                inputMode="numeric"
                className={`rounded-md border bg-surface px-3 py-2 font-mono text-sm ${
                  dwellInvalid ? "border-danger" : "border-line"
                }`}
                data-alert-rule-dwell
              />
              <span className="text-xs text-muted">
                {dwellInvalid ? (
                  <span className="text-danger">
                    Must be between 0 and {maxForSeconds}. 0 fires on the first breaching sample.
                  </span>
                ) : (
                  `Hold the threshold for ${humanDuration(dwell)} before firing. 0 fires immediately.`
                )}
              </span>
            </label>
          </div>

          <label className="grid gap-1.5 text-sm">
            <span className="font-medium text-ink">Summary</span>
            <input
              value={form.summary}
              onChange={(event) => set("summary", event.target.value)}
              placeholder="More than 5% of requests answered 5xx."
              className="rounded-md border border-line bg-surface px-3 py-2 text-sm"
              data-alert-rule-summary
            />
            <span className="text-xs text-muted">
              The one line a notification leads with. Write it for 3am.
            </span>
          </label>

          <label className="grid gap-1.5 text-sm">
            <span className="font-medium text-ink">Runbook URL</span>
            <input
              value={form.runbook_url}
              onChange={(event) => set("runbook_url", event.target.value)}
              placeholder="https://runbook.example/error-rate"
              className="rounded-md border border-line bg-surface px-3 py-2 font-mono text-sm"
              data-alert-rule-runbook
            />
          </label>
        </div>

        {error && (
          <p className="mt-4 rounded-md border border-danger/30 bg-danger-soft p-3 text-sm text-danger" data-alert-rule-error>
            {error}
          </p>
        )}

        <div className="mt-5 flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md border border-line px-3 py-1.5 text-sm hover:bg-quiet-soft"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={() => onSave(form)}
            disabled={saving || dwellInvalid}
            className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3 py-1.5 text-sm text-white disabled:opacity-50"
            data-alert-rule-save
          >
            {saving ? (
              <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
            ) : (
              <Save className="h-4 w-4" aria-hidden />
            )}
            Save rule
          </button>
        </div>
      </div>
    </div>
  );
}

function SilenceForm({
  ruleName,
  onSave,
  onClose,
  saving,
  error,
}: {
  ruleName: string | null;
  onSave: (reason: string, minutes: number) => void;
  onClose: () => void;
  saving: boolean;
  error: string | null;
}) {
  const [reason, setReason] = useState("");
  const [minutes, setMinutes] = useState("60");
  const reasonRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    reasonRef.current?.focus();
  }, []);

  const duration = Number(minutes);
  const invalid = !Number.isFinite(duration) || duration < 1 || duration > 7 * 24 * 60;

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4"
      role="dialog"
      aria-modal="true"
      aria-labelledby="silence-form-title"
      data-silence-form
    >
      <div className="w-full max-w-md rounded-lg border border-line bg-panel p-5 shadow-xl">
        <div className="flex items-start justify-between gap-4">
          <h2 id="silence-form-title" className="text-base font-semibold">
            Silence {ruleName ?? "every rule"}
          </h2>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="rounded p-1 text-muted hover:bg-quiet-soft"
          >
            <X className="h-4 w-4" aria-hidden />
          </button>
        </div>

        <p className="mt-2 text-sm text-muted">
          A silence suppresses the <em>notification</em>. A firing rule keeps firing and keeps its
          history — silencing during an incident is how you say &quot;I know&quot;, not how you make
          it stop.
        </p>

        <div className="mt-4 grid gap-4">
          <label className="grid gap-1.5 text-sm">
            <span className="font-medium text-ink">Reason</span>
            <input
              ref={reasonRef}
              value={reason}
              onChange={(event) => setReason(event.target.value)}
              placeholder="Database maintenance window"
              className="rounded-md border border-line bg-surface px-3 py-2 text-sm"
              data-silence-reason
            />
            <span className="text-xs text-muted">
              Required. A silence with no reason is one nobody dares to remove.
            </span>
          </label>

          <label className="grid gap-1.5 text-sm">
            <span className="font-medium text-ink">For (minutes)</span>
            <input
              value={minutes}
              onChange={(event) => setMinutes(event.target.value)}
              inputMode="numeric"
              className={`rounded-md border bg-surface px-3 py-2 font-mono text-sm ${
                invalid ? "border-danger" : "border-line"
              }`}
              data-silence-minutes
            />
            <span className="text-xs text-muted">
              {invalid ? (
                <span className="text-danger">Must be between 1 and 10080 (7 days).</span>
              ) : (
                `Ends ${new Date(Date.now() + duration * 60_000).toLocaleString()}.`
              )}
            </span>
          </label>
        </div>

        {error && (
          <p className="mt-4 rounded-md border border-danger/30 bg-danger-soft p-3 text-sm text-danger" data-silence-error>
            {error}
          </p>
        )}

        <div className="mt-5 flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md border border-line px-3 py-1.5 text-sm hover:bg-quiet-soft"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={() => onSave(reason, duration)}
            disabled={saving || invalid || !reason.trim()}
            className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3 py-1.5 text-sm text-white disabled:opacity-50"
            data-silence-save
          >
            {saving ? (
              <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
            ) : (
              <BellOff className="h-4 w-4" aria-hidden />
            )}
            Silence
          </button>
        </div>
      </div>
    </div>
  );
}

/** One event on the timeline. */
function EventRow({ event, ruleName }: { event: AlertEventRow; ruleName: string | null }) {
  const state = stateOf(event.state);
  const Icon = state.icon;
  return (
    <tr data-alert-event={event.state}>
      <td className="px-3 py-2 text-sm text-ink">
        <span className="font-medium">{ruleName ?? "a deleted rule"}</span>
      </td>
      <td className="px-3 py-2">
        <span
          className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-xs font-medium ${state.tone}`}
          title={state.meaning}
        >
          <Icon className="h-3 w-3" aria-hidden />
          {event.state}
        </span>
      </td>
      <td className="px-3 py-2 font-mono text-xs tabular-nums text-muted">
        {event.firing_value ?? event.value ?? "—"}
      </td>
      <td className="px-3 py-2 text-xs text-muted">
        {formatTimestamp(event.started_at)}
        {event.ended_at && (
          <span className="block text-muted">
            → {formatTimestamp(event.ended_at)}
          </span>
        )}
      </td>
      <td className="px-3 py-2 text-xs text-muted">{event.reason}</td>
      <td className="px-3 py-2">
        {event.notified ? (
          <span
            className="inline-flex items-center gap-1 text-xs text-positive"
            title="A notification was claimed for this event, exactly once."
          >
            <CheckCircle2 className="h-3.5 w-3.5" aria-hidden />
            notified
          </span>
        ) : (
          <span
            className="text-xs text-muted"
            title="Still firing, or a pending event that has not fired yet."
          >
            —
          </span>
        )}
      </td>
    </tr>
  );
}

export function AlertsView() {
  const [catalogue, setCatalogue] = useState<AlertRulesResponse | null>(null);
  const [alerts, setAlerts] = useState<AlertsResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [filter, setFilter] = useState("");
  const [form, setForm] = useState<{ open: boolean; initial: FormState; id: string | null }>({
    open: false,
    initial: BLANK,
    id: null,
  });
  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [silence, setSilence] = useState<{ ruleId: string | null; ruleName: string | null } | null>(
    null,
  );
  const [silenceError, setSilenceError] = useState<string | null>(null);
  const filterRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    setRefreshing(true);
    try {
      const [rules, states] = await Promise.all([fetchAlertRules(), fetchAlerts()]);
      setCatalogue(rules);
      setAlerts(states);
      setError(null);
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The alert state could not be read.");
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.tagName === "SELECT";
      if (typing) return;
      if (event.key === "/") {
        event.preventDefault();
        filterRef.current?.focus();
      } else if (event.key === "n") {
        event.preventDefault();
        setFormError(null);
        setForm({ open: true, initial: BLANK, id: null });
      } else if (event.key === "r") {
        event.preventDefault();
        void load();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load]);

  const rules = catalogue?.rules ?? [];
  const visible = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    if (!needle) return rules;
    return rules.filter(
      (rule) =>
        rule.name.toLowerCase().includes(needle) ||
        rule.expr.toLowerCase().includes(needle) ||
        rule.summary.toLowerCase().includes(needle),
    );
  }, [rules, filter]);

  const ruleNames = useMemo(() => {
    const map = new Map<string, string>();
    for (const rule of rules) map.set(rule.id, rule.name);
    return map;
  }, [rules]);

  async function saveRule(formState: FormState) {
    setSaving(true);
    setFormError(null);
    const forSeconds = Number(formState.for_seconds);
    try {
      if (form.id) {
        await updateAlertRule(form.id, {
          name: formState.name.trim(),
          expr: formState.expr.trim(),
          severity: formState.severity,
          for_seconds: Number.isFinite(forSeconds) ? forSeconds : 0,
          summary: formState.summary,
          runbook_url: formState.runbook_url || null,
        });
      } else {
        await createAlertRule({
          name: formState.name.trim(),
          expr: formState.expr.trim(),
          severity: formState.severity,
          for_seconds: Number.isFinite(forSeconds) ? forSeconds : 300,
          summary: formState.summary,
          runbook_url: formState.runbook_url || null,
        });
      }
      setForm({ open: false, initial: BLANK, id: null });
      await load();
    } catch (caught) {
      setFormError(caught instanceof ApiError ? caught.message : "The rule could not be saved.");
    } finally {
      setSaving(false);
    }
  }

  async function removeRule(rule: AlertRuleRow) {
    try {
      await deleteAlertRule(rule.id);
      await load();
    } catch (caught) {
      // A bundled rule's refusal is the interesting one, and it arrives here verbatim — the API
      // explains that it is re-seeded at every boot. Swallowing it would leave the operator
      // clicking a button that silently does nothing.
      setError(caught instanceof ApiError ? caught.message : "The rule could not be deleted.");
    }
  }

  async function toggleRule(rule: AlertRuleRow) {
    try {
      await updateAlertRule(rule.id, { enabled: !rule.enabled });
      await load();
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The rule could not be toggled.");
    }
  }

  async function saveSilence(reason: string, minutes: number) {
    setSaving(true);
    setSilenceError(null);
    try {
      await createSilence({
        rule_id: silence?.ruleId ?? null,
        reason: reason.trim(),
        ends_at: inMinutes(minutes),
      });
      setSilence(null);
      await load();
    } catch (caught) {
      setSilenceError(
        caught instanceof ApiError ? caught.message : "The silence could not be created.",
      );
    } finally {
      setSaving(false);
    }
  }

  async function liftSilence(row: SilenceRow) {
    try {
      await deleteSilence(row.id);
      await load();
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The silence could not be lifted.");
    }
  }

  if (loading) return <LoadingTable columns={6} rows={4} />;

  const counts = alerts?.counts;

  return (
    <div className="grid gap-6" data-view="observability-alerts" data-alerts-view>
      {error && (
        <p
          className="rounded-md border border-danger/30 bg-danger-soft p-3 text-sm text-danger"
          data-alerts-error
        >
          {error}
        </p>
      )}

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        {[
          { label: "Firing", value: counts?.firing ?? 0, tone: "text-danger" },
          { label: "Pending", value: counts?.pending ?? 0, tone: "text-caution" },
          { label: "Silenced", value: counts?.silenced ?? 0, tone: "text-muted" },
          {
            label: "Worst severity",
            value: counts?.worst_severity ?? "—",
            tone: "text-ink",
          },
        ].map((stat) => (
          <div
            key={stat.label}
            className="rounded-lg border border-line bg-panel p-4"
            data-alert-stat={stat.label}
          >
            <p className="text-xs uppercase tracking-wide text-muted">{stat.label}</p>
            <p className={`mt-1 text-2xl font-semibold tabular-nums ${stat.tone}`}>{stat.value}</p>
          </div>
        ))}
      </div>

      <section className="grid gap-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 className="text-sm font-semibold uppercase tracking-wide text-muted">
            Rules ({visible.length}
            {rules.length !== visible.length ? ` of ${rules.length}` : ""})
          </h2>
          <div className="flex items-center gap-2">
            <label className="relative">
              <span className="sr-only">Filter rules</span>
              <Search
                className="pointer-events-none absolute left-2 top-1/2 h-4 w-4 -translate-y-1/2 text-muted"
                aria-hidden
              />
              <input
                ref={filterRef}
                value={filter}
                onChange={(event) => setFilter(event.target.value)}
                placeholder="Filter rules"
                className="rounded-md border border-line bg-surface py-1.5 pl-8 pr-2 text-sm"
                data-alert-filter
              />
            </label>
            <button
              type="button"
              onClick={() => void load()}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-sm hover:bg-quiet-soft"
              data-alert-refresh
            >
              <RefreshCw
                className={`h-4 w-4 ${refreshing ? "animate-spin" : ""}`}
                aria-hidden
              />
              Refresh
            </button>
            <button
              type="button"
              onClick={() => {
                setFormError(null);
                setForm({ open: true, initial: BLANK, id: null });
              }}
              className="inline-flex items-center gap-1.5 rounded-md bg-accent px-2.5 py-1.5 text-sm text-white"
              data-alert-new
            >
              <Plus className="h-4 w-4" aria-hidden />
              New rule
              <kbd className="ml-1 rounded border border-white/30 px-1 text-[10px]">n</kbd>
            </button>
          </div>
        </div>

        {visible.length === 0 ? (
          <EmptyState
            title={rules.length === 0 ? "No alert rules yet" : "No rule matches that filter"}
            hint={
              rules.length === 0
                ? "A rule watches one metric family and says so when it crosses. Without any, an outage is only visible on a dashboard somebody happens to be looking at."
                : "Try a shorter filter — it matches the name, the expression and the summary."
            }
            action={
              rules.length === 0 ? (
                <button
                  type="button"
                  onClick={() => {
                    setFormError(null);
                    setForm({ open: true, initial: BLANK, id: null });
                  }}
                  className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3 py-1.5 text-sm text-white"
                >
                  <Plus className="h-4 w-4" aria-hidden />
                  Write the first rule
                </button>
              ) : undefined
            }
          />
        ) : (
          <div className="overflow-x-auto rounded-lg border border-line">
            <table className="w-full min-w-[720px] border-collapse text-left">
              <thead className="bg-quiet-soft text-xs uppercase tracking-wide text-muted">
                <tr>
                  <th className="px-3 py-2 font-medium">Rule</th>
                  <th className="px-3 py-2 font-medium">State</th>
                  <th className="px-3 py-2 font-medium">Value</th>
                  <th className="px-3 py-2 font-medium">Severity</th>
                  <th className="px-3 py-2 font-medium">Expression</th>
                  <th className="px-3 py-2 font-medium">Actions</th>
                </tr>
              </thead>
              <tbody>
                {visible.map((rule) => {
                  const state = stateOf(rule.state);
                  const StateIcon = state.icon;
                  const severity = severityOf(rule.severity);
                  const SeverityIcon = severity.icon;
                  return (
                    <tr
                      key={rule.id}
                      className="border-t border-line align-top"
                      data-alert-rule={rule.name}
                    >
                      <td className="px-3 py-2">
                        <p className="text-sm font-medium text-ink">{rule.name}</p>
                        <p className="text-xs text-muted">{rule.summary || "No summary."}</p>
                        {rule.source === "bundled" && (
                          <span className="mt-1 inline-block rounded bg-quiet-soft px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-muted">
                            bundled
                          </span>
                        )}
                      </td>
                      <td className="px-3 py-2">
                        <span
                          className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-xs font-medium ${state.tone}`}
                          title={state.meaning}
                          data-alert-rule-state
                        >
                          <StateIcon className="h-3 w-3" aria-hidden />
                          {rule.state ?? "ok"}
                        </span>
                        {rule.silenced && (
                          <span
                            className="mt-1 flex items-center gap-1 text-xs text-muted"
                            title={`Silenced until ${formatTimestamp(rule.silenced_until ?? "")}`}
                          >
                            <BellOff className="h-3 w-3" aria-hidden />
                            silenced
                          </span>
                        )}
                      </td>
                      <td className="px-3 py-2 font-mono text-xs tabular-nums text-muted">
                        {rule.value ?? "—"}
                      </td>
                      <td className="px-3 py-2">
                        <span
                          className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-xs font-medium ${severity.tone}`}
                          title={severity.meaning}
                        >
                          <SeverityIcon className="h-3 w-3" aria-hidden />
                          {rule.severity}
                        </span>
                      </td>
                      <td className="px-3 py-2">
                        {/* An expression that stopped parsing is a RED cell with the refusal
                            under it. A grey cell would render a rule that can never fire as
                            indistinguishable from one that is merely quiet. */}
                        {rule.expression_valid ? (
                          <code className="font-mono text-xs text-muted">{rule.expr}</code>
                        ) : (
                          <p className="text-xs text-danger" data-alert-rule-invalid>
                            This expression no longer parses:{" "}
                            {rule.expression_error}. The rule cannot fire.
                          </p>
                        )}
                        <p className="mt-0.5 text-[11px] text-muted">
                          holds {humanDuration(rule.for_seconds)} ·{" "}
                          {rule.enabled ? "evaluating" : "disabled"}
                        </p>
                      </td>
                      <td className="px-3 py-2">
                        <div className="flex flex-wrap items-center gap-1.5">
                          <button
                            type="button"
                            onClick={() => {
                              setFormError(null);
                              setForm({ open: true, initial: toForm(rule), id: rule.id });
                            }}
                            className="rounded border border-line px-2 py-1 text-xs hover:bg-quiet-soft"
                            data-alert-rule-edit
                          >
                            Edit
                          </button>
                          <button
                            type="button"
                            onClick={() => void toggleRule(rule)}
                            className="rounded border border-line px-2 py-1 text-xs hover:bg-quiet-soft"
                            data-alert-rule-toggle
                          >
                            {rule.enabled ? "Disable" : "Enable"}
                          </button>
                          <button
                            type="button"
                            onClick={() => {
                              setSilenceError(null);
                              setSilence({ ruleId: rule.id, ruleName: rule.name });
                            }}
                            className="rounded border border-line px-2 py-1 text-xs hover:bg-quiet-soft"
                            data-alert-rule-silence
                          >
                            Silence
                          </button>
                          <button
                            type="button"
                            onClick={() => void removeRule(rule)}
                            className="rounded border border-line px-2 py-1 text-xs text-danger hover:bg-danger-soft"
                            data-alert-rule-delete
                          >
                            <Trash2 className="inline h-3 w-3" aria-hidden />
                          </button>
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </section>

      <section className="grid gap-3">
        <h2 className="text-sm font-semibold uppercase tracking-wide text-muted">Silences</h2>
        {(alerts?.silences.length ?? 0) === 0 ? (
          <p className="rounded-lg border border-line bg-panel p-4 text-sm text-muted">
            No silences. Every firing rule notifies.
          </p>
        ) : (
          <ul className="grid gap-2" data-silence-list>
            {alerts?.silences.map((row) => (
              <li
                key={row.id}
                className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-line bg-panel p-3"
              >
                <div>
                  <p className="text-sm font-medium text-ink">
                    {row.rule_id ? (ruleNames.get(row.rule_id) ?? "a deleted rule") : "Every rule"}
                    {row.active && (
                      <span className="ml-2 rounded bg-quiet-soft px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-muted">
                        active
                      </span>
                    )}
                  </p>
                  <p className="text-xs text-muted">
                    {row.reason} · ends {formatTimestamp(row.ends_at)} (
                    {humanDuration(row.minutes_remaining * 60)} left)
                  </p>
                </div>
                <button
                  type="button"
                  onClick={() => void liftSilence(row)}
                  className="rounded border border-line px-2 py-1 text-xs hover:bg-quiet-soft"
                  data-silence-lift
                >
                  Lift now
                </button>
              </li>
            ))}
          </ul>
        )}
        <div>
          <button
            type="button"
            onClick={() => {
              setSilenceError(null);
              setSilence({ ruleId: null, ruleName: null });
            }}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-sm hover:bg-quiet-soft"
            data-silence-new
          >
            <BellOff className="h-4 w-4" aria-hidden />
            Silence every rule
          </button>
        </div>
      </section>

      <section className="grid gap-3">
        <h2 className="text-sm font-semibold uppercase tracking-wide text-muted">Timeline</h2>
        {(alerts?.firing.length ?? 0) + (alerts?.pending.length ?? 0) + (alerts?.resolved.length ?? 0) ===
        0 ? (
          <p className="rounded-lg border border-line bg-panel p-4 text-sm text-muted">
            No events yet. The evaluator runs every 15 seconds; the first event appears when a
            rule crosses its threshold.
          </p>
        ) : (
          <div className="overflow-x-auto rounded-lg border border-line">
            <table className="w-full min-w-[640px] border-collapse text-left">
              <thead className="bg-quiet-soft text-xs uppercase tracking-wide text-muted">
                <tr>
                  <th className="px-3 py-2 font-medium">Rule</th>
                  <th className="px-3 py-2 font-medium">State</th>
                  <th className="px-3 py-2 font-medium">Value</th>
                  <th className="px-3 py-2 font-medium">Window</th>
                  <th className="px-3 py-2 font-medium">Reason</th>
                  <th className="px-3 py-2 font-medium">Notified</th>
                </tr>
              </thead>
              <tbody>
                {[...(alerts?.firing ?? []), ...(alerts?.pending ?? []), ...(alerts?.resolved ?? [])].map(
                  (event) => (
                    <EventRow
                      key={event.id}
                      event={event}
                      ruleName={ruleNames.get(event.rule_id) ?? null}
                    />
                  ),
                )}
              </tbody>
            </table>
          </div>
        )}
      </section>

      {form.open && catalogue && (
        <AlertRuleForm
          initial={form.initial}
          catalogue={catalogue}
          maxForSeconds={catalogue.max_for_seconds}
          saving={saving}
          error={formError}
          onSave={(next) => void saveRule(next)}
          onClose={() => setForm({ open: false, initial: BLANK, id: null })}
        />
      )}

      {silence && (
        <SilenceForm
          ruleName={silence.ruleName}
          saving={saving}
          error={silenceError}
          onSave={(reason, minutes) => void saveSilence(reason, minutes)}
          onClose={() => setSilence(null)}
        />
      )}
    </div>
  );
}
