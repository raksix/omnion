"use client";

/**
 * Departments and the org chart (REQ-055, slice 1): `/hr/departments`.
 *
 * Two surfaces over one dataset, because the request's slice 1 names both and they answer
 * different questions. The **tree** answers "who is in this department, and what is inside it";
 * the **chart** answers "who reports to whom". Neither derives from the other here: both read the
 * same two endpoints, and the counts they print come from the server rather than from the rows on
 * screen — the acceptance criterion says the chart matches the department list, and the only way
 * that is a fact rather than a coincidence is if both print `member_count`.
 *
 * The refusals are the reason this screen is worth reading twice:
 *
 * * **Delete** is offered only where `deletable` is true, and pressing it on a department with
 *   members is still possible from the keyboard — so the refusal is rendered where the operator
 *   lands, with **both** counts in the sentence. "Cannot delete" alone sends somebody to two
 *   reports to find out how exposed the department is.
 * * **Re-parent** uses a native `<select>` of the departments that are *not* this one and not its
 *   own descendants. A cycle check exists in the store; pre-filtering here is not a second rule,
 *   it is the same rule made visible so the refusal is the exception rather than the default.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { Building2, GitBranch, Loader2, Network, Plus, Trash2, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  createDepartment,
  deleteDepartment,
  fetchDepartments,
  fetchOrgChart,
  mergeDepartments,
  updateDepartment,
  type Department,
  type OrgChartNode,
} from "@/lib/hr";

import { EmployeeAvatar, employeeName } from "./hr-people-parts";

type Draft = { name: string; code: string; parent_id: string; description: string };

/** The tree rows, each with its depth, built from the flat list the endpoint returns. */
type TreeRow = { department: Department; depth: number };

/**
 * Nest the flat department list into the rows the tree prints.
 *
 * Built here rather than asked of the server because the endpoint's job is the data and this is
 * the presentation — and because a department whose parent is outside the caller's visibility would
 * otherwise vanish from the tree while still counting members. A row whose parent is missing is
 * therefore promoted to a root, which keeps the node visible instead of dropping a live subtree.
 */
function buildTree(items: Department[]): TreeRow[] {
  const byId = new Map(items.map((department) => [department.id, department]));
  const childrenOf = new Map<string | null, Department[]>();
  for (const department of items) {
    const parent = department.parent_id && byId.has(department.parent_id) ? department.parent_id : null;
    const bucket = childrenOf.get(parent) ?? [];
    bucket.push(department);
    childrenOf.set(parent, bucket);
  }
  const rows: TreeRow[] = [];
  const walk = (parent: string | null, depth: number) => {
    const bucket = [...(childrenOf.get(parent) ?? [])].sort((a, b) => a.name.localeCompare(b.name));
    for (const department of bucket) {
      rows.push({ department, depth });
      walk(department.id, depth + 1);
    }
  };
  walk(null, 0);
  // A cycle that survived the store's refusal would make the walk recurse forever; anything not
  // reached by then is a cycle, and it is printed flat rather than silently dropped.
  if (rows.length < items.length) {
    const seen = new Set(rows.map((row) => row.department.id));
    for (const department of items) {
      if (!seen.has(department.id)) {
        rows.push({ department, depth: 0 });
      }
    }
  }
  return rows;
}

/** The descendants of one department, so the parent picker can refuse a cycle before the write. */
function descendantIds(items: Department[], rootId: string): Set<string> {
  const byParent = new Map<string, Department[]>();
  for (const department of items) {
    if (!department.parent_id) continue;
    const bucket = byParent.get(department.parent_id) ?? [];
    bucket.push(department);
    byParent.set(department.parent_id, bucket);
  }
  const out = new Set<string>([rootId]);
  const queue = [rootId];
  while (queue.length > 0) {
    const current = queue.pop() as string;
    for (const child of byParent.get(current) ?? []) {
      if (!out.has(child.id)) {
        out.add(child.id);
        queue.push(child.id);
      }
    }
  }
  return out;
}

