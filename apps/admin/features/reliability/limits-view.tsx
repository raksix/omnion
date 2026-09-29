"use client";

/**
 * `/settings/reliability/limits` — the platform-wide rate-limit policies, the refusal rollup, and
 * the dry-run an operator reaches for at 3am (docs/requests/REQ-127, slice 1).
 *
 * Four things this screen refuses to blur, because each pair otherwise renders as the same chip
 * and each costs an operator an hour of widening the wrong document:
 *
 * - **A stored policy is not an enforced one.** The platform limiter runs before the router
 *   publishes a matched route, so a `route`-scoped row is stored and listed but cannot be spent
 *   by this layer. The row says `not enforced here` rather than showing a budget that no request
 *   will ever meet.
 * - **`Unlimited` is not `Allowed { remaining: 0 }`.** No policy at all means the budget is not
 *   finite; the tile says "no policy applies" instead of "0 left", because a zero is a claim about
 *   a policy and there is none.
 * - **`Uncounted` is not `Allowed`.** The counter was unreadable and the deployment fails open:
 *   the request went through and **nothing was counted**, so there is no `remaining` to show. A
 *   number here would be a measurement nobody took.
 * - **A `429` from this layer is not a `429` from the gateway.** Two limiters are in the chain,
 *   so every refusal names which document answered. The header shown next to the verdict is the
 *   evidence for that claim.
 *
 * The dry-run is a **server** call, not a local computation: it must name the same policy the
 * middleware resolves, and only the server holds that resolver. It also does not spend the budget
 * it measures, so an operator can ask "what would happen to this caller" without the asking
 * becoming the thing that refuses them.
 *
 * Keyboard: `/` filters, `n` opens the new-policy form, `e` runs the dry-run, `Esc` closes.
 * Under `sm:` the table becomes cards and the numeric validation messages stay visible.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  CheckCircle2,
  Gauge,
  Info,
  Loader2,
  Plus,
  RefreshCw,
  Save,
  Search,
  ShieldAlert,
  Trash2,
  X,
  XCircle,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  createReliabilityPolicy,
  deleteReliabilityPolicy,
  evaluateReliabilityLimit,
  fetchReliabilityPolicies,
  fetchReliabilityRefusals,
  reliabilityVerdictKind,
  updateReliabilityPolicy,
  type ReliabilityEvaluated,
  type ReliabilityPolicies,
  type ReliabilityPolicy,
  type ReliabilityPolicyInput,
  type ReliabilityRefusal,
} from "@/lib/api";

/** A blank form. Every field the API requires is present, so the form cannot post a hole. */
const BLANK: ReliabilityPolicyInput = {
  name: "",
  scope: "user",
  target_id: null,
  route_pattern: null,
  limit_count: 60,
  window_seconds: 60,
  burst: 0,
  priority: 100,
  enabled: true,
};

/** A window rendered in the unit an operator thinks in, never as a bare second count. */
function humanWindow(seconds: number): string {
  if (seconds % 86400 === 0 && seconds >= 86400) {
    const days = seconds / 86400;
    return days === 1 ? "1 day" : `${days} days`;
  }
  if (seconds % 3600 === 0 && seconds >= 3600) return `${seconds / 3600} hour${seconds === 3600 ? "" : "s"}`;
  if (seconds % 60 === 0 && seconds >= 60) return `${seconds / 60} minute${seconds === 60 ? "" : "s"}`;
  return `${seconds} second${seconds === 1 ? "" : "s"}`;
}

/** `user` → `User`, for a column that is otherwise four lowercase words. */
function humanScope(scope: string): string {
  return scope.charAt(0).toUpperCase() + scope.slice(1);
}

/**
 * The reason the screen shows for a verdict, in the operator's language rather than the enum's.
 *
 * Reads the fields directly rather than narrowing on the variant, because the API tags the
 * variant as a `decision` KEY with the fields alongside it — a client written for serde's
 * external tagging would find every field `undefined` and print a confident sentence about
 * nothing.
 */
