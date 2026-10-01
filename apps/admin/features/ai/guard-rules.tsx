"use client";

/**
 * `/ai/guard/rules` — the detector's rule set (docs/requests/REQ-105, slice 1).
 *
 * A rule is a regular expression, a label it reports under, and what happens on a match. The
 * table is where an operator decides what their tenant's guard actually inspects, and five rules
 * shape it:
 *
 * 1. **A built-in says so before the buttons do.** Platform rules carry `organization_id is
 *    null`: they can be disabled and they can never be edited or deleted, because deleting one
 *    would remove a protection the installation documents. Delete is *absent* on those rows
 *    rather than greyed out — a greyed Delete teaches an operator that the screen is stale, and
 *    a 403 teaches the same thing more slowly.
 *
 * 2. **An invalid pattern is a field error, and the form proves it before the save does.** The
 *    create form compiles the expression locally as the operator types, so a rule that the
 *    server would refuse never reaches the server. The server remains the authority — the walk
 *    proves a malformed pattern is rejected — but the local check is what makes the failure
 *    legible instead of a red toast.
 *
 * 3. **A new rule arrives enabled, and the row says so before it is saved.** The server's
 *    default is `enabled`, which is the right default for a control (a rule that ships switched
 *    off protects nothing and looks as if it does). The form previews that default as a
 *    checkbox rather than hiding it in a sentence, because the dangerous reading is "I created a
 *    rule and it is not doing anything".
 *
 * 4. **The pattern is truncated with a way to see all of it.** A regex that does not fit in a
 *    cell cannot be reviewed, and an unreviewable control is a control nobody signs off. The
 *    toggle is per row rather than a global "expand all" so scanning still works.
 *
 * 5. **The budget is shown before it is hit.** The detector refuses to run more than a fixed
 *    number of enabled rules, and the refusal at create time — after the operator has written a
 *    rule — is the worst possible moment to learn the ceiling.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useSearchParams } from "next/navigation";

import {
  ChevronDown,
  ChevronRight,
  Copy,
  Pencil,
  Plus,
  Search,
  Trash2,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type GuardLabel,
  type GuardRule,
  type GuardRuleList,
  createGuardRule,
  deleteGuardRule,
  fetchGuardRules,
  runGuardTest,
  updateGuardRule,
} from "@/lib/guard-api";
import { formatTimestamp } from "@/lib/format";

/** What the form holds while it is open. */
type Draft = {
  key: string;
  label: string;
  custom_label: string;
  pattern: string;
  validator: string;
  action: string;
  severity: number;
  priority: number;
  providers: string;
  features: string;
  enabled: boolean;
  sample: string;
};

const BLANK: Draft = {
  key: "",
  label: "",
  custom_label: "",
  pattern: "",
  validator: "none",
  action: "flag",
  severity: 3,
  priority: 100,
  providers: "",
  features: "",
  enabled: true,
  sample: "",
};

/** Splits a comma/space separated scope box into the array the API takes. */
function scopeList(value: string): string[] {
  return value
    .split(/[\s,]+/)
    .map((item) => item.trim())
    .filter(Boolean);
}

/**
 * Why a key the form typed would be refused, or `null` when it is fine.
 *
 * The charset and the length bounds are quoted from the request's spec
 * (`[a-z0-9_.-]{2,60}`) rather than invented, and a key that is already taken is checked
 * against the loaded list — the server would reject the duplicate too, but as a raw 409 with no
 * indication of which field.
 */
export function checkRuleKey(key: string, taken: string[]): string | null {
  const trimmed = key.trim();
  if (!trimmed) return "A rule needs a key — the stable name the log and the detector refer to.";
  if (trimmed.length < 2) return `The key is ${trimmed.length} characters; the minimum is 2.`;
  if (trimmed.length > 60) return `The key is ${trimmed.length} characters; the limit is 60.`;
  if (!/^[a-z0-9_.-]+$/.test(trimmed)) {
    return "A key uses only lower-case letters, digits, dot, _ and -.";
  }
  if (taken.includes(trimmed)) return `Another rule already uses the key “${trimmed}”.`;
  return null;
}

