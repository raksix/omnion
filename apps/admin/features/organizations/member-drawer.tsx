"use client";

/**
 * The member drawer (REQ-005, slice 4).
 *
 * The Members tab lists; this is where an administrator *acts on one person*. The REQ names what
 * it carries — identity, membership status, role bindings with scope and expiry, department
 * memberships and that member's recent audit — and the QA plan opens it and extends a binding.
 *
 * Four decisions the screen depends on, each of which a different implementation would get
 * silently wrong:
 *
 * * **One read.** The whole panel comes from one request. Four parallel requests would let the
 *   identity render while the binding list was still loading, and an empty list under a name
 *   reads as "this person holds nothing" — which is a statement about a colleague, not a
 *   statement about a spinner.
 * * **A revoked grant stays on screen, marked.** Deleting the row would make the drawer disagree
 *   with the tenant's Audit tab, which records the revocation. The row is the history.
 * * **Extend edits the expiry in place.** The alternative — revoke and re-grant — passes every
 *   assertion a person can make by eye and leaves two rows for one role, which the effective
 *   permissions screen then renders twice.
 * * **The mobile width is a phone, not a narrow desktop.** Below `sm` the drawer is full-screen
 *   and its action buttons sit in a row of their own at the bottom, because a five-button toolbar
 *   in a 390px panel is a toolbar nobody can hit.
 */
import { useCallback, useEffect, useState } from "react";

import {
  AlertTriangle,
  CalendarClock,
  Loader2,
  Plus,
  RefreshCw,
  ShieldOff,
  X,
} from "lucide-react";