function verdictSentence(result: ReliabilityEvaluated): string {
  const v = result.verdict;
  if (v.decision === "allowed") {
    return `Allowed — ${v.remaining ?? 0} of ${v.limit ?? 0} left in this window.`;
  }
  if (v.decision === "limited") {
    return `Refused — over the ${v.ceiling ?? 0}-request ceiling. Retry in ${v.retry_after ?? 0}s.`;
  }
  if (v.decision === "unlimited") {
    return "Allowed — no policy applies, so there is no budget to spend. This is not a zero budget.";
  }
  if (v.decision === "uncounted") {
    return `Allowed but NOT counted — the ${v.scope ?? "matching"} policy applies, the counter was unreadable, and this deployment fails open. No budget was spent.`;
  }
  return "Refused because the counter was unreadable and this deployment fails closed. No retry time is published — the platform cannot compute one.";
}

/** One refusal rollup, and the maximum in the table, so the bars are scaled against each other. */
function RefusalBars({ refusals }: { refusals: ReliabilityRefusal[] }) {
  const worst = Math.max(1, ...refusals.map((row) => row.refusals));
  return (
    <div className="space-y-2">
      {refusals.map((row) => {
        const key = `${row.scope}|${row.target_id ?? ""}|${row.route}|${row.window_start}`;
        const width = Math.round((row.refusals / worst) * 100);
        return (
          <div key={key} className="grid grid-cols-1 gap-1 sm:grid-cols-[12rem_1fr_4rem] sm:items-center">
            <span className="truncate text-[12.5px] text-muted">
              {humanScope(row.scope)} · <span className="font-mono text-[12px]">{row.route}</span>
            </span>
            <span className="h-2 w-full overflow-hidden rounded-full bg-quiet-soft">
              <span
                className="block h-full rounded-full bg-amber-500/70"
                style={{ width: `${width}%` }}
                data-testid="refusal-bar"
                data-refusals={row.refusals}
              />
            </span>
            <span className="text-right text-[12.5px] tabular-nums">{row.refusals}</span>
          </div>
        );
      })}
    </div>
  );
}

