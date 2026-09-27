"use client";

/**
 * `/settings/iam/groups` — teams that carry roles (REQ-006, slice 2).
 *
 * A group is a subject like any other: the panel attaches roles to it and manages who belongs to
 * it, and every member holds those roles on their next request. Membership and the roles are
 * edited in the panel beside the list, so the whole lifecycle stays on one screen.
 */
import { useCallback, useEffect, useState } from "react";

import { Plus, RefreshCw, Trash2, UsersRound, X } from "lucide-react";

import { useSession } from "@/lib/session";
import {
  ApiError,
  createIamBinding,
  createIamGroup,
  deleteIamGroup,
  fetchIamGroup,
  fetchIamGroups,
  fetchIamUsers,
  fetchOrganizations,
  fetchRoles,
  revokeIamBinding,
  setIamGroupMembers,
  type IamGroup,
  type IamGroupDetail,
  type IamRole,
  type IamUserRow,
} from "@/lib/api";
import type { Organization } from "@/lib/types";

/** `/settings/iam/groups`. */
export function GroupsView() {
  const { user } = useSession();
  const [organizations, setOrganizations] = useState<Organization[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string | null>(null);
  const [groups, setGroups] = useState<IamGroup[]>([]);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // The open group.
  const [openId, setOpenId] = useState<string | null>(null);
  const [detail, setDetail] = useState<IamGroupDetail | null>(null);
  const [directory, setDirectory] = useState<IamUserRow[]>([]);
  const [memberIds, setMemberIds] = useState<string[]>([]);
  const [roles, setRoles] = useState<IamRole[]>([]);
  const [roleToAttach, setRoleToAttach] = useState("");
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);

  const platformAccount = user ? user.organization_id === null : false;
  const activeOrg = platformAccount ? selectedOrg : (user?.organization_id ?? null);

  const load = useCallback(async (organizationId: string | null) => {
    setStatus("loading");
    setLoadError(null);
    try {
      setGroups(await fetchIamGroups(organizationId));
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The groups could not be read." },
      );
    }
  }, []);

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

  // The open group's detail, the directory (for the member picker) and the roles.
  const openGroup = useCallback(
    async (groupId: string) => {
      setBusy(true);
      setError(null);
      try {
        const [group, users, roleList] = await Promise.all([
          fetchIamGroup(groupId),
          fetchIamUsers({ organizationId: activeOrg ?? undefined }),
          fetchRoles(activeOrg),
        ]);
        setDetail(group);
        setMemberIds(group.members.map((member) => member.user_id));
        setDirectory(users.users);
        setRoles(roleList);
        setOpenId(groupId);
      } catch (cause) {
        setError(
          cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The group could not be opened.",
        );
      } finally {
        setBusy(false);
      }
    },
    [activeOrg],
  );

  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      const created = await createIamGroup({
        name,
        description,
        organizationId: activeOrg,
      });
      setName("");
      setDescription("");
      setCreating(false);
      setNotice(`“${created.name}” was created.`);
      await load(activeOrg);
      await openGroup(created.id);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The group was not created.",
      );
    } finally {
      setBusy(false);
    }
  };

  const saveMembers = async () => {
    if (!openId) return;
    setBusy(true);
    setError(null);
    try {
      const answer = await setIamGroupMembers(openId, memberIds);
      setNotice(`Membership saved — ${answer.member_count} member(s).`);
      await openGroup(openId);
      await load(activeOrg);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The membership was not saved.",
      );
    } finally {
      setBusy(false);
    }
  };

  const attachRole = async () => {
    if (!openId || !roleToAttach || !detail) return;
    setBusy(true);
    setError(null);
    try {
      await createIamBinding({
        subjectType: "group",
        subjectId: openId,
        roleId: roleToAttach,
        scopeType: "organization",
        organizationId: detail.group.organization_id,
      });
      setRoleToAttach("");
      setNotice("The role was attached to the group — every member holds it on their next request.");
      await openGroup(openId);
      await load(activeOrg);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The role was not attached.",
      );
    } finally {
      setBusy(false);
    }
  };

  const remove = async (groupId: string) => {
    setBusy(true);
    setError(null);
    try {
      await deleteIamGroup(groupId);
      setNotice("The group was deleted; the roles it carried stopped applying.");
      setConfirmDelete(null);
      if (openId === groupId) {
        setOpenId(null);
        setDetail(null);
      }
      await load(activeOrg);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The group was not deleted.",
      );
    } finally {
      setBusy(false);
    }
  };

  if (status === "error") {
    return (
      <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <UsersRound className="size-4 text-caution" aria-hidden />
        <p className="text-[13.5px] font-medium">The groups are unavailable</p>
        <p className="max-w-md text-[12.5px] text-muted">
          {loadError?.message} <span className="font-mono text-[11.5px]">({loadError?.code})</span>
        </p>
        <button
          type="button"
          onClick={() => void load(activeOrg)}
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
              data-groups-organization
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
          {groups.length} group{groups.length === 1 ? "" : "s"} — membership and roles are edited
          beside the list.
        </p>
        <button
          type="button"
          onClick={() => setCreating(true)}
          data-group-create-open
          className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
        >
          <Plus className="size-3.5" aria-hidden />
          New group
        </button>
      </div>

      {creating ? (
        <form
          className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
          data-group-create-form
          onSubmit={(event) => {
            event.preventDefault();
            void create();
          }}
        >
          <p className="text-[13px] font-semibold">New group</p>
          <div className="grid gap-3 sm:grid-cols-2">
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Name</span>
              <input
                value={name}
                data-group-new-name
                onChange={(event) => setName(event.target.value)}
                placeholder="Marketing team"
                className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Description</span>
              <input
                value={description}
                data-group-new-description
                onChange={(event) => setDescription(event.target.value)}
                placeholder="Who belongs, and why"
                className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
          </div>
          <div className="flex items-center gap-2">
            <button
              type="submit"
              disabled={busy || !name.trim()}
              data-group-create-submit
              data-qa-guard="iam-group-create"
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
            >
              <Plus className="size-3.5" aria-hidden />
              Create group
            </button>
            <button
              type="button"
              onClick={() => setCreating(false)}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
            >
              Cancel
            </button>
          </div>
        </form>
      ) : null}

      {notice ? (
        <p role="status" data-groups-notice className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p role="alert" data-groups-error className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution">
          {error}
        </p>
      ) : null}

      <div className="flex flex-col gap-4 lg:flex-row">
        <div className="flex-1 overflow-x-auto rounded-xl border border-line bg-surface">
          {status === "loading" && groups.length === 0 ? (
            <div className="p-4" aria-live="polite">
              {[0, 1, 2].map((row) => (
                <div key={row} className="mb-2 h-10 animate-pulse rounded-lg bg-quiet-soft" />
              ))}
            </div>
          ) : groups.length === 0 ? (
            <div className="flex flex-col items-center gap-2 px-6 py-10 text-center">
              <UsersRound className="size-4 text-muted" aria-hidden />
              <p className="text-[13.5px] font-medium">No group yet</p>
              <p className="max-w-md text-[12.5px] text-muted">
                A group carries roles for everyone in it — create one and attach the roles the team
                should hold.
              </p>
            </div>
          ) : (
            <table className="w-full text-left">
              <thead className="bg-canvas text-[11.5px] tracking-wide text-muted uppercase">
                <tr>
                  <th className="px-3 py-2 font-medium">Group</th>
                  <th className="px-3 py-2 text-right font-medium">Members</th>
                  <th className="px-3 py-2 text-right font-medium">Roles</th>
                  <th className="px-3 py-2" />
                </tr>
              </thead>
              <tbody>
                {groups.map((group) => (
                  <tr key={group.id} data-group-row={group.slug} className="border-t border-line">
                    <td className="px-3 py-2.5">
                      <button
                        type="button"
                        onClick={() => void openGroup(group.id)}
                        data-group-open={group.slug}
                        className="text-left text-[13px] font-medium text-ink hover:text-accent-strong"
                      >
                        {group.name}
                      </button>
                      <p className="font-mono text-[11px] text-muted">{group.slug}</p>
                    </td>
                    <td className="px-3 py-2.5 text-right font-mono text-[12.5px]">{group.member_count}</td>
                    <td className="px-3 py-2.5 text-right font-mono text-[12.5px]">{group.role_count}</td>
                    <td className="px-3 py-2.5 text-right">
                      {confirmDelete === group.id ? (
                        <span className="flex items-center justify-end gap-1">
                          <button
                            type="button"
                            data-group-delete-confirm={group.slug}
                            data-qa-guard="iam-group-delete"
                            disabled={busy}
                            onClick={() => void remove(group.id)}
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
                        <span className="flex items-center justify-end gap-1.5">
                          <button
                            type="button"
                            onClick={() => void openGroup(group.id)}
                            className="rounded-lg border border-line px-2.5 py-1 text-[12px] font-medium transition hover:bg-quiet-soft"
                          >
                            Open
                          </button>
                          <button
                            type="button"
                            data-group-delete={group.slug}
                            aria-label={`Delete ${group.name}`}
                            title="Delete group"
                            onClick={() => setConfirmDelete(group.id)}
                            className="flex items-center rounded-lg border border-line px-2 py-1 text-caution transition hover:bg-danger-soft"
                          >
                            <Trash2 className="size-3.5" aria-hidden />
                          </button>
                        </span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>

        {openId && detail ? (
          <aside className="flex w-full flex-col gap-3 rounded-xl border border-line bg-surface p-4 lg:w-[380px]" data-group-panel>
            <div className="flex items-start justify-between gap-2">
              <div>
                <p className="text-[13.5px] font-semibold" data-group-panel-name>
                  {detail.group.name}
                </p>
                <p className="text-[11.5px] text-muted">{detail.group.description || "no description"}</p>
              </div>
              <button
                type="button"
                onClick={() => {
                  setOpenId(null);
                  setDetail(null);
                }}
                aria-label="Close the group panel"
                className="rounded-lg border border-line p-1 text-muted transition hover:bg-quiet-soft"
              >
                <X className="size-3.5" aria-hidden />
              </button>
            </div>

            <div className="flex flex-col gap-1.5">
              <p className="text-[12.5px] font-medium text-ink">
                Members ({memberIds.length} of {directory.length})
              </p>
              <div className="max-h-56 overflow-y-auto rounded-lg border border-line" data-group-members>
                {directory.length === 0 ? (
                  <p className="px-2.5 py-2 text-[12px] text-muted">No account in this organization yet.</p>
                ) : (
                  directory.map((account) => (
                    <label
                      key={account.id}
                      data-group-member-option={account.email}
                      className="flex items-center gap-2 border-b border-line px-2.5 py-1.5 text-[12.5px] last:border-b-0"
                    >
                      <input
                        type="checkbox"
                        checked={memberIds.includes(account.id)}
                        onChange={(event) => {
                          setMemberIds((current) =>
                            event.target.checked
                              ? [...current, account.id]
                              : current.filter((id) => id !== account.id),
                          );
                        }}
                        className="size-3.5 rounded border-line"
                      />
                      <span className="flex-1 truncate">{account.display_name || account.email}</span>
                      <span className="truncate text-[11px] text-muted">{account.email}</span>
                    </label>
                  ))
                )}
              </div>
              <button
                type="button"
                disabled={busy}
                data-group-members-save
                data-qa-guard="iam-group-members"
                onClick={() => void saveMembers()}
                className="self-start rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
              >
                Save membership
              </button>
            </div>

            <div className="flex flex-col gap-1.5 border-t border-line pt-3">
              <p className="text-[12.5px] font-medium text-ink">Roles carried ({detail.roles.length})</p>
              {detail.roles.length === 0 ? (
                <p className="text-[12px] text-muted">
                  No role attached — the group grants nothing yet.
                </p>
              ) : (
                <ul className="flex flex-col gap-1">
                  {detail.roles.map((binding) => (
                    <li
                      key={binding.binding_id}
                      data-group-role={binding.binding_id}
                      className="flex items-center justify-between gap-2 rounded-lg border border-line px-2.5 py-1.5"
                    >
                      <span className="text-[12.5px]">
                        {roles.find((role) => role.id === binding.role_id)?.name ?? binding.role_id.slice(0, 8)}
                        <span className="ml-1.5 font-mono text-[11px] text-muted">
                          {binding.scope.type}
                        </span>
                      </span>
                      <button
                        type="button"
                        data-group-role-revoke={binding.binding_id}
                        data-qa-guard="iam-group-role-revoke"
                        disabled={busy}
                        onClick={() => {
                          void (async () => {
                            setBusy(true);
                            setError(null);
                            try {
                              await revokeIamBinding(binding.binding_id);
                              setNotice("The role was detached from the group.");
                              await openGroup(openId);
                              await load(activeOrg);
                            } catch (cause) {
                              setError(
                                cause instanceof ApiError
                                  ? `${cause.message} (${cause.code})`
                                  : "The role was not detached.",
                              );
                            } finally {
                              setBusy(false);
                            }
                          })();
                        }}
                        className="rounded-lg border border-line px-2 py-0.5 text-[11.5px] transition hover:bg-quiet-soft disabled:opacity-60"
                      >
                        Detach
                      </button>
                    </li>
                  ))}
                </ul>
              )}
              <div className="flex items-center gap-2">
                <select
                  value={roleToAttach}
                  data-group-role-select
                  onChange={(event) => setRoleToAttach(event.target.value)}
                  className="h-8 flex-1 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent"
                >
                  <option value="">Pick a role…</option>
                  {roles.map((role) => (
                    <option key={role.id} value={role.id}>
                      {role.name} ({role.key})
                    </option>
                  ))}
                </select>
                <button
                  type="button"
                  disabled={busy || !roleToAttach}
                  data-group-role-attach
                  data-qa-guard="iam-group-role-attach"
                  onClick={() => void attachRole()}
                  className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft disabled:opacity-60"
                >
                  Attach
                </button>
              </div>
            </div>
          </aside>
        ) : null}
      </div>
    </div>
  );
}
