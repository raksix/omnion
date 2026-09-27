"use client";

/**
 * `/settings/iam/policies` — the ABAC policy builder (REQ-006, slice 4a).
 *
 * A policy says *when* (a condition tree over attributes), *what* (effect: allow or deny) and
 * *about which permissions* (targets, exact or with `*`). It is evaluated after the roles have
 * had their say, so an allow policy can grant what RBAC did not and a deny policy takes away what
 * RBAC granted; the highest priority decides first and equal priorities resolve to deny.
 *
 * The screen is deliberately not a code editor: conditions are rows (attribute → operator →
 * value, ALL/ANY, per-row NOT) with a JSON view for the shapes the rows cannot express, the THEN
 * block holds the effect, the targets, the priority and the enabled switch, and **Test** runs the
 * dry run over sample attributes and highlights which leaves matched. Every save stores a
 * version; the History panel shows them.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { FlaskConical, History, Plus, Save, ScrollText, Trash2, X } from "lucide-react";

import { useSession } from "@/lib/session";
import {
  ApiError,
  createIamPolicy,
  deleteIamPolicy,
  fetchIamPolicies,
  fetchIamPolicyVersions,
  fetchOrganizations,
  fetchPermissionCatalogue,
  testIamPolicy,
  updateIamPolicy,
  type IamPermissionDef,
  type IamPolicy,
  type IamPolicyInput,
  type IamPolicyTest,
  type IamPolicyVersion,
} from "@/lib/api";
import type { Organization } from "@/lib/types";

/** Operators the engine knows, in the order the builder lists them. */
const OPERATORS = ["==", "!=", ">", "<", "in", "starts_with", "contains"] as const;

/** Attributes the editor suggests; any dotted path is allowed. */
const SUGGESTED_ATTRIBUTES = [
  "action",
  "resource.site_id",
  "resource.path",
  "resource.department",
  "resource.module",
  "organization.id",
  "subject.type",
  "user.plan",
] as const;

/** One builder row. */
type ConditionRow = {
  id: string;
  negate: boolean;
  attribute: string;
  operator: string;
  value: string;
};

/**
 * The row the editor opens with.
 *
 * Its id is a constant on purpose: this component is server-rendered first, and a counter that
 * runs during the render would give the server and the client different ids — the classic
 * hydration mismatch. Rows added later exist only on the client, so a counter is safe there.
 */
function initialRow(): ConditionRow {
  return {
    id: "row-initial",
    negate: false,
    attribute: "resource.path",
    operator: "starts_with",
    value: "/blog",
  };
}

let rowSeq = 0;
/** A row the reader added (client-only). */
function nextRow(): ConditionRow {
  rowSeq += 1;
  return {
    id: `row-added-${rowSeq}`,
    negate: false,
    attribute: "resource.path",
    operator: "starts_with",
    value: "/blog",
  };
}

/** Parse a value input: JSON when it parses, a plain string otherwise. */
function parseValue(input: string): unknown {
  const trimmed = input.trim();
  if (trimmed === "") return "";
  try {
    return JSON.parse(trimmed);
  } catch {
    return trimmed;
  }
}

/** Read a stored tree back into rows when it is expressible; `null` when it is not. */
function rowsFromConditions(
  conditions: unknown,
): { mode: "all" | "any"; rows: ConditionRow[] } | null {
  if (typeof conditions !== "object" || conditions === null) return null;
  const record = conditions as Record<string, unknown>;
  const key = "all" in record ? "all" : "any" in record ? "any" : null;
  if (!key) return null;
  const children = record[key];
  if (!Array.isArray(children)) return null;

  const rows: ConditionRow[] = [];
  for (const child of children) {
    let node = child as unknown;
    let negate = false;
    if (typeof node === "object" && node !== null && "not" in (node as Record<string, unknown>)) {
      negate = true;
      node = (node as Record<string, unknown>).not;
    }
    if (typeof node !== "object" || node === null) return null;
    const leaf = node as Record<string, unknown>;
    const attribute = typeof leaf.attribute === "string" ? leaf.attribute : null;
    const operator = typeof leaf.operator === "string" ? leaf.operator : null;
    if (!attribute || !operator || !("value" in leaf)) return null;
    rows.push({
      id: `stored-${rows.length}-${attribute}`,
      negate,
      attribute,
      operator,
      value: typeof leaf.value === "string" ? leaf.value : JSON.stringify(leaf.value),
    });
  }
  return { mode: key, rows };
}