/** The policy form. Validation is thin on purpose — the API owns every range and names the field. */
function PolicyForm({
  vocabulary,
  editing,
  onSaved,
  onCancel,
}: {
  vocabulary: string[];
  editing: ReliabilityPolicy | null;
  onSaved: (message: string) => void;
  onCancel: () => void;
}) {
  const [form, setForm] = useState<ReliabilityPolicyInput>(() =>
    editing
      ? {
          name: editing.name,
          scope: editing.scope,
          target_id: editing.target_id,
          route_pattern: editing.route_pattern,
          limit_count: editing.limit_count,
          window_seconds: editing.window_seconds,
          burst: editing.burst,
          priority: editing.priority,
          enabled: editing.enabled,
        }
      : BLANK,
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ApiError | null>(null);

  function set<K extends keyof ReliabilityPolicyInput>(key: K, value: ReliabilityPolicyInput[K]) {
    setForm((previous) => ({ ...previous, [key]: value }));
  }

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      // An empty string is the form's way of saying "every subject in the scope"; the column is
      // nullable, so it goes as null rather than as an empty target that would match nothing.
      const payload: ReliabilityPolicyInput = {
        ...form,
        target_id: form.target_id?.trim() ? form.target_id.trim() : null,
        route_pattern: form.route_pattern?.trim() ? form.route_pattern.trim() : null,
      };
      if (editing?.id) {
        await updateReliabilityPolicy(editing.id, payload);
        onSaved(`Policy “${payload.name}” saved. It applies to the next request — no restart.`);
      } else {
        await createReliabilityPolicy(payload);
        onSaved(`Policy “${payload.name}” created.`);
      }
    } catch (caught) {
      setError(caught instanceof ApiError ? caught : new ApiError(0, "request_failed", String(caught)));
    } finally {
      setBusy(false);
    }
  }

  const field =
    "w-full rounded-md border border-line bg-elevated px-2.5 py-1.5 text-[13px] outline-none focus-visible:border-accent";
  const label = "block text-[12px] font-medium text-muted";

  return (
    <form
      onSubmit={submit}
      data-testid="policy-form"
      className="space-y-4 rounded-lg border border-line bg-panel p-4"
    >
      <h3 className="text-[13.5px] font-medium">
        {editing ? `Edit “${editing.name}”` : "New rate-limit policy"}
      </h3>

      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <div>
          <label className={label} htmlFor="policy-name">Name</label>
          <input
            id="policy-name"
            className={field}
            value={form.name}
            onChange={(e) => set("name", e.target.value)}
            placeholder="API burst ceiling"
            required
          />
        </div>
        <div>
          <label className={label} htmlFor="policy-scope">Scope</label>
          <select
            id="policy-scope"
            className={field}
            value={form.scope}
            onChange={(e) => set("scope", e.target.value)}
          >
            {vocabulary.map((scope) => (
              <option key={scope} value={scope}>{humanScope(scope)}</option>
            ))}
          </select>
          <p className="mt-1 text-[11.5px] text-muted">
            {form.scope === "route"
              ? "A route policy is stored here, but this layer runs before the router matches a route — the row will be marked “not enforced here”."
              : "The subject this budget is spent against."}
          </p>
        </div>
        <div>
          <label className={label} htmlFor="policy-target">Target id (optional)</label>
          <input
            id="policy-target"
            className={field}
            value={form.target_id ?? ""}
            onChange={(e) => set("target_id", e.target.value)}
            placeholder="Empty = every subject in the scope"
          />
        </div>
        <div>
          <label className={label} htmlFor="policy-route">Route pattern (optional)</label>
          <input
            id="policy-route"
            className={field}
            value={form.route_pattern ?? ""}
            onChange={(e) => set("route_pattern", e.target.value)}
            placeholder="/api/v1/public/*"
          />
        </div>
        <div>
          <label className={label} htmlFor="policy-limit">Limit (requests per window)</label>
          <input
            id="policy-limit"
            type="number"
            min={1}
            className={field}
            value={form.limit_count}
            onChange={(e) => set("limit_count", Number(e.target.value))}
            required
          />
        </div>
        <div>
          <label className={label} htmlFor="policy-window">Window (seconds)</label>
          <input
            id="policy-window"
            type="number"
            min={1}
            max={86400}
            className={field}
            value={form.window_seconds}
            onChange={(e) => set("window_seconds", Number(e.target.value))}
            required
          />
          <p className="mt-1 text-[11.5px] text-muted">{humanWindow(form.window_seconds || 0)}</p>
        </div>
        <div>
          <label className={label} htmlFor="policy-burst">Burst headroom</label>
          <input
            id="policy-burst"
            type="number"
            min={0}
            className={field}
            value={form.burst}
            onChange={(e) => set("burst", Number(e.target.value))}
          />
          <p className="mt-1 text-[11.5px] text-muted">
            Ceiling is {form.limit_count + (form.burst ?? 0)} inside the same window.
          </p>
        </div>
        <div>
          <label className={label} htmlFor="policy-priority">Priority (lower wins)</label>
          <input
            id="policy-priority"
            type="number"
            className={field}
            value={form.priority}
            onChange={(e) => set("priority", Number(e.target.value))}
          />
        </div>
      </div>

      <label className="flex items-center gap-2 text-[12.5px]">
        <input
          type="checkbox"
          checked={form.enabled}
          onChange={(e) => set("enabled", e.target.checked)}
        />
        Enforced
      </label>

      {error ? (
        <p className="flex items-start gap-1.5 text-[12.5px] text-danger" data-testid="form-error">
          <XCircle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>
            {error.message}
            {typeof error.details?.field === "string" ? ` (field: ${error.details.field})` : ""}
            {typeof error.details?.request_id === "string" ? ` · request ${error.details.request_id}` : ""}
          </span>
        </p>
      ) : null}

      <div className="flex gap-2">
        <button
          type="submit"
          disabled={busy}
          data-testid="policy-save"
          className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3 py-1.5 text-[12.5px] font-medium text-accent-fg disabled:opacity-60"
        >
          {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <Save className="size-3.5" aria-hidden />}
          {editing ? "Save policy" : "Create policy"}
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px]"
        >
          <X className="size-3.5" aria-hidden />
          Cancel
        </button>
      </div>
    </form>
  );
}

