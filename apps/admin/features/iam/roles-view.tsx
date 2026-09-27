"use client";

/**
 * `/settings/iam/roles` — the role list (REQ-006, slice 1).
 *
 * Platform roles arrive with the installation; custom roles belong to the organization. The
 * screen reads both from the API — counts included — and offers the whole lifecycle: create,
 * open, duplicate (the way an organization customises a base role) and delete (refused while
 * the role still carries bindings, with the API's own message shown).
 */
import { useCallback, useEffect, useState } from "react";

import { Copy, Plus, RefreshCw, ShieldAlert, ShieldCheck, Trash2 } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";

import { useSession } from "@/lib/session";
import {
  ApiError,
  createIamRole,
  deleteIamRole,
  duplicateIamRole,
  fetchOrganizations,
  fetchRoles,
  type IamRole,
} from "@/lib/api";
import type { Organization } from "@/lib/types";

/** A badge for the role's origin. */
function OriginBadge({ role }: { role: IamRole }) {
  return role.is_system ? (
    <span className="rounded-md border border-line bg-canvas px-1.5 py-0.5 text-[11px] font-medium text-muted">
      Platform
    </span>
  ) : (
    <span className="rounded-md border border-accent/40 bg-accent-soft px-1.5 py-0.5 text-[11px] font-medium text-accent-strong">
      Custom
    </span>
  );
}