import {
  ApiError,
  extendMemberRole,
  fetchOrganizationMember,
  fetchRoles,
  grantMemberRole,
  revokeMemberRole,
  type IamRole,
  type MemberBinding,
  type OrganizationMemberDetail,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { Organization } from "@/lib/types";

/** A loadable panel: never a spinner over rows that are already there. */
type DrawerState = "loading" | "ready" | "error";

/**
 * How a grant is worded on screen.
 *
 * One sentence per scope rather than a raw `scope_type`, because "organization" is a storage
 * word and "the whole of this organization" is what an administrator is about to hand somebody.
 */
function describeScope(binding: MemberBinding): string {
  if (binding.department) return `Department ${binding.department}`;
  if (binding.module) return `Module ${binding.module}`;
  if (binding.resource_id) return `Resource ${binding.resource_id}`;
  if (binding.site_id) return "One site";
  if (binding.scope_type === "global") return "Everywhere (platform)";
  return "This whole organization";
}

/** One grant row, with the three operations the REQ names. */
function BindingRow({
  binding,
  busy,
  onExtend,
  onRevoke,
}: {
  binding: MemberBinding;
  busy: boolean;
  onExtend: (binding: MemberBinding) => void;
  onRevoke: (binding: MemberBinding) => void;
}) {
  // A permanent grant has no date to extend, so the control is absent rather than disabled with
  // no reason — a control that can never succeed is a dead control.
  const temporary = binding.expires_at !== null;

  return (
    <li
      data-member-binding={binding.id}
      className="flex flex-col gap-2 rounded-lg border border-line px-3 py-2.5"
    >
      <div className="flex flex-wrap items-start justify-between gap-2">
        <span className="flex min-w-0 flex-col">
          <span className="truncate text-[13px] font-medium">{binding.role_name}</span>
          <span className="truncate text-[12px] text-muted">{describeScope(binding)}</span>
        </span>
        <span
          className={
            binding.active
              ? "rounded-full bg-accent-soft px-2 py-0.5 text-[11px] font-medium text-accent-strong"
              : "rounded-full border border-line px-2 py-0.5 text-[11px] font-medium text-muted"
          }
        >
          {binding.active ? "Active" : binding.expired ? "Expired" : "Revoked"}
        </span>
      </div>

      {temporary ? (
        <span className="flex items-center gap-1.5 text-[12px] text-muted">
          <CalendarClock className="size-3.5 shrink-0" aria-hidden />
          {binding.active ? "Ends" : "Ended"}{" "}
          {formatTimestamp(binding.expires_at as string)}
        </span>
      ) : null}

      {binding.active ? (
        <div className="flex flex-wrap gap-1.5">
          {temporary ? (
            <button
              type="button"
              data-qa-guard="write"
              data-member-binding-extend={binding.id}
              onClick={() => onExtend(binding)}
              disabled={busy}
              className="rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-60"
            >
              Extend…
            </button>
          ) : null}
          <button
            type="button"
            data-qa-guard="write"
            data-member-binding-revoke={binding.id}
            onClick={() => onRevoke(binding)}
            disabled={busy}
            className="flex items-center gap-1 rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-60"
          >
            <ShieldOff className="size-3" aria-hidden />
            Revoke
          </button>
        </div>
      ) : (
        <p className="text-[12px] text-muted">
          Granted {formatTimestamp(binding.created_at)} and{" "}
          {binding.expired
            ? "ran out"
            : `revoked ${binding.revoked_at ? formatTimestamp(binding.revoked_at) : "later"}`}
          . The row stays here because the tenant's audit trail records it.
        </p>
      )}
    </li>
  );
}

/** The extend form: a new end date for a temporary grant, with the reason on screen if refused. */
function ExtendForm({
  binding,
  busy,
  error,
  onSubmit,
  onCancel,
}: {
  binding: MemberBinding;
  busy: boolean;
  error: string | null;
  onSubmit: (iso: string) => void;
  onCancel: () => void;
}) {
  // A date input speaks `YYYY-MM-DD`; the API wants an RFC 3339 instant. Midnight UTC is the
  // conversion, and it is said out loud in the label so nobody reads "ends 3 March" as an
  // all-day event that expires at the end of it.
  const [value, setValue] = useState(() => {
    const current = binding.expires_at ? new Date(binding.expires_at) : new Date();
    return current.toISOString().slice(0, 10);
  });
  const invalid = !/^\d{4}-\d{2}-\d{2}$/.test(value);

  return (
    <div className="flex flex-col gap-2 rounded-lg border border-line bg-canvas/60 px-3 py-2.5">
      <label className="flex flex-col gap-1 text-[12.5px] font-medium">
        New end date for {binding.role_name}
        <input
          type="date"
          data-member-extend-date="1"
          value={value}
          onChange={(event) => setValue(event.target.value)}
          className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[13px] font-normal outline-none focus:border-accent"
        />
        <span className="text-[11.5px] font-normal text-muted">
          The grant runs until 00:00 UTC on this date. The same row is updated — nothing is revoked
          and re-created.
        </span>
      </label>
      {error ? (
        <p role="alert" className="text-[12px] text-accent-strong">
          {error}
        </p>
      ) : null}
      <div className="flex gap-2">
        <button
          type="button"
          data-qa-guard="write"
          data-member-extend-submit="1"
          onClick={() => onSubmit(`${value}T00:00:00Z`)}
          disabled={busy || invalid}
          className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-60"
        >
          {busy ? "Extending…" : "Extend grant"}
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
        >
          Cancel
        </button>
      </div>
    </div>
  );
}

/** The grant form: a role from this organization, granted at organization scope by default. */
function GrantForm({
  roles,
  busy,
  error,
  onSubmit,
  onCancel,
}: {
  roles: IamRole[];
  busy: boolean;
  error: string | null;
  onSubmit: (roleId: string) => void;
  onCancel: () => void;
}) {
  const [roleId, setRoleId] = useState("");

  return (
    <div className="flex flex-col gap-2 rounded-lg border border-line bg-canvas/60 px-3 py-2.5">
      <label className="flex flex-col gap-1 text-[12.5px] font-medium">
        Role to grant
        <select
          value={roleId}
          data-member-grant-role="1"
          onChange={(event) => setRoleId(event.target.value)}
          className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[13px] font-normal outline-none"
        >
          <option value="">Choose a role…</option>
          {roles.map((role) => (
            <option key={role.id} value={role.id}>
              {role.name}
            </option>
          ))}
        </select>
      </label>
      <p className="text-[11.5px] text-muted">
        Granted for this whole organization. Narrower scopes — a single site, a department — are
        managed on the Departments tab, where the people who receive them are listed.
      </p>
      {error ? (
        <p role="alert" className="text-[12px] text-accent-strong">
          {error}
        </p>
      ) : null}
      <div className="flex gap-2">
        <button
          type="button"
          data-qa-guard="write"
          data-member-grant-submit="1"
          onClick={() => onSubmit(roleId)}
          disabled={busy || !roleId}
          className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-60"
        >
          {busy ? "Granting…" : "Grant role"}
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
        >
          Cancel
        </button>
      </div>
    </div>
  );
}

/** The drawer itself. */
export function MemberDrawer({
  organization,
  memberId,
  memberName,
  onClose,
  onChanged,
}: {
  organization: Organization;
  memberId: string;
  /** Shown while the panel loads, so the heading is never blank. */
  memberName: string;
  onClose: () => void;
  /** Tell the Members tab to reload — a granted role changes the chip on the row behind. */
  onChanged: () => void;
}) {
  const [detail, setDetail] = useState<OrganizationMemberDetail | null>(null);
  const [state, setState] = useState<DrawerState>("loading");
  const [error, setError] = useState<string | null>(null);
  const [roles, setRoles] = useState<IamRole[]>([]);
  const [granting, setGranting] = useState(false);
  const [extending, setExtending] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    setState("loading");
    setError(null);
    try {
      setDetail(await fetchOrganizationMember(organization.id, memberId));
      setState("ready");
    } catch (cause) {
      setState("error");
      setError(
        cause instanceof ApiError ? cause.message : "This member could not be loaded.",
      );
    }
  }, [organization.id, memberId]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    let cancelled = false;
    fetchRoles(organization.id)
      .then((list) => {
        if (!cancelled) setRoles(list);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [organization.id]);

  // `Esc` closes, and the backdrop closes — the two things a person reaching for without
  // reading. The tab itself does not trap focus: a drawer that steals Tab away from a form
  // behind it is worse than one that lets a keyboard user out through the backdrop.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const guard = async (run: () => Promise<string>) => {
    setBusy(true);
    setActionError(null);
    setNotice(null);
    try {
      setNotice(await run());
      await load();
      onChanged();
    } catch (cause) {
      setActionError(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The change was refused.",
      );
    } finally {
      setBusy(false);
    }
  };

  const grant = (roleId: string) =>
    guard(async () => {
      const binding = await grantMemberRole(organization.id, memberId, { role_id: roleId });
      setGranting(false);
      return `Granted ${binding.role_name} for this organization.`;
    });

  const extend = (binding: MemberBinding, iso: string) =>
    guard(async () => {
      const updated = await extendMemberRole(organization.id, memberId, binding.id, iso);
      setExtending(null);
      return `Extended ${updated.role_name} until ${formatTimestamp(updated.expires_at ?? iso)}.`;
    });

  const revoke = (binding: MemberBinding) =>
    guard(async () => {
      await revokeMemberRole(organization.id, memberId, binding.id);
      return `Revoked ${binding.role_name}. The row stays in the list and in the audit trail.`;
    });

  const live = detail?.bindings.filter((binding) => binding.active) ?? [];
  const past = detail?.bindings.filter((binding) => !binding.active) ?? [];

  return (
    <div className="fixed inset-0 z-50 flex justify-end">
      <button
        type="button"
        aria-label="Close the member drawer"
        onClick={onClose}
        className="absolute inset-0 bg-ink/40"
      />
      <aside
        role="dialog"
        aria-label={`Member ${detail?.display_name || memberName}`}
        data-member-drawer="1"
        // On a phone the drawer takes the whole screen: a 512px panel pushed to the right edge
        // of a 390px viewport leaves the reader a sliver to peek past and every control on the
        // far side out of reach. Below `sm` it is a full-height sheet; from `sm` up it is the
        // side panel, which is the only place it fits.
        className="relative flex h-full w-full flex-col gap-4 overflow-y-auto bg-surface p-4 shadow-xl sm:max-w-lg sm:border-l sm:border-line sm:p-5"
      >
        <div className="flex items-start justify-between gap-3">
          <div className="flex min-w-0 flex-col gap-1">
            <h2 className="truncate text-[14px] font-semibold">
              {detail?.display_name || memberName}
            </h2>
            <span className="truncate text-[12px] text-muted">{detail?.email ?? "…"}</span>
          </div>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="shrink-0 rounded-lg p-1.5 text-muted hover:bg-quiet-soft hover:text-ink"
          >
            <X className="size-4" aria-hidden />
          </button>
        </div>

        {state === "loading" ? (
          <p className="flex items-center gap-2 text-[12.5px] text-muted">
            <Loader2 className="size-3.5 animate-spin" aria-hidden />
            Loading this member…
          </p>
        ) : null}

        {state === "error" ? (
          <div className="flex flex-col items-start gap-2 rounded-lg border border-line px-3 py-3">
            <p role="alert" className="text-[12.5px] text-accent-strong">
              {error}
            </p>
            <button
              type="button"
              onClick={() => void load()}
              className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas"
            >
              <RefreshCw className="size-3" aria-hidden />
              Try again
            </button>
          </div>
        ) : null}

        {notice ? (
          <p role="status" className="rounded-lg border border-line px-3 py-2 text-[12.5px]">
            {notice}
          </p>
        ) : null}

        {state === "ready" && detail ? (
          <>
            {/* Identity and membership: the facts that do not change while the drawer is open. */}
            <section
              data-member-identity="1"
              className="grid grid-cols-2 gap-2 rounded-lg border border-line px-3 py-2.5 text-[12px] sm:grid-cols-4"
            >
              <span className="flex flex-col">
                <span className="text-muted">Membership</span>
                <span className="font-medium">{detail.status}</span>
              </span>
              <span className="flex flex-col">
                <span className="text-muted">Account</span>
                <span className="font-medium">{detail.user_status}</span>
              </span>
              <span className="flex flex-col">
                <span className="text-muted">Joined</span>
                <span className="font-medium">
                  {detail.joined_at ? formatTimestamp(detail.joined_at) : "—"}
                </span>
              </span>
              <span className="flex flex-col">
                <span className="text-muted">Last active</span>
                <span className="font-medium">
                  {detail.last_active_at ? formatTimestamp(detail.last_active_at) : "Never"}
                </span>
              </span>
            </section>

            {detail.departments.length > 0 ? (
              <section className="flex flex-col gap-1.5">
                <h3 className="text-[12.5px] font-semibold">Departments</h3>
                <ul data-member-departments="1" className="flex flex-wrap gap-1.5">
                  {detail.departments.map((department) => (
                    <li
                      key={department.id}
                      className="rounded-md border border-line bg-canvas px-2 py-0.5 text-[11.5px] font-medium text-muted"
                    >
                      {department.name}
                    </li>
                  ))}
                </ul>
              </section>
            ) : null}

            {/* The bindings. */}
            <section className="flex flex-col gap-2">
              <div className="flex items-center justify-between gap-2">
                <h3 className="text-[12.5px] font-semibold">
                  Role bindings
                  <span className="ml-1.5 font-normal text-muted">
                    {live.length} active
                    {past.length > 0 ? `, ${past.length} past` : ""}
                  </span>
                </h3>
                <button
                  type="button"
                  data-qa-guard="write"
                  data-member-grant-open="1"
                  onClick={() => {
                    setGranting((open) => !open);
                    setExtending(null);
                    setActionError(null);
                  }}
                  aria-expanded={granting}
                  className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12px] transition hover:bg-canvas"
                >
                  <Plus className="size-3.5" aria-hidden />
                  Grant
                </button>
              </div>

              {granting ? (
                <GrantForm
                  roles={roles}
                  busy={busy}
                  error={actionError}
                  onSubmit={(roleId) => void grant(roleId)}
                  onCancel={() => setGranting(false)}
                />
              ) : null}

              {detail.bindings.length === 0 ? (
                <p
                  data-member-bindings-empty="1"
                  className="rounded-lg border border-dashed border-line px-3 py-2.5 text-[12px] text-muted"
                >
                  This member holds no role in this organization — they can read what the
                  organization shares and change nothing. Grant a role above to give them more.
                </p>
              ) : (
                <ul className="flex flex-col gap-2">
                  {detail.bindings.map((binding) => (
                    <div key={binding.id} className="flex flex-col gap-2">
                      <BindingRow
                        binding={binding}
                        busy={busy}
                        onExtend={(target) => {
                          setExtending((current) =>
                            current === target.id ? null : target.id,
                          );
                          setGranting(false);
                          setActionError(null);
                        }}
                        onRevoke={(target) => void revoke(target)}
                      />
                      {extending === binding.id ? (
                        <ExtendForm
                          binding={binding}
                          busy={busy}
                          error={actionError}
                          onSubmit={(iso) => void extend(binding, iso)}
                          onCancel={() => setExtending(null)}
                        />
                      ) : null}
                    </div>
                  ))}
                </ul>
              )}
            </section>

            {/* The member's own trail in this tenant. */}
            <section className="flex flex-col gap-1.5">
              <h3 className="text-[12.5px] font-semibold">Recent activity</h3>
              {detail.recent_audit.length === 0 ? (
                <p
                  data-member-audit-empty="1"
                  className="rounded-lg border border-dashed border-line px-3 py-2.5 text-[12px] text-muted"
                >
                  Nothing yet. Rows appear here as this person acts inside the organization.
                </p>
              ) : (
                <ul data-member-audit="1" className="flex flex-col gap-1.5">
                  {detail.recent_audit.map((row, index) => (
                    <li
                      key={`${row.action}-${row.created_at}-${index}`}
                      className="flex flex-wrap items-baseline justify-between gap-2 border-b border-line pb-1.5 text-[12px] last:border-0"
                    >
                      <span className="flex min-w-0 flex-col">
                        <span className="font-medium">{row.action}</span>
                        <span className="text-muted">{row.actor_name}</span>
                      </span>
                      <span className="shrink-0 text-[11.5px] text-muted">
                        {formatTimestamp(row.created_at)}
                      </span>
                    </li>
                  ))}
                </ul>
              )}
            </section>

            {organization.status !== "active" ? (
              <p className="flex items-start gap-2 rounded-lg border border-line bg-canvas/60 px-3 py-2 text-[12px] text-muted">
                <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
                This organization is {organization.status}, so the API refuses every change on this
                screen. Reactivate the organization to grant or revoke again.
              </p>
            ) : null}
          </>
        ) : null}
      </aside>
    </div>
  );
}
