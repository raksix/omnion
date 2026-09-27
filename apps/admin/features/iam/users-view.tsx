"use client";

/**
 * `/settings/iam/users` — the user list (REQ-006, slice 2).
 *
 * The account surface every other IAM screen hangs off: search and filters over the whole
 * directory, the role chips an account holds (own bindings plus the ones its groups carry), and
 * the create form — invite an address without a password, or set one directly.
 */
import { useCallback, useEffect, useState } from "react";

import { Plus, RefreshCw, Search, UserPlus, UsersRound } from "lucide-react";
import Link from "next/link";

import { useSession } from "@/lib/session";
import {
  ApiError,
  createIamUser,
  fetchIamUsers,
  fetchOrganizations,
  fetchRoles,
  type IamRole,
  type IamUserRow,
} from "@/lib/api";
import type { Organization } from "@/lib/types";

/** A small chip for a role an account holds. */
function RoleChip({ chip }: { chip: IamUserRow["roles"][number] }) {
  return (
    <span
      className={`rounded-md border px-1.5 py-0.5 text-[11px] font-medium ${
        chip.via === "group"
          ? "border-accent/40 bg-accent-soft text-accent-strong"
          : "border-line bg-canvas text-muted"
      }`}
      title={chip.via === "group" ? "Through a group" : "Direct binding"}
    >
      {chip.name}
    </span>
  );
}

