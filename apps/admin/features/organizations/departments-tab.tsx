"use client";

/**
 * The Departments tab of `/organizations/[id]` (REQ-005, slice 2).
 *
 * A membership answers *who is in this organization*; this tab answers *how they are arranged*
 * and *which roles a whole team carries*. The screen is a tree table — Department, Parent,
 * Members, Roles, Status, Updated — with the structure expressed by indentation rather than by
 * a separate expand control, because an operator's first question is "what hangs under what",
 * not "what is collapsed".
 *
 * Two rules the UI has to honour, both of which the API enforces as well:
 *
 * * **The key is the address.** It is shown next to the name and the edit dialog never offers
 *   it. A role binding stores the key, so changing it would silently re-scope every role bound
 *   to this department.
 * * **A binding is a grant to the people in it.** The drawer therefore shows both halves of
 *   that sentence: who is in the department, and which roles it carries.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  Archive,
  ChevronRight,
  FolderTree,
  Plus,
  RefreshCw,
  Search,
  Trash2,
  Users,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import {
  ApiError,
  addDepartmentMember,
  archiveOrganizationDepartment,
  bindDepartmentRole,
  createOrganizationDepartment,
  deleteOrganizationDepartment,
  fetchDepartment,
  fetchOrganizationDepartments,
  fetchOrganizationMembers,
  fetchRoles,
  removeDepartmentMember,
  unbindDepartmentRole,
  updateOrganizationDepartment,
  type DepartmentDetail,
  type IamRole,
  type OrganizationDepartment,
  type OrganizationMember,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { Organization } from "@/lib/types";

/** How the parent picker spells "no parent" without an ambiguous empty option. */
const ROOT_VALUE = "__root__";

