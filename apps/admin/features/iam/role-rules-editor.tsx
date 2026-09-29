"use client";

/**
 * The role rules editor (REQ-065, slice 3) — the wizard's fourth step.
 *
 * *Which role does a directory identity become* is the second half of enterprise sign-in, and it
 * is the half that bites: the attribute map failing gives you a broken login, while a rule set
 * quietly granting the wrong role gives you a working login and the wrong access. So the editor
 * shows the **whole walk**, not just the answer: every rule, what it read off the sample, and
 * whether it matched, was skipped, or was never reached because an earlier rule already decided.
 *
 * Four decisions the screen makes on purpose:
 *
 * * **The vocabulary comes from the server.** `when_kinds`, `when_operators` and `scope_types`
 *   arrive with the set, so an option the evaluator does not accept is never offered. A
 *   hard-coded list here is a second source of truth that goes stale quietly.
 * * **Save is one PUT of every rule.** The order is the semantics, so a save that applied rows
 *   one at a time would let a real sign-in land mid-save and see an empty set.
 * * **The dry run names the matched rule.** "Which of my six rules fired" cannot be answered by a
 *   role id, and the `reason` string shown here is the same one the audit line will carry.
 * * **A rule that did not fire says so per rule.** `no_match` (it read something and none of it
 *   matched) is a different bug from `not_reached` (an earlier rule decided) and a different one
 *   again from a rule that read nothing. Collapsing them into "no match" is what sends somebody
 *   to edit a rule that was fine.
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
} from "lucide-react";

import {
  ApiError,
  dryRunIamRoleRules,
  fetchIamRoleRules,
  saveIamRoleRules,
  type IamRoleRule,
  type IamRoleRuleDryRun,
  type IamRoleRules,
  type IamRuleOption,
} from "@/lib/api";

/** A sample identity that already answers the rules, so the dry run is usable on first open. */
const STARTER_SAMPLE = JSON.stringify(
  {
    sub: "00u-sample",
    email: "ada@example.com",
    name: "Ada Lovelace",
    department: "platform",
    title: "staff engineer",
    groups: ["engineering", "oncall"],
  },
  null,
  2,
);

const VERDICT_STYLE: Record<string, { label: string; className: string }> = {
  matched: { label: "matched", className: "border-emerald-300 bg-emerald-50 text-emerald-800" },
  no_match: { label: "no match", className: "border-line bg-muted/40 text-muted" },
  not_reached: { label: "not reached", className: "border-line bg-muted/20 text-muted" },
  disabled: { label: "disabled", className: "border-amber-300 bg-amber-50 text-amber-800" },
};

function blankRule(position: number, defaultRoleId: string | null): IamRoleRule {
  return {
    id: null,
    position,
    when_kind: "group",
    needs_key: true,
    when_key: "groups",
    when_operator: "equals",
    when_value: "",
    role_id: defaultRoleId ?? "",
    scope_type: "organization",
    site_id: null,
    stop: true,
    enabled: true,
  };
}