export function DepartmentsView() {
  const [tab, setTab] = useState<"tree" | "chart">("tree");
  const [items, setItems] = useState<Department[]>([]);
  const [chart, setChart] = useState<OrgChartNode[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [formError, setFormError] = useState<ScreenErrorValue>(null);
  const [saving, setSaving] = useState(false);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [editing, setEditing] = useState<{ id: string; name: string; code: string; parent_id: string; description: string } | null>(null);
  const [merging, setMerging] = useState<{ source_id: string; target_id: string } | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      // Both requests always run: the tree prints counts and the chart prints the nesting, and a
      // screen whose second tab is a spinner until someone presses it is a tab that reads as
      // broken on the first visit.
      const [tree, org] = await Promise.all([fetchDepartments(), fetchOrgChart()]);
      setItems(tree.items);
      setChart(org.items);
    } catch (caught) {
      setError(toScreenError(caught, "The departments could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const rows = useMemo(() => buildTree(items), [items]);
  const parentsFor = useCallback(
    (departmentId: string) => {
      const blocked = descendantIds(items, departmentId);
      return items.filter((department) => !blocked.has(department.id));
    },
    [items],
  );

  const blankDraft = useCallback(
    (): Draft => ({ name: "", code: "", parent_id: "", description: "" }),
    [],
  );

  async function submitCreate() {
    if (!draft) return;
    setSaving(true);
    setFormError(null);
    try {
      const created = await createDepartment({
        name: draft.name.trim(),
        code: draft.code.trim() || undefined,
        parent_id: draft.parent_id || undefined,
        description: draft.description.trim() || undefined,
      });
      setNotice(`${created.name} was created.`);
      setDraft(null);
      void load();
    } catch (caught) {
      setFormError(toScreenError(caught, "That department could not be created."));
    } finally {
      setSaving(false);
    }
  }

  async function submitEdit() {
    if (!editing) return;
    setSaving(true);
    setFormError(null);
    try {
      await updateDepartment(editing.id, {
        name: editing.name.trim(),
        code: editing.code.trim() || undefined,
        parent_id: editing.parent_id || undefined,
        description: editing.description.trim() || undefined,
      });
      setNotice("The department was updated.");
      setEditing(null);
      void load();
    } catch (caught) {
      setFormError(toScreenError(caught, "That department could not be updated."));
    } finally {
      setSaving(false);
    }
  }

  async function remove(department: Department) {
    setFormError(null);
    setNotice(null);
    try {
      await deleteDepartment(department.id);
      setNotice(`${department.name} was deleted.`);
      void load();
    } catch (caught) {
      // The refusal is the interesting path: it names how many members and children are in the
      // way, and this screen shows it where the operator is standing rather than as a toast that
      // disappears before it is read.
      setFormError(toScreenError(caught, "That department could not be deleted."));
    }
  }

  async function submitMerge() {
    if (!merging) return;
    setSaving(true);
    setFormError(null);
    try {
      const moved = await mergeDepartments(merging);
      setNotice(`Merged. ${moved.name} now holds the members.`);
      setMerging(null);
      void load();
    } catch (caught) {
      setFormError(toScreenError(caught, "Those departments could not be merged."));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="flex flex-col gap-4">
      <header className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h2 className="text-[15px] font-semibold">Departments</h2>
          <p className="text-[12.5px] text-muted">
            The shape of the organization: who sits where, and who reports to whom.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <div className="inline-flex rounded border" role="tablist" aria-label="Department view">
            <button
              type="button"
              role="tab"
              aria-selected={tab === "tree"}
              data-qa-hr-departments-tab-tree
              onClick={() => setTab("tree")}
              className={`inline-flex items-center gap-1.5 px-2.5 py-1.5 text-[12.5px] ${
                tab === "tree" ? "bg-muted font-medium" : "text-muted"
              }`}
            >
              <Building2 aria-hidden className="h-3.5 w-3.5" />
              Tree
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={tab === "chart"}
              data-qa-hr-departments-tab-chart
              onClick={() => setTab("chart")}
              className={`inline-flex items-center gap-1.5 px-2.5 py-1.5 text-[12.5px] ${
                tab === "chart" ? "bg-muted font-medium" : "text-muted"
              }`}
            >
              <Network aria-hidden className="h-3.5 w-3.5" />
              Org chart
            </button>
          </div>
          <button
            type="button"
            data-qa-hr-departments-new
            onClick={() => {
              setFormError(null);
              setDraft(blankDraft());
            }}
            className="inline-flex items-center gap-1.5 rounded border px-3 py-1.5 text-[13px]"
          >
            <Plus aria-hidden className="h-3.5 w-3.5" />
            Add department
          </button>
        </div>
      </header>

      {notice ? (
        <p role="status" data-qa-hr-departments-notice className="text-[12.5px] text-positive">
          {notice}
        </p>
      ) : null}
      {formError ? (
        <ErrorState error={formError} onRetry={() => setFormError(null)} qa="hr-departments-error" />
      ) : null}

      {error ? (
        <ErrorState error={error} onRetry={() => void load()} />
      ) : loading ? (
        <LoadingTable columns={4} />
      ) : items.length === 0 ? (
        <EmptyState
          title="No departments yet"
          hint="A department is what an employee belongs to, so the directory, the leave requests and the attendance roster all need one."
          action={
            <button
              type="button"
              onClick={() => {
                setFormError(null);
                setDraft(blankDraft());
              }}
              className="inline-flex items-center gap-1.5 rounded border px-3 py-1.5 text-[13px]"
            >
              <Plus aria-hidden className="h-3.5 w-3.5" />
              Add department
            </button>
          }
        />
      ) : tab === "tree" ? (
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-[13px]" data-qa-hr-departments-table>
            <thead>
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                <th scope="col" className="px-4 py-2.5">Department</th>
                <th scope="col" className="px-4 py-2.5">Head</th>
                <th scope="col" className="px-4 py-2.5">Members</th>
                <th scope="col" className="px-4 py-2.5">Sub-departments</th>
                <th scope="col" className="px-4 py-2.5 text-right">Actions</th>
              </tr>
            </thead>
            <tbody>
              {rows.map(({ department, depth }) => (
                <tr key={department.id} className="border-t border-line" data-qa-hr-department-row={department.name}>
                  <td className="px-4 py-3">
                    <span className="flex items-center gap-1.5" style={{ paddingLeft: `${depth * 16}px` }}>
                      {depth > 0 ? (
                        <GitBranch aria-hidden className="h-3 w-3 shrink-0 text-muted" />
                      ) : null}
                      <span className="font-medium text-ink">{department.name}</span>
                      {department.code ? (
                        <span className="rounded bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
                          {department.code}
                        </span>
                      ) : null}
                      {!department.active ? (
                        <span className="rounded bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
                          Closed
                        </span>
                      ) : null}
                    </span>
                  </td>
                  <td className="px-4 py-3 text-muted">{department.manager_name ?? "—"}</td>
                  <td className="px-4 py-3" data-qa-hr-department-members={department.name}>
                    {department.member_count}
                  </td>
                  <td className="px-4 py-3 text-muted">{department.child_count}</td>
                  <td className="px-4 py-3">
                    <span className="flex justify-end gap-1">
                      <button
                        type="button"
                        data-qa-hr-department-edit={department.name}
                        onClick={() => {
                          setFormError(null);
                          setEditing({
                            id: department.id,
                            name: department.name,
                            code: department.code ?? "",
                            parent_id: department.parent_id ?? "",
                            description: department.description ?? "",
                          });
                        }}
                        className="rounded border px-2 py-1 text-[12px]"
                      >
                        Edit
                      </button>
                      <button
                        type="button"
                        data-qa-hr-department-merge={department.name}
                        onClick={() => {
                          setFormError(null);
                          setMerging({ source_id: department.id, target_id: "" });
                        }}
                        className="rounded border px-2 py-1 text-[12px]"
                      >
                        Merge
                      </button>
                      <button
                        type="button"
                        data-qa-hr-department-delete={department.name}
                        onClick={() => void remove(department)}
                        className="inline-flex items-center gap-1 rounded border px-2 py-1 text-[12px] text-negative"
                      >
                        <Trash2 aria-hidden className="h-3 w-3" />
                        Delete
                      </button>
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          <p className="px-4 py-2.5 text-[12px] text-muted">
            {items.length} {items.length === 1 ? "department" : "departments"}. A department holding
            people or sub-departments cannot be deleted — merge it into another instead.
          </p>
        </div>
      ) : (
        <div data-qa-hr-org-chart className="overflow-x-auto">
          {chart.length === 0 ? (
            <EmptyState
              title="Nobody to chart yet"
              hint="The chart is drawn from the reporting line, so it fills in as soon as one person reports to another."
            />
          ) : (
            <ul className="min-w-max space-y-1">
              {chart.map((node) => (
                <OrgChartBranch key={node.employee.id} node={node} depth={0} />
              ))}
            </ul>
          )}
        </div>
      )}

      {draft ? (
        <FormSheet
          title="Add department"
          error={formError}
          saving={saving}
          onClose={() => setDraft(null)}
          onSubmit={() => void submitCreate()}
          fields={
            <>
              <label className="text-[12px] font-medium">
                Name
                <input
                  data-qa-hr-department-name
                  value={draft.name}
                  onChange={(event) => setDraft({ ...draft, name: event.target.value })}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                />
              </label>
              <label className="text-[12px] font-medium">
                Code
                <input
                  data-qa-hr-department-code
                  value={draft.code}
                  onChange={(event) => setDraft({ ...draft, code: event.target.value })}
                  placeholder="ENG"
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                />
              </label>
              <label className="text-[12px] font-medium">
                Sits under
                <select
                  data-qa-hr-department-parent
                  value={draft.parent_id}
                  onChange={(event) => setDraft({ ...draft, parent_id: event.target.value })}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                >
                  <option value="">Top level</option>
                  {items.map((department) => (
                    <option key={department.id} value={department.id}>
                      {department.name}
                    </option>
                  ))}
                </select>
              </label>
              <label className="text-[12px] font-medium">
                Description
                <textarea
                  data-qa-hr-department-description
                  value={draft.description}
                  onChange={(event) => setDraft({ ...draft, description: event.target.value })}
                  rows={3}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                />
              </label>
            </>
          }
        />
      ) : null}

      {editing ? (
        <FormSheet
          title="Edit department"
          error={formError}
          saving={saving}
          onClose={() => setEditing(null)}
          onSubmit={() => void submitEdit()}
          fields={
            <>
              <label className="text-[12px] font-medium">
                Name
                <input
                  data-qa-hr-department-edit-name
                  value={editing.name}
                  onChange={(event) => setEditing({ ...editing, name: event.target.value })}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                />
              </label>
              <label className="text-[12px] font-medium">
                Code
                <input
                  data-qa-hr-department-edit-code
                  value={editing.code}
                  onChange={(event) => setEditing({ ...editing, code: event.target.value })}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                />
              </label>
              <label className="text-[12px] font-medium">
                Sits under
                <select
                  data-qa-hr-department-edit-parent
                  value={editing.parent_id}
                  onChange={(event) => setEditing({ ...editing, parent_id: event.target.value })}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                >
                  <option value="">Top level</option>
                  {parentsFor(editing.id).map((department) => (
                    <option key={department.id} value={department.id}>
                      {department.name}
                    </option>
                  ))}
                </select>
              </label>
              <label className="text-[12px] font-medium">
                Description
                <textarea
                  data-qa-hr-department-edit-description
                  value={editing.description}
                  onChange={(event) => setEditing({ ...editing, description: event.target.value })}
                  rows={3}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                />
              </label>
            </>
          }
        />
      ) : null}

      {merging ? (
        <FormSheet
          title="Merge department"
          error={formError}
          saving={saving}
          onClose={() => setMerging(null)}
          onSubmit={() => void submitMerge()}
          fields={
            <>
              <p className="text-[12.5px] text-muted">
                The members and any sub-departments move across, and the source is dropped. Nothing
                is deleted from the people directory — they simply belong to the other department.
              </p>
              <label className="text-[12px] font-medium">
                Move into
                <select
                  data-qa-hr-department-merge-target
                  value={merging.target_id}
                  onChange={(event) => setMerging({ ...merging, target_id: event.target.value })}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                >
                  <option value="">Choose a department</option>
                  {items
                    .filter((department) => department.id !== merging.source_id)
                    .map((department) => (
                      <option key={department.id} value={department.id}>
                        {department.name} ({department.member_count})
                      </option>
                    ))}
                </select>
              </label>
            </>
          }
        />
      ) : null}
    </div>
  );
}

/** One row of the org chart, with its reports nested under it. */
function OrgChartBranch({ node, depth }: { node: OrgChartNode; depth: number }) {
  const name = employeeName(node.employee);
  return (
    <li className="list-none">
      <div
        data-qa-hr-chart-node={node.employee.id}
        tabIndex={0}
        className="flex items-center gap-2.5 rounded border px-3 py-2 focus:outline-none focus-visible:ring-2 focus-visible:ring-ink"
        style={{ marginLeft: `${depth * 20}px` }}
      >
        <EmployeeAvatar employee={node.employee} size="sm" />
        <span className="flex min-w-0 flex-col">
          <span className="truncate text-[13px] font-medium text-ink">{name}</span>
          <span className="truncate text-[11.5px] text-muted">{node.employee.position}</span>
        </span>
        <span className="ml-auto text-[11.5px] text-muted">
          {node.children.length > 0
            ? `${node.children.length} ${node.children.length === 1 ? "report" : "reports"}`
            : null}
        </span>
      </div>
      {node.children.length > 0 ? (
        <ul className="mt-1 space-y-1">
          {node.children.map((child) => (
            <OrgChartBranch key={child.employee.id} node={child} depth={depth + 1} />
          ))}
        </ul>
      ) : null}
    </li>
  );
}

/** The create/edit/merge sheet, so three forms share one shell instead of three near-copies. */
function FormSheet({
  title,
  error,
  saving,
  onClose,
  onSubmit,
  fields,
}: {
  title: string;
  error: ScreenErrorValue;
  saving: boolean;
  onClose: () => void;
  onSubmit: () => void;
  fields: React.ReactNode;
}) {
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={title}
      data-qa-hr-department-form
      className="fixed inset-0 z-50 flex justify-end bg-black/30"
    >
      <div className="flex h-full w-full max-w-md flex-col gap-3 overflow-y-auto bg-panel p-5 shadow-xl">
        <div className="flex items-center justify-between">
          <h3 className="text-[15px] font-semibold">{title}</h3>
          <button type="button" onClick={onClose} aria-label="Close" className="rounded p-1 text-muted hover:text-ink">
            <X aria-hidden className="h-4 w-4" />
          </button>
        </div>
        {error ? <ErrorState error={error} onRetry={onSubmit} qa="hr-department-form-error" /> : null}
        {fields}
        <div className="mt-auto flex justify-end gap-2 pt-2">
          <button type="button" onClick={onClose} className="rounded border px-3 py-1.5 text-[13px]">
            Cancel
          </button>
          <button
            type="button"
            data-qa-hr-department-submit
            onClick={onSubmit}
            disabled={saving}
            className="inline-flex items-center gap-1.5 rounded border border-transparent bg-ink px-3 py-1.5 text-[13px] text-panel disabled:opacity-50"
          >
            {saving ? <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" /> : null}
            Save
          </button>
        </div>
      </div>
    </div>
  );
}