/** The dry-run: the four facts off a request, and the server's own answer about them. */
function DryRun({ vocabulary }: { vocabulary: string[] }) {
  const [scope, setScope] = useState("");
  const [userId, setUserId] = useState("");
  const [ip, setIp] = useState("");
  const [route, setRoute] = useState("");
  const [count, setCount] = useState("");
  const [result, setResult] = useState<ReliabilityEvaluated | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [busy, setBusy] = useState(false);

  async function run() {
    setBusy(true);
    setError(null);
    try {
      setResult(
        await evaluateReliabilityLimit({
          scope: scope || null,
          user_id: userId || null,
          ip: ip || null,
          route: route || null,
          // An empty field is `null`, not `0`: "no counter given" asks the real one, and `0` would
          // claim a measurement the operator never took.
          count: count === "" ? null : Number(count),
        }),
      );
    } catch (caught) {
      setError(caught instanceof ApiError ? caught : new ApiError(0, "request_failed", String(caught)));
    } finally {
      setBusy(false);
    }
  }

  const field =
    "w-full rounded-md border border-line bg-elevated px-2.5 py-1.5 text-[13px] outline-none focus-visible:border-accent";
  const label = "block text-[12px] font-medium text-muted";
  const kind = result ? reliabilityVerdictKind(result.verdict) : null;
  const tone =
    kind === "allowed" || kind === "unlimited" || kind === "uncounted"
      ? "border-emerald-500/40 bg-emerald-500/5"
      : "border-amber-500/40 bg-amber-500/5";

  return (
    <section className="rounded-lg border border-line" data-testid="dry-run">
      <header className="flex items-center gap-2 border-b border-line px-4 py-3">
        <Gauge className="size-4" aria-hidden />
        <h2 className="text-[13.5px] font-medium">Dry-run — which policy wins?</h2>
        <span className="ml-auto text-[11.5px] text-muted">Does not spend the budget it measures</span>
      </header>

      <div className="grid grid-cols-1 gap-3 p-4 sm:grid-cols-2 lg:grid-cols-5">
        <div>
          <label className={label} htmlFor="dry-scope">Scope (blank = let it choose)</label>
          <select id="dry-scope" className={field} value={scope} onChange={(e) => setScope(e.target.value)}>
            <option value="">Any</option>
            {vocabulary.map((value) => (
              <option key={value} value={value}>{humanScope(value)}</option>
            ))}
          </select>
        </div>
        <div>
          <label className={label} htmlFor="dry-user">User id</label>
          <input id="dry-user" className={field} value={userId} onChange={(e) => setUserId(e.target.value)} placeholder="uuid" />
        </div>
        <div>
          <label className={label} htmlFor="dry-ip">IP</label>
          <input id="dry-ip" className={field} value={ip} onChange={(e) => setIp(e.target.value)} placeholder="203.0.113.7" />
        </div>
        <div>
          <label className={label} htmlFor="dry-route">Route</label>
          <input id="dry-route" className={field} value={route} onChange={(e) => setRoute(e.target.value)} placeholder="/api/v1/public/contact" />
        </div>
        <div>
          <label className={label} htmlFor="dry-count">Counter reading (optional)</label>
          <input id="dry-count" type="number" className={field} value={count} onChange={(e) => setCount(e.target.value)} placeholder="reads the real counter" />
        </div>
      </div>

      <div className="px-4 pb-4">
        <button
          type="button"
          onClick={run}
          disabled={busy}
          data-testid="dry-run-submit"
          className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3 py-1.5 text-[12.5px] font-medium text-accent-fg disabled:opacity-60"
        >
          {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <Gauge className="size-3.5" aria-hidden />}
          Evaluate
        </button>

        {error ? (
          <p className="mt-3 text-[12.5px] text-danger">{error.message}</p>
        ) : null}

        {result ? (
          <div className={`mt-3 rounded-md border p-3 ${tone}`} data-testid="dry-run-result" data-verdict={kind ?? ""}>
            <p className="text-[13px] font-medium" data-testid="dry-run-verdict">{verdictSentence(result)}</p>
            <dl className="mt-2 grid grid-cols-1 gap-x-6 gap-y-1 text-[12.5px] sm:grid-cols-2">
              <div className="flex gap-2">
                <dt className="text-muted">Winning policy</dt>
                <dd data-testid="dry-run-policy">
                  {result.policy ? result.policy.name : "none applies"}
                  {result.policy ? (
                    <span className="ml-1 text-muted">
                      ({humanScope(result.policy.scope)} · {result.policy.limit_count}/{humanWindow(result.policy.window_seconds)}
                    </span>
                  ) : null}
                </dd>
              </div>
              <div className="flex gap-2">
                <dt className="text-muted">Counter</dt>
                <dd>
                  {result.counted.count}
                  {result.counted.authoritative ? "" : " (unreadable — not a measurement)"}
                </dd>
              </div>
              <div className="flex gap-2">
                <dt className="text-muted">Window</dt>
                <dd>{result.window_start ?? "—"}</dd>
              </div>
              <div className="flex gap-2">
                <dt className="text-muted">Fails</dt>
                <dd>{result.fail_mode}</dd>
              </div>
              <div className="flex gap-2 sm:col-span-2">
                <dt className="text-muted">Counter key</dt>
                <dd className="font-mono text-[11.5px] break-all">{result.counter_key ?? "—"}</dd>
              </div>
            </dl>
            {result.policy && !result.policy.enforced_here ? (
              <p className="mt-2 flex items-start gap-1.5 text-[12px] text-muted">
                <Info className="mt-0.5 size-3.5 shrink-0" aria-hidden />
                This row is a route policy, so it is stored here but not spent by this layer — the
                platform limiter runs before the router matches a route.
              </p>
            ) : null}
          </div>
        ) : null}
      </div>
    </section>
  );
}

export function ReliabilityLimitsView() {
  const [document, setDocument] = useState<ReliabilityPolicies | null>(null);
  const [refusals, setRefusals] = useState<ReliabilityRefusal[]>([]);
  const [refusals24h, setRefusals24h] = useState(0);
  const [error, setError] = useState<ApiError | null>(null);
  const [loading, setLoading] = useState(true);
  const [filter, setFilter] = useState("");
  const [formOpen, setFormOpen] = useState(false);
  const [editing, setEditing] = useState<ReliabilityPolicy | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      // Both reads together: the policies answer "what is configured", the rollups answer "is it
      // biting". A screen showing only the first is a configuration viewer.
      const [policies, rolls] = await Promise.all([
        fetchReliabilityPolicies(),
        fetchReliabilityRefusals(50),
      ]);
      setDocument(policies);
      setRefusals(rolls.refusals);
      setRefusals24h(rolls.last_24_hours);
    } catch (caught) {
      setError(caught instanceof ApiError ? caught : new ApiError(0, "request_failed", String(caught)));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    function onKey(event: KeyboardEvent) {
      const target = event.target as HTMLElement | null;
      const typing = target?.tagName === "INPUT" || target?.tagName === "SELECT" || target?.tagName === "TEXTAREA";
      if (event.key === "/" && !typing) {
        event.preventDefault();
        searchRef.current?.focus();
      } else if (event.key === "n" && !typing) {
        event.preventDefault();
        setEditing(null);
        setFormOpen(true);
      } else if (event.key === "Escape") {
        setFormOpen(false);
        setEditing(null);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const rows = useMemo(() => {
    const all = document?.policies ?? [];
    const needle = filter.trim().toLowerCase();
    if (!needle) return all;
    return all.filter((policy) =>
      [policy.name, policy.scope, policy.target_id ?? "", policy.route_pattern ?? ""]
        .join(" ")
        .toLowerCase()
        .includes(needle),
    );
  }, [document, filter]);

  async function remove(policy: ReliabilityPolicy) {
    if (!policy.id) return;
    // The wording of the confirmation names the scope, because "delete" on a default row
    // disables it and the operator deserves to know which of the two is about to happen.
    const verb = policy.is_default ? "Disable" : "Delete";
    if (!window.confirm(`${verb} the ${humanScope(policy.scope).toLowerCase()} policy “${policy.name}”?`)) return;
    try {
      const answer = await deleteReliabilityPolicy(policy.id);
      setNotice(answer.message);
      await load();
    } catch (caught) {
      setError(caught instanceof ApiError ? caught : new ApiError(0, "request_failed", String(caught)));
    }
  }

  if (loading && !document) {
    return (
      <div className="rounded-lg border border-line">
        <LoadingTable columns={6} />
      </div>
    );
  }

  if (error && !document) {
    return (
      <div className="rounded-lg border border-danger/40 p-6 text-center" data-testid="load-error">
        <XCircle className="mx-auto size-6 text-danger" aria-hidden />
        <p className="mt-2 text-[13.5px] font-medium">The limiter configuration could not be read</p>
        <p className="mt-1 text-[12.5px] text-muted">
          {error.message}
          {typeof error.details?.request_id === "string" ? ` · request ${error.details.request_id}` : ""}
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="mt-3 inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px]"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  const vocabulary = document?.vocabulary ?? [];
  const notEnforced = rows.filter((policy) => !policy.enforced_here).length;

  return (
    <div className="space-y-5">
      {error ? (
        <p className="flex items-start gap-1.5 rounded-md border border-danger/40 px-3 py-2 text-[12.5px] text-danger" data-testid="inline-error">
          <XCircle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>
            {error.message}
            {typeof error.details?.request_id === "string" ? ` · request ${error.details.request_id}` : ""}
          </span>
        </p>
      ) : null}
      {notice ? (
        <p className="flex items-start gap-1.5 rounded-md border border-emerald-500/40 px-3 py-2 text-[12.5px]" data-testid="notice">
          <CheckCircle2 className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>{notice}</span>
        </p>
      ) : null}

      <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
        <div className="rounded-lg border border-line p-4">
          <p className="text-[12px] text-muted">Refusals, last 24 h</p>
          <p className="mt-1 text-2xl font-semibold tabular-nums" data-testid="refusals-24h">{refusals24h}</p>
        </div>
        <div className="rounded-lg border border-line p-4">
          <p className="text-[12px] text-muted">Policies configured</p>
          <p className="mt-1 text-2xl font-semibold tabular-nums" data-testid="policy-count">{document?.policies.length ?? 0}</p>
        </div>
        <div className="rounded-lg border border-line p-4">
          <p className="text-[12px] text-muted">Limiter</p>
          <p className="mt-1 text-[13.5px] font-medium">{document?.limiter}</p>
          <p className="text-[12px] text-muted">
            fails <span className="font-medium text-foreground">{document?.fail_mode}</span> when the counter is unreadable
          </p>
        </div>
      </div>

      <section className="rounded-lg border border-line">
        <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <ShieldAlert className="size-4" aria-hidden />
          <h2 className="text-[13.5px] font-medium">Policies, in resolution order</h2>
          {notEnforced > 0 ? (
            <span className="inline-flex items-center gap-1 rounded-full border border-amber-500/40 px-2 py-0.5 text-[11.5px]" data-testid="not-enforced-badge">
              <AlertTriangle className="size-3" aria-hidden />
              {notEnforced} not enforced here
            </span>
          ) : null}
          <div className="ml-auto flex items-center gap-2">
            <div className="relative">
              <Search className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted" aria-hidden />
              <input
                ref={searchRef}
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
                placeholder="Filter  ( / )"
                aria-label="Filter policies"
                data-testid="policy-filter"
                className="rounded-md border border-line bg-elevated py-1.5 pl-7 pr-2 text-[12.5px] outline-none focus-visible:border-accent"
              />
            </div>
            <button
              type="button"
              onClick={() => void load()}
              aria-label="Reload"
              className="rounded-md border border-line p-1.5"
            >
              <RefreshCw className="size-3.5" aria-hidden />
            </button>
            <button
              type="button"
              data-testid="policy-new"
              onClick={() => { setEditing(null); setFormOpen(true); }}
              className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3 py-1.5 text-[12.5px] font-medium text-accent-fg"
            >
              <Plus className="size-3.5" aria-hidden />
              New policy  ( n )
            </button>
          </div>
        </header>

        {rows.length === 0 ? (
          <EmptyState
            title={filter ? "No policy matches that filter" : "No rate-limit policy is configured"}
            hint={
              filter
                ? "The filter is matched against the name, scope, target and route pattern."
                : "Without a policy the limiter is off: every request is allowed and no budget exists to spend. Start with the sign-in and public-form budgets — they are the two an unauthenticated caller can exhaust."
            }
            action={
              filter ? (
                <button type="button" onClick={() => setFilter("")} className="rounded-md border border-line px-3 py-1.5 text-[12.5px]">
                  Clear the filter
                </button>
              ) : (
                <button
                  type="button"
                  onClick={() => { setEditing(null); setFormOpen(true); }}
                  className="rounded-md bg-accent px-3 py-1.5 text-[12.5px] font-medium text-accent-fg"
                >
                  Create the first policy
                </button>
              )
            }
          />
        ) : (
          <>
            <div className="hidden overflow-x-auto md:block">
              <table className="w-full border-collapse text-left text-[13px]">
                <thead>
                  <tr className="border-b border-line text-[12px] text-muted">
                    <th className="px-4 py-2.5 font-medium">Policy</th>
                    <th className="px-4 py-2.5 font-medium">Scope</th>
                    <th className="px-4 py-2.5 font-medium">Target</th>
                    <th className="px-4 py-2.5 font-medium">Route</th>
                    <th className="px-4 py-2.5 font-medium">Budget</th>
                    <th className="px-4 py-2.5 font-medium">Priority</th>
                    <th className="px-4 py-2.5 font-medium">Enforcement</th>
                    <th className="px-4 py-2.5" />
                  </tr>
                </thead>
                <tbody>
                  {rows.map((policy) => (
                    <tr key={policy.id ?? policy.name} className="border-b border-line last:border-b-0" data-testid="policy-row">
                      <td className="px-4 py-3">
                        <span className="font-medium">{policy.name}</span>
                        {policy.is_default ? (
                          <span className="ml-2 rounded-full border border-line px-1.5 py-0.5 text-[11px] text-muted">default</span>
                        ) : null}
                        {!policy.enabled ? (
                          <span className="ml-2 rounded-full border border-line px-1.5 py-0.5 text-[11px] text-muted">off</span>
                        ) : null}
                      </td>
                      <td className="px-4 py-3">{humanScope(policy.scope)}</td>
                      <td className="px-4 py-3 font-mono text-[12px] break-all">{policy.target_id ?? "—"}</td>
                      <td className="px-4 py-3 font-mono text-[12px] break-all">{policy.route_pattern ?? "—"}</td>
                      <td className="px-4 py-3 tabular-nums">
                        {policy.limit_count} / {humanWindow(policy.window_seconds)}
                        {policy.burst > 0 ? <span className="text-muted"> (+{policy.burst})</span> : null}
                      </td>
                      <td className="px-4 py-3 tabular-nums">{policy.priority}</td>
                      <td className="px-4 py-3">
                        {policy.enforced_here ? (
                          <span className="text-[12.5px]">enforced</span>
                        ) : (
                          <span className="text-[12.5px] text-muted" data-testid="not-enforced">
                            not enforced here
                          </span>
                        )}
                      </td>
                      <td className="px-4 py-3">
                        <div className="flex justify-end gap-1">
                          <button
                            type="button"
                            onClick={() => { setEditing(policy); setFormOpen(true); }}
                            data-testid="policy-edit"
                            aria-label={`Edit ${policy.name}`}
                            className="rounded-md border border-line p-1.5"
                          >
                            <Save className="size-3.5" aria-hidden />
                          </button>
                          <button
                            type="button"
                            onClick={() => void remove(policy)}
                            data-testid="policy-delete"
                            aria-label={`${policy.is_default ? "Disable" : "Delete"} ${policy.name}`}
                            className="rounded-md border border-line p-1.5"
                          >
                            <Trash2 className="size-3.5" aria-hidden />
                          </button>
                        </div>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>

            <ul className="divide-y divide-line md:hidden">
              {rows.map((policy) => (
                <li key={policy.id ?? policy.name} className="space-y-1.5 p-4" data-testid="policy-card">
                  <div className="flex items-start justify-between gap-2">
                    <span className="font-medium">{policy.name}</span>
                    <span className="shrink-0 text-[12px] text-muted">{humanScope(policy.scope)}</span>
                  </div>
                  <p className="text-[12.5px] tabular-nums">
                    {policy.limit_count} / {humanWindow(policy.window_seconds)}
                    {policy.burst > 0 ? ` (+${policy.burst})` : ""} · priority {policy.priority}
                  </p>
                  {policy.route_pattern ? <p className="font-mono text-[11.5px] break-all">{policy.route_pattern}</p> : null}
                  <p className="text-[12px] text-muted">
                    {policy.enforced_here ? "enforced" : "not enforced here (route policies are spent by the gateway, not this layer)"}
                  </p>
                  <div className="flex gap-2 pt-1">
                    <button
                      type="button"
                      onClick={() => { setEditing(policy); setFormOpen(true); }}
                      className="rounded-md border border-line px-2.5 py-1 text-[12px]"
                    >
                      Edit
                    </button>
                    <button
                      type="button"
                      onClick={() => void remove(policy)}
                      className="rounded-md border border-line px-2.5 py-1 text-[12px]"
                    >
                      {policy.is_default ? "Disable" : "Delete"}
                    </button>
                  </div>
                </li>
              ))}
            </ul>
          </>
        )}
      </section>

      {formOpen ? (
        <PolicyForm
          vocabulary={vocabulary}
          editing={editing}
          onSaved={(message) => { setNotice(message); setFormOpen(false); setEditing(null); void load(); }}
          onCancel={() => { setFormOpen(false); setEditing(null); }}
        />
      ) : null}

      <DryRun vocabulary={vocabulary} />

      <section className="rounded-lg border border-line" data-testid="refusal-rollup">
        <header className="flex items-center gap-2 border-b border-line px-4 py-3">
          <AlertTriangle className="size-4" aria-hidden />
          <h2 className="text-[13.5px] font-medium">Refusal rollup</h2>
          <span className="ml-auto text-[11.5px] text-muted">
            One row per scope, route and window — never one per refused request
          </span>
        </header>
        <div className="p-4">
          {refusals.length === 0 ? (
            <EmptyState
              title="No request has been refused by this limiter"
              hint="Refusals roll up one row per window, so a caller hammering a capped route shows here as a single counted entry rather than a flood."
            />
          ) : (
            <RefusalBars refusals={refusals} />
          )}
        </div>
      </section>
    </div>
  );
}