/** The create form of the screen. */
function CreateRoleForm({
  roles,
  organizations,
  defaultOrganizationId,
  onDone,
  onCancel,
}: {
  roles: IamRole[];
  /** Organizations to pick from — a platform account has no primary one of its own. */
  organizations: Organization[] | null;
  /** Tenant the list is looking at, so the form starts where the reader is. */
  defaultOrganizationId?: string | null;
  onDone: (role: IamRole) => void;
  onCancel: () => void;
}) {
  const [key, setKey] = useState("");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [priority, setPriority] = useState("400");
  const [parentId, setParentId] = useState("");
  const [organizationId, setOrganizationId] = useState(
    defaultOrganizationId ?? organizations?.[0]?.id ?? "",
  );
  const needsOrganization = organizations !== null;

  // The list may arrive after the form opened: adopt the first tenant rather than leaving the
  // picker empty (an empty picker would disable the submit for no visible reason).
  useEffect(() => {
    if (!organizationId && organizations && organizations.length > 0) {
      setOrganizationId(organizations[0].id);
    }
  }, [organizations, organizationId]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    setError(null);
    setBusy(true);
    try {
      const created = await createIamRole({
        key,
        name,
        description,
        priority: Number(priority),
        inheritsRoleId: parentId || null,
        organizationId: organizations ? organizationId : null,
      });
      onDone(created);
    } catch (cause) {
      setError(cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The role could not be created.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <form
      data-role-create-form
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
      className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
    >
      <h2 className="text-[13.5px] font-semibold">New role</h2>
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1.5">
          <span className="text-[12.5px] font-medium text-ink">Key</span>
          <input
            value={key}
            data-role-new-key
            onChange={(event) => setKey(event.target.value)}
            placeholder="marketing-manager"
            required
            className="h-9 rounded-lg border border-line bg-surface px-2.5 font-mono text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
          <span className="text-[11.5px] text-muted">Lowercase slug; it never changes.</span>
        </label>
        <label className="flex flex-col gap-1.5">
          <span className="text-[12.5px] font-medium text-ink">Name</span>
          <input
            value={name}
            data-role-new-name
            onChange={(event) => setName(event.target.value)}
            placeholder="Marketing Manager"
            required
            className="h-9 rounded-lg border border-line bg-surface px-2.5 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
        <label className="flex flex-col gap-1.5 sm:col-span-2">
          <span className="text-[12.5px] font-medium text-ink">Description</span>
          <input
            value={description}
            data-role-new-description
            onChange={(event) => setDescription(event.target.value)}
            placeholder="What this role is for"
            className="h-9 rounded-lg border border-line bg-surface px-2.5 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
        <label className="flex flex-col gap-1.5">
          <span className="text-[12.5px] font-medium text-ink">Priority</span>
          <input
            type="number"
            min={0}
            max={1000}
            value={priority}
            data-role-new-priority
            onChange={(event) => setPriority(event.target.value)}
            className="h-9 rounded-lg border border-line bg-surface px-2.5 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
          <span className="text-[11.5px] text-muted">0–1000; higher outranks lower.</span>
        </label>
        {organizations && organizations.length > 0 ? (
          <label className="flex flex-col gap-1.5">
            <span className="text-[12.5px] font-medium text-ink">Organization</span>
            <select
              value={organizationId}
              data-role-new-organization
              onChange={(event) => setOrganizationId(event.target.value)}
              className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            >
              {organizations.map((organization) => (
                <option key={organization.id} value={organization.id}>
                  {organization.name}
                </option>
              ))}
            </select>
            <span className="text-[11.5px] text-muted">
              This account works platform-wide, so the role needs a tenant.
            </span>
          </label>
        ) : null}
        <label className="flex flex-col gap-1.5">
          <span className="text-[12.5px] font-medium text-ink">Inherits from</span>
          <select
            value={parentId}
            data-role-new-parent
            onChange={(event) => setParentId(event.target.value)}
            className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          >
            <option value="">Nothing — starts empty</option>
            {roles.map((role) => (
              <option key={role.id} value={role.id}>
                {role.name} ({role.key})
              </option>
            ))}
          </select>
        </label>
      </div>
      {needsOrganization && organizations !== null && organizations.length === 0 ? (
        <p className="rounded-lg border border-caution/40 bg-caution-soft px-3 py-2 text-[12.5px] text-caution">
          No organization exists yet — a custom role belongs to one. The setup flow creates the first.
        </p>
      ) : null}
      {error ? (
        <p role="alert" data-role-create-error className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution">
          {error}
        </p>
      ) : null}
      <div className="flex items-center gap-2">
        <button
          type="submit"
          disabled={busy || (organizations !== null && organizations.length > 0 && !organizationId)}
          data-role-create-submit
          data-qa-guard="iam-role-create"
          className="flex items-center gap-1.5 rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
        >
          <Plus className="size-3.5" aria-hidden />
          Create role
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
        >
          Cancel
        </button>
      </div>
    </form>
  );
}

/** `/settings/iam/roles`. */
export function RolesView() {
  const router = useRouter();
  const { user } = useSession();
  const [organizations, setOrganizations] = useState<Organization[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [roles, setRoles] = useState<IamRole[]>([]);
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [creating, setCreating] = useState(false);
  const [duplicating, setDuplicating] = useState<string | null>(null);
  const [duplicateKey, setDuplicateKey] = useState("");
  const [duplicateName, setDuplicateName] = useState("");
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const platformAccount = user ? user.organization_id === null : false;

  const load = useCallback(async (organizationId: string | null) => {
    setStatus("loading");
    setLoadError(null);
    try {
      setRoles(await fetchRoles(organizationId));
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The roles could not be read." },
      );
    }
  }, []);

  // A platform account (no primary organization) reads one tenant at a time: load the tenants,
  // then the roles of the selected one. A tenant account answers for its own organization.
  useEffect(() => {
    if (!user) return;
    if (user.organization_id !== null) {
      void load(null);
      return;
    }
    if (organizations !== null) return;
    void fetchOrganizations()
      .then((list) => {
        setOrganizations(list);
        setSelectedOrg(list[0]?.id ?? null);
      })
      .catch(() => setOrganizations([]));
  }, [user, organizations, load]);

  useEffect(() => {
    if (platformAccount && selectedOrg) {
      void load(selectedOrg);
    }
  }, [platformAccount, selectedOrg, load]);

  const remove = async (role: IamRole) => {
    setBusy(role.id);
    setError(null);
    try {
      await deleteIamRole(role.id);
      setRoles((current) => current.filter((entry) => entry.id !== role.id));
      setNotice(`“${role.name}” was deleted.`);
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? `The role was not deleted: ${cause.message} (${cause.code})`
          : "The role was not deleted.",
      );
    } finally {
      setBusy(null);
      setConfirmDelete(null);
    }
  };

  const duplicate = async (role: IamRole) => {
    setBusy(role.id);
    setError(null);
    try {
      const source = roles.find((entry) => entry.id === role.id);
      const copy = await duplicateIamRole(role.id, {
        key: duplicateKey,
        name: duplicateName,
        organizationId:
          user && user.organization_id === null
            ? (source?.organization_id ?? organizations?.[0]?.id ?? null)
            : null,
      });
      setDuplicating(null);
      setDuplicateKey("");
      setDuplicateName("");
      router.push(`/settings/iam/roles/${copy.id}`);
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? `The role was not copied: ${cause.message} (${cause.code})`
          : "The role was not copied.",
      );
    } finally {
      setBusy(null);
    }
  };

  if (status === "loading" && roles.length === 0 && !platformAccount) {
    return (
      <div className="rounded-xl border border-line bg-surface p-4" aria-live="polite">
        <span className="sr-only">Reading the roles…</span>
        {[0, 1, 2, 3].map((row) => (
          <div key={row} className="mb-2 h-10 animate-pulse rounded-lg bg-quiet-soft" />
        ))}
      </div>
    );
  }

  if (status === "error") {
    return (
      <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <ShieldAlert className="size-4 text-caution" aria-hidden />
        <p className="text-[13.5px] font-medium">The roles are unavailable</p>
        <p className="max-w-md text-[12.5px] text-muted">
          {loadError?.message} <span className="font-mono text-[11.5px]">({loadError?.code})</span>
        </p>
        <button
          type="button"
          onClick={() => void load(platformAccount ? selectedOrg : null)}
          className="mt-1 flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  const platform = roles.filter((role) => role.is_system);
  const custom = roles.filter((role) => !role.is_system);

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        {platformAccount && organizations && organizations.length > 0 ? (
          <label className="flex items-center gap-2 text-[12.5px]">
            <span className="text-muted">Organization</span>
            <select
              value={selectedOrg ?? ""}
              data-roles-organization
              onChange={(event) => setSelectedOrg(event.target.value)}
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
        <p className="text-[12.5px] text-muted">
          {platform.length} platform role{platform.length === 1 ? "" : "s"} · {custom.length} custom.
          Custom roles belong to this organization; platform roles are duplicated, never edited.
        </p>
        <button
          type="button"
          onClick={() => {
            setCreating(true);
            setNotice(null);
          }}
          data-role-create-open
          className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
        >
          <Plus className="size-3.5" aria-hidden />
          New role
        </button>
      </div>

      {creating ? (
        <CreateRoleForm
          roles={roles}
          organizations={platformAccount ? organizations : null}
          defaultOrganizationId={selectedOrg}
          onDone={(role) => {
            setCreating(false);
            router.push(`/settings/iam/roles/${role.id}`);
          }}
          onCancel={() => setCreating(false)}
        />
      ) : null}

      {notice ? (
        <p role="status" data-role-notice className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p role="alert" data-role-error className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution">
          {error}
        </p>
      ) : null}

      {/* The table needs the room; a phone gets the same rows as cards. */}
      <div className="hidden overflow-x-auto rounded-xl border border-line bg-surface lg:block">
        <table className="w-full text-left">
          <thead className="bg-canvas text-[11.5px] tracking-wide text-muted uppercase">
            <tr>
              <th className="px-3 py-2 font-medium">Role</th>
              <th className="px-3 py-2 font-medium">Origin</th>
              <th className="px-3 py-2 text-right font-medium">Priority</th>
              <th className="px-3 py-2 text-right font-medium">Allowed</th>
              <th className="px-3 py-2 text-right font-medium">Denied</th>
              <th className="px-3 py-2 font-medium">Inherits</th>
              <th className="px-3 py-2" />
            </tr>
          </thead>
          <tbody>
            {roles.map((role) => {
              const parent = role.inherits_role_id
                ? roles.find((entry) => entry.id === role.inherits_role_id)
                : undefined;
              return (
                <tr key={role.id} data-role-row={role.key} className="border-t border-line">
                  <td className="px-3 py-2.5">
                    <Link
                      href={`/settings/iam/roles/${role.id}`}
                      className="flex items-center gap-2 text-[13px] font-medium text-ink hover:text-accent-strong"
                    >
                      <ShieldCheck className="size-3.5 text-muted" aria-hidden />
                      {role.name}
                    </Link>
                    <span className="font-mono text-[11px] text-muted">{role.key}</span>
                  </td>
                  <td className="px-3 py-2.5">
                    <OriginBadge role={role} />
                  </td>
                  <td className="px-3 py-2.5 text-right font-mono text-[12.5px]">{role.priority}</td>
                  <td className="px-3 py-2.5 text-right font-mono text-[12.5px] text-positive">
                    {role.allowed_permissions}
                  </td>
                  <td className="px-3 py-2.5 text-right font-mono text-[12.5px] text-caution">
                    {role.denied_permissions}
                  </td>
                  <td className="px-3 py-2.5 text-[12px] text-muted">
                    {parent ? parent.name : role.inherits_role_id ? "—" : "nothing"}
                  </td>
                  <td className="px-3 py-2.5">
                    <div className="flex items-center justify-end gap-1.5">
                      <Link
                        href={`/settings/iam/roles/${role.id}`}
                        data-role-open={role.key}
                        className="rounded-lg border border-line px-2.5 py-1 text-[12px] font-medium transition hover:bg-quiet-soft"
                      >
                        Open
                      </Link>
                      <button
                        type="button"
                        data-role-duplicate={role.key}
                        onClick={() => {
                          setDuplicating(role.id);
                          setDuplicateKey(`${role.key}-copy`);
                          setDuplicateName(`${role.name} (copy)`);
                          setError(null);
                        }}
                        className="flex items-center gap-1 rounded-lg border border-line px-2.5 py-1 text-[12px] font-medium transition hover:bg-quiet-soft"
                      >
                        <Copy className="size-3" aria-hidden />
                        Duplicate
                      </button>
                      {!role.is_system ? (
                        confirmDelete === role.id ? (
                          <span className="flex items-center gap-1">
                            <button
                              type="button"
                              data-role-delete-confirm={role.key}
                              data-qa-guard="iam-role-delete"
                              onClick={() => void remove(role)}
                              disabled={busy === role.id}
                              className="rounded-lg border border-danger/50 bg-danger-soft px-2.5 py-1 text-[12px] font-medium text-caution transition hover:bg-danger/20 disabled:opacity-60"
                            >
                              Confirm delete
                            </button>
                            <button
                              type="button"
                              onClick={() => setConfirmDelete(null)}
                              className="rounded-lg border border-line px-2 py-1 text-[12px] transition hover:bg-quiet-soft"
                            >
                              Keep
                            </button>
                          </span>
                        ) : (
                          <button
                            type="button"
                            data-role-delete={role.key}
                            onClick={() => setConfirmDelete(role.id)}
                            className="flex items-center gap-1 rounded-lg border border-line px-2.5 py-1 text-[12px] font-medium text-caution transition hover:bg-quiet-soft"
                          >
                            <Trash2 className="size-3" aria-hidden />
                            Delete
                          </button>
                        )
                      ) : null}
                    </div>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      <ul className="flex flex-col divide-y divide-line rounded-xl border border-line bg-surface lg:hidden">
        {roles.map((role) => (
          <li key={role.id} data-role-card={role.key} className="flex flex-col gap-2 p-3">
            <div className="flex items-start justify-between gap-2">
              <div className="min-w-0">
                <Link href={`/settings/iam/roles/${role.id}`} className="text-[13px] font-medium text-ink">
                  {role.name}
                </Link>
                <p className="font-mono text-[11px] text-muted">{role.key}</p>
              </div>
              <OriginBadge role={role} />
            </div>
            <dl className="grid grid-cols-3 gap-x-3 text-[12px]">
              <dt className="text-muted">Priority</dt>
              <dt className="text-muted">Allowed</dt>
              <dt className="text-muted">Denied</dt>
              <dd className="font-mono">{role.priority}</dd>
              <dd className="font-mono text-positive">{role.allowed_permissions}</dd>
              <dd className="font-mono text-caution">{role.denied_permissions}</dd>
            </dl>
            <div className="flex flex-wrap items-center gap-1.5">
              <Link
                href={`/settings/iam/roles/${role.id}`}
                className="rounded-lg border border-line px-2.5 py-1 text-[12px] font-medium transition hover:bg-quiet-soft"
              >
                Open
              </Link>
              <button
                type="button"
                onClick={() => {
                  setDuplicating(role.id);
                  setDuplicateKey(`${role.key}-copy`);
                  setDuplicateName(`${role.name} (copy)`);
                }}
                className="rounded-lg border border-line px-2.5 py-1 text-[12px] font-medium transition hover:bg-quiet-soft"
              >
                Duplicate
              </button>
            </div>
          </li>
        ))}
      </ul>

      {roles.length === 0 ? (
        <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
          <ShieldCheck className="size-5 text-muted" aria-hidden />
          <p className="text-[13.5px] font-medium">No roles yet</p>
          <p className="max-w-md text-[12.5px] text-muted">
            Every installation ships the platform ladder; create a custom role to build on it.
          </p>
        </div>
      ) : null}

      {duplicating ? (
        <form
          data-role-duplicate-form
          onSubmit={(event) => {
            event.preventDefault();
            const role = roles.find((entry) => entry.id === duplicating);
            if (role) {
              void duplicate(role);
            }
          }}
          className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
        >
          <h2 className="text-[13.5px] font-semibold">Duplicate role</h2>
          <div className="grid gap-3 sm:grid-cols-2">
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Key of the copy</span>
              <input
                value={duplicateKey}
                data-role-duplicate-key
                onChange={(event) => setDuplicateKey(event.target.value)}
                required
                className="h-9 rounded-lg border border-line bg-surface px-2.5 font-mono text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Name of the copy</span>
              <input
                value={duplicateName}
                data-role-duplicate-name
                onChange={(event) => setDuplicateName(event.target.value)}
                required
                className="h-9 rounded-lg border border-line bg-surface px-2.5 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
          </div>
          <div className="flex items-center gap-2">
            <button
              type="submit"
              disabled={busy !== null}
              data-role-duplicate-submit
              data-qa-guard="iam-role-duplicate"
              className="rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
            >
              Duplicate
            </button>
            <button
              type="button"
              onClick={() => setDuplicating(null)}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
            >
              Cancel
            </button>
          </div>
        </form>
      ) : null}
    </div>
  );
}
