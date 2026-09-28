"use client";

/**
 * `/organizations/[id]` — one tenant in full (REQ-005, slices 1 and 2).
 *
 * Members (slice 1) answers who belongs to the organization, with the invitations waiting for
 * an answer and the dialog that starts one. Departments (slice 2) answers how they are arranged
 * and which roles a whole team carries. Every tab keeps its own loading, empty and error state,
 * and the tab is readable from the URL (`?tab=`) so a deep link lands on the right one.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { Check, MailPlus, RefreshCw, Search, Send, Trash2, UserMinus, X } from "lucide-react";
import { useSearchParams } from "next/navigation";

import { DepartmentsTab } from "@/features/organizations/departments-tab";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import {
  ApiError,
  createOrganizationInvitation,
  fetchOrganization,
  fetchOrganizationInvitations,
  fetchOrganizationMembers,
  fetchRoles,
  removeOrganizationMember,
  revokeOrganizationInvitation,
  updateOrganizationMember,
  type IamRole,
  type OrganizationInvitation,
  type OrganizationMember,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { Organization } from "@/lib/types";

/** A role chip; a member may hold several. */
function RoleChips({ member }: { member: OrganizationMember }) {
  if (member.roles.length === 0) {
    return <span className="text-[12px] text-muted">No roles</span>;
  }
  return (
    <span className="flex flex-wrap gap-1">
      {member.roles.map((role) => (
        <span
          key={role.id}
          title={role.key}
          className="max-w-40 truncate rounded-md border border-line bg-canvas px-1.5 py-0.5 text-[11px] font-medium text-muted"
        >
          {role.name}
        </span>
      ))}
    </span>
  );
}