/**
 * Why the pattern would be refused, or `null` when it compiles.
 *
 * `new RegExp` throws on a malformed expression, which is exactly what the server's compiler
 * does. This is a *local* check and the server stays the authority: the walk proves the server
 * refuses a malformed pattern even when this one has not run.
 */
export function checkPattern(pattern: string): string | null {
  const trimmed = pattern.trim();
  if (!trimmed) return "A rule needs a pattern — without one it never matches anything.";
  try {
    // Constructed without flags on purpose: the server compiles the stored expression the same
    // way, and a locally-valid pattern that the server rejects would be a worse false negative
    // than no check at all.
    new RegExp(trimmed);
    return null;
  } catch (cause) {
    return `The pattern is not a valid regular expression: ${(cause as Error).message}`;
  }
}

/** The rules table. */
export function GuardRules() {
  const search = useSearchParams();
  // The policy panel links here with `?label=<key>`; reading it once on mount turns that link
  // into a working filter instead of a dead query string.
  const initialLabel = search?.get("label") ?? "";

  const [data, setData] = useState<GuardRuleList | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const [query, setQuery] = useState("");
  const [kind, setKind] = useState("");
  const [action, setAction] = useState("");
  const [enabledOnly, setEnabledOnly] = useState(false);
  const [labelFilter, setLabelFilter] = useState(initialLabel);

  const [editing, setEditing] = useState<GuardRule | "new" | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [confirming, setConfirming] = useState<GuardRule | null>(null);
  const [busy, setBusy] = useState(false);
  const [flash, setFlash] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setData(await fetchGuardRules());
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  /**
   * `/` focuses the search, `N` opens the form, `Esc` closes it.
   *
   * Bound on the container rather than on `window` so the shortcuts cannot fire while the
   * operator is typing in a field: `N` is a perfectly ordinary letter to type into a pattern.
   */
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.tagName === "SELECT";
      if (event.key === "Escape") {
        setEditing(null);
        setConfirming(null);
        return;
      }
      if (typing || event.metaKey || event.ctrlKey || event.altKey) return;
      if (event.key === "/") {
        event.preventDefault();
        searchRef.current?.focus();
      } else if (event.key === "n" || event.key === "N") {
        event.preventDefault();
        setEditing("new");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const takenKeys = useMemo(() => (data?.rows ?? []).map((row) => row.key), [data]);

  // Filtering client-side: the rule set is bounded by the server's own budget (a fixed ceiling on
  // enabled rules), so this is a small list and a request per keystroke would be pure overhead.
  const visible = useMemo(() => {
    if (!data) return [];
    const needle = query.trim().toLowerCase();
    return data.rows.filter((row) => {
      if (kind && row.kind !== kind) return false;
      if (action && row.action !== action) return false;
      if (enabledOnly && !row.enabled) return false;
      if (labelFilter && row.label !== labelFilter) return false;
      if (!needle) return true;
      return (
        row.key.toLowerCase().includes(needle) ||
        row.label.toLowerCase().includes(needle) ||
        row.pattern.toLowerCase().includes(needle)
      );
    });
  }, [data, query, kind, action, enabledOnly, labelFilter]);

  const toggleEnabled = useCallback(
    async (rule: GuardRule) => {
      setBusy(true);
      setError(null);
      try {
        // Only the switch is sent. Sending the whole row would clear a scope the operator never
        // opened — the server treats `null` as "leave it alone", which is exactly what a toggle
        // needs and a form submit does not.
        await updateGuardRule(rule.id, { enabled: !rule.enabled });
        await load();
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : String(cause));
      } finally {
        setBusy(false);
      }
    },
    [load],
  );

  const duplicate = useCallback(
    async (rule: GuardRule) => {
      setBusy(true);
      setError(null);
      try {
        const copy = await createGuardRule({
          key: `${rule.key}_copy`,
          label: rule.label,
          pattern: rule.pattern,
          validator: rule.validator,
          action: rule.action,
          severity: rule.severity,
          priority: rule.priority,
          providers: rule.providers,
          features: rule.features,
          enabled: rule.enabled,
          sample: rule.sample ?? undefined,
        });
        setFlash(`Duplicated as “${copy.key}”.`);
        await load();
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : String(cause));
      } finally {
        setBusy(false);
      }
    },
    [load],
  );

  const remove = useCallback(async () => {
    if (!confirming) return;
    setBusy(true);
    setError(null);
    try {
      await deleteGuardRule(confirming.id);
      setFlash(`Deleted “${confirming.key}”.`);
      setConfirming(null);
      await load();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  }, [confirming, load]);

  const toggleExpanded = useCallback((key: string) => {
    setExpanded((current) => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }, []);

  if (loading) return <LoadingTable columns={7} />;

  if (error && !data) {
    return (
      <div role="alert" className="rounded-lg border border-danger/40 bg-danger/5 p-5">
        <p className="text-[13.5px] font-medium text-danger">The rules could not be loaded.</p>
        <p className="mt-1 text-[12.5px] text-muted">{error}</p>
        <button
          type="button"
          onClick={() => void load()}
          className="mt-3 rounded-md border border-line px-3 py-1.5 text-[12.5px]"
        >
          Try again
        </button>
      </div>
    );
  }

  if (!data) return null;

  const budgetFull = data.enabled >= data.budget;

  return (
    <div className="flex flex-col gap-4" data-guard-rules>
      {error ? (
        <p role="alert" className="rounded-md bg-danger/10 px-3 py-2 text-[12.5px] text-danger">
          {error}
        </p>
      ) : null}
      {flash ? (
        <p role="status" className="rounded-md bg-muted/60 px-3 py-2 text-[12.5px]">
          {flash}
        </p>
      ) : null}

      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-[12.5px] text-muted">
          <span className={budgetFull ? "text-warning" : undefined}>
            {data.enabled} of {data.budget} rules enabled
          </span>
          {" · "}
          <span className="font-mono text-[11.5px]">
            / focuses search · N new rule · Esc closes
          </span>
        </p>
        <button
          type="button"
          onClick={() => setEditing("new")}
          data-guard-rules-new
          className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-bg"
        >
          <Plus aria-hidden className="size-3.5" />
          New rule
        </button>
      </div>

      {/* Filters. Each control has a visible label — a placeholder alone is not a label. */}
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <div className="lg:col-span-2">
          <label htmlFor="rules-search" className="block text-[12px] font-medium">
            Search key, label or pattern
          </label>
          <div className="relative mt-1">
            <Search
              aria-hidden
              className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted"
            />
            <input
              id="rules-search"
              ref={searchRef}
              data-guard-rules-search
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="email"
              className="w-full rounded-md border border-line bg-bg py-1.5 pl-7 pr-2 text-[12.5px]"
            />
          </div>
        </div>
        <div>
          <label htmlFor="rules-kind" className="block text-[12px] font-medium">
            Type
          </label>
          <select
            id="rules-kind"
            value={kind}
            onChange={(event) => setKind(event.target.value)}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          >
            <option value="">All</option>
            <option value="builtin">Built-in</option>
            <option value="custom">Custom</option>
          </select>
        </div>
        <div>
          <label htmlFor="rules-action" className="block text-[12px] font-medium">
            Action
          </label>
          <select
            id="rules-action"
            value={action}
            onChange={(event) => setAction(event.target.value)}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          >
            <option value="">All</option>
            {data.actions.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
        </div>
        <div className="sm:col-span-2 lg:col-span-4">
          <label htmlFor="rules-label" className="block text-[12px] font-medium">
            Label
          </label>
          <div className="mt-1 flex items-center gap-3">
            <select
              id="rules-label"
              value={labelFilter}
              onChange={(event) => setLabelFilter(event.target.value)}
              className="max-w-xs rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
            >
              <option value="">All labels</option>
              {data.labels.map((name) => (
                <option key={name} value={name}>
                  {name}
                </option>
              ))}
            </select>
            <label className="flex items-center gap-2 text-[12.5px]">
              <input
                type="checkbox"
                checked={enabledOnly}
                onChange={(event) => setEnabledOnly(event.target.checked)}
              />
              Enabled only
            </label>
          </div>
        </div>
      </div>

      {visible.length === 0 ? (
        <EmptyState
          title={
            data.rows.length === 0
              ? "No rules in this tenant."
              : "No rule matches these filters."
          }
          hint={
            data.rows.length === 0
              ? "A tenant starts with the platform's built-in rules; a custom one is added here."
              : "Clear the search box or widen the filters to see the rest of the set."
          }
          action={
            data.rows.length === 0 ? (
              <button
                type="button"
                onClick={() => setEditing("new")}
                className="rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-bg"
              >
                New rule
              </button>
            ) : (
              <button
                type="button"
                onClick={() => {
                  setQuery("");
                  setKind("");
                  setAction("");
                  setLabelFilter("");
                  setEnabledOnly(false);
                }}
                className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
              >
                Clear filters
              </button>
            )
          }
        />
      ) : (
        <>
          {/* Desktop: a table. Mobile: cards, because a seven-column table at 390px is a
              horizontal-scroll trap that hides the action buttons entirely. */}
          <div className="hidden overflow-x-auto rounded-lg border border-line lg:block">
            <table className="w-full text-left text-[12.5px]">
              <caption className="sr-only">
                Detector rules, their label, pattern, action and state
              </caption>
              <thead className="border-b border-line text-[11.5px] text-muted">
                <tr>
                  <th scope="col" className="px-3 py-2 font-medium">Key</th>
                  <th scope="col" className="px-3 py-2 font-medium">Label</th>
                  <th scope="col" className="px-3 py-2 font-medium">Type</th>
                  <th scope="col" className="px-3 py-2 font-medium">Pattern</th>
                  <th scope="col" className="px-3 py-2 font-medium">Action</th>
                  <th scope="col" className="px-3 py-2 font-medium">Severity</th>
                  <th scope="col" className="px-3 py-2 font-medium">Priority</th>
                  <th scope="col" className="px-3 py-2 font-medium">Scope</th>
                  <th scope="col" className="px-3 py-2 font-medium">Enabled</th>
                  <th scope="col" className="px-3 py-2 font-medium">Actions</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-line">
                {visible.map((rule) => (
                  <tr key={rule.id} data-guard-rule={rule.key} className="align-top">
                    <td className="px-3 py-2 font-mono text-[12px]">
                      {rule.key}
                      <p className="mt-0.5 text-[11px] text-muted">
                        {formatTimestamp(rule.updated_at)}
                      </p>
                    </td>
                    <td className="px-3 py-2 font-mono text-[12px]">{rule.label}</td>
                    <td className="px-3 py-2">
                      <span className="rounded bg-muted/60 px-1.5 py-0.5 text-[11px]">
                        {rule.kind}
                      </span>
                      {rule.validator !== "none" ? (
                        <p className="mt-0.5 text-[11px] text-muted">{rule.validator}</p>
                      ) : null}
                    </td>
                    <td className="max-w-[22rem] px-3 py-2">
                      <PatternCell
                        pattern={rule.pattern}
                        expanded={expanded.has(rule.id)}
                        onToggle={() => toggleExpanded(rule.id)}
                      />
                    </td>
                    <td className="px-3 py-2">{rule.action}</td>
                    <td className="px-3 py-2 tabular-nums">{rule.severity}</td>
                    <td className="px-3 py-2 tabular-nums">{rule.priority}</td>
                    <td className="px-3 py-2">
                      <ScopeCell providers={rule.providers} features={rule.features} />
                    </td>
                    <td className="px-3 py-2">
                      <label className="flex items-center gap-1.5">
                        <input
                          type="checkbox"
                          data-guard-rule-toggle={rule.key}
                          checked={rule.enabled}
                          disabled={busy}
                          onChange={() => void toggleEnabled(rule)}
                        />
                        <span className="sr-only">Enabled for {rule.key}</span>
                      </label>
                    </td>
                    <td className="px-3 py-2">
                      <RowActions
                        rule={rule}
                        busy={busy}
                        confirming={confirming?.id === rule.id}
                        onEdit={() => setEditing(rule)}
                        onDuplicate={() => void duplicate(rule)}
                        onDelete={() => setConfirming(rule)}
                        onCancelDelete={() => setConfirming(null)}
                        onConfirmDelete={() => void remove()}
                      />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <ul className="flex flex-col gap-3 lg:hidden">
            {visible.map((rule) => (
              <li key={rule.id} data-guard-rule-card={rule.key} className="rounded-lg border border-line p-3">
                <div className="flex items-start justify-between gap-2">
                  <div className="min-w-0">
                    <p className="font-mono text-[12.5px] font-medium">{rule.key}</p>
                    <p className="font-mono text-[11.5px] text-muted">{rule.label}</p>
                  </div>
                  <span className="shrink-0 rounded bg-muted/60 px-1.5 py-0.5 text-[11px]">
                    {rule.kind}
                  </span>
                </div>
                <p className="mt-1 text-[11.5px] text-muted">
                  {rule.action} · severity {rule.severity} · priority {rule.priority}
                </p>
                <div className="mt-2">
                  <PatternCell
                    pattern={rule.pattern}
                    expanded={expanded.has(rule.id)}
                    onToggle={() => toggleExpanded(rule.id)}
                  />
                </div>
                <p className="mt-2 text-[11.5px] text-muted">
                  <ScopeCell providers={rule.providers} features={rule.features} />
                </p>
                <div className="mt-3 flex items-center justify-between gap-2">
                  <label className="flex items-center gap-2 text-[12px]">
                    <input
                      type="checkbox"
                      checked={rule.enabled}
                      disabled={busy}
                      onChange={() => void toggleEnabled(rule)}
                    />
                    Enabled
                  </label>
                  <RowActions
                    rule={rule}
                    busy={busy}
                    confirming={confirming?.id === rule.id}
                    onEdit={() => setEditing(rule)}
                    onDuplicate={() => void duplicate(rule)}
                    onDelete={() => setConfirming(rule)}
                    onCancelDelete={() => setConfirming(null)}
                    onConfirmDelete={() => void remove()}
                  />
                </div>
              </li>
            ))}
          </ul>
        </>
      )}

      {editing ? (
        <RuleForm
          rule={editing === "new" ? null : editing}
          vocab={data}
          takenKeys={editing === "new" ? takenKeys : takenKeys.filter((key) => key !== editing.key)}
          onClose={() => setEditing(null)}
          onSaved={async (message) => {
            setEditing(null);
            setFlash(message);
            await load();
          }}
        />
      ) : null}
    </div>
  );
}

/**
 * A row's actions.
 *
 * Delete is **absent** for a built-in. The row already says `builtin`; refusing the click teaches
 * less than not offering it, and the walk asserts the absence rather than a disabled control,
 * because a disabled Delete on a platform rule reads as a broken screen rather than a policy.
 */
function RowActions({
  rule,
  busy,
  confirming,
  onEdit,
  onDuplicate,
  onDelete,
  onCancelDelete,
  onConfirmDelete,
}: {
  rule: GuardRule;
  busy: boolean;
  confirming: boolean;
  onEdit: () => void;
  onDuplicate: () => void;
  onDelete: () => void;
  onCancelDelete: () => void;
  onConfirmDelete: () => void;
}) {
  if (confirming) {
    return (
      <div className="flex items-center gap-1.5">
        <button
          type="button"
          onClick={onConfirmDelete}
          disabled={busy}
          data-guard-rule-delete-confirm={rule.key}
          className="rounded-md bg-danger px-2 py-1 text-[11.5px] text-white disabled:opacity-50"
        >
          Delete {rule.key}?
        </button>
        <button
          type="button"
          onClick={onCancelDelete}
          className="rounded-md border border-line px-2 py-1 text-[11.5px]"
        >
          Cancel
        </button>
      </div>
    );
  }
  return (
    <div className="flex items-center gap-1.5">
      {rule.kind === "custom" ? (
        <>
          <IconButton
            label={`Edit ${rule.key}`}
            hook="data-guard-rule-edit"
            onClick={onEdit}
            disabled={busy}
          >
            <Pencil aria-hidden className="size-3.5" />
          </IconButton>
          <IconButton
            label={`Delete ${rule.key}`}
            hook="data-guard-rule-delete"
            onClick={onDelete}
            disabled={busy}
          >
            <Trash2 aria-hidden className="size-3.5" />
          </IconButton>
        </>
      ) : null}
      <IconButton label={`Duplicate ${rule.key}`} onClick={onDuplicate} disabled={busy}>
        <Copy aria-hidden className="size-3.5" />
      </IconButton>
    </div>
  );
}

/** An icon button that always names its target — the label is the accessible name. */
function IconButton({
  label,
  onClick,
  disabled,
  hook,
  children,
}: {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  /** The stable selector the walkthrough clicks; `label` is the human string. */
  hook?: string;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      onClick={onClick}
      disabled={disabled}
      {...(hook ? { [hook]: "" } : {})}
      className="rounded-md border border-line p-1.5 hover:bg-muted/40 disabled:opacity-40"
    >
      {children}
    </button>
  );
}

/** The pattern, truncated with a per-row reveal. */
function PatternCell({
  pattern,
  expanded,
  onToggle,
}: {
  pattern: string;
  expanded: boolean;
  onToggle: () => void;
}) {
  return (
    <div>
      <p className={`font-mono text-[11.5px] ${expanded ? "break-all" : "truncate"}`}>
        {expanded ? pattern : pattern.length > 48 ? `${pattern.slice(0, 48)}…` : pattern}
      </p>
      {pattern.length > 48 ? (
        <button
          type="button"
          onClick={onToggle}
          aria-expanded={expanded}
          data-guard-pattern-toggle={pattern}
          className="mt-1 inline-flex items-center gap-1 text-[11.5px] underline underline-offset-2"
        >
          {expanded ? (
            <ChevronDown aria-hidden className="size-3" />
          ) : (
            <ChevronRight aria-hidden className="size-3" />
          )}
          {expanded ? "Hide pattern" : "Show pattern"}
        </button>
      ) : null}
    </div>
  );
}

/** The scope, or "everywhere". A blank scope is a real decision and says itself. */
function ScopeCell({ providers, features }: { providers: string[]; features: string[] }) {
  if (providers.length === 0 && features.length === 0) {
    return <span className="text-muted">everywhere</span>;
  }
  return (
    <span className="text-[11.5px]">
      {providers.length > 0 ? (
        <span>
          providers: <span className="font-mono">{providers.join(", ")}</span>
        </span>
      ) : null}
      {providers.length > 0 && features.length > 0 ? <br /> : null}
      {features.length > 0 ? (
        <span>
          features: <span className="font-mono">{features.join(", ")}</span>
        </span>
      ) : null}
    </span>
  );
}

/**
 * The create/edit form, with a live "test this rule" panel.
 *
 * The live test calls the same `/ai/guard/test` endpoint the tester screen uses, so what the
 * form previews is what the guard would do — not a client-side approximation of the pattern. That
 * matters most for a rule whose action is `mask`: a form that showed the raw pattern and called
 * it a match would be describing the wrong outcome, because the operator is about to see
 * `[EMAIL_1]` where the payload said an address.
 */
function RuleForm({
  rule,
  vocab,
  takenKeys,
  onClose,
  onSaved,
}: {
  rule: GuardRule | null;
  vocab: GuardRuleList;
  takenKeys: string[];
  onClose: () => void;
  onSaved: (message: string) => void | Promise<void>;
}) {
  const [draft, setDraft] = useState<Draft>(
    rule
      ? {
          key: rule.key,
          label: rule.label.startsWith("custom:") ? "custom" : rule.label,
          custom_label: rule.label.startsWith("custom:") ? rule.label.slice(7) : "",
          pattern: rule.pattern,
          validator: rule.validator,
          action: rule.action,
          severity: rule.severity,
          priority: rule.priority,
          providers: rule.providers.join(", "),
          features: rule.features.join(", "),
          enabled: rule.enabled,
          sample: rule.sample ?? "",
        }
      : BLANK,
  );
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [probe, setProbe] = useState<{ verdict: string; masked_text: string; matches: number } | null>(
    null,
  );

  const keyProblem = checkRuleKey(draft.key, takenKeys);
  const patternProblem = checkPattern(draft.pattern);
  const labelNote: GuardLabel | undefined = vocab.label_notes.find(
    (note) => note.key === (draft.label === "custom" ? draft.custom_label : draft.label),
  );

  const set = <K extends keyof Draft>(key: K, value: Draft[K]) =>
    setDraft((current) => ({ ...current, [key]: value }));

  /** Dry-run the sample through the real guard, on demand. */
  const runProbe = useCallback(async () => {
    if (!draft.sample.trim()) return;
    setError(null);
    try {
      const result = await runGuardTest({
        payload: draft.sample,
        provider: scopeList(draft.providers)[0],
        feature: scopeList(draft.features)[0],
      });
      setProbe({
        verdict: result.verdict,
        masked_text: result.masked_text,
        matches: result.matches.length,
      });
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    }
  }, [draft.sample, draft.providers, draft.features]);

  const submit = useCallback(async () => {
    if (keyProblem || patternProblem) return;
    setSaving(true);
    setError(null);
    try {
      const body = {
        key: draft.key.trim(),
        label: draft.label,
        custom_label:
          draft.label === "custom" ? draft.custom_label.trim() : undefined,
        pattern: draft.pattern,
        validator: draft.validator,
        action: draft.action,
        severity: draft.severity,
        priority: draft.priority,
        providers: scopeList(draft.providers),
        features: scopeList(draft.features),
        enabled: draft.enabled,
        sample: draft.sample || undefined,
      };
      if (rule) {
        await updateGuardRule(rule.id, body);
        await onSaved(`Saved “${draft.key.trim()}”.`);
      } else {
        await createGuardRule(body);
        await onSaved(`Created “${draft.key.trim()}”.`);
      }
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setSaving(false);
    }
  }, [draft, keyProblem, patternProblem, rule, onSaved]);

  return (
    <section
      aria-label={rule ? `Edit ${rule.key}` : "New rule"}
      data-guard-rule-form
      className="rounded-lg border border-line"
    >
      <header className="flex items-center justify-between border-b border-line px-4 py-3">
        <h2 className="text-[14px] font-medium">{rule ? `Edit ${rule.key}` : "New rule"}</h2>
        <button
          type="button"
          onClick={onClose}
          aria-label="Close the rule form"
          className="rounded-md border border-line p-1.5 hover:bg-muted/40"
        >
          <X aria-hidden className="size-3.5" />
        </button>
      </header>

      <div className="grid gap-3 p-4 sm:grid-cols-2">
        <div>
          <label htmlFor="rule-key" className="block text-[12px] font-medium">
            Key
          </label>
          <input
            id="rule-key"
            data-guard-form-key
            value={draft.key}
            onChange={(event) => set("key", event.target.value)}
            aria-invalid={Boolean(keyProblem)}
            aria-describedby={keyProblem ? "rule-key-error" : undefined}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-[12.5px]"
          />
          {keyProblem ? (
            <p id="rule-key-error" className="mt-1 text-[11.5px] text-danger">
              {keyProblem}
            </p>
          ) : null}
        </div>

        <div>
          <label htmlFor="rule-label" className="block text-[12px] font-medium">
            Label
          </label>
          <select
            id="rule-label"
            value={draft.label}
            onChange={(event) => set("label", event.target.value)}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          >
            {vocab.labels.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
          {labelNote ? (
            <p className="mt-1 text-[11.5px] text-muted">
              Catches {labelNote.catches}. Does not catch {labelNote.misses}.
            </p>
          ) : null}
        </div>

        {draft.label === "custom" ? (
          <div>
            <label htmlFor="rule-custom-label" className="block text-[12px] font-medium">
              Custom label name
            </label>
            <input
              id="rule-custom-label"
              value={draft.custom_label}
              onChange={(event) => set("custom_label", event.target.value)}
              className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
            />
          </div>
        ) : null}

        <div className="sm:col-span-2">
          <label htmlFor="rule-pattern" className="block text-[12px] font-medium">
            Pattern (regular expression)
          </label>
          <input
            id="rule-pattern"
            data-guard-form-pattern
            value={draft.pattern}
            onChange={(event) => set("pattern", event.target.value)}
            aria-invalid={Boolean(patternProblem)}
            aria-describedby={patternProblem ? "rule-pattern-error" : undefined}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-[12.5px]"
          />
          {patternProblem ? (
            <p id="rule-pattern-error" className="mt-1 text-[11.5px] text-danger">
              {patternProblem}
            </p>
          ) : (
            <p className="mt-1 text-[11.5px] text-muted">
              Compiled when you save. An expression that does not compile is never stored.
            </p>
          )}
        </div>

        <div>
          <label htmlFor="rule-validator" className="block text-[12px] font-medium">
            Validator
          </label>
          <select
            id="rule-validator"
            value={draft.validator}
            onChange={(event) => set("validator", event.target.value)}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          >
            {vocab.validators.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
        </div>

        <div>
          <label htmlFor="rule-action" className="block text-[12px] font-medium">
            Action
          </label>
          <select
            id="rule-action"
            value={draft.action}
            onChange={(event) => set("action", event.target.value)}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          >
            {vocab.actions.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
        </div>

        <div>
          <label htmlFor="rule-severity" className="block text-[12px] font-medium">
            Severity (1–5)
          </label>
          <input
            id="rule-severity"
            type="number"
            min={1}
            max={5}
            value={draft.severity}
            onChange={(event) => set("severity", Number(event.target.value))}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          />
        </div>

        <div>
          <label htmlFor="rule-priority" className="block text-[12px] font-medium">
            Priority (1–999)
          </label>
          <input
            id="rule-priority"
            type="number"
            min={1}
            max={999}
            value={draft.priority}
            onChange={(event) => set("priority", Number(event.target.value))}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          />
        </div>

        <div>
          <label htmlFor="rule-providers" className="block text-[12px] font-medium">
            Providers (blank = everywhere)
          </label>
          <input
            id="rule-providers"
            value={draft.providers}
            onChange={(event) => set("providers", event.target.value)}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          />
        </div>

        <div>
          <label htmlFor="rule-features" className="block text-[12px] font-medium">
            Features (blank = everywhere)
          </label>
          <input
            id="rule-features"
            value={draft.features}
            onChange={(event) => set("features", event.target.value)}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          />
        </div>

        <div className="sm:col-span-2">
          <label htmlFor="rule-sample" className="block text-[12px] font-medium">
            Sample text
          </label>
          <input
            id="rule-sample"
            data-guard-form-sample
            value={draft.sample}
            onChange={(event) => set("sample", event.target.value)}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          />
          <p className="mt-1 text-[11.5px] text-muted">
            A sample is never sent to a provider. The probe below runs the guard's own detector
            over it in this process.
          </p>
        </div>

        <label className="flex items-center gap-2 text-[12.5px] sm:col-span-2">
          <input
            type="checkbox"
            checked={draft.enabled}
            onChange={(event) => set("enabled", event.target.checked)}
          />
          Enabled — a rule created switched off protects nothing while looking as if it does
        </label>

        {draft.sample.trim() ? (
          <div className="rounded-md border border-line bg-muted/20 p-3 sm:col-span-2">
            <div className="flex items-center justify-between gap-2">
              <h3 className="text-[12.5px] font-medium">Test this rule</h3>
              <button
                type="button"
                onClick={() => void runProbe()}
                data-guard-form-probe
                className="rounded-md border border-line px-2.5 py-1 text-[12px] hover:bg-muted/40"
              >
                Run
              </button>
            </div>
            {probe ? (
              <div className="mt-2">
                <p className="text-[12px] text-muted">
                  Verdict <span className="font-medium text-ink">{probe.verdict}</span> ·{" "}
                  {probe.matches} match{probe.matches === 1 ? "" : "es"}
                </p>
                <pre className="mt-1.5 overflow-x-auto whitespace-pre-wrap break-words rounded bg-bg p-2 font-mono text-[11.5px]">
                  {probe.masked_text}
                </pre>
              </div>
            ) : (
              <p className="mt-2 text-[12px] text-muted">
                Not run yet. The result is what the provider would receive.
              </p>
            )}
          </div>
        ) : null}

        {error ? (
          <p role="alert" className="text-[12.5px] text-danger sm:col-span-2">
            {error}
          </p>
        ) : null}
      </div>

      <footer className="flex gap-2 border-t border-line px-4 py-3">
        <button
          type="button"
          onClick={() => void submit()}
          data-guard-form-save
          disabled={saving || Boolean(keyProblem) || Boolean(patternProblem)}
          className="rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-bg disabled:opacity-40"
        >
          {saving ? "Saving…" : rule ? "Save rule" : "Create rule"}
        </button>
        <button
          type="button"
          onClick={onClose}
          className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
        >
          Cancel
        </button>
      </footer>
    </section>
  );
}