export function RoleRulesEditor({ providerId }: { providerId: string }) {
  const [rules, setRules] = useState<IamRoleRule[]>([]);
  const [kinds, setKinds] = useState<IamRuleOption[]>([]);
  const [operators, setOperators] = useState<IamRuleOption[]>([]);
  const [scopes, setScopes] = useState<IamRuleOption[]>([]);
  const [defaultRoleId, setDefaultRoleId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [savedAt, setSavedAt] = useState<string | null>(null);

  const [sample, setSample] = useState(STARTER_SAMPLE);
  const [dryRun, setDryRun] = useState<IamRoleRuleDryRun | null>(null);
  const [running, setRunning] = useState(false);
  const [runError, setRunError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const set: IamRoleRules = await fetchIamRoleRules(providerId);
      setRules(set.rules);
      setKinds(set.when_kinds);
      setOperators(set.when_operators);
      setScopes(set.scope_types);
      setDefaultRoleId(set.default_role_id);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "the role rules could not be read");
    } finally {
      setLoading(false);
    }
  }, [providerId]);

  useEffect(() => {
    void load();
  }, [load]);

  // Renumbered on every change rather than on save: the position is what the dry run and the audit
  // read, and a stale index that is only fixed on save is a preview of a set that does not exist.
  const commit = useCallback((next: IamRoleRule[]) => {
    setSavedAt(null);
    setDryRun(null);
    setRules(next.map((rule, index) => ({ ...rule, position: index })));
  }, []);

  const update = useCallback(
    (index: number, patch: Partial<IamRoleRule>) => {
      commit(rules.map((rule, at) => (at === index ? { ...rule, ...patch } : rule)));
    },
    [rules, commit],
  );

  const move = useCallback(
    (index: number, to: number) => {
      if (to < 0 || to >= rules.length) return;
      const next = [...rules];
      const [row] = next.splice(index, 1);
      next.splice(to, 0, row);
      commit(next);
    },
    [rules, commit],
  );

  // Keyboard reordering: the handle is a button, so the same move is reachable without a pointer.
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

  const kindOption = (name: string) => kinds.find((item) => item.name === name);

  const save = useCallback(async () => {
    setSaving(true);
    setError(null);
    try {
      const set = await saveIamRoleRules(providerId, rules);
      setRules(set.rules);
      setSavedAt(new Date().toLocaleTimeString());
      // A saved set invalidates any dry run: the answer on screen was about the previous rules,
      // and leaving it up would let somebody read a stale match as the current one.
      setDryRun(null);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "the rules could not be saved");
    } finally {
      setSaving(false);
    }
  }, [providerId, rules]);

  const run = useCallback(async () => {
    setRunning(true);
    setRunError(null);
    setDryRun(null);
    let parsed: unknown;
    try {
      parsed = JSON.parse(sample);
    } catch {
      setRunError(
        "the sample must be valid JSON — paste the claims object, LDAP entry or assertion summary",
      );
      setRunning(false);
      return;
    }
    try {
      setDryRun(await dryRunIamRoleRules(providerId, parsed));
    } catch (cause) {
      setRunError(cause instanceof ApiError ? cause.message : "the sample could not be read");
    } finally {
      setRunning(false);
    }
  }, [providerId, sample]);

  // A rule that cannot be saved is refused here, before the round trip, so the button that
  // explains the problem is the one next to the field that causes it.
  const incomplete = useMemo(
    () =>
      rules.filter(
        (rule) =>
          !rule.role_id || (rule.needs_key && !rule.when_key.trim()) ||
          (rule.when_kind !== "always" && !rule.when_value.trim()),
      ).length,
    [rules],
  );

  // An `always` rule above a narrower one can never fire, which is a configuration mistake the
  // panel can see but the server cannot: both rules are individually valid.
  const shadowed = useMemo(() => {
    const firstAlways = rules.findIndex((rule) => rule.when_kind === "always" && rule.enabled);
    return firstAlways === -1 ? [] : rules.slice(firstAlways + 1);
  }, [rules]);

  if (loading) {
    return (
      <div data-role-rules-loading className="flex items-center gap-2 text-[12px] text-muted">
        <Loader2 className="size-3.5 animate-spin" aria-hidden />
        reading the role rules…
      </div>
    );
  }

  return (
    <section data-role-rules className="flex flex-col gap-4">
      <header className="flex flex-wrap items-start justify-between gap-2">
        <div>
          <h3 className="text-[13px] font-semibold">Role mapping</h3>
          <p className="text-[12px] text-muted">
            First match wins, top to bottom. A sign-in that matches nothing takes the default role
            {defaultRoleId ? " configured on this provider" : " — and this provider has none, so it gets none"}.
          </p>
        </div>
        <div className="flex items-center gap-2">
          {savedAt ? (
            <span data-role-rules-saved className="text-[11.5px] text-emerald-700">
              saved at {savedAt}
            </span>
          ) : null}
          <button
            type="button"
            data-role-rules-add
            onClick={() => commit([...rules, blankRule(rules.length, defaultRoleId)])}
            className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px] text-ink"
          >
            <Plus className="size-3.5" aria-hidden />
            Add rule
          </button>
          <button
            type="button"
            data-role-rules-save
            disabled={saving || rules.length === 0}
            onClick={() => void save()}
            className="flex h-8 items-center gap-1.5 rounded-lg bg-ink px-2.5 text-[12px] text-background disabled:opacity-40"
          >
            {saving ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <Save className="size-3.5" aria-hidden />
            )}
            Save rules
          </button>
        </div>
      </header>

      {error ? (
        <p
          data-role-rules-error
          role="alert"
          className="flex items-start gap-1.5 rounded-lg border border-red-300 bg-red-50 px-2.5 py-2 text-[12px] text-red-800"
        >
          <AlertTriangle className="mt-px size-3.5 shrink-0" aria-hidden />
          {error}
        </p>
      ) : null}

      {incomplete > 0 ? (
        <p className="flex items-start gap-1.5 rounded-lg border border-amber-300 bg-amber-50 px-2.5 py-2 text-[12px] text-amber-800">
          <AlertTriangle className="mt-px size-3.5 shrink-0" aria-hidden />
          {incomplete} rule{incomplete === 1 ? "" : "s"} cannot be saved yet — a rule that grants no
          role, reads no key, or compares against nothing would match nothing, and the panel will
          not pretend otherwise.
        </p>
      ) : null}

      {shadowed.length > 0 ? (
        <p
          data-role-rules-shadowed
          className="flex items-start gap-1.5 rounded-lg border border-amber-300 bg-amber-50 px-2.5 py-2 text-[12px] text-amber-800"
        >
          <AlertTriangle className="mt-px size-3.5 shrink-0" aria-hidden />
          A rule that matches everybody is above {shadowed.length} narrower rule
          {shadowed.length === 1 ? "" : "s"}, so {shadowed.length === 1 ? "it" : "they"} can never
          fire. Both are valid on their own — only the order is wrong.
        </p>
      ) : null}

      {rules.length === 0 ? (
        <div
          data-role-rules-empty
          className="rounded-xl border border-dashed border-line px-4 py-8 text-center"
        >
          <p className="text-[13px] font-medium">No rules yet</p>
          <p className="mx-auto mt-1 max-w-md text-[12px] text-muted">
            Without a rule, every directory sign-in resolves to the default role
            {defaultRoleId ? "" : " — and this provider has none, so it is granted nothing"}. Add a
            rule to map a group or a department to a role.
          </p>
          <button
            type="button"
            onClick={() => commit([blankRule(0, defaultRoleId)])}
            className="mt-3 inline-flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px]"
          >
            <Plus className="size-3.5" aria-hidden />
            Add the first rule
          </button>
        </div>
      ) : (
        <ul data-role-rules-list className="flex flex-col gap-2">
          {rules.map((rule, index) => {
            const isAlways = rule.when_kind === "always";
            const kind = kindOption(rule.when_kind);
            const needsSite = scopes.find((item) => item.name === rule.scope_type)?.needs_site;
            return (
              <li
                key={`${index}-${rule.role_id}`}
                data-role-rule-row
                data-position={rule.position}
                className="rounded-xl border border-line p-2.5"
              >
                <div className="flex flex-col gap-2 lg:flex-row lg:items-end">
                  <div className="flex items-center gap-1 lg:w-14">
                    <button
                      type="button"
                      data-role-rule-grip
                      aria-label={`Reorder rule ${index + 1}`}
                      onClick={() => move(index, index - 1)}
                      onKeyDown={(event) => onRowKey(event, index)}
                      className="flex size-7 items-center justify-center rounded-md border border-line text-muted"
                    >
                      <GripVertical className="size-3.5" aria-hidden />
                    </button>
                    <span
                      data-role-rule-label
                      className="text-[11.5px] font-mono text-muted"
                      title="the position the audit reason names"
                    >
                      #{index + 1}
                    </span>
                  </div>

                  <div className="flex min-w-0 flex-1 flex-col gap-1.5">
                    <div className="grid grid-cols-2 gap-1.5 sm:grid-cols-4">
                      <label className="flex flex-col gap-0.5 text-[11px] text-muted">
                        When
                        <select
                          data-role-rule-kind
                          value={rule.when_kind}
                          onChange={(event) => {
                            const next = event.target.value;
                            const option = kindOption(next);
                            update(index, {
                              when_kind: next,
                              needs_key: option?.needs_key ?? true,
                              // Switching to `always` clears the condition; switching away from it
                              // leaves an empty key the row then reports, rather than inventing a
                              // key the operator did not type.
                              when_key: option?.needs_key === false ? "" : rule.when_key,
                              when_value: option?.needs_key === false ? "" : rule.when_value,
                            });
                          }}
                          className="h-8 rounded-lg border border-line bg-background px-2 text-[12px] text-ink"
                        >
                          {kinds.map((option) => (
                            <option key={option.name} value={option.name} title={option.hint}>
                              {option.name}
                            </option>
                          ))}
                        </select>
                      </label>

                      <label className="flex flex-col gap-0.5 text-[11px] text-muted">
                        Key
                        <input
                          data-role-rule-key
                          value={rule.when_key}
                          disabled={isAlways || !rule.needs_key}
                          placeholder={isAlways ? "not needed" : "groups"}
                          onChange={(event) => update(index, { when_key: event.target.value })}
                          className="h-8 rounded-lg border border-line bg-background px-2 text-[12px] text-ink disabled:opacity-50"
                        />
                      </label>

                      <label className="flex flex-col gap-0.5 text-[11px] text-muted">
                        Operator
                        <select
                          data-role-rule-operator
                          value={rule.when_operator}
                          disabled={isAlways}
                          onChange={(event) =>
                            update(index, { when_operator: event.target.value })
                          }
                          className="h-8 rounded-lg border border-line bg-background px-2 text-[12px] text-ink disabled:opacity-50"
                        >
                          {operators.map((option) => (
                            <option key={option.name} value={option.name} title={option.hint}>
                              {option.name}
                            </option>
                          ))}
                        </select>
                      </label>

                      <label className="flex flex-col gap-0.5 text-[11px] text-muted">
                        Value
                        <input
                          data-role-rule-value
                          value={rule.when_value}
                          disabled={isAlways}
                          placeholder={isAlways ? "matches everybody" : "engineering"}
                          onChange={(event) => update(index, { when_value: event.target.value })}
                          className="h-8 rounded-lg border border-line bg-background px-2 text-[12px] text-ink disabled:opacity-50"
                        />
                      </label>
                    </div>

                    <div className="grid grid-cols-2 gap-1.5 sm:grid-cols-3">
                      <label className="flex flex-col gap-0.5 text-[11px] text-muted">
                        Then role
                        <input
                          data-role-rule-role
                          value={rule.role_id}
                          placeholder={defaultRoleId ?? "role uuid"}
                          onChange={(event) => update(index, { role_id: event.target.value.trim() })}
                          className="h-8 rounded-lg border border-line bg-background px-2 font-mono text-[11.5px] text-ink"
                        />
                      </label>

                      <label className="flex flex-col gap-0.5 text-[11px] text-muted">
                        Scope
                        <select
                          data-role-rule-scope
                          value={rule.scope_type}
                          onChange={(event) => {
                            const next = event.target.value;
                            update(index, {
                              scope_type: next,
                              // The site is cleared with the scope rather than left behind: a
                              // rule that grants across the organization must not keep naming a
                              // site, and the server refuses that combination by name.
                              site_id: next === "site" ? rule.site_id : null,
                            });
                          }}
                          className="h-8 rounded-lg border border-line bg-background px-2 text-[12px] text-ink"
                        >
                          {scopes.map((option) => (
                            <option key={option.name} value={option.name} title={option.hint}>
                              {option.name}
                            </option>
                          ))}
                        </select>
                      </label>

                      <label className="flex flex-col gap-0.5 text-[11px] text-muted">
                        Site
                        <input
                          data-role-rule-site
                          value={rule.site_id ?? ""}
                          disabled={!needsSite}
                          placeholder={needsSite ? "site uuid" : "not needed"}
                          onChange={(event) =>
                            update(index, { site_id: event.target.value.trim() || null })
                          }
                          className="h-8 rounded-lg border border-line bg-background px-2 font-mono text-[11.5px] text-ink disabled:opacity-50"
                        />
                      </label>
                    </div>

                    <div className="flex flex-wrap items-center gap-3">
                      <label className="flex items-center gap-1.5 text-[11.5px] text-muted">
                        <input
                          type="checkbox"
                          data-role-rule-enabled
                          checked={rule.enabled}
                          onChange={(event) => update(index, { enabled: event.target.checked })}
                          className="size-3.5 accent-ink"
                        />
                        enabled
                      </label>
                      <label
                        className="flex items-center gap-1.5 text-[11.5px] text-muted"
                        title="recorded as `stopped` in the dry run; the first match still decides either way"
                      >
                        <input
                          type="checkbox"
                          data-role-rule-stop
                          checked={rule.stop}
                          onChange={(event) => update(index, { stop: event.target.checked })}
                          className="size-3.5 accent-ink"
                        />
                        end the search here
                      </label>
                      <button
                        type="button"
                        data-role-rule-remove
                        onClick={() => commit(rules.filter((_, at) => at !== index))}
                        className="ml-auto flex h-7 items-center gap-1 rounded-md border border-line px-2 text-[11.5px] text-muted"
                      >
                        <Trash2 className="size-3" aria-hidden />
                        Remove
                      </button>
                    </div>
                  </div>
                </div>
              </li>
            );
          })}
        </ul>
      )}

      <div data-role-rules-dry-run className="flex flex-col gap-2 rounded-xl border border-line p-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div>
            <h4 className="text-[12.5px] font-semibold">Dry run</h4>
            <p className="text-[11.5px] text-muted">
              Pastes a sample identity through the same evaluator a real sign-in uses, against the
              saved rules. Nothing is written.
            </p>
          </div>
          <button
            type="button"
            data-role-rules-run
            disabled={running}
            onClick={() => void run()}
            className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px]"
          >
            {running ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <PlayCircle className="size-3.5" aria-hidden />
            )}
            Preview
          </button>
        </div>

        <label className="flex flex-col gap-1 text-[11px] text-muted">
          Sample identity — a claims object, an LDAP entry or an assertion summary
          <textarea
            data-role-rules-sample
            value={sample}
            onChange={(event) => setSample(event.target.value)}
            rows={9}
            spellCheck={false}
            className="rounded-lg border border-line bg-background px-2 py-1.5 font-mono text-[11.5px] text-ink"
          />
        </label>

        {runError ? (
          <p
            data-role-rules-run-error
            role="alert"
            className="rounded-lg border border-red-300 bg-red-50 px-2.5 py-2 text-[12px] text-red-800"
          >
            {runError}
          </p>
        ) : null}

        {dryRun ? (
          <div data-role-rules-result className="flex flex-col gap-2">
            <p
              data-role-rules-reason
              className="flex items-center gap-1.5 rounded-lg border border-emerald-300 bg-emerald-50 px-2.5 py-2 text-[12px] text-emerald-800"
            >
              <CheckCircle2 className="size-3.5 shrink-0" aria-hidden />
              <span>
                {dryRun.reason}
                {dryRun.role_id ? (
                  <>
                    {" · role "}
                    <code className="font-mono">{short(dryRun.role_id)}</code>
                  </>
                ) : (
                  " · no role granted"
                )}
              </span>
            </p>
            <ul className="flex flex-col gap-1">
              {dryRun.trace.map((entry) => {
                const style = VERDICT_STYLE[entry.verdict] ?? VERDICT_STYLE.no_match;
                return (
                  <li
                    key={entry.index}
                    data-role-rule-trace
                    data-verdict={entry.verdict}
                    className={`flex flex-col gap-1 rounded-lg border px-2.5 py-1.5 text-[11.5px] sm:flex-row sm:items-center sm:gap-2 ${style.className} ${
                      entry.matched ? "ring-1 ring-emerald-400" : ""
                    }`}
                  >
                    <span className="font-mono">{entry.label}</span>
                    <span className="font-mono">
                      {entry.when_kind}
                      {entry.when_key ? ` ${entry.when_key}` : ""}{" "}
                      {entry.when_operator} {entry.when_value}
                    </span>
                    <span className="font-mono">→ {short(entry.role_id)}</span>
                    <span className="sm:ml-auto">
                      {style.label}
                      {entry.read.length > 0 ? (
                        <span className="ml-1 opacity-80">read: {entry.read.join(", ")}</span>
                      ) : entry.verdict === "no_match" ? (
                        <span className="ml-1 opacity-80">read nothing from this sample</span>
                      ) : null}
                    </span>
                  </li>
                );
              })}
            </ul>
          </div>
        ) : null}
      </div>
    </section>
  );
}

/** A uuid is a column's worth of characters in a table; the full value is in the title. */
function short(id: string): string {
  return id.length > 12 ? `${id.slice(0, 8)}…` : id;
}