/** Build the stored tree from the rows. */
function conditionsFromRows(mode: "all" | "any", rows: ConditionRow[]): unknown {
  const leaves = rows
    .filter((row) => row.attribute.trim() !== "")
    .map((row) => {
      const leaf = {
        attribute: row.attribute.trim(),
        operator: row.operator,
        value: parseValue(row.value),
      };
      return row.negate ? { not: leaf } : leaf;
    });
  return mode === "all" ? { all: leaves } : { any: leaves };
}

/** Short label for one row's value. */
function valueLabel(value: unknown): string {
  if (typeof value === "string") return `"${value}"`;
  return JSON.stringify(value);
}

/** `/settings/iam/policies`. */
export function PoliciesView() {
  const { user } = useSession();
  const platformAccount = user ? user.organization_id === null : false;

  const [organizations, setOrganizations] = useState<Organization[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string | null>(null);
  const [catalogue, setCatalogue] = useState<IamPermissionDef[]>([]);

  const [policies, setPolicies] = useState<IamPolicy[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [versions, setVersions] = useState<IamPolicyVersion[] | null>(null);
  const [showHistory, setShowHistory] = useState(false);

  // The draft under edit.
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [effect, setEffect] = useState<"allow" | "deny">("deny");
  const [priority, setPriority] = useState(500);
  const [enabled, setEnabled] = useState(true);
  const [targets, setTargets] = useState<string[]>([]);
  const [targetInput, setTargetInput] = useState("");
  const [mode, setMode] = useState<"all" | "any">("all");
  const [rows, setRows] = useState<ConditionRow[]>(() => [initialRow()]);
  const [jsonMode, setJsonMode] = useState(false);
  const [jsonText, setJsonText] = useState('{\n  "all": []\n}');
  const [noticeAboutNesting, setNoticeAboutNesting] = useState<string | null>(null);

  // The dry run.
  const [testPermission, setTestPermission] = useState("");
  const [testAttributes, setTestAttributes] = useState('{\n  "resource": { "path": "/blog/hello" }\n}');
  const [testResult, setTestResult] = useState<IamPolicyTest | null>(null);
  const [testing, setTesting] = useState(false);

  const [busy, setBusy] = useState(false);
  const [deleteArmed, setDeleteArmed] = useState(false);

  const activeOrg = platformAccount ? selectedOrg : (user?.organization_id ?? null);

  const loadPolicies = useCallback(async (organizationId: string | null) => {
    setLoading(true);
    setError(null);
    try {
      const answer = await fetchIamPolicies(organizationId ?? undefined);
      setPolicies(answer.policies);
    } catch (cause) {
      setPolicies([]);
      setError(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The list could not load.",
      );
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void fetchPermissionCatalogue()
      .then(setCatalogue)
      .catch(() => setCatalogue([]));
  }, []);

  useEffect(() => {
    if (!user) return;
    if (user.organization_id !== null) {
      void loadPolicies(null);
      return;
    }
    if (organizations !== null) return;
    void fetchOrganizations()
      .then((list) => {
        setOrganizations(list);
        setSelectedOrg(list[0]?.id ?? null);
      })
      .catch(() => setOrganizations([]));
  }, [user, organizations, loadPolicies]);

  useEffect(() => {
    if (platformAccount && selectedOrg) {
      void loadPolicies(selectedOrg);
    }
  }, [platformAccount, selectedOrg, loadPolicies]);

  /** Load a policy into the editor. */
  const openPolicy = useCallback(async (policy: IamPolicy) => {
    setSelectedId(policy.id);
    setName(policy.name);
    setDescription(policy.description);
    setEffect(policy.effect);
    setPriority(policy.priority);
    setEnabled(policy.enabled);
    setTargets(policy.target_permissions);
    setTestPermission(policy.target_permissions[0] ?? "");
    setTestResult(null);
    setDeleteArmed(false);
    setNoticeAboutNesting(null);

    const parsed = rowsFromConditions(policy.conditions);
    if (parsed) {
      setMode(parsed.mode);
      setRows(parsed.rows.length > 0 ? parsed.rows : []);
      setJsonMode(false);
      setJsonText(JSON.stringify(policy.conditions, null, 2));
    } else {
      setJsonMode(true);
      setJsonText(JSON.stringify(policy.conditions, null, 2));
      setNoticeAboutNesting(
        "This condition tree uses nesting the row editor does not show — it is editable as JSON.",
      );
    }

    try {
      const answer = await fetchIamPolicyVersions(policy.id);
      setVersions(answer.versions);
    } catch {
      setVersions([]);
    }
  }, []);

  /** Start a new policy. */
  const startNew = useCallback(() => {
    setSelectedId(null);
    setName("");
    setDescription("");
    setEffect("deny");
    setPriority(500);
    setEnabled(true);
    setTargets([]);
    setTargetInput("");
    setMode("all");
    setRows([initialRow()]);
    setJsonMode(false);
    setJsonText('{\n  "all": []\n}');
    setVersions(null);
    setShowHistory(false);
    setDeleteArmed(false);
    setTestResult(null);
    setTestPermission("");
    setNoticeAboutNesting(null);
    setNotice(null);
  }, []);

  /** The draft as the API receives it. */
  const draft = useMemo<IamPolicyInput>(() => {
    // Never throw out of a render: a JSON body that does not parse falls back to the rows, and
    // the draft problem below tells the reader before anything is sent.
    let conditions = conditionsFromRows(mode, rows);
    if (jsonMode) {
      try {
        conditions = JSON.parse(jsonText) as unknown;
      } catch {
        conditions = conditionsFromRows(mode, rows);
      }
    }
    return {
      name,
      description,
      effect,
      priority,
      conditions,
      target_permissions: targets,
      enabled,
      organizationId: activeOrg,
    };
    // `jsonText` is parsed lazily below — a broken JSON body is refused before sending.
  }, [jsonMode, jsonText, mode, rows, name, description, effect, priority, targets, enabled, activeOrg]);

  /** `null` when the draft is coherent; otherwise the sentence to show. */
  const draftProblem = useMemo<string | null>(() => {
    if (name.trim() === "") return "A policy needs a name.";
    if (!Number.isFinite(priority) || priority < 0 || priority > 1000) {
      return "Priority must be between 0 and 1000.";
    }
    if (targets.length === 0) return "Add at least one target permission.";
    if (jsonMode) {
      try {
        JSON.parse(jsonText);
      } catch {
        return "The JSON conditions do not parse.";
      }
    }
    return null;
  }, [name, priority, targets, jsonMode, jsonText]);

  const addTarget = (raw?: string) => {
    const candidate = (raw ?? targetInput).trim();
    if (candidate === "") return;
    if (!targets.includes(candidate)) {
      setTargets([...targets, candidate]);
    }
    setTargetInput("");
  };

  const updateRow = (id: string, patch: Partial<ConditionRow>) => {
    setRows((current) => current.map((row) => (row.id === id ? { ...row, ...patch } : row)));
  };

  const save = async () => {
    if (draftProblem) {
      setError(draftProblem);
      return;
    }
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const saved = selectedId
        ? await updateIamPolicy(selectedId, draft)
        : await createIamPolicy(draft);
      setNotice(
        selectedId
          ? `Saved — version ${saved.version}.`
          : `Created — version ${saved.version}. It is evaluated after the roles from now on.`,
      );
      await loadPolicies(activeOrg);
      await openPolicy(saved);
      setPolicies((current) => {
        if (!current) return current;
        const without = current.filter((policy) => policy.id !== saved.id);
        return [...without, saved].sort((left, right) =>
          right.priority - left.priority || left.name.localeCompare(right.name),
        );
      });
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? `${cause.message} (${cause.code})`
          : "The policy could not be saved.",
      );
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!selectedId) return;
    if (!deleteArmed) {
      setDeleteArmed(true);
      setNotice("Press delete again to remove this policy.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await deleteIamPolicy(selectedId);
      setNotice("The policy was removed.");
      startNew();
      await loadPolicies(activeOrg);
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? `${cause.message} (${cause.code})`
          : "The policy could not be removed.",
      );
    } finally {
      setBusy(false);
      setDeleteArmed(false);
    }
  };

  const runTest = async () => {
    let attributes: Record<string, unknown> = {};
    try {
      const parsed = JSON.parse(testAttributes) as unknown;
      if (typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)) {
        attributes = parsed as Record<string, unknown>;
      } else {
        setError("Sample attributes must be a JSON object.");
        return;
      }
    } catch {
      setError("Sample attributes must parse as JSON.");
      return;
    }
    if (testPermission.trim() === "") {
      setError("Pick the permission to test against.");
      return;
    }

    // A dry run needs a stored policy to hang from; an unsaved draft is tested through it.
    setTesting(true);
    setError(null);
    setTestResult(null);
    try {
      let anchorId = selectedId;
      if (!anchorId) {
        if (draftProblem) {
          setError(draftProblem);
          return;
        }
        const created = await createIamPolicy({ ...draft, enabled: false });
        anchorId = created.id;
        setSelectedId(created.id);
        setNotice("A disabled copy was saved so the draft can be tested; enable it when you are happy.");
        await loadPolicies(activeOrg);
      }
      const answer = await testIamPolicy(anchorId, {
        permission: testPermission.trim(),
        attributes,
        policy: draft,
      });
      setTestResult(answer);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The test could not run.",
      );
    } finally {
      setTesting(false);
    }
  };

  return (
    <div className="flex flex-col gap-4" data-policies-view>
      <div className="flex flex-wrap items-center justify-between gap-3">
        {platformAccount && organizations && organizations.length > 0 ? (
          <label className="flex items-center gap-2 text-[12.5px]">
            <span className="text-muted">Organization</span>
            <select
              value={selectedOrg ?? ""}
              data-policies-organization
              onChange={(event) => {
                setSelectedOrg(event.target.value);
                startNew();
              }}
              className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            >
              {organizations.map((organization) => (
                <option key={organization.id} value={organization.id}>
                  {organization.name}
                </option>
              ))}
            </select>
          </label>
        ) : (
          <span />
        )}
        <button
          type="button"
          data-policy-new
          onClick={startNew}
          className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
        >
          <Plus className="size-3.5" aria-hidden /> New policy
        </button>
      </div>

      <p className="text-[12.5px] text-muted">
        Policies run after the roles: an <strong>allow</strong> grants what RBAC did not, a{" "}
        <strong>deny</strong> takes away what RBAC granted. Higher priority decides first; equal
        priorities resolve to deny.
      </p>

      {error ? (
        <p
          role="alert"
          data-policies-error
          className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
        >
          {error}
        </p>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-[320px_1fr]">
        {/* The list */}
        <section className="flex flex-col gap-2" aria-label="Policies" data-policies-list>
          {loading ? (
            <div className="flex flex-col gap-2">
              {[0, 1, 2].map((index) => (
                <div key={index} className="h-16 animate-pulse rounded-xl border border-line bg-quiet-soft" />
              ))}
            </div>
          ) : policies && policies.length === 0 ? (
            <div className="rounded-xl border border-dashed border-line bg-surface p-4 text-[12.5px] text-muted" data-policies-empty>
              No policies yet. A policy states a condition, an effect and the permissions it
              speaks about — start with <strong>New policy</strong>.
            </div>
          ) : (
            policies?.map((policy) => (
              <button
                key={policy.id}
                type="button"
                data-policy-row={policy.id}
                data-policy-effect={policy.effect}
                data-policy-enabled={policy.enabled ? "true" : "false"}
                onClick={() => void openPolicy(policy)}
                className={`rounded-xl border p-3 text-left transition ${
                  selectedId === policy.id
                    ? "border-accent bg-accent-soft/40"
                    : "border-line bg-surface hover:bg-quiet-soft"
                }`}
              >
                <span className="flex items-center justify-between gap-2">
                  <span className="text-[13px] font-medium text-ink">{policy.name}</span>
                  <span
                    className={`rounded-md border px-1.5 py-0.5 text-[10.5px] font-semibold uppercase ${
                      policy.effect === "deny"
                        ? "border-danger/40 bg-danger-soft text-caution"
                        : "border-positive/40 bg-positive-soft text-positive"
                    }`}
                  >
                    {policy.effect}
                  </span>
                </span>
                <span className="mt-1 block text-[11.5px] text-muted">
                  priority {policy.priority} · {policy.target_permissions.length} target(s) · v
                  {policy.version}
                  {policy.enabled ? "" : " · disabled"}
                </span>
              </button>
            ))
          )}
        </section>

        {/* The editor */}
        <section
          className="flex flex-col gap-4 rounded-xl border border-line bg-surface p-4"
          data-policy-editor
        >
          <header className="flex flex-wrap items-center justify-between gap-2">
            <h2 className="flex items-center gap-2 text-[14px] font-semibold text-ink">
              <ScrollText className="size-4 text-muted" aria-hidden />
              {selectedId ? "Edit policy" : "New policy"}
            </h2>
            {selectedId ? (
              <button
                type="button"
                data-policy-history-toggle
                onClick={() => setShowHistory((current) => !current)}
                className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-quiet-soft"
              >
                <History className="size-3.5" aria-hidden /> History
                {versions ? ` (${versions.length})` : ""}
              </button>
            ) : null}
          </header>

          {noticeAboutNesting ? (
            <p className="rounded-lg border border-line bg-canvas px-3 py-2 text-[12px] text-muted" data-policy-nesting-note>
              {noticeAboutNesting}
            </p>
          ) : null}

          <div className="grid gap-3 sm:grid-cols-2">
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Name</span>
              <input
                value={name}
                data-policy-name
                onChange={(event) => setName(event.target.value)}
                placeholder="Legal hold"
                className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Description</span>
              <input
                value={description}
                data-policy-description
                onChange={(event) => setDescription(event.target.value)}
                placeholder="Why this policy exists"
                className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
          </div>

          {/* WHEN */}
          <div className="rounded-xl border border-line bg-canvas p-3">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <h3 className="text-[12.5px] font-semibold tracking-wide text-muted uppercase">
                When
              </h3>
              <div className="flex items-center gap-2 text-[12px]">
                <label className="flex items-center gap-1.5">
                  <span className="text-muted">Match</span>
                  <select
                    value={mode}
                    data-conditions-mode
                    disabled={jsonMode}
                    onChange={(event) => setMode(event.target.value as "all" | "any")}
                    className="h-8 rounded-lg border border-line bg-surface px-2 text-[12px] text-ink outline-none focus:border-accent"
                  >
                    <option value="all">ALL of</option>
                    <option value="any">ANY of</option>
                  </select>
                </label>
                <label className="flex items-center gap-1.5 text-muted">
                  <input
                    type="checkbox"
                    checked={jsonMode}
                    data-conditions-json-toggle
                    onChange={(event) => {
                      if (event.target.checked) {
                        setJsonText(JSON.stringify(conditionsFromRows(mode, rows), null, 2));
                        setError(null);
                      } else {
                        let parsed: { mode: "all" | "any"; rows: ConditionRow[] } | null = null;
                        try {
                          parsed = rowsFromConditions(JSON.parse(jsonText));
                        } catch {
                          setError("The JSON conditions do not parse — fix them before leaving the JSON view.");
                          return;
                        }
                        if (parsed) {
                          setMode(parsed.mode);
                          setRows(parsed.rows);
                          setError(null);
                        } else {
                          setError(
                            "This JSON nests groups deeper than the row editor shows; keep the JSON view to edit it.",
                          );
                          return;
                        }
                      }
                      setJsonMode(event.target.checked);
                    }}
                    className="size-3.5 rounded border-line"
                  />
                  JSON view
                </label>
              </div>
            </div>

            {jsonMode ? (
              <textarea
                value={jsonText}
                aria-label="Condition tree as JSON"
                data-conditions-json
                onChange={(event) => setJsonText(event.target.value)}
                rows={8}
                spellCheck={false}
                className="mt-2 w-full rounded-lg border border-line bg-surface px-2 py-2 font-mono text-[12px] text-ink outline-none focus:border-accent"
              />
            ) : (
              <div className="mt-2 flex flex-col gap-2">
                {rows.length === 0 ? (
                  <p className="text-[12px] text-muted">
                    No conditions: the policy applies to every request its targets cover.
                  </p>
                ) : null}
                {rows.map((row) => (
                  <div
                    key={row.id}
                    data-condition-row={row.id}
                    className="flex flex-wrap items-center gap-2"
                  >
                    <label className="flex items-center gap-1 text-[11.5px] text-muted">
                      <input
                        type="checkbox"
                        checked={row.negate}
                        data-condition-not={row.id}
                        onChange={(event) => updateRow(row.id, { negate: event.target.checked })}
                        className="size-3.5 rounded border-line"
                      />
                      NOT
                    </label>
                    <input
                      value={row.attribute}
                      aria-label="Condition attribute"
                      data-condition-attribute={row.id}
                      list="policy-attributes"
                      onChange={(event) => updateRow(row.id, { attribute: event.target.value })}
                      placeholder="resource.path"
                      className="h-8 w-40 rounded-lg border border-line bg-surface px-2 font-mono text-[12px] text-ink outline-none focus:border-accent"
                    />
                    <select
                      value={row.operator}
                      aria-label="Condition operator"
                      data-condition-operator={row.id}
                      onChange={(event) => updateRow(row.id, { operator: event.target.value })}
                      className="h-8 rounded-lg border border-line bg-surface px-2 font-mono text-[12px] text-ink outline-none focus:border-accent"
                    >
                      {OPERATORS.map((operator) => (
                        <option key={operator} value={operator}>
                          {operator}
                        </option>
                      ))}
                    </select>
                    <input
                      value={row.value}
                      aria-label="Condition value"
                      data-condition-value={row.id}
                      onChange={(event) => updateRow(row.id, { value: event.target.value })}
                      placeholder="/legal"
                      className="h-8 min-w-40 flex-1 rounded-lg border border-line bg-surface px-2 font-mono text-[12px] text-ink outline-none focus:border-accent"
                    />
                    <button
                      type="button"
                      data-condition-remove={row.id}
                      aria-label="Remove condition"
                      onClick={() => setRows((current) => current.filter((entry) => entry.id !== row.id))}
                      className="rounded-md border border-line p-1.5 text-muted transition hover:bg-quiet-soft"
                    >
                      <X className="size-3.5" aria-hidden />
                    </button>
                  </div>
                ))}
                <div className="flex items-center gap-2">
                  <button
                    type="button"
                    data-condition-add
                    onClick={() => setRows((current) => [...current, nextRow()])}
                    className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-quiet-soft"
                  >
                    <Plus className="size-3.5" aria-hidden /> Add condition
                  </button>
                  <span className="text-[11.5px] text-muted">
                    Values parse as JSON when they can; a missing attribute compares as null.
                  </span>
                </div>
                <datalist id="policy-attributes">
                  {SUGGESTED_ATTRIBUTES.map((attribute) => (
                    <option key={attribute} value={attribute} />
                  ))}
                </datalist>
              </div>
            )}
          </div>

          {/* THEN */}
          <div className="rounded-xl border border-line bg-canvas p-3">
            <h3 className="text-[12.5px] font-semibold tracking-wide text-muted uppercase">Then</h3>
            <div className="mt-2 grid gap-3 sm:grid-cols-3">
              <label className="flex flex-col gap-1.5">
                <span className="text-[12.5px] font-medium text-ink">Effect</span>
                <select
                  value={effect}
                  data-policy-effect
                  onChange={(event) => setEffect(event.target.value as "allow" | "deny")}
                  className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent"
                >
                  <option value="deny">deny</option>
                  <option value="allow">allow</option>
                </select>
              </label>
              <label className="flex flex-col gap-1.5">
                <span className="text-[12.5px] font-medium text-ink">Priority (0–1000)</span>
                <input
                  type="number"
                  min={0}
                  max={1000}
                  value={priority}
                  data-policy-priority
                  onChange={(event) => setPriority(Number(event.target.value))}
                  className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent"
                />
              </label>
              <label className="flex items-center gap-2 pt-5 text-[12.5px] text-ink">
                <input
                  type="checkbox"
                  checked={enabled}
                  data-policy-enabled
                  onChange={(event) => setEnabled(event.target.checked)}
                  className="size-4 rounded border-line"
                />
                Enabled (evaluated)
              </label>
            </div>

            <div className="mt-3 flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Target permissions</span>
              <div className="flex flex-wrap gap-1.5" data-policy-targets>
                {targets.length === 0 ? (
                  <span className="text-[12px] text-muted">No targets yet.</span>
                ) : (
                  targets.map((target) => (
                    <span
                      key={target}
                      data-policy-target={target}
                      className="flex items-center gap-1 rounded-md border border-line bg-surface px-1.5 py-0.5 font-mono text-[11.5px]"
                    >
                      {target}
                      <button
                        type="button"
                        aria-label={`Remove ${target}`}
                        onClick={() => setTargets(targets.filter((entry) => entry !== target))}
                        className="text-muted transition hover:text-caution"
                      >
                        <X className="size-3" aria-hidden />
                      </button>
                    </span>
                  ))
                )}
              </div>
              <div className="flex items-center gap-2">
                <input
                  value={targetInput}
                  aria-label="Target permission"
                  data-policy-target-input
                  list="policy-permissions"
                  onChange={(event) => setTargetInput(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.preventDefault();
                      addTarget();
                    }
                  }}
                  placeholder="content.pages.*"
                  className="h-9 flex-1 rounded-lg border border-line bg-surface px-2 font-mono text-[12px] text-ink outline-none focus:border-accent"
                />
                <button
                  type="button"
                  data-policy-target-add
                  onClick={() => addTarget()}
                  className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-quiet-soft"
                >
                  Add
                </button>
              </div>
              <datalist id="policy-permissions">
                {catalogue.slice(0, 400).map((entry) => (
                  <option key={entry.key} value={entry.key} />
                ))}
              </datalist>
              <span className="text-[11.5px] text-muted">
                An exact key or a pattern with `*` (for example <code>content.pages.*</code>).
              </span>
            </div>
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <button
              type="button"
              data-policy-save
              disabled={busy}
              onClick={() => void save()}
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
            >
              <Save className="size-3.5" aria-hidden /> {selectedId ? "Save" : "Create"}
            </button>
            {selectedId ? (
              <button
                type="button"
                data-policy-delete
                disabled={busy}
                onClick={() => void remove()}
                className="flex items-center gap-1.5 rounded-lg border border-danger/40 px-3 py-1.5 text-[12.5px] font-medium text-caution transition hover:bg-danger-soft"
              >
                <Trash2 className="size-3.5" aria-hidden /> {deleteArmed ? "Confirm delete" : "Delete"}
              </button>
            ) : null}
            {draftProblem ? (
              <span data-policy-draft-problem className="text-[12px] text-caution">
                {draftProblem}
              </span>
            ) : null}
          </div>

          {/* The dry run */}
          <div className="rounded-xl border border-line bg-canvas p-3" data-policy-test>
            <h3 className="flex items-center gap-2 text-[12.5px] font-semibold tracking-wide text-muted uppercase">
              <FlaskConical className="size-3.5" aria-hidden /> Test (dry run)
            </h3>
            <div className="mt-2 grid gap-3 sm:grid-cols-2">
              <label className="flex flex-col gap-1.5">
                <span className="text-[12.5px] font-medium text-ink">Permission</span>
                <input
                  value={testPermission}
                  data-test-permission
                  list="policy-permissions"
                  onChange={(event) => setTestPermission(event.target.value)}
                  placeholder="content.pages.read"
                  className="h-9 rounded-lg border border-line bg-surface px-2 font-mono text-[12px] text-ink outline-none focus:border-accent"
                />
              </label>
              <label className="flex flex-col gap-1.5">
                <span className="text-[12.5px] font-medium text-ink">Sample attributes (JSON)</span>
                <textarea
                  value={testAttributes}
                  data-test-attributes
                  rows={3}
                  spellCheck={false}
                  onChange={(event) => setTestAttributes(event.target.value)}
                  className="w-full rounded-lg border border-line bg-surface px-2 py-1.5 font-mono text-[12px] text-ink outline-none focus:border-accent"
                />
              </label>
            </div>
            <button
              type="button"
              data-test-run
              disabled={testing}
              onClick={() => void runTest()}
              className="mt-2 flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft disabled:opacity-60"
            >
              <FlaskConical className="size-3.5" aria-hidden /> {testing ? "Testing…" : "Test"}
            </button>

            {testResult ? (
              <div className="mt-3 flex flex-col gap-2" data-test-result>
                <div
                  className={`flex flex-wrap items-center gap-2 rounded-lg border p-2.5 ${
                    testResult.applies
                      ? "border-positive/40 bg-positive-soft"
                      : "border-line bg-surface"
                  }`}
                >
                  <span
                    data-test-verdict
                    data-test-applies={testResult.applies ? "true" : "false"}
                    className={`text-[13px] font-semibold ${testResult.applies ? "text-positive" : "text-muted"}`}
                  >
                    {testResult.applies
                      ? `APPLIES (${testResult.effect})`
                      : "DOES NOT APPLY"}
                  </span>
                  <span data-test-decides className="text-[12px] text-muted">
                    {testResult.decides
                      ? "would decide this request"
                      : testResult.decision
                        ? `another policy wins: ${testResult.decision.policy_name} (${testResult.decision.effect}, priority ${testResult.decision.priority})`
                        : "nothing decides this request"}
                  </span>
                </div>
                <p data-test-note className="text-[12px] text-muted">
                  {testResult.note} Targeted: {String(testResult.targeted)} · Conditions:{" "}
                  {String(testResult.conditions_satisfied)}.
                </p>
                <ul className="flex flex-col gap-1">
                  {testResult.trace.map((leaf) => (
                    <li
                      key={leaf.path}
                      data-test-leaf={leaf.path}
                      data-leaf-satisfied={leaf.satisfied ? "true" : "false"}
                      className={`rounded-lg border px-2 py-1 font-mono text-[11.5px] ${
                        leaf.satisfied
                          ? "border-positive/40 bg-positive-soft"
                          : "border-line bg-surface text-muted"
                      }`}
                    >
                      {leaf.attribute} {leaf.operator} {valueLabel(leaf.expected)} →{" "}
                      {valueLabel(leaf.resolved ?? null)}
                    </li>
                  ))}
                  {testResult.trace.length === 0 ? (
                    <li className="text-[11.5px] text-muted" data-test-leaf="none">
                      No conditions: the targets alone decide.
                    </li>
                  ) : null}
                </ul>
              </div>
            ) : null}
          </div>

          {showHistory && selectedId ? (
            <div className="rounded-xl border border-line bg-canvas p-3" data-policy-versions>
              {versions && versions.length > 0 ? (
                <table className="w-full text-left text-[12px]">
                  <thead className="text-[11px] tracking-wide text-muted uppercase">
                    <tr>
                      <th className="py-1 pr-2 font-medium">Version</th>
                      <th className="py-1 pr-2 font-medium">Effect</th>
                      <th className="py-1 pr-2 font-medium">Priority</th>
                      <th className="py-1 pr-2 font-medium">State</th>
                      <th className="py-1 font-medium">Saved</th>
                    </tr>
                  </thead>
                  <tbody>
                    {versions.map((version) => (
                      <tr key={version.version} data-policy-version={version.version} className="border-t border-line">
                        <td className="py-1.5 pr-2 font-mono">v{version.version}</td>
                        <td className="py-1.5 pr-2">{version.effect}</td>
                        <td className="py-1.5 pr-2">{version.priority}</td>
                        <td className="py-1.5 pr-2">{version.enabled ? "enabled" : "disabled"}</td>
                        <td className="py-1.5 text-muted">
                          {new Date(version.created_at).toLocaleString()}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              ) : (
                <p className="text-[12px] text-muted">No versions recorded yet.</p>
              )}
            </div>
          ) : null}
        </section>
      </div>

      {notice ? (
        <p role="status" data-policies-notice className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}
    </div>
  );
}