/** The invite dialog: one address (or several), the role, an optional message, a preview. */
function InviteDialog({
  organizationId,
  onDone,
  onClose,
}: {
  organizationId: string;
  onDone: (token: string) => void;
  onClose: () => void;
}) {
  const [email, setEmail] = useState("");
  const [roleId, setRoleId] = useState("");
  const [message, setMessage] = useState("");
  const [roles, setRoles] = useState<IamRole[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [fieldError, setFieldError] = useState<string | null>(null);

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

  const submit = async () => {
    const address = email.trim();
    if (!address || !address.includes("@")) {
      setFieldError("Enter a valid e-mail address — name@example.com");
      return;
    }
    setFieldError(null);
    setError(null);
    setBusy(true);
    try {
      const created = await createOrganizationInvitation(organizationId, {
        email: address,
        role_id: roleId || null,
        message: message.trim() || undefined,
      });
      onDone(created.token);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? cause.message : "The invitation could not be sent.",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center px-4">
      <button
        type="button"
        aria-label="Close the invite dialog"
        onClick={onClose}
        className="absolute inset-0 bg-ink/40"
      />
      <div
        role="dialog"
        aria-label="Invite a member"
        className="relative flex max-h-[90vh] w-full max-w-md flex-col gap-3 overflow-y-auto rounded-xl border border-line bg-surface p-5 shadow-xl"
      >
        <div className="flex items-center justify-between">
          <h2 className="text-[14px] font-semibold">Invite a member</h2>
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
          E-mail address
          <input
            value={email}
            data-invite-email="1"
            onChange={(event) => setEmail(event.target.value)}
            placeholder="name@example.com"
            autoComplete="off"
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] font-normal outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
          {fieldError ? (
            <span role="alert" className="text-[12px] font-normal text-accent-strong">
              {fieldError}
            </span>
          ) : null}
        </label>

        <label className="flex flex-col gap-1.5 text-[12.5px] font-medium">
          Role on acceptance
          <select
            value={roleId}
            onChange={(event) => setRoleId(event.target.value)}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] font-normal outline-none"
          >
            <option value="">No role — a plain member</option>
            {roles.map((role) => (
              <option key={role.id} value={role.id}>
                {role.name}
              </option>
            ))}
          </select>
        </label>

        <label className="flex flex-col gap-1.5 text-[12.5px] font-medium">
          Personal message
          <textarea
            value={message}
            onChange={(event) => setMessage(event.target.value)}
            maxLength={400}
            rows={3}
            placeholder="Welcome aboard — the marketing site is yours."
            className="resize-y rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] font-normal outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
          <span className="text-[11.5px] font-normal text-muted">
            {message.length}/400
          </span>
        </label>

        <div className="rounded-lg border border-dashed border-line px-3 py-2.5 text-[12px] text-muted">
          <span className="font-medium text-ink">The recipient receives</span> a single-use link
          that joins this organization
          {roleId ? " with the selected role" : ""}
          {message.trim() ? ", together with your message" : ""}. The link expires in 14 days and
          can only be used once.
        </div>

        {error ? (
          <p role="alert" className="text-[12.5px] text-accent-strong">
            {error}
          </p>
        ) : null}

        <div className="flex gap-2">
          <button
            type="button"
            data-qa-guard="write"
            data-invite-submit="1"
            onClick={() => void submit()}
            disabled={busy}
            className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition disabled:opacity-60"
          >
            <Send className="size-3.5" aria-hidden />
            {busy ? "Sending…" : "Send invitation"}
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

/** The Members tab: members and the invitations waiting for an answer. */
function MembersTab({ organization }: { organization: Organization }) {
  const [members, setMembers] = useState<OrganizationMember[]>([]);
  const [invitations, setInvitations] = useState<OrganizationInvitation[]>([]);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [statusFilter, setStatusFilter] = useState("");
  const [busyId, setBusyId] = useState<string | null>(null);
  const [inviting, setInviting] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    setStatus("loading");
    setError(null);
    try {
      const [memberList, invitationList] = await Promise.all([
        fetchOrganizationMembers(organization.id),
        fetchOrganizationInvitations(organization.id),
      ]);
      setMembers(memberList.members);
      setInvitations(invitationList.invitations);
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      setError(
        cause instanceof ApiError ? cause.message : "The members could not be loaded.",
      );
    }
  }, [organization.id]);

  useEffect(() => {
    void load();
  }, [load]);

  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return members.filter((member) => {
      if (statusFilter && member.status !== statusFilter) return false;
      if (!needle) return true;
      return (
        member.display_name.toLowerCase().includes(needle) ||
        member.email.toLowerCase().includes(needle)
      );
    });
  }, [members, query, statusFilter]);

  const setMemberStatus = async (member: OrganizationMember, next: string) => {
    setBusyId(member.user_id);
    setNotice(null);
    try {
      await updateOrganizationMember(organization.id, member.user_id, { status: next });
      setNotice(`${member.display_name || member.email} is now ${next}.`);
      await load();
    } catch (cause) {
      setNotice(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The change was refused.",
      );
    } finally {
      setBusyId(null);
    }
  };

  const removeMember = async (member: OrganizationMember) => {
    setBusyId(member.user_id);
    setNotice(null);
    try {
      await removeOrganizationMember(organization.id, member.user_id);
      setNotice(`${member.display_name || member.email} was removed.`);
      await load();
    } catch (cause) {
      setNotice(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The removal was refused.",
      );
    } finally {
      setBusyId(null);
    }
  };

  const revoke = async (invitation: OrganizationInvitation) => {
    setBusyId(invitation.id);
    setNotice(null);
    try {
      await revokeOrganizationInvitation(organization.id, invitation.id);
      setNotice(`The invitation to ${invitation.email} was revoked.`);
      await load();
    } catch (cause) {
      setNotice(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The revocation was refused.",
      );
    } finally {
      setBusyId(null);
    }
  };

  return (
    <div className="flex flex-col gap-4">
      {notice ? (
        <p role="status" className="rounded-xl border border-line bg-surface px-4 py-3 text-[12.5px]">
          {notice}
        </p>
      ) : null}

      <div className="overflow-hidden rounded-xl border border-line bg-surface">
        <div className="flex flex-wrap items-center justify-between gap-3 border-b border-line px-4 py-3">
          <div className="flex items-baseline gap-2">
            <h2 className="text-[13.5px] font-medium" data-members-heading="1">
              Members
            </h2>
            <span className="text-[12px] text-muted">
              {status === "ready" ? `${filtered.length} of ${members.length}` : "Loading…"}
            </span>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <label className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5">
              <Search className="size-3.5 text-muted" aria-hidden />
              <span className="sr-only">Search members</span>
              <input
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                placeholder="Name or e-mail"
                className="w-40 bg-transparent text-[12.5px] outline-none"
              />
            </label>
            <label className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5">
              <span className="sr-only">Filter members by status</span>
              <select
                value={statusFilter}
                onChange={(event) => setStatusFilter(event.target.value)}
                className="bg-transparent text-[12.5px] outline-none"
              >
                <option value="">All statuses</option>
                {["active", "invited", "suspended"].map((value) => (
                  <option key={value} value={value}>
                    {value}
                  </option>
                ))}
              </select>
            </label>
            <button
              type="button"
              onClick={() => void load()}
              aria-label="Reload members"
              className="rounded-lg border border-line bg-surface p-2 text-muted transition hover:text-ink"
            >
              <RefreshCw className="size-3.5" aria-hidden />
            </button>
            <button
              type="button"
              data-invite-open="1"
              onClick={() => setInviting(true)}
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition"
            >
              <MailPlus className="size-3.5" aria-hidden />
              Invite member
            </button>
          </div>
        </div>

        {error ? (
          <div className="flex flex-col items-center gap-3 px-6 py-10 text-center">
            <p className="text-[12.5px] text-accent-strong">{error}</p>
            <button
              type="button"
              onClick={() => void load()}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              Try again
            </button>
          </div>
        ) : status !== "ready" ? (
          <LoadingTable columns={6} />
        ) : filtered.length === 0 ? (
          <EmptyState
            title={members.length === 0 ? "No members yet" : "Nothing matches that search"}
            hint={
              members.length === 0
                ? "Invite the first person into this organization; the link they receive signs them up and joins them here."
                : "Clear the search or the status filter to see every member."
            }
            action={
              members.length === 0 ? (
                <button
                  type="button"
                  onClick={() => setInviting(true)}
                  className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
                >
                  Invite the first member
                </button>
              ) : null
            }
          />
        ) : (
          <>
            {/* Desktop: a table. Below `md` the same rows become cards, because a seven-column
                table is unreadable on a phone. */}
            <div className="hidden overflow-x-auto md:block">
              <table className="w-full border-collapse text-left text-[13px]">
                <thead>
                  <tr className="bg-canvas/60 text-[11px] font-medium tracking-wide text-muted uppercase">
                    <th scope="col" className="px-4 py-2.5">Member</th>
                    <th scope="col" className="px-4 py-2.5">Roles</th>
                    <th scope="col" className="px-4 py-2.5">Status</th>
                    <th scope="col" className="px-4 py-2.5">Last active</th>
                    <th scope="col" className="px-4 py-2.5">Joined</th>
                    <th scope="col" className="px-4 py-2.5"><span className="sr-only">Actions</span></th>
                  </tr>
                </thead>
                <tbody>
                  {filtered.map((member) => (
                    <tr
                  key={member.id}
                  data-member-row={member.email}
                  className="border-t border-line transition hover:bg-canvas/60"
                >
                      <td className="px-4 py-3.5">
                        <span className="flex min-w-0 flex-col">
                          <span className="flex items-center gap-2">
                            <span className="truncate font-medium">
                              {member.display_name || member.email}
                            </span>
                            {member.is_primary ? (
                              <span
                                title="This account's home organization"
                                className="rounded-full bg-accent-soft px-2 py-0.5 text-[11px] font-medium text-accent-strong"
                              >
                                Home
                              </span>
                            ) : null}
                          </span>
                          <span className="truncate text-[12px] text-muted">{member.email}</span>
                        </span>
                      </td>
                      <td className="px-4 py-3.5"><RoleChips member={member} /></td>
                      <td className="px-4 py-3.5"><StatusBadge status={member.status} /></td>
                      <td className="px-4 py-3.5 text-muted">
                        {member.last_active_at ? formatTimestamp(member.last_active_at) : "Never"}
                      </td>
                      <td className="px-4 py-3.5 text-muted">
                        {member.joined_at ? formatTimestamp(member.joined_at) : "—"}
                      </td>
                      <td className="px-4 py-3.5 text-right">
                        <span className="flex justify-end gap-1.5">
                          {member.status === "suspended" ? (
                            <button
                              type="button"
                              data-qa-guard="write"
                              onClick={() => void setMemberStatus(member, "active")}
                              disabled={busyId === member.user_id}
                              className="flex items-center gap-1 rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-60"
                            >
                              <Check className="size-3" aria-hidden />
                              Reactivate
                            </button>
                          ) : (
                            <button
                              type="button"
                              data-qa-guard="write"
                              onClick={() => void setMemberStatus(member, "suspended")}
                              disabled={busyId === member.user_id}
                              className="rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-60"
                            >
                              Suspend
                            </button>
                          )}
                          <button
                            type="button"
                            data-qa-guard="write"
                            onClick={() => void removeMember(member)}
                            disabled={busyId === member.user_id || member.is_primary}
                            title={
                              member.is_primary
                                ? "This account's home organization — switch it elsewhere first"
                                : `Remove ${member.email} from this organization`
                            }
                            className="flex items-center gap-1 rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-50"
                          >
                            <UserMinus className="size-3" aria-hidden />
                            Remove
                          </button>
                        </span>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>

            <ul className="flex flex-col gap-2 px-3 py-3 md:hidden">
              {filtered.map((member) => (
                <li key={member.id} className="rounded-lg border border-line px-3 py-2.5">
                  <div className="flex items-start justify-between gap-2">
                    <span className="flex min-w-0 flex-col">
                      <span className="truncate text-[13px] font-medium">
                        {member.display_name || member.email}
                      </span>
                      <span className="truncate text-[12px] text-muted">{member.email}</span>
                    </span>
                    <StatusBadge status={member.status} />
                  </div>
                  <div className="mt-1.5">
                    <RoleChips member={member} />
                  </div>
                  <div className="mt-2 flex flex-wrap gap-1.5">
                    {member.status === "suspended" ? (
                      <button
                        type="button"
                        data-qa-guard="write"
                        onClick={() => void setMemberStatus(member, "active")}
                        className="rounded-lg border border-line px-2.5 py-1.5 text-[12px]"
                      >
                        Reactivate
                      </button>
                    ) : (
                      <button
                        type="button"
                        data-qa-guard="write"
                        onClick={() => void setMemberStatus(member, "suspended")}
                        className="rounded-lg border border-line px-2.5 py-1.5 text-[12px]"
                      >
                        Suspend
                      </button>
                    )}
                    <button
                      type="button"
                      data-qa-guard="write"
                      onClick={() => void removeMember(member)}
                      disabled={member.is_primary}
                      className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] disabled:opacity-50"
                    >
                      Remove
                    </button>
                  </div>
                </li>
              ))}
            </ul>
          </>
        )}
      </div>

      <div className="overflow-hidden rounded-xl border border-line bg-surface">
        <div className="flex items-baseline gap-2 border-b border-line px-4 py-3">
          <h2 className="text-[13.5px] font-medium">Invitations</h2>
          <span className="text-[12px] text-muted">
            {status === "ready" ? `${invitations.length} total` : "Loading…"}
          </span>
        </div>
        {status !== "ready" ? (
          <LoadingTable columns={3} rows={2} />
        ) : invitations.length === 0 ? (
          <EmptyState
            title="No invitations"
            hint="An invitation is a single-use link with an expiry — nothing is waiting for an answer."
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="bg-canvas/60 text-[11px] font-medium tracking-wide text-muted uppercase">
                  <th scope="col" className="px-4 py-2.5">Address</th>
                  <th scope="col" className="px-4 py-2.5">Status</th>
                  <th scope="col" className="px-4 py-2.5">Expires</th>
                  <th scope="col" className="px-4 py-2.5"><span className="sr-only">Actions</span></th>
                </tr>
              </thead>
              <tbody>
                {invitations.map((invitation) => (
                  <tr
                  key={invitation.id}
                  data-invitation-row={invitation.email}
                  className="border-t border-line transition hover:bg-canvas/60"
                >
                    <td className="px-4 py-3.5">
                      <span className="flex min-w-0 flex-col">
                        <span className="truncate font-medium">{invitation.email}</span>
                        {invitation.message ? (
                          <span className="truncate text-[12px] text-muted">
                            “{invitation.message}”
                          </span>
                        ) : null}
                      </span>
                    </td>
                    <td className="px-4 py-3.5"><StatusBadge status={invitation.status} /></td>
                    <td className="px-4 py-3.5 text-muted">
                      {formatTimestamp(invitation.expires_at)}
                    </td>
                    <td className="px-4 py-3.5 text-right">
                      {invitation.status === "pending" ? (
                        <button
                          type="button"
                          data-qa-guard="write"
                          data-invitation-revoke={invitation.email}
                          onClick={() => void revoke(invitation)}
                          disabled={busyId === invitation.id}
                          className="flex items-center gap-1 rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-60"
                        >
                          <Trash2 className="size-3" aria-hidden />
                          Revoke
                        </button>
                      ) : (
                        <span className="text-[12px] text-muted">—</span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {inviting ? (
        <InviteDialog
          organizationId={organization.id}
          onClose={() => setInviting(false)}
          onDone={(token) => {
            setInviting(false);
            setNotice(
              `Invitation sent. The single-use link is /invite/${token.slice(0, 8)}… — it is shown once, so copy it into your mail now.`,
            );
            void load();
          }}
        />
      ) : null}
    </div>
  );
}

/**
 * The tabs this slice ships. Roles, modules, API keys, billing, settings and audit arrive with
 * slice 3; the Overview summary with it as well.
 */
const TABS = [
  { key: "members", label: "Members" },
  { key: "departments", label: "Departments" },
] as const;

/** `/organizations/[id]`. */
export function OrganizationDetailView({ organizationId }: { organizationId: string }) {
  const params = useSearchParams();
  const [organization, setOrganization] = useState<Organization | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [error, setError] = useState<string | null>(null);
  const [tab, setTab] = useState<string>(params.get("tab") ?? "members");

  const load = useCallback(async () => {
    setStatus("loading");
    setError(null);
    try {
      setOrganization(await fetchOrganization(organizationId));
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      setError(
        cause instanceof ApiError ? cause.message : "The organization could not be loaded.",
      );
    }
  }, [organizationId]);

  useEffect(() => {
    void load();
  }, [load]);

  if (status === "loading") {
    return <LoadingTable columns={4} />;
  }

  if (status === "error" || !organization) {
    return (
      <div className="flex flex-col items-center gap-3 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <p className="text-[12.5px] text-accent-strong">{error}</p>
        <button
          type="button"
          onClick={() => void load()}
          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
        >
          Try again
        </button>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-3">
        <h2 className="text-[15px] font-semibold">{organization.name}</h2>
        <StatusBadge status={organization.status} />
        <span className="text-[12px] text-muted">{organization.slug}</span>
      </div>

      {organization.status !== "active" ? (
        <p
          role="status"
          className="rounded-xl border border-caution/40 bg-caution-soft px-4 py-3 text-[12.5px] text-caution"
        >
          This organization is {organization.status}. Reads still work, and the owner has to
          reactivate it before anything can be written.
        </p>
      ) : null}

      <nav
        aria-label="Organization sections"
        className="flex gap-1 overflow-x-auto border-b border-line"
      >
        {TABS.map((entry) => (
          <button
            key={entry.key}
            type="button"
            onClick={() => setTab(entry.key)}
            aria-current={tab === entry.key ? "page" : undefined}
            className={`shrink-0 border-b-2 px-3 py-2 text-[13px] transition ${
              tab === entry.key
                ? "border-accent font-medium text-accent-strong"
                : "border-transparent text-muted hover:text-ink"
            }`}
          >
            {entry.label}
          </button>
        ))}
      </nav>

      {tab === "members" ? <MembersTab organization={organization} /> : null}
      {tab === "departments" ? <DepartmentsTab organization={organization} /> : null}
    </div>
  );
}