/** The create / edit dialog. One form, two shapes. */
function DepartmentDialog({
  organizationId,
  departments,
  editing,
  onDone,
  onClose,
}: {
  organizationId: string;
  departments: OrganizationDepartment[];
  /** `null` creates; a row edits. */
  editing: OrganizationDepartment | null;
  onDone: () => void;
  onClose: () => void;
}) {
  const [name, setName] = useState(editing?.name ?? "");
  const [key, setKey] = useState("");
  const [description, setDescription] = useState(editing?.description ?? "");
  const [parentId, setParentId] = useState(
    editing ? (editing.parent_id ?? ROOT_VALUE) : ROOT_VALUE,
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [nameError, setNameError] = useState<string | null>(null);
  const [keyError, setKeyError] = useState<string | null>(null);

  // A department cannot be moved under itself, and the API refuses the deeper cycles too —
  // filtering the obvious one keeps the picker from offering a move that is known to fail.
  const candidates = departments.filter(
    (row) => row.id !== editing?.id && row.status === "active",
  );

  const submit = async () => {
    const trimmed = name.trim();
    if (!trimmed) {
      setNameError("Give the department a name — it is what people see.");
      return;
    }
    setNameError(null);

    if (!editing) {
      const proposed = key.trim().toLowerCase();
      if (!proposed) {
        setKeyError("A key is required: it is the address roles are bound to.");
        return;
      }
      if (!/^[a-z0-9]([a-z0-9-]{0,62}[a-z0-9])?$/.test(proposed)) {
        setKeyError("Lowercase letters, digits and dashes only — for example `marketing`.");
        return;
      }
    }
    setKeyError(null);
    setError(null);
    setBusy(true);

    const parent = parentId === ROOT_VALUE ? null : parentId;

    try {
      if (editing) {
        await updateOrganizationDepartment(organizationId, editing.id, {
          name: trimmed,
          description: description.trim(),
          parent_id: parent,
        });
      } else {
        await createOrganizationDepartment(organizationId, {
          key: key.trim(),
          name: trimmed,
          description: description.trim() || undefined,
          parent_id: parent,
        });
      }
      onDone();
    } catch (cause) {
      setError(
        cause instanceof ApiError ? cause.message : "The department could not be saved.",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center px-4">
      <button
        type="button"
        aria-label="Close the department dialog"
        onClick={onClose}
        className="absolute inset-0 bg-ink/40"
      />
      <div
        role="dialog"
        aria-label={editing ? "Edit department" : "Add a department"}
        className="relative flex max-h-[90vh] w-full max-w-md flex-col gap-3 overflow-y-auto rounded-xl border border-line bg-surface p-5 shadow-xl"
      >
        <div className="flex items-center justify-between">
          <h2 className="text-[14px] font-semibold">
            {editing ? "Edit department" : "Add a department"}
          </h2>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="rounded-lg p-1.5 text-muted hover:bg-quiet-soft hover:text-ink"
          >
            <X className="size-4" aria-hidden />
          </button>
        </div>

        <label className="flex flex-col gap-1.5 text-[12.5px] font-medium">
          Name
          <input
            value={name}
            data-department-name="1"
            onChange={(event) => setName(event.target.value)}
            placeholder="Marketing"
            autoFocus
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] font-normal outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
          {nameError ? (
            <span role="alert" className="text-[12px] font-normal text-accent-strong">
              {nameError}
            </span>
          ) : null}
        </label>

        {editing ? (
          <p className="rounded-lg border border-dashed border-line px-3 py-2 text-[12px] text-muted">
            <span className="font-medium text-ink">Key {editing.key}</span> stays as it is.
            Roles bound to this department address it by that key, so changing it would
            silently re-scope every one of them.
          </p>
        ) : (
          <label className="flex flex-col gap-1.5 text-[12.5px] font-medium">
            Key
            <input
              value={key}
              data-department-key="1"
              onChange={(event) => setKey(event.target.value)}
              placeholder="marketing"
              autoComplete="off"
              className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] font-normal outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
            {keyError ? (
              <span role="alert" className="text-[12px] font-normal text-accent-strong">
                {keyError}
              </span>
            ) : (
              <span className="text-[11.5px] font-normal text-muted">
                Lowercase letters, digits and dashes. This is the address roles are bound to,
                and it never changes once the department exists.
              </span>
            )}
          </label>
        )}

        <label className="flex flex-col gap-1.5 text-[12.5px] font-medium">
          Parent
          <select
            value={parentId}
            data-department-parent="1"
            onChange={(event) => setParentId(event.target.value)}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] font-normal outline-none"
          >
            <option value={ROOT_VALUE}>No parent — a top-level department</option>
            {candidates.map((row) => (
              <option key={row.id} value={row.id}>
                {row.name} ({row.key})
              </option>
            ))}
          </select>
          {candidates.length === 0 ? (
            <span className="text-[11.5px] font-normal text-muted">
              No other active department exists yet, so this one is a root.
            </span>
          ) : null}
        </label>

        <label className="flex flex-col gap-1.5 text-[12.5px] font-medium">
          Description
          <textarea
            value={description}
            onChange={(event) => setDescription(event.target.value)}
            rows={2}
            placeholder="Owns the public site and the campaigns behind it."
            className="resize-y rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] font-normal outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>

        {error ? (
          <p role="alert" className="text-[12.5px] text-accent-strong">
            {error}
          </p>
        ) : null}

        <div className="flex gap-2">
          <button
            type="button"
            onClick={() => void submit()}
            disabled={busy}
            data-department-submit="1"
            className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition disabled:opacity-60"
          >
            {busy ? "Saving…" : editing ? "Save changes" : "Create department"}
          </button>
          <button
            type="button"
            onClick={onClose}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * The department drawer: who is in it, and which roles it carries.
 *
 * The two halves are the same fact read from both sides — a role bound here reaches exactly
 * the people listed under "Members", and stops reaching them when they leave.
 */
function DepartmentDrawer({
  organizationId,
  department,
  members,
  onClose,
  onChanged,
}: {
  organizationId: string;
  department: OrganizationDepartment;
  members: OrganizationMember[];
  onClose: () => void;
  onChanged: () => void;
}) {
  const [detail, setDetail] = useState<DepartmentDetail | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [error, setError] = useState<string | null>(null);
  const [roles, setRoles] = useState<IamRole[]>([]);
  const [roleId, setRoleId] = useState("");
  const [picking, setPicking] = useState(false);
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setStatus("loading");
    setError(null);
    try {
      setDetail(await fetchDepartment(organizationId, department.id));
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      setError(
        cause instanceof ApiError ? cause.message : "The department could not be loaded.",
      );
    }
  }, [organizationId, department.id]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    let cancelled = false;
    fetchRoles(organizationId)
      .then((list) => {
        if (!cancelled) setRoles(list);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [organizationId]);

  // Only members of the organization can be added: a department splits the tenant's people, it
  // does not import anybody from outside it.
  const candidates = members.filter(
    (member) =>
      member.status === "active" &&
      !detail?.members.some((present) => present.user_id === member.user_id),
  );

  const addPerson = async (userId: string) => {
    setBusy(true);
    setActionError(null);
    try {
      await addDepartmentMember(organizationId, department.id, userId);
      setPicking(false);
      await load();
      onChanged();
    } catch (cause) {
      setActionError(
        cause instanceof ApiError ? cause.message : "The account could not be added.",
      );
    } finally {
      setBusy(false);
    }
  };

  const removePerson = async (userId: string) => {
    setBusy(true);
    setActionError(null);
    try {
      await removeDepartmentMember(organizationId, department.id, userId);
      await load();
      onChanged();
    } catch (cause) {
      setActionError(
        cause instanceof ApiError ? cause.message : "The account could not be removed.",
      );
    } finally {
      setBusy(false);
    }
  };

  const bindRole = async () => {
    if (!roleId) return;
    setBusy(true);
    setActionError(null);
    try {
      await bindDepartmentRole(organizationId, department.id, { role_id: roleId });
      setRoleId("");
      await load();
      onChanged();
    } catch (cause) {
      setActionError(
        cause instanceof ApiError ? cause.message : "The role could not be bound.",
      );
    } finally {
      setBusy(false);
    }
  };

  const unbindRole = async (bindingId: string) => {
    setBusy(true);
    setActionError(null);
    try {
      await unbindDepartmentRole(organizationId, department.id, bindingId);
      await load();
      onChanged();
    } catch (cause) {
      setActionError(
        cause instanceof ApiError ? cause.message : "The role could not be revoked.",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex justify-end">
      <button
        type="button"
        aria-label="Close the department drawer"
        onClick={onClose}
        className="absolute inset-0 bg-ink/40"
      />
      <aside
        role="dialog"
        aria-label={`Department ${department.name}`}
        className="relative flex h-full w-full max-w-lg flex-col gap-4 overflow-y-auto border-l border-line bg-surface p-5 shadow-xl"
      >
        <div className="flex items-start justify-between gap-3">
          <div className="flex flex-col gap-1">
            <h2 className="text-[14px] font-semibold">{department.name}</h2>
            <span className="text-[12px] text-muted">
              {department.key} · depth {department.depth}
            </span>
          </div>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="rounded-lg p-1.5 text-muted hover:bg-quiet-soft hover:text-ink"
          >
            <X className="size-4" aria-hidden />
          </button>
        </div>

        {department.description ? (
          <p className="rounded-lg border border-line px-3 py-2 text-[12.5px] text-muted">
            {department.description}
          </p>
        ) : null}

        {status === "loading" ? <LoadingTable columns={2} rows={2} /> : null}
        {status === "error" ? (
          <p role="alert" className="text-[12.5px] text-accent-strong">
            {error}
          </p>
        ) : null}

        {status === "ready" && detail ? (
          <>
            <section className="flex flex-col gap-2">
              <div className="flex items-center justify-between">
                <h3 className="text-[12.5px] font-semibold">Members</h3>
                <button
                  type="button"
                  onClick={() => setPicking((open) => !open)}
                  aria-expanded={picking}
                  data-department-add-member="1"
                  className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12px] transition hover:bg-canvas"
                >
                  <Plus className="size-3.5" aria-hidden />
                  Add
                </button>
              </div>

              {picking ? (
                candidates.length === 0 ? (
                  <p className="rounded-lg border border-dashed border-line px-3 py-2 text-[12px] text-muted">
                    Every active member of this organization is already in the department.
                  </p>
                ) : (
                  <select
                    value=""
                    data-department-member-picker="1"
                    onChange={(event) => {
                      if (event.target.value) void addPerson(event.target.value);
                    }}
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px]"
                    aria-label="Choose a member to add"
                  >
                    <option value="">Choose an account…</option>
                    {candidates.map((member) => (
                      <option key={member.user_id} value={member.user_id}>
                        {member.display_name} — {member.email}
                      </option>
                    ))}
                  </select>
                )
              ) : null}

              {detail.members.length === 0 ? (
                <p className="rounded-lg border border-dashed border-line px-3 py-3 text-[12px] text-muted">
                  Nobody is in this department yet. A role bound to it reaches nobody until
                  somebody is.
                </p>
              ) : (
                <ul className="flex flex-col gap-1">
                  {detail.members.map((member) => (
                    <li
                      key={member.user_id}
                      className="flex items-center justify-between gap-2 rounded-lg border border-line px-2.5 py-1.5"
                    >
                      <span className="flex min-w-0 flex-col">
                        <span className="truncate text-[12.5px]">{member.display_name}</span>
                        <span className="truncate text-[11.5px] text-muted">{member.email}</span>
                      </span>
                      <button
                        type="button"
                        onClick={() => void removePerson(member.user_id)}
                        disabled={busy}
                        className="shrink-0 rounded-lg p-1 text-muted hover:bg-quiet-soft hover:text-accent-strong disabled:opacity-50"
                        aria-label={`Remove ${member.display_name} from ${department.name}`}
                      >
                        <X className="size-3.5" aria-hidden />
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </section>

            <section className="flex flex-col gap-2">
              <h3 className="text-[12.5px] font-semibold">Roles bound to this department</h3>
              <p className="text-[11.5px] text-muted">
                Everybody above holds these while they are in the department. A binding on a
                parent department reaches this one too.
              </p>

              {detail.roles.length === 0 ? (
                <p className="rounded-lg border border-dashed border-line px-3 py-3 text-[12px] text-muted">
                  No role is bound here, so membership changes nothing about what its members
                  may do.
                </p>
              ) : (
                <ul className="flex flex-col gap-1">
                  {detail.roles.map((role) => (
                    <li
                      key={role.binding_id}
                      className="flex items-center justify-between gap-2 rounded-lg border border-line px-2.5 py-1.5"
                    >
                      <span className="flex min-w-0 flex-col">
                        <span className="truncate text-[12.5px]">{role.role_name}</span>
                        <span className="truncate text-[11.5px] text-muted">
                          {role.role_key}
                          {role.expires_at
                            ? ` · until ${formatTimestamp(role.expires_at)}`
                            : ""}
                        </span>
                      </span>
                      <button
                        type="button"
                        onClick={() => void unbindRole(role.binding_id)}
                        disabled={busy}
                        data-department-unbind="1"
                        className="shrink-0 rounded-lg p-1 text-muted hover:bg-quiet-soft hover:text-accent-strong disabled:opacity-50"
                        aria-label={`Revoke ${role.role_name} from ${department.name}`}
                      >
                        <X className="size-3.5" aria-hidden />
                      </button>
                    </li>
                  ))}
                </ul>
              )}

              <div className="flex gap-2">
                <select
                  value={roleId}
                  onChange={(event) => setRoleId(event.target.value)}
                  data-department-role-picker="1"
                  aria-label="Choose a role to bind"
                  className="min-w-0 flex-1 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px]"
                >
                  <option value="">Bind a role to the whole department…</option>
                  {roles
                    .filter((role) => !detail.roles.some((bound) => bound.role_id === role.id))
                    .map((role) => (
                      <option key={role.id} value={role.id}>
                        {role.name}
                      </option>
                    ))}
                </select>
                <button
                  type="button"
                  onClick={() => void bindRole()}
                  disabled={busy || !roleId}
                  data-department-bind="1"
                  className="shrink-0 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition disabled:opacity-50"
                >
                  Bind
                </button>
              </div>
            </section>
          </>
        ) : null}

        {actionError ? (
          <p role="alert" className="text-[12.5px] text-accent-strong">
            {actionError}
          </p>
        ) : null}
      </aside>
    </div>
  );
}

/** The Departments tab. */
export function DepartmentsTab({ organization }: { organization: Organization }) {
  const [departments, setDepartments] = useState<OrganizationDepartment[]>([]);
  const [members, setMembers] = useState<OrganizationMember[]>([]);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [statusFilter, setStatusFilter] = useState("");
  const [dialog, setDialog] = useState<"open" | OrganizationDepartment | null>(null);
  const [open, setOpen] = useState<OrganizationDepartment | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    setStatus("loading");
    setError(null);
    try {
      const [tree, memberList] = await Promise.all([
        fetchOrganizationDepartments(organization.id),
        fetchOrganizationMembers(organization.id),
      ]);
      setDepartments(tree.departments);
      setMembers(memberList.members);
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      setError(
        cause instanceof ApiError ? cause.message : "The departments could not be loaded.",
      );
    }
  }, [organization.id]);

  useEffect(() => {
    void load();
  }, [load]);

  // `/` focuses the search, the way every other list on the panel does.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "/" || dialog || open) return;
      const target = event.target as HTMLElement | null;
      if (target && ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName)) return;
      event.preventDefault();
      document.querySelector<HTMLInputElement>('[data-department-search="1"]')?.focus();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [dialog, open]);

  // Filtering happens here rather than in the query string because `depth` is computed
  // server-side over the *unfiltered* tree: a parent that matches keeps its indentation, so
  // the rows never look like roots.
  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return departments.filter((row) => {
      if (statusFilter && row.status !== statusFilter) return false;
      if (!needle) return true;
      return (
        row.name.toLowerCase().includes(needle) ||
        row.key.includes(needle) ||
        row.description.toLowerCase().includes(needle)
      );
    });
  }, [departments, query, statusFilter]);

  const run = async (id: string, action: () => Promise<unknown>, message: string) => {
    setBusyId(id);
    setNotice(null);
    try {
      await action();
      setNotice(message);
      await load();
    } catch (cause) {
      setNotice(
        cause instanceof ApiError ? cause.message : "The change could not be applied.",
      );
    } finally {
      setBusyId(null);
    }
  };

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          onClick={() => setDialog("open")}
          data-department-add="1"
          className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:opacity-90"
        >
          <Plus className="size-3.5" aria-hidden />
          Add department
        </button>

        <label className="relative min-w-48 flex-1">
          <span className="sr-only">Search departments</span>
          <Search
            className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted"
            aria-hidden
          />
          <input
            value={query}
            data-department-search="1"
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Search name, key or description  ( / )"
            className="w-full rounded-lg border border-line bg-canvas py-1.5 pl-8 pr-2.5 text-[12.5px] outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>

        <select
          value={statusFilter}
          onChange={(event) => setStatusFilter(event.target.value)}
          data-department-status="1"
          aria-label="Filter by status"
          className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px]"
        >
          <option value="">All statuses</option>
          <option value="active">Active</option>
          <option value="archived">Archived</option>
        </select>

        <button
          type="button"
          onClick={() => void load()}
          aria-label="Refresh departments"
          className="rounded-lg border border-line p-1.5 text-muted transition hover:bg-canvas hover:text-ink"
        >
          <RefreshCw className="size-3.5" aria-hidden />
        </button>
      </div>

      {notice ? (
        <p role="status" className="rounded-lg border border-line px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}

      {status === "error" ? (
        <div className="flex flex-col items-center gap-3 rounded-xl border border-line px-6 py-8 text-center">
          <p className="text-[12.5px] text-accent-strong">{error}</p>
          <button
            type="button"
            onClick={() => void load()}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Try again
          </button>
        </div>
      ) : null}

      {status === "loading" ? <LoadingTable columns={7} /> : null}

      {status === "ready" && departments.length === 0 ? (
        <EmptyState
          title="No departments yet"
          hint="A department splits this organization into teams, and it is what a role can be bound to. Create the first one to get started."
          action={
            <button
              type="button"
              onClick={() => setDialog("open")}
              className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
            >
              Add department
            </button>
          }
        />
      ) : null}

      {status === "ready" && departments.length > 0 && visible.length === 0 ? (
        <EmptyState
          title="No department matches"
          hint="Nothing here carries that name, key or status. Clear the filters to see the whole tree."
          action={
            <button
              type="button"
              onClick={() => {
                setQuery("");
                setStatusFilter("");
              }}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px]"
            >
              Clear filters
            </button>
          }
        />
      ) : null}

      {status === "ready" && visible.length > 0 ? (
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                <th scope="col" className="px-3 py-2 font-medium">
                  Department
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  Parent
                </th>
                <th scope="col" className="px-3 py-2 text-right font-medium">
                  Members
                </th>
                <th scope="col" className="px-3 py-2 text-right font-medium">
                  Roles
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  Status
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  Updated
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {visible.map((row) => (
                <tr
                  key={row.id}
                  data-department-row={row.key}
                  className="border-b border-line last:border-b-0"
                >
                  <td className="px-3 py-2.5">
                    <button
                      type="button"
                      onClick={() => setOpen(row)}
                      data-department-open={row.key}
                      className="flex items-center gap-1.5 text-left"
                      style={{ paddingLeft: `${row.depth * 14}px` }}
                    >
                      {row.depth > 0 ? (
                        <ChevronRight className="size-3 shrink-0 text-muted" aria-hidden />
                      ) : (
                        <FolderTree className="size-3.5 shrink-0 text-muted" aria-hidden />
                      )}
                      <span className="flex min-w-0 flex-col">
                        <span className="truncate font-medium">{row.name}</span>
                        <span className="truncate text-[11.5px] text-muted">
                          {row.key}
                          {row.description ? ` · ${row.description}` : ""}
                        </span>
                      </span>
                    </button>
                  </td>
                  <td className="px-3 py-2.5 text-[12.5px] text-muted">
                    {row.parent_key ?? "—"}
                  </td>
                  <td className="px-3 py-2.5 text-right">
                    <span className="inline-flex items-center gap-1 text-[12.5px]">
                      <Users className="size-3.5 text-muted" aria-hidden />
                      {row.member_count}
                    </span>
                  </td>
                  <td className="px-3 py-2.5 text-right text-[12.5px]">{row.role_count}</td>
                  <td className="px-3 py-2.5">
                    <StatusBadge status={row.status} />
                  </td>
                  <td className="px-3 py-2.5 text-[12.5px] text-muted">
                    {formatTimestamp(row.updated_at)}
                  </td>
                  <td className="px-3 py-2.5">
                    <span className="flex items-center justify-end gap-1">
                      <button
                        type="button"
                        onClick={() => setDialog(row)}
                        aria-label={`Edit ${row.name}`}
                        className="rounded-lg border border-line px-2 py-1 text-[12px] transition hover:bg-canvas"
                      >
                        Edit
                      </button>
                      {row.status === "active" ? (
                        <button
                          type="button"
                          onClick={() =>
                            void run(
                              row.id,
                              () =>
                                archiveOrganizationDepartment(organization.id, row.id),
                              `${row.name} is archived. It keeps its structure but no longer grants its roles.`,
                            )
                          }
                          disabled={busyId === row.id}
                          data-department-archive={row.key}
                          aria-label={`Archive ${row.name}`}
                          className="rounded-lg border border-line p-1.5 text-muted transition hover:bg-canvas hover:text-ink disabled:opacity-50"
                        >
                          <Archive className="size-3.5" aria-hidden />
                        </button>
                      ) : null}
                      <button
                        type="button"
                        onClick={() =>
                          void run(
                            row.id,
                            () => deleteOrganizationDepartment(organization.id, row.id),
                            `${row.name} is deleted.`,
                          )
                        }
                        disabled={busyId === row.id}
                        data-department-delete={row.key}
                        aria-label={`Delete ${row.name}`}
                        className="rounded-lg border border-line p-1.5 text-muted transition hover:bg-canvas hover:text-accent-strong disabled:opacity-50"
                      >
                        <Trash2 className="size-3.5" aria-hidden />
                      </button>
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : null}

      {dialog ? (
        <DepartmentDialog
          organizationId={organization.id}
          departments={departments}
          editing={dialog === "open" ? null : dialog}
          onDone={() => {
            setDialog(null);
            void load();
          }}
          onClose={() => setDialog(null)}
        />
      ) : null}

      {open ? (
        <DepartmentDrawer
          organizationId={organization.id}
          department={open}
          members={members}
          onClose={() => setOpen(null)}
          onChanged={() => void load()}
        />
      ) : null}
    </div>
  );
}
