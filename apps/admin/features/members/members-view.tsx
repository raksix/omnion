"use client";

/**
 * `/members` — visitor accounts, their sessions, and the site's membership policy (REQ-064,
 * slice 4c).
 *
 * This screen is the panel side of a boundary the REQ calls its single most important one, so it
 * carries the reasoning rather than just the controls. Six decisions, each one a place the obvious
 * version leaks or lies:
 *
 * 1. **A visitor account is not a panel user, and the screen says so in its own subtitle.** The
 *    table is `cms_members`; the roles column holds the SITE's role names, which resolve to
 *    nothing in the permission system. An operator who grants "editor" here and expects a panel
 *    login to appear has been told by a table that never said otherwise.
 * 2. **`pending` is a state, not a failure.** A signup that has not clicked its link is rendered
 *    as "Waiting", with the resend button next to it. A panel that coloured pending as broken
 *    teaches every operator that verification is broken.
 * 3. **`has_password` is a boolean and the drawer says what it means.** "Invited, never claimed"
 *    and "signed in yesterday" are two different rows, and neither is answered by a hash — so the
 *    panel never renders one and never claims to.
 * 4. **Blocking takes a reason, and deleting names the address it erases.** One is a decision an
 *    operator has to be able to reverse and explain later; the other is irreversible, so the
 *    confirmation quotes the address rather than asking "are you sure".
 * 5. **Sign-out-everywhere reports how many sessions died.** "Signed out" against a member with
 *    three devices is a claim the panel cannot support, and the number is the whole point of
 *    pressing the button.
 * 6. **`gated_page_behaviour` is a radio with its consequence written out, and the default is
 *    shown.** `not_found` answers 404 and discloses nothing; `prompt` answers 401 with a sign-in
 *    link, which ADVERTISES that the page exists. The REQ's criterion demands the first, so the
 *    first is what a fresh site gets — and the screen prints which one is in force rather than
 *    leaving an owner to infer it from behaviour nobody can see.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  Ban as BanIcon,
  Check,
  KeyRound,
  Loader2,
  LogOut,
  Mail,
  Plus,
  RefreshCw,
  Search,
  ShieldCheck,
  Trash2,
  UserPlus,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  blockMember,
  createMember,
  deleteMember,
  fetchMember,
  fetchMemberList,
  fetchMemberSettings,
  patchMember,
  saveMemberSettings,
  sendMemberReset,
  sendMemberVerification,
  signOutMemberEverywhere,
  verifyMember,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { useSites } from "@/lib/sites";
import type {
  Member,
  MemberDetail,
  MemberSettings,
  MemberSettingsDocument,
  MemberStatus,
} from "@/lib/types";

/** The three states, in the order an operator works them. */
const FILTERS: { key: MemberStatus | "all"; label: string; empty: string }[] = [
  {
    key: "all",
    label: "Everyone",
    empty:
      "No visitor has signed up on this site yet. Turn signup on in the policy above, and the site's own form is where they arrive from.",
  },
  {
    key: "pending",
    label: "Waiting",
    empty:
      "Nobody is waiting to confirm. A signup that has not clicked its verification link lands here — resending the link is one click on the row.",
  },
  {
    key: "verified",
    label: "Verified",
    empty:
      "No account can sign in yet. Verify an address yourself when you know the person, or wait for the site's own confirmation link.",
  },
  {
    key: "blocked",
    label: "Blocked",
    empty:
      "Nobody is blocked. Blocking stops every live session on the next request and keeps the account and its history.",
  },
];

/** How a state is labelled on a row. `pending` is "Waiting" and never "Failed". */
const STATUS_LABEL: Record<MemberStatus, string> = {
  pending: "Waiting",
  verified: "Verified",
  blocked: "Blocked",
};

/** The member's name if there is one, and the address otherwise. */
function who(member: Member): string {
  return member.name?.trim() || member.email;
}