/** The create form of the screen. */
function CreateUserForm({
  roles,
  organizations,
  defaultOrganizationId,
  onDone,
  onCancel,
}: {
  roles: IamRole[];
  organizations: Organization[] | null;
  defaultOrganizationId?: string | null;
  onDone: (user: { id: string; email: string }) => void;
  onCancel: () => void;
}) {
  const [email, setEmail] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [roleId, setRoleId] = useState("");
  const [organizationId, setOrganizationId] = useState(defaultOrganizationId ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!organizationId && organizations && organizations.length > 0) {
      setOrganizationId(organizations[0].id);
    }
  }, [organizations, organizationId]);

  const submit = async () => {
    setError(null);
    if (!email.trim()) {
      setError("An e-mail address is required.");
      return;
    }
    if (password && password.length < 8) {
      setError("A password needs at least 8 characters — or leave it empty to invite instead.");
      return;
    }
    setBusy(true);
    try {
      const created = await createIamUser({
        email: email.trim(),
        displayName: displayName.trim(),
        organizationId: organizationId || null,
        password: password || undefined,
        roleId: roleId || undefined,
        roleScopeType: roleId ? "organization" : undefined,
      });
      onDone(created);
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? `${cause.message} (${cause.code})`
          : "The account could not be created.",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <form
      className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
      data-user-create-form
    >
      <p className="text-[13px] font-semibold">Add an account</p>
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <label className="flex flex-col gap-1.5">
          <span className="text-[12.5px] font-medium text-ink">E-mail</span>
          <input
            type="email"
            value={email}
            data-user-new-email
            onChange={(event) => setEmail(event.target.value)}
            placeholder="ada@example.com"
            className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
        <label className="flex flex-col gap-1.5">
          <span className="text-[12.5px] font-medium text-ink">Display name</span>
          <input
            value={displayName}
            data-user-new-name
            onChange={(event) => setDisplayName(event.target.value)}
            placeholder="Ada Lovelace"
            className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
        <label className="flex flex-col gap-1.5">
          <span className="text-[12.5px] font-medium text-ink">Password (optional)</span>
          <input
            type="password"
            value={password}
            data-user-new-password
            onChange={(event) => setPassword(event.target.value)}
            placeholder="Leave empty to invite"
            className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
        <label className="flex flex-col gap-1.5">
          <span className="text-[12.5px] font-medium text-ink">First role (optional)</span>
          <select
            value={roleId}
            data-user-new-role
            onChange={(event) => setRoleId(event.target.value)}
            className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          >
            <option value="">None</option>
            {roles.map((role) => (
              <option key={role.id} value={role.id}>
                {role.name} ({role.key})
              </option>
            ))}
          </select>
        </label>
      </div>
      {organizations !== null ? (
        <label className="flex flex-col gap-1.5 sm:max-w-sm">
          <span className="text-[12.5px] font-medium text-ink">Organization</span>
          <select
            value={organizationId}
            data-user-new-organization
            onChange={(event) => setOrganizationId(event.target.value)}
            className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          >
            {organizations.length === 0 ? <option value="">No organization exists yet</option> : null}
            {organizations.map((organization) => (
              <option key={organization.id} value={organization.id}>
                {organization.name}
              </option>
            ))}
          </select>
          <span className="text-[11.5px] text-muted">
            This account works platform-wide, so the account needs a tenant (or none at all — a
            platform account).
          </span>
        </label>
      ) : null}
      {error ? (
        <p role="alert" data-user-create-error className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution">
          {error}
        </p>
      ) : null}
      <div className="flex items-center gap-2">
        <button
          type="submit"
          disabled={busy}
          data-user-create-submit
          data-qa-guard="iam-user-create"
          className="flex items-center gap-1.5 rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
        >
          <UserPlus className="size-3.5" aria-hidden />
          {password ? "Create account" : "Invite account"}
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

/** `/settings/iam/users`. */
export function UsersView() {
  const { user } = useSession();
  const [organizations, setOrganizations] = useState<Organization[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string | null>(null);
  const [roles, setRoles] = useState<IamRole[]>([]);
  const [rows, setRows] = useState<IamUserRow[]>([]);
  const [total, setTotal] = useState(0);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [search, setSearch] = useState("");
  const [statusFilter, setStatusFilter] = useState("");
  const [mfaFilter, setMfaFilter] = useState("");
  const [roleFilter, setRoleFilter] = useState("");
  const [creating, setCreating] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const platformAccount = user ? user.organization_id === null : false;

  const load = useCallback(
    async (organizationId: string | null) => {
      setStatus("loading");
      setLoadError(null);
      try {
        const answer = await fetchIamUsers({
          search: search || undefined,
          status: statusFilter || undefined,
          mfa: mfaFilter === "" ? undefined : mfaFilter === "yes",
          roleId: roleFilter || undefined,
          organizationId,
        });
        setRows(answer.users);
        setTotal(answer.total);
        setStatus("ready");
      } catch (cause) {
        setStatus("error");
        setLoadError(
          cause instanceof ApiError
            ? { code: cause.code, message: cause.message }
            : { code: "unknown_error", message: "The accounts could not be read." },
        );
      }
    },
    [search, statusFilter, mfaFilter, roleFilter],
  );

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

  useEffect(() => {
    void fetchRoles(platformAccount ? selectedOrg : null)
      .then(setRoles)
      .catch(() => setRoles([]));
  }, [platformAccount, selectedOrg]);

  // The search box waits 250 ms; the other filters act at once.
  useEffect(() => {
    if (!user) return;
    const handle = window.setTimeout(() => {
      void load(platformAccount ? selectedOrg : null);
    }, 250);
    return () => window.clearTimeout(handle);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [search]);

  if (status === "error") {
    return (
      <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <UsersRound className="size-4 text-caution" aria-hidden />
        <p className="text-[13.5px] font-medium">The accounts are unavailable</p>
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

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        {platformAccount && organizations && organizations.length > 0 ? (
          <label className="flex items-center gap-2 text-[12.5px]">
            <span className="text-muted">Organization</span>
            <select
              value={selectedOrg ?? ""}
              data-users-organization
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
          {total} account{total === 1 ? "" : "s"}
          {search || statusFilter || mfaFilter || roleFilter ? " matching the filters" : ""} · role
          chips include the bindings an account carries through its groups.
        </p>
        <button
          type="button"
          onClick={() => {
            setCreating(true);
            setNotice(null);
          }}
          data-user-create-open
          className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
        >
          <Plus className="size-3.5" aria-hidden />
          Add account
        </button>
      </div>

      <div className="flex flex-wrap items-center gap-2" data-users-filters>
        <label className="flex flex-1 items-center gap-2 rounded-lg border border-line bg-surface px-2.5 py-1.5 sm:max-w-xs">
          <Search className="size-3.5 text-muted" aria-hidden />
          <input
            value={search}
            data-users-search
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Search name or e-mail"
            className="w-full bg-transparent text-[12.5px] text-ink outline-none"
          />
        </label>
        <select
          value={statusFilter}
          data-users-status
          onChange={(event) => setStatusFilter(event.target.value)}
          className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent"
        >
          <option value="">Any status</option>
          <option value="active">Active</option>
          <option value="invited">Invited</option>
          <option value="disabled">Disabled</option>
        </select>
        <select
          value={mfaFilter}
          data-users-mfa
          onChange={(event) => setMfaFilter(event.target.value)}
          className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent"
        >
          <option value="">Any MFA</option>
          <option value="yes">MFA required</option>
          <option value="no">MFA not required</option>
        </select>
        <select
          value={roleFilter}
          data-users-role
          onChange={(event) => setRoleFilter(event.target.value)}
          className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent"
        >
          <option value="">Any role</option>
          {roles.map((role) => (
            <option key={role.id} value={role.id}>
              {role.name}
            </option>
          ))}
        </select>
      </div>

      {creating ? (
        <CreateUserForm
          roles={roles}
          organizations={platformAccount ? organizations : null}
          defaultOrganizationId={selectedOrg}
          onDone={(created) => {
            setCreating(false);
            setNotice(`${created.email} was created.`);
            void load(platformAccount ? selectedOrg : null);
          }}
          onCancel={() => setCreating(false)}
        />
      ) : null}

      {notice ? (
        <p role="status" data-users-notice className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}

      {status === "loading" && rows.length === 0 ? (
        <div className="rounded-xl border border-line bg-surface p-4" aria-live="polite">
          <span className="sr-only">Reading the accounts…</span>
          {[0, 1, 2, 3, 4].map((row) => (
            <div key={row} className="mb-2 h-10 animate-pulse rounded-lg bg-quiet-soft" />
          ))}
        </div>
      ) : rows.length === 0 ? (
        <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
          <UsersRound className="size-4 text-muted" aria-hidden />
          <p className="text-[13.5px] font-medium">No account matches</p>
          <p className="max-w-md text-[12.5px] text-muted">
            Clear the filters, or add the first account — an invited address can sign in once a
            password is set for it.
          </p>
          <button
            type="button"
            onClick={() => setCreating(true)}
            className="mt-1 flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
          >
            <Plus className="size-3.5" aria-hidden />
            Add account
          </button>
        </div>
      ) : (
        <>
          {/* The table needs the room; a phone gets the same rows as cards. */}
          <div className="hidden overflow-x-auto rounded-xl border border-line bg-surface lg:block">
            <table className="w-full text-left">
              <thead className="bg-canvas text-[11.5px] tracking-wide text-muted uppercase">
                <tr>
                  <th className="px-3 py-2 font-medium">Account</th>
                  <th className="px-3 py-2 font-medium">Status</th>
                  <th className="px-3 py-2 font-medium">Roles</th>
                  <th className="px-3 py-2 font-medium">MFA</th>
                  <th className="px-3 py-2 font-medium">Last sign-in</th>
                  <th className="px-3 py-2" />
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => (
                  <tr key={row.id} data-user-row={row.email} className="border-t border-line">
                    <td className="px-3 py-2.5">
                      <Link
                        href={`/settings/iam/users/${row.id}`}
                        className="text-[13px] font-medium text-ink hover:text-accent-strong"
                      >
                        {row.display_name || row.email}
                      </Link>
                      <p className="text-[11.5px] text-muted">{row.email}</p>
                    </td>
                    <td className="px-3 py-2.5">
                      <span
                        className={`rounded-md border px-1.5 py-0.5 text-[11px] font-medium ${
                          row.status === "active"
                            ? "border-positive/40 bg-positive-soft text-positive"
                            : row.status === "disabled"
                              ? "border-danger/40 bg-danger-soft text-caution"
                              : "border-line bg-canvas text-muted"
                        }`}
                      >
                        {row.status}
                      </span>
                    </td>
                    <td className="px-3 py-2.5">
                      <span className="flex flex-wrap items-center gap-1">
                        {row.roles.slice(0, 3).map((chip) => (
                          <RoleChip key={`${row.id}-${chip.role_id}`} chip={chip} />
                        ))}
                        {row.roles.length > 3 ? (
                          <span className="text-[11px] text-muted">+{row.roles.length - 3}</span>
                        ) : null}
                        {row.roles.length === 0 ? (
                          <span className="text-[11.5px] text-muted">no role</span>
                        ) : null}
                        {row.group_count > 0 ? (
                          <span className="text-[11px] text-muted">· {row.group_count} group{row.group_count === 1 ? "" : "s"}</span>
                        ) : null}
                      </span>
                    </td>
                    <td className="px-3 py-2.5 text-[12px] text-muted">
                      {row.mfa_enforced ? "required" : "—"}
                    </td>
                    <td className="px-3 py-2.5 text-[12px] text-muted">
                      {row.last_sign_in_at ? row.last_sign_in_at.slice(0, 16).replace("T", " ") : "never"}
                    </td>
                    <td className="px-3 py-2.5 text-right">
                      <Link
                        href={`/settings/iam/users/${row.id}`}
                        data-user-open={row.email}
                        className="rounded-lg border border-line px-2.5 py-1 text-[12px] font-medium transition hover:bg-quiet-soft"
                      >
                        Open
                      </Link>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <div className="flex flex-col gap-2 lg:hidden">
            {rows.map((row) => (
              <Link
                key={row.id}
                href={`/settings/iam/users/${row.id}`}
                data-user-card={row.email}
                className="flex flex-col gap-1.5 rounded-xl border border-line bg-surface p-3"
              >
                <span className="flex items-center justify-between gap-2">
                  <span className="text-[13px] font-medium text-ink">
                    {row.display_name || row.email}
                  </span>
                  <span className="text-[11px] text-muted">{row.status}</span>
                </span>
                <span className="text-[11.5px] text-muted">{row.email}</span>
                <span className="flex flex-wrap items-center gap-1">
                  {row.roles.slice(0, 3).map((chip) => (
                    <RoleChip key={`${row.id}-m-${chip.role_id}`} chip={chip} />
                  ))}
                  {row.roles.length > 3 ? (
                    <span className="text-[11px] text-muted">+{row.roles.length - 3}</span>
                  ) : null}
                </span>
              </Link>
            ))}
          </div>
        </>
      )}
    </div>
  );
}