export function MembersView() {
  const { selectedSite, status: siteStatus, error: siteError } = useSites();
  const siteId = selectedSite?.id ?? null;

  const [tab, setTab] = useState<MemberStatus | "all">("all");
  const [search, setSearch] = useState("");
  const [appliedSearch, setAppliedSearch] = useState("");
  const [list, setList] = useState<Awaited<ReturnType<typeof fetchMemberList>> | null>(null);
  const [document, setDocument] = useState<Awaited<ReturnType<typeof fetchMemberSettings>> | null>(
    null,
  );
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [open, setOpen] = useState<MemberDetail | null>(null);
  const [inviting, setInviting] = useState(false);
  const [blocking, setBlocking] = useState<Member | null>(null);
  const [purging, setPurging] = useState<Member | null>(null);
  const [savingPolicy, setSavingPolicy] = useState(false);

  const filters = useMemo(
    () => ({
      site_id: siteId ?? "",
      status: tab === "all" ? undefined : tab,
      search: appliedSearch || undefined,
      limit: 100,
    }),
    [siteId, tab, appliedSearch],
  );

  const load = useCallback(async () => {
    if (!siteId) return;
    setError(null);
    try {
      const [next, settings] = await Promise.all([
        fetchMemberList(filters),
        fetchMemberSettings(siteId),
      ]);
      setList(next);
      setDocument(settings);
    } catch (caught) {
      setError((caught as ApiError).message);
    }
  }, [siteId, filters]);

  useEffect(() => {
    void load();
  }, [load]);

  // A site change invalidates everything on screen, including a drawer opened against the
  // previous tenant — a member detail that survives a site switch is somebody else's member.
  useEffect(() => {
    setOpen(null);
    setInviting(false);
    setBlocking(null);
    setPurging(null);
    setNotice(null);
  }, [siteId]);

  const openDetail = useCallback(
    async (member: Member) => {
      if (!siteId) return;
      setError(null);
      try {
        setOpen(await fetchMember(member.id, siteId));
      } catch (caught) {
        setError((caught as ApiError).message);
      }
    },
    [siteId],
  );

  const refreshDrawer = useCallback(async () => {
    if (!siteId || !open) return;
    try {
      setOpen(await fetchMember(open.member.id, siteId));
    } catch (caught) {
      setError((caught as ApiError).message);
    }
  }, [siteId, open]);

  const verify = useCallback(
    async (member: Member) => {
      if (!siteId) return;
      setBusy(true);
      setError(null);
      try {
        await verifyMember(member.id, siteId);
        setNotice(`${member.email} can sign in now. The panel vouched for the address.`);
        await load();
        await refreshDrawer();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [siteId, load, refreshDrawer],
  );

  const doBlock = useCallback(
    async (member: Member, reason: string) => {
      if (!siteId) return;
      setBusy(true);
      setError(null);
      try {
        await blockMember(member.id, siteId, reason.trim() || undefined);
        setNotice(
          `${member.email} is blocked and every live session they held stopped on the next request. The account and its history stay.`,
        );
        setBlocking(null);
        await load();
        await refreshDrawer();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [siteId, load, refreshDrawer],
  );

  const purge = useCallback(
    async (member: Member) => {
      if (!siteId) return;
      setBusy(true);
      setError(null);
      try {
        await deleteMember(member.id, siteId);
        setNotice(`${member.email} is gone. This is the only irreversible action on this screen.`);
        setPurging(null);
        setOpen(null);
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [siteId, load],
  );

  const signOutAll = useCallback(
    async (member: Member) => {
      if (!siteId) return;
      setBusy(true);
      setError(null);
      try {
        const result = await signOutMemberEverywhere(member.id, siteId);
        setNotice(
          result.sessions_removed === 0
            ? `${member.email} had no live session to end.`
            : `Ended ${result.sessions_removed} live session${result.sessions_removed === 1 ? "" : "s"} for ${member.email}.`,
        );
        await load();
        await refreshDrawer();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [siteId, load, refreshDrawer],
  );

  const mintLink = useCallback(
    async (member: Member, kind: "verification" | "reset") => {
      if (!siteId) return;
      setBusy(true);
      setError(null);
      try {
        const result =
          kind === "verification"
            ? await sendMemberVerification(member.id, siteId)
            : await sendMemberReset(member.id, siteId);
        setNotice(
          result.delivery === "unavailable"
            ? `No mail transport is configured on this site, so nothing was sent to ${member.email}. The token was minted and discarded — the member cannot use it.`
            : kind === "verification"
              ? `A fresh verification link is on its way to ${member.email}. The previous link still works until this one is used.`
              : `A password reset link is on its way to ${member.email}. It works once and expires.`,
        );
        await refreshDrawer();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [siteId, refreshDrawer],
  );

  const saveMember = useCallback(
    async (detail: MemberDetail, changes: { name?: string | null; roles?: string[]; signin_note?: string | null }) => {
      if (!siteId) return;
      setBusy(true);
      setError(null);
      try {
        const updated = await patchMember(detail.member.id, siteId, changes);
        setOpen({ member: updated, recent_signins: detail.recent_signins });
        setNotice(
          "Saved. A member's roles REPLACE the whole list, so removing one here removes it from the site.",
        );
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [siteId, load],
  );

  if (siteStatus === "loading" && !list) {
    return (
      <div className="space-y-3" data-members-state="loading">
        <div className="h-8 w-64 animate-pulse rounded-md bg-surface" />
        <div className="h-64 animate-pulse rounded-md bg-surface" />
      </div>
    );
  }

  if (!siteId) {
    return (
      <div data-members-state="no-site">
        <EmptyState
          title="Pick a site"
          hint="Visitor accounts belong to a site. Choose one from the header to open its list."
        />
      </div>
    );
  }

  const rows = list?.members ?? [];

  return (
    <div className="space-y-6" data-members-state="ready" data-members-site={siteId}>
      <PolicyPanel
        document={document}
        verifiedCount={document?.verified_count ?? null}
        saving={savingPolicy}
        onSave={async (next) => {
          setSavingPolicy(true);
          setError(null);
          try {
            const saved = await saveMemberSettings(siteId, next);
            setDocument(saved);
            setNotice(
              saved.settings.gated_page_behaviour === "not_found"
                ? "Saved. A gated page now answers 404 to somebody who cannot read it — the same answer a page that does not exist gives."
                : "Saved. A gated page now answers 401 with a sign-in link, which tells a visitor that the page exists.",
            );
          } catch (caught) {
            setError((caught as ApiError).message);
          } finally {
            setSavingPolicy(false);
          }
        }}
      />

      {siteError ? <ErrorStrip message={siteError} onRetry={() => void load()} /> : null}
      {error ? <ErrorStrip message={error} onRetry={() => void load()} /> : null}
      {notice ? (
        <div
          role="status"
          data-members-notice
          className="flex items-start gap-2 rounded-md border border-line bg-surface px-3 py-2 text-[12.5px]"
        >
          <Check className="mt-0.5 h-3.5 w-3.5 shrink-0 text-ok" aria-hidden />
          <span className="flex-1">{notice}</span>
          <button type="button" onClick={() => setNotice(null)} aria-label="Dismiss">
            <X className="h-3.5 w-3.5" aria-hidden />
          </button>
        </div>
      ) : null}

      <section className="space-y-3" aria-label="Visitor accounts">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex flex-wrap gap-1" role="tablist" aria-label="Member states">
            {FILTERS.map((entry) => {
              // `list` is null on the first paint and the chip still has to render, so the count
              // is read through a local rather than dereferenced in the JSX — the alternative is
              // a tab bar that does not exist for one frame and shifts the layout.
              const counts = list?.counts;
              const count =
                entry.key === "all"
                  ? (counts?.pending ?? 0) + (counts?.verified ?? 0) + (counts?.blocked ?? 0)
                  : (counts?.[entry.key] ?? 0);
              const active = entry.key === tab;
              return (
                <button
                  key={entry.key}
                  type="button"
                  role="tab"
                  aria-selected={active}
                  data-member-tab={entry.key}
                  onClick={() => {
                    setTab(entry.key);
                    setError(null);
                    setNotice(null);
                  }}
                  className={[
                    "inline-flex items-center gap-1.5 rounded-md border px-2.5 py-1.5 text-[12.5px]",
                    active
                      ? "border-accent bg-accent/10 text-ink"
                      : "border-line text-muted hover:text-ink",
                  ].join(" ")}
                >
                  {entry.label}
                  <span
                    data-member-tab-count={entry.key}
                    className={[
                      "rounded px-1.5 py-0.5 text-[11px] tabular-nums",
                      count > 0 ? "bg-surface-2 text-ink" : "text-muted",
                    ].join(" ")}
                  >
                    {count}
                  </span>
                </button>
              );
            })}
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <form
              data-member-search-form
              onSubmit={(event) => {
                event.preventDefault();
                setAppliedSearch(search.trim());
              }}
              className="flex items-center gap-1.5"
            >
              <label htmlFor="member-search" className="sr-only">
                Search members
              </label>
              <input
                id="member-search"
                data-member-search
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder="Address or name"
                className="w-52 rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
              />
              <button
                type="submit"
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
              >
                <Search className="h-3.5 w-3.5" aria-hidden />
                Search
              </button>
            </form>
            <button
              type="button"
              data-members-refresh
              onClick={() => void load()}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
            >
              <RefreshCw className="h-3.5 w-3.5" aria-hidden />
              Refresh
            </button>
            <button
              type="button"
              data-member-invite-open
              onClick={() => setInviting(true)}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
            >
              <UserPlus className="h-3.5 w-3.5" aria-hidden />
              Add a member
            </button>
          </div>
        </div>

        {list === null ? (
          <div className="space-y-2" aria-busy="true">
            {[0, 1, 2].map((row) => (
              <div key={row} className="h-14 animate-pulse rounded-md bg-surface" />
            ))}
          </div>
        ) : rows.length === 0 ? (
          <EmptyState
            title={
              tab === "all"
                ? "No visitors have signed up yet"
                : `${FILTERS.find((entry) => entry.key === tab)?.label} is empty`
            }
            hint={FILTERS.find((entry) => entry.key === tab)?.empty}
            action={
              appliedSearch ? (
                <button
                  type="button"
                  onClick={() => {
                    setSearch("");
                    setAppliedSearch("");
                  }}
                  className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
                >
                  Clear the search
                </button>
              ) : (
                <button
                  type="button"
                  onClick={() => setInviting(true)}
                  className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
                >
                  <Plus className="h-3.5 w-3.5" aria-hidden />
                  Add a member yourself
                </button>
              )
            }
          />
        ) : (
          <>
            <div className="hidden overflow-x-auto sm:block">
              <table className="w-full min-w-[760px] border-collapse text-left text-[12.5px]">
                <thead>
                  <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                    <th scope="col" className="py-2">Visitor</th>
                    <th scope="col" className="py-2">Status</th>
                    <th scope="col" className="py-2">Site roles</th>
                    <th scope="col" className="py-2">Joined</th>
                    <th scope="col" className="py-2">Last sign-in</th>
                    <th scope="col" className="py-2">Sessions</th>
                    <th scope="col" className="py-2 text-right">Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((member) => (
                    <tr
                      key={member.id}
                      data-member-row={member.id}
                      data-member-status={member.status}
                      className="border-b border-line/60 align-top"
                    >
                      <td className="py-2.5 pr-3">
                        <button
                          type="button"
                          data-member-open={member.id}
                          onClick={() => void openDetail(member)}
                          className="text-left hover:underline"
                        >
                          <span className="block font-medium">{who(member)}</span>
                          {member.name ? (
                            <span className="block text-muted">{member.email}</span>
                          ) : null}
                        </button>
                        {!member.has_password ? (
                          <span
                            data-member-never-claimed={member.id}
                            className="mt-1 inline-block rounded bg-surface-2 px-1.5 py-0.5 text-[11px]"
                          >
                            Invited, never claimed
                          </span>
                        ) : null}
                      </td>
                      <td className="py-2.5 pr-3">
                        <span
                          data-member-status-badge={member.id}
                          className={[
                            "inline-block rounded px-1.5 py-0.5 text-[11px]",
                            member.status === "verified"
                              ? "bg-ok/10 text-ok"
                              : member.status === "blocked"
                                ? "bg-danger/10 text-danger"
                                : "bg-warn/10 text-warn",
                          ].join(" ")}
                        >
                          {STATUS_LABEL[member.status]}
                        </span>
                      </td>
                      <td className="py-2.5 pr-3">
                        {member.roles.length === 0 ? (
                          <span className="text-muted">—</span>
                        ) : (
                          <span className="flex flex-wrap gap-1">
                            {member.roles.map((role) => (
                              <span
                                key={role}
                                data-member-role={role}
                                className="rounded bg-surface-2 px-1.5 py-0.5 text-[11px]"
                              >
                                {role}
                              </span>
                            ))}
                          </span>
                        )}
                      </td>
                      <td className="py-2.5 pr-3 whitespace-nowrap text-muted">
                        {formatTimestamp(member.created_at)}
                      </td>
                      <td className="py-2.5 pr-3 whitespace-nowrap text-muted">
                        {member.last_signin_at
                          ? formatTimestamp(member.last_signin_at)
                          : "Never"}
                      </td>
                      <td className="py-2.5 pr-3 tabular-nums text-muted">{member.live_sessions}</td>
                      <td className="py-2.5 text-right">
                        <RowActions
                          member={member}
                          busy={busy}
                          onOpen={() => void openDetail(member)}
                          onVerify={() => void verify(member)}
                          onBlock={() => setBlocking(member)}
                        />
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>

            {/* The same rows as cards, because a seven-column table on a phone is a horizontal
                scroll and the REQ asks for 390 px without one. The table above is hidden below
                `sm` rather than being the only rendering. */}
            <ul className="space-y-2 sm:hidden" data-member-cards>
              {rows.map((member) => (
                <li
                  key={member.id}
                  data-member-card={member.id}
                  data-member-status={member.status}
                  className="space-y-2 rounded-md border border-line p-3 text-[12.5px]"
                >
                  <div className="flex items-start justify-between gap-2">
                    <button
                      type="button"
                      data-member-open={member.id}
                      onClick={() => void openDetail(member)}
                      className="text-left"
                    >
                      <span className="block font-medium">{who(member)}</span>
                      <span className="block text-muted">{member.email}</span>
                    </button>
                    <span
                      data-member-status-badge={member.id}
                      className={[
                        "shrink-0 rounded px-1.5 py-0.5 text-[11px]",
                        member.status === "verified"
                          ? "bg-ok/10 text-ok"
                          : member.status === "blocked"
                            ? "bg-danger/10 text-danger"
                            : "bg-warn/10 text-warn",
                      ].join(" ")}
                    >
                      {STATUS_LABEL[member.status]}
                    </span>
                  </div>
                  <dl className="grid grid-cols-2 gap-x-3 gap-y-1 text-muted">
                    <dt>Joined</dt>
                    <dd className="text-right">{formatTimestamp(member.created_at)}</dd>
                    <dt>Last sign-in</dt>
                    <dd className="text-right">
                      {member.last_signin_at
                        ? formatTimestamp(member.last_signin_at)
                        : "Never"}
                    </dd>
                    <dt>Sessions</dt>
                    <dd className="text-right tabular-nums">{member.live_sessions}</dd>
                    <dt>Roles</dt>
                    <dd className="text-right">{member.roles.join(", ") || "—"}</dd>
                  </dl>
                  <div className="flex flex-wrap gap-1.5">
                    {member.status === "pending" ? (
                      <SmallButton
                        label="Verify"
                        hook="verify"
                        icon={<ShieldCheck className="h-3.5 w-3.5" aria-hidden />}
                        disabled={busy}
                        onClick={() => void verify(member)}
                      />
                    ) : null}
                    {member.status !== "blocked" ? (
                      <SmallButton
                        label="Block"
                        hook="block"
                        icon={<BanIcon className="h-3.5 w-3.5" aria-hidden />}
                        disabled={busy}
                        onClick={() => setBlocking(member)}
                      />
                    ) : (
                      <SmallButton
                        label="Verify"
                        hook="verify"
                        icon={<ShieldCheck className="h-3.5 w-3.5" aria-hidden />}
                        disabled={busy}
                        onClick={() => void verify(member)}
                      />
                    )}
                  </div>
                </li>
              ))}
            </ul>
          </>
        )}
      </section>

      {open ? (
        <MemberDrawer
          detail={open}
          busy={busy}
          onClose={() => setOpen(null)}
          onSave={(changes) => void saveMember(open, changes)}
          onMint={(kind) => void mintLink(open.member, kind)}
          onSignOutAll={() => void signOutAll(open.member)}
          onBlock={() => setBlocking(open.member)}
          onPurge={() => setPurging(open.member)}
        />
      ) : null}

      {inviting ? (
        <InviteDialog
          busy={busy}
          onClose={() => setInviting(false)}
          onCreate={async (input) => {
            setBusy(true);
            setError(null);
            try {
              const created = await createMember({ site_id: siteId, ...input });
              setInviting(false);
              setNotice(
                created.has_password
                  ? `${created.email} can sign in with the password you set.`
                  : `${created.email} was invited. They set their own password from the verification link.`,
              );
              await load();
            } catch (caught) {
              setError((caught as ApiError).message);
              setInviting(false);
            } finally {
              setBusy(false);
            }
          }}
        />
      ) : null}

      {blocking ? (
        <BlockDialog
          member={blocking}
          busy={busy}
          onClose={() => setBlocking(null)}
          onBlock={(reason) => void doBlock(blocking, reason)}
        />
      ) : null}

      {purging ? (
        <ConfirmDelete
          member={purging}
          busy={busy}
          onClose={() => setPurging(null)}
          onConfirm={() => void purge(purging)}
        />
      ) : null}
    </div>
  );
}

/** One row's actions. Every button is a route the panel actually calls. */
function RowActions({
  member,
  busy,
  onOpen,
  onVerify,
  onBlock,
}: {
  member: Member;
  busy: boolean;
  onOpen: () => void;
  onVerify: () => void;
  onBlock: () => void;
}) {
  return (
    <span className="inline-flex flex-wrap justify-end gap-1">
      <SmallButton
        label="Details"
        hook="open"
        icon={<Mail className="h-3.5 w-3.5" aria-hidden />}
        onClick={onOpen}
      />
      {member.status !== "verified" ? (
        <SmallButton
          label="Verify"
          hook="verify"
          icon={<ShieldCheck className="h-3.5 w-3.5" aria-hidden />}
          disabled={busy}
          onClick={onVerify}
        />
      ) : null}
      {member.status !== "blocked" ? (
        <SmallButton
          label="Block"
          hook="block"
          icon={<BanIcon className="h-3.5 w-3.5" aria-hidden />}
          disabled={busy}
          onClick={onBlock}
        />
      ) : null}
    </span>
  );
}

/** One small table/card action. */
function SmallButton({
  label,
  hook,
  icon,
  onClick,
  disabled,
}: {
  label: string;
  hook: string;
  icon: React.ReactNode;
  onClick: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      data-member-action={hook}
      disabled={disabled}
      onClick={onClick}
      className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] disabled:opacity-50"
    >
      {icon}
      {label}
    </button>
  );
}

/** The drawer: the profile, the roles, and the last ten sign-ins. */
function MemberDrawer({
  detail,
  busy,
  onClose,
  onSave,
  onMint,
  onSignOutAll,
  onBlock,
  onPurge,
}: {
  detail: MemberDetail;
  busy: boolean;
  onClose: () => void;
  onSave: (changes: { name?: string | null; roles?: string[]; signin_note?: string | null }) => void;
  onMint: (kind: "verification" | "reset") => void;
  onSignOutAll: () => void;
  onBlock: () => void;
  onPurge: () => void;
}) {
  const { member, recent_signins } = detail;
  const [name, setName] = useState(member.name ?? "");
  const [roles, setRoles] = useState(member.roles.join(", "));
  const [note, setNote] = useState(member.signin_note ?? "");

  // A reload of the drawer after a save carries the new row, and a control that keeps the value
  // the operator just replaced is a panel that appears to have saved nothing.
  useEffect(() => {
    setName(member.name ?? "");
    setRoles(member.roles.join(", "));
    setNote(member.signin_note ?? "");
  }, [member]);

  const parseRoles = (raw: string) =>
    raw
      .split(",")
      .map((role) => role.trim())
      .filter((role) => role !== "");

  const dirty =
    name !== (member.name ?? "") ||
    roles !== member.roles.join(", ") ||
    note !== (member.signin_note ?? "");

  return (
    <div
      data-member-drawer={member.id}
      className="fixed inset-0 z-30 flex justify-end bg-black/30"
      role="dialog"
      aria-modal="true"
      aria-label={`Member ${member.email}`}
      onKeyDown={(event) => {
        if (event.key === "Escape") onClose();
      }}
    >
      <div
        className="flex h-full w-full max-w-lg flex-col gap-4 overflow-y-auto border-l border-line bg-panel p-5"
        onClick={(event) => event.stopPropagation()}
      >
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0">
            <h2 className="truncate text-[15px] font-medium">{member.email}</h2>
            <p className="text-[12px] text-muted">
              Joined {formatTimestamp(member.created_at)} ·{" "}
              {STATUS_LABEL[member.status]}
              {member.verified_at
                ? ` since ${formatTimestamp(member.verified_at)}`
                : " · never verified"}
            </p>
            <p className="mt-1 text-[11.5px] text-muted">
              {member.has_password
                ? "Has set a password."
                : "Invited and never claimed — no password has ever been set on this account."}
            </p>
          </div>
          <button type="button" onClick={onClose} aria-label="Close" data-member-drawer-close>
            <X className="h-4 w-4" aria-hidden />
          </button>
        </div>

        <p className="rounded-md border border-line bg-surface px-3 py-2 text-[11.5px] text-muted">
          This is a visitor account in <code>cms_members</code>, not a panel user. The roles below
          are the site&apos;s own names — they gate the site&apos;s pages and resolve to nothing in
          the panel&apos;s permission system, so granting one here never creates a panel login.
        </p>

        <form
          data-member-edit-form
          className="space-y-3"
          onSubmit={(event) => {
            event.preventDefault();
            onSave({
              name: name.trim() === "" ? null : name.trim(),
              roles: parseRoles(roles),
              signin_note: note.trim() === "" ? null : note.trim(),
            });
          }}
        >
          <div className="space-y-1.5">
            <label htmlFor="member-drawer-name" className="block text-[12.5px]">
              Display name
              <span className="block text-muted">
                Optional. An empty value clears it, and clearing is a separate button from leaving
                it alone.
              </span>
            </label>
            <input
              id="member-drawer-name"
              data-member-name
              value={name}
              onChange={(event) => setName(event.target.value)}
              className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
            />
          </div>

          <div className="space-y-1.5">
            <label htmlFor="member-drawer-roles" className="block text-[12.5px]">
              Site roles
              <span className="block text-muted">
                Comma separated. Saving REPLACES the whole list — a member who must keep a role
                they were granted by mistake would hold it for as long as the site exists.
              </span>
            </label>
            <input
              id="member-drawer-roles"
              data-member-roles
              value={roles}
              onChange={(event) => setRoles(event.target.value)}
              className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
            />
          </div>

          <div className="space-y-1.5">
            <label htmlFor="member-drawer-note" className="block text-[12.5px]">
              Panel note
              <span className="block text-muted">
                Only the panel reads this. A member never sees it, so it is safe to write why an
                address was blocked or why somebody was vouched for.
              </span>
            </label>
            <input
              id="member-drawer-note"
              data-member-note
              value={note}
              onChange={(event) => setNote(event.target.value)}
              className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
            />
          </div>

          <button
            type="submit"
            data-member-save
            disabled={!dirty || busy}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            {busy ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
            ) : (
              <Check className="h-3.5 w-3.5" aria-hidden />
            )}
            Save the member
          </button>
        </form>

        <section className="space-y-1.5" aria-label="Live sessions">
          <h3 className="text-[13px] font-medium">
            Recent sign-ins
            <span className="ml-1.5 text-[11.5px] font-normal text-muted">
              {member.live_sessions} live now
            </span>
          </h3>
          {recent_signins.length === 0 ? (
            <p className="rounded-md border border-line bg-surface px-3 py-2 text-[12px] text-muted">
              This account has never signed in. An invited address with no password has nothing to
              sign in with until the member claims it.
            </p>
          ) : (
            <ul className="space-y-1" data-member-signins>
              {recent_signins.map((session) => (
                <li
                  key={session.id}
                  data-member-signin={session.id}
                  className="flex flex-wrap justify-between gap-2 rounded-md border border-line px-3 py-1.5 text-[11.5px]"
                >
                  <span>Started {formatTimestamp(session.created_at)}</span>
                  <span className="text-muted">
                    last seen {formatTimestamp(session.last_seen_at)} · expires{" "}
                    {formatTimestamp(session.expires_at)}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </section>

        <div className="mt-auto flex flex-wrap gap-1.5 border-t border-line pt-3">
          <SmallButton
            label="Send verification link"
            hook="send-verification"
            icon={<Mail className="h-3.5 w-3.5" aria-hidden />}
            disabled={busy}
            onClick={() => onMint("verification")}
          />
          <SmallButton
            label="Send password reset"
            hook="send-reset"
            icon={<KeyRound className="h-3.5 w-3.5" aria-hidden />}
            disabled={busy}
            onClick={() => onMint("reset")}
          />
          <SmallButton
            label="Sign out everywhere"
            hook="sign-out"
            icon={<LogOut className="h-3.5 w-3.5" aria-hidden />}
            disabled={busy}
            onClick={onSignOutAll}
          />
          {member.status !== "blocked" ? (
            <SmallButton
              label="Block"
              hook="block"
              icon={<BanIcon className="h-3.5 w-3.5" aria-hidden />}
              disabled={busy}
              onClick={onBlock}
            />
          ) : null}
          <SmallButton
            label="Delete for good"
            hook="delete"
            icon={<Trash2 className="h-3.5 w-3.5" aria-hidden />}
            disabled={busy}
            onClick={onPurge}
          />
        </div>
      </div>
    </div>
  );
}

/** The operator's own "add a member" form. */
function InviteDialog({
  busy,
  onClose,
  onCreate,
}: {
  busy: boolean;
  onClose: () => void;
  onCreate: (input: { email: string; name?: string; password?: string; roles: string[] }) => void;
}) {
  const [email, setEmail] = useState("");
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [withPassword, setWithPassword] = useState(false);
  const [roles, setRoles] = useState("");
  const [error, setError] = useState<string | null>(null);

  return (
    <div
      data-member-invite-dialog
      className="fixed inset-0 z-30 flex items-center justify-center bg-black/30 p-4"
      role="dialog"
      aria-modal="true"
      aria-label="Add a member"
      onKeyDown={(event) => {
        if (event.key === "Escape") onClose();
      }}
    >
      <form
        className="w-full max-w-md space-y-3 rounded-md border border-line bg-panel p-5"
        onClick={(event) => event.stopPropagation()}
        onSubmit={(event) => {
          event.preventDefault();
          const trimmed = email.trim();
          if (!trimmed.includes("@")) {
            setError("That is not an e-mail address.");
            return;
          }
          if (withPassword && password.length < 10) {
            setError("A password you set here needs at least 10 characters.");
            return;
          }
          setError(null);
          onCreate({
            email: trimmed,
            name: name.trim() || undefined,
            password: withPassword ? password : undefined,
            roles: roles
              .split(",")
              .map((role) => role.trim())
              .filter((role) => role !== ""),
          });
        }}
      >
        <div className="flex items-start justify-between gap-3">
          <div>
            <h2 className="text-[15px] font-medium">Add a visitor account</h2>
            <p className="text-[12px] text-muted">
              A visitor account in this site&apos;s own list. It never becomes a panel user.
            </p>
          </div>
          <button type="button" onClick={onClose} aria-label="Close">
            <X className="h-4 w-4" aria-hidden />
          </button>
        </div>

        <div className="space-y-1.5">
          <label htmlFor="member-invite-email" className="block text-[12.5px]">
            E-mail address
          </label>
          <input
            id="member-invite-email"
            data-member-invite-email
            type="email"
            value={email}
            onChange={(event) => setEmail(event.target.value)}
            className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
        </div>

        <div className="space-y-1.5">
          <label htmlFor="member-invite-name" className="block text-[12.5px]">
            Display name <span className="text-muted">· optional</span>
          </label>
          <input
            id="member-invite-name"
            data-member-invite-name
            value={name}
            onChange={(event) => setName(event.target.value)}
            className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
        </div>

        <div className="space-y-1.5">
          <label htmlFor="member-invite-roles" className="block text-[12.5px]">
            Site roles <span className="text-muted">· comma separated, optional</span>
          </label>
          <input
            id="member-invite-roles"
            data-member-invite-roles
            value={roles}
            onChange={(event) => setRoles(event.target.value)}
            className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
        </div>

        <label className="flex items-start gap-2 text-[12.5px]">
          <input
            type="checkbox"
            data-member-invite-with-password
            checked={withPassword}
            onChange={(event) => setWithPassword(event.target.checked)}
            className="mt-0.5"
          />
          <span>
            I set the password now
            <span className="block text-muted">
              Leave this off to send an invitation: the member picks their own password from the
              verification link. Turning it on makes an account usable immediately — at the cost
              of a password you typed into a browser.
            </span>
          </span>
        </label>

        {withPassword ? (
          <div className="space-y-1.5">
            <label htmlFor="member-invite-password" className="block text-[12.5px]">
              Password
              <span className="block text-muted">At least 10 characters. Stored as Argon2id.</span>
            </label>
            <input
              id="member-invite-password"
              data-member-invite-password
              type="password"
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
            />
          </div>
        ) : null}

        {error ? <p className="text-[12px] text-danger">{error}</p> : null}

        <div className="flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            Cancel
          </button>
          <button
            type="submit"
            data-member-invite-submit
            disabled={busy || email.trim() === ""}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            {busy ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
            ) : (
              <UserPlus className="h-3.5 w-3.5" aria-hidden />
            )}
            Create the account
          </button>
        </div>
      </form>
    </div>
  );
}

/** The block dialog, which asks for the reason rather than taking a silent decision. */
function BlockDialog({
  member,
  busy,
  onClose,
  onBlock,
}: {
  member: Member;
  busy: boolean;
  onClose: () => void;
  onBlock: (reason: string) => void;
}) {
  const [reason, setReason] = useState("");
  return (
    <div
      data-member-block-dialog
      className="fixed inset-0 z-30 flex items-center justify-center bg-black/30 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={`Block ${member.email}`}
      onKeyDown={(event) => {
        if (event.key === "Escape") onClose();
      }}
    >
      <div
        className="w-full max-w-md space-y-3 rounded-md border border-line bg-panel p-5"
        onClick={(event) => event.stopPropagation()}
      >
        <h2 className="text-[15px] font-medium">Block {member.email}?</h2>
        <p className="text-[12.5px] text-muted">
          Every live session they hold stops on their next request — not in thirty days, on the next
          one. The account and its history stay, and you can verify them again.
        </p>
        <div className="space-y-1.5">
          <label htmlFor="member-block-reason" className="block text-[12.5px]">
            Reason <span className="text-muted">· shown in the panel, never to the member</span>
          </label>
          <textarea
            id="member-block-reason"
            data-member-block-reason
            rows={3}
            value={reason}
            onChange={(event) => setReason(event.target.value)}
            className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
        </div>
        <div className="flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            Cancel
          </button>
          <button
            type="button"
            data-member-block-submit
            disabled={busy}
            onClick={() => onBlock(reason)}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            {busy ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
            ) : (
              <BanIcon className="h-3.5 w-3.5" aria-hidden />
            )}
            Block the account
          </button>
        </div>
      </div>
    </div>
  );
}

/** The irreversible confirmation, which quotes the address it is about to erase. */
function ConfirmDelete({
  member,
  busy,
  onClose,
  onConfirm,
}: {
  member: Member;
  busy: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  return (
    <div
      data-member-delete-dialog
      className="fixed inset-0 z-30 flex items-center justify-center bg-black/30 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={`Delete ${member.email}`}
      onKeyDown={(event) => {
        if (event.key === "Escape") onClose();
      }}
    >
      <div
        className="w-full max-w-md space-y-3 rounded-md border border-line bg-panel p-5"
        onClick={(event) => event.stopPropagation()}
      >
        <h2 className="flex items-center gap-2 text-[15px] font-medium">
          <AlertTriangle className="h-4 w-4 text-danger" aria-hidden />
          Delete {member.email}?
        </h2>
        <p className="text-[12.5px]">
          This erases the account, its sessions and its sign-in history. Nothing brings it back, and
          blocking the account instead would keep the record.
        </p>
        <div className="flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            Keep the account
          </button>
          <button
            type="button"
            data-member-delete-submit
            disabled={busy}
            onClick={onConfirm}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            {busy ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
            ) : (
              <Trash2 className="h-3.5 w-3.5" aria-hidden />
            )}
            Delete for good
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * The membership policy, every control visible at once.
 *
 * The gated-page behaviour is a radio with its consequence spelled out rather than a checkbox with
 * a label, because the two answers differ in what they DISCLOSE: `not_found` is indistinguishable
 * from a page that does not exist, `prompt` tells a stranger that it does. An owner picks the
 * second deliberately, which is exactly what a panel that explains the difference makes possible.
 */
function PolicyPanel({
  document,
  verifiedCount,
  saving,
  onSave,
}: {
  document: MemberSettingsDocument | null;
  verifiedCount: number | null;
  saving: boolean;
  onSave: (next: Partial<MemberSettings>) => void;
}) {
  const [signup, setSignup] = useState<boolean | null>(null);
  const [verification, setVerification] = useState<boolean | null>(null);
  const [roles, setRoles] = useState<string | null>(null);
  const [redirect, setRedirect] = useState<string | null>(null);
  const [behaviour, setBehaviour] = useState<"not_found" | "prompt" | null>(null);

  useEffect(() => {
    if (!document) return;
    setSignup(document.settings.signup_enabled);
    setVerification(document.settings.require_verification);
    setRoles(document.settings.default_roles.join(", "));
    setRedirect(document.settings.post_signin_redirect ?? "");
    setBehaviour(document.settings.gated_page_behaviour);
  }, [document]);

  if (!document || signup === null || behaviour === null) {
    return (
      <section
        aria-label="Membership policy"
        data-member-policy="loading"
        className="h-44 animate-pulse rounded-md bg-surface"
      />
    );
  }

  const settings = document.settings;
  const dirty =
    signup !== settings.signup_enabled ||
    verification !== settings.require_verification ||
    roles !== settings.default_roles.join(", ") ||
    redirect !== (settings.post_signin_redirect ?? "") ||
    behaviour !== settings.gated_page_behaviour;

  return (
    <section
      aria-label="Membership policy"
      data-member-policy="ready"
      data-member-policy-signup={signup ? "on" : "off"}
      data-member-policy-behaviour={behaviour}
      className="space-y-3 rounded-md border border-line bg-panel p-4"
    >
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-[14px] font-medium">Membership policy</h2>
          <p className="text-[12px] text-muted">
            This site&apos;s own rules for visitor accounts.{" "}
            <span data-member-policy-verified className="tabular-nums">
              {verifiedCount ?? 0}
            </span>{" "}
            account{(verifiedCount ?? 0) === 1 ? "" : "s"} can sign in right now. Last changed{" "}
            {formatTimestamp(settings.updated_at)}.
          </p>
        </div>
        <button
          type="button"
          data-member-policy-save
          disabled={!dirty || saving}
          onClick={() =>
            onSave({
              signup_enabled: signup,
              require_verification: verification ?? true,
              default_roles: (roles ?? "")
                .split(",")
                .map((role) => role.trim())
                .filter((role) => role !== ""),
              post_signin_redirect: (redirect ?? "").trim() === "" ? null : (redirect ?? "").trim(),
              gated_page_behaviour: behaviour,
            })
          }
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
        >
          {saving ? (
            <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
          ) : (
            <Check className="h-3.5 w-3.5" aria-hidden />
          )}
          Save the policy
        </button>
      </div>

      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex items-start gap-2 text-[12.5px]">
          <input
            type="checkbox"
            data-member-policy-signup-toggle
            checked={signup}
            onChange={(event) => setSignup(event.target.checked)}
            className="mt-0.5"
          />
          <span>
            Accept signups
            <span className="block text-muted">
              The site&apos;s own form creates accounts. Off means the public route refuses every
              signup; existing accounts keep working.
            </span>
          </span>
        </label>

        <label className="flex items-start gap-2 text-[12.5px]">
          <input
            type="checkbox"
            data-member-policy-verification-toggle
            checked={verification ?? true}
            onChange={(event) => setVerification(event.target.checked)}
            className="mt-0.5"
          />
          <span>
            Require a confirmation link
            <span className="block text-muted">
              A signup stays unusable until the address is confirmed. Turning this off makes every
              signup immediately usable — anyone who knows an address can claim it.
            </span>
          </span>
        </label>

        <div className="space-y-1.5">
          <label htmlFor="member-policy-roles" className="block text-[12.5px]">
            Roles every new member gets
            <span className="block text-muted">
              Comma separated. The site&apos;s own names, not panel roles.
            </span>
          </label>
          <input
            id="member-policy-roles"
            data-member-policy-default-roles
            value={roles ?? ""}
            onChange={(event) => setRoles(event.target.value)}
            className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
        </div>

        <div className="space-y-1.5">
          <label htmlFor="member-policy-redirect" className="block text-[12.5px]">
            Where a member lands after signing in
            <span className="block text-muted">
              A path on this site, like <code>/account</code>. Empty means the site decides.
            </span>
          </label>
          <input
            id="member-policy-redirect"
            data-member-policy-redirect
            value={redirect ?? ""}
            onChange={(event) => setRedirect(event.target.value)}
            className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
        </div>
      </div>

      <fieldset className="space-y-2 rounded-md border border-line p-3">
        <legend className="px-1 text-[12.5px]">What a gated page answers to somebody who cannot read it</legend>
        {(
          [
            {
              key: "not_found",
              title: "404 — as if the page did not exist",
              body: "The visitor cannot tell a members area from a typo. This is the default, and the choice a site makes by not making one.",
            },
            {
              key: "prompt",
              title: "401 — offer a sign-in link",
              body: "Useful when you want visitors to know a members area exists. It also tells every stranger exactly which URLs are worth a password.",
            },
          ] as const
        ).map((option) => (
          <label key={option.key} className="flex items-start gap-2 text-[12.5px]">
            <input
              type="radio"
              name="member-gated-behaviour"
              data-member-policy-behaviour-option={option.key}
              checked={behaviour === option.key}
              onChange={() => setBehaviour(option.key)}
              className="mt-0.5"
            />
            <span>
              <span className="block font-medium">{option.title}</span>
              <span className="block text-muted">{option.body}</span>
            </span>
          </label>
        ))}
      </fieldset>
    </section>
  );
}

/** An error with a retry, because every read on this screen can fail. */
function ErrorStrip({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div
      role="alert"
      data-members-error
      className="flex flex-wrap items-center gap-2 rounded-md border border-danger/40 bg-danger/5 px-3 py-2 text-[12.5px] text-danger"
    >
      <AlertTriangle className="h-3.5 w-3.5 shrink-0" aria-hidden />
      <span className="flex-1">{message}</span>
      <button type="button" onClick={onRetry} className="rounded-md border border-line px-2 py-1">
        Retry
      </button>
    </div>
  );
}
