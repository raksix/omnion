"use client";

/**
 * `/settings/iam/sessions` — the live sessions of an organization (REQ-006, slice 3).
 *
 * A row is one sign-in: who, from where, on which device, how they authenticated, when they last
 * moved and when the policy ends them. The state badge comes from the same values the resolver
 * reads, so "idle" here means the session will be refused on its next request.
 *
 * Revoking one session is a single click; signing every session of an account out is the row's
 * second action, and the bulk bar does the same for a selection.
 */
import { useCallback, useEffect, useState } from "react";

import { Ban, LogOut, RefreshCw, Search, ShieldOff } from "lucide-react";

import { useSession } from "@/lib/session";
import {
  ApiError,
  fetchIamSessions,
  fetchOrganizations,
  revokeIamSession,
  signOutAllSessions,
  type IamSession,
} from "@/lib/api";
import type { Organization } from "@/lib/types";

/** How a state reads. */
const STATE_LABEL: Record<string, string> = {
  live: "Live",
  idle: "Idle",
  expired: "Expired",
  revoked: "Revoked",
};

/** Badge classes per state. */
const STATE_CLASS: Record<string, string> = {
  live: "border-emerald-500/40 bg-emerald-500/10 text-emerald-700",
  idle: "border-amber-500/40 bg-amber-500/10 text-amber-700",
  expired: "border-line bg-panel text-muted",
  revoked: "border-danger/40 bg-danger-soft text-caution",
};

/** `/settings/iam/sessions`. */
export function SessionsView() {
  const { user } = useSession();
  const [organizations, setOrganizations] = useState<Organization[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string | null>(null);
  const [sessions, setSessions] = useState<IamSession[]>([]);
  const [idleMinutes, setIdleMinutes] = useState(120);
  const [search, setSearch] = useState("");
  const [state, setState] = useState("");
  const [includeInactive, setIncludeInactive] = useState(false);
  const [selected, setSelected] = useState<Record<string, boolean>>({});
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const platformAccount = user ? user.organization_id === null : false;
  // A platform account starts on every organization: its own accounts carry no organization of
  // their own, so defaulting to the first tenant would show it an empty list.
  const activeOrg = platformAccount ? (selectedOrg ?? "") : (user?.organization_id ?? null);

  const load = useCallback(
    async (organizationId: string | null, query: { search: string; state: string; includeInactive: boolean }) => {
      setStatus("loading");
      setLoadError(null);
      try {
        const body = await fetchIamSessions({
          organizationId,
          search: query.search || undefined,
          state: query.state || undefined,
          includeInactive: query.includeInactive,
        });
        setSessions(body.sessions);
        setIdleMinutes(body.idle_minutes);
        setStatus("ready");
      } catch (cause) {
        setStatus("error");
        setLoadError(
          cause instanceof ApiError
            ? { code: cause.code, message: cause.message }
            : { code: "unknown_error", message: "The sessions could not be read." },
        );
      }
    },
    [],
  );

  useEffect(() => {
    if (!user) return;
    if (user.organization_id !== null) {
      void load(null, { search: "", state: "", includeInactive: false });
      return;
    }
    // A platform account: every organization at once (the picker narrows it afterwards).
    void load(null, { search: "", state: "", includeInactive: false });
    if (organizations !== null) return;
    void fetchOrganizations()
      .then((list) => {
        setOrganizations(list);
        setSelectedOrg("");
      })
      .catch(() => setOrganizations([]));
  }, [user, organizations, load]);

  useEffect(() => {
    if (!platformAccount || !selectedOrg) return;
    void load(selectedOrg, { search, state, includeInactive });
    // The filters are the effect's inputs; the load is not re-created on every keystroke.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [platformAccount, selectedOrg]);

  const applyFilters = () => {
    void load(activeOrg, { search, state, includeInactive });
  };

  const revoke = async (session: IamSession) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await revokeIamSession(session.id);
      setNotice(`Revoked the session of ${session.user_email}.`);
      await load(activeOrg, { search, state, includeInactive });
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The session could not be revoked.");
    } finally {
      setBusy(false);
    }
  };

  const revokeSelected = async () => {
    const targets = sessions.filter((session) => selected[session.id] && session.revocable);
    if (targets.length === 0) {
      setError("Select at least one live session first.");
      return;
    }
    setBusy(true);
    setError(null);
    setNotice(null);
    let revoked = 0;
    try {
      for (const target of targets) {
        await revokeIamSession(target.id);
        revoked += 1;
      }
      setSelected({});
      setNotice(`Revoked ${revoked} session${revoked === 1 ? "" : "s"}.`);
      await load(activeOrg, { search, state, includeInactive });
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The sessions could not be revoked.");
    } finally {
      setBusy(false);
    }
  };

  const signOutAll = async (session: IamSession) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const body = await signOutAllSessions(session.user_id);
      setNotice(
        `Signed ${session.user_email} out of ${body.revoked} session${body.revoked === 1 ? "" : "s"}.`,
      );
      await load(activeOrg, { search, state, includeInactive });
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The sessions could not be revoked.");
    } finally {
      setBusy(false);
    }
  };

  const selectedCount = sessions.filter((session) => selected[session.id] && session.revocable).length;

  return (
    <div className="flex flex-col gap-4">
      <form
        className="flex flex-wrap items-end gap-2 rounded-xl border border-line bg-surface p-3"
        onSubmit={(event) => {
          event.preventDefault();
          applyFilters();
        }}
      >
        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] text-muted">Search</span>
          <span className="flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-2">
            <Search className="size-3.5 text-muted" aria-hidden />
            <input
              value={search}
              data-sessions-search
              onChange={(event) => setSearch(event.target.value)}
              placeholder="qa-user@example.com"
              className="w-48 bg-transparent text-[12.5px] text-ink outline-none"
            />
          </span>
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] text-muted">State</span>
          <select
            value={state}
            data-session-state
            onChange={(event) => setState(event.target.value)}
            className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          >
            <option value="">Any state</option>
            <option value="live">Live</option>
            <option value="idle">Idle</option>
            <option value="expired">Expired</option>
            <option value="revoked">Revoked</option>
          </select>
        </label>
        <label className="flex items-center gap-2 pb-1.5 text-[12.5px] text-muted">
          <input
            type="checkbox"
            checked={includeInactive}
            data-session-inactive
            onChange={(event) => setIncludeInactive(event.target.checked)}
            className="size-4 rounded border-line"
          />
          Include ended sessions
        </label>
        <button
          type="submit"
          className="h-8 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
        >
          Apply
        </button>
        <button
          type="button"
          onClick={() => {
            setSearch("");
            setState("");
            setIncludeInactive(false);
            void load(activeOrg, { search: "", state: "", includeInactive: false });
          }}
          className="h-8 rounded-lg border border-line px-3 text-[12.5px] text-ink transition hover:bg-panel"
        >
          Clear
        </button>
        <button
          type="button"
          onClick={() => void load(activeOrg, { search, state, includeInactive })}
          className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px] text-ink transition hover:bg-panel"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Reload
        </button>
        {platformAccount ? (
          <label className="flex flex-col gap-1">
            <span className="text-[11.5px] text-muted">Organization</span>
            <select
              value={selectedOrg ?? ""}
              data-sessions-org
              onChange={(event) => setSelectedOrg(event.target.value)}
              className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            >
              <option value="">All organizations</option>
              {(organizations ?? []).map((organization) => (
                <option key={organization.id} value={organization.id}>
                  {organization.name}
                </option>
              ))}
            </select>
          </label>
        ) : null}
        <span className="ml-auto pb-1.5 text-[11.5px] text-muted">
          Idle window {idleMinutes} minutes — a session untouched longer than that is refused.
        </span>
      </form>

      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          onClick={() => void revokeSelected()}
          disabled={busy || selectedCount === 0}
          data-sessions-bulk
          className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-ink transition hover:bg-panel disabled:opacity-50"
        >
          <ShieldOff className="size-3.5" aria-hidden />
          Revoke selected ({selectedCount})
        </button>
        {notice ? (
          <span data-sessions-notice className="text-[12.5px] text-muted">
            {notice}
          </span>
        ) : null}
      </div>

      {error ? (
        <p
          role="alert"
          data-sessions-error
          className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
        >
          {error}
        </p>
      ) : null}

      {status === "loading" ? (
        <div className="flex flex-col gap-2" data-sessions-loading>
          {[0, 1, 2].map((index) => (
            <div key={index} className="h-11 animate-pulse rounded-lg bg-panel" />
          ))}
        </div>
      ) : null}

      {status === "error" && loadError ? (
        <div
          role="alert"
          data-sessions-load-error
          className="flex flex-col gap-2 rounded-xl border border-danger/40 bg-danger-soft p-4 text-[12.5px] text-caution"
        >
          <span>{loadError.message}</span>
          <button
            type="button"
            onClick={() => void load(activeOrg, { search, state, includeInactive })}
            className="w-fit rounded-lg border border-caution/40 px-3 py-1 text-[12px]"
          >
            Try again
          </button>
        </div>
      ) : null}

      {status === "ready" && sessions.length === 0 ? (
        <div
          data-sessions-empty
          className="flex flex-col items-center gap-2 rounded-xl border border-dashed border-line bg-surface p-8 text-center"
        >
          <LogOut className="size-5 text-muted" aria-hidden />
          <p className="text-[13px] font-medium text-ink">No sessions match the filter</p>
          <p className="max-w-md text-[12px] text-muted">
            Every sign-in appears here as soon as it happens. An empty list with no filter means
            nobody is signed in — the state filter is the usual reason a session is missing.
          </p>
        </div>
      ) : null}

      {status === "ready" && sessions.length > 0 ? (
        <>
          <div className="hidden overflow-x-auto rounded-xl border border-line bg-surface lg:block">
            <table className="w-full min-w-[900px] text-left text-[12.5px]">
              <thead className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                <tr>
                  <th className="w-8 px-3 py-2" />
                  <th className="px-3 py-2">Account</th>
                  <th className="px-3 py-2">Address</th>
                  <th className="px-3 py-2">Device</th>
                  <th className="px-3 py-2">Auth</th>
                  <th className="px-3 py-2">Last seen</th>
                  <th className="px-3 py-2">Expires</th>
                  <th className="px-3 py-2">State</th>
                  <th className="px-3 py-2" />
                </tr>
              </thead>
              <tbody>
                {sessions.map((session) => (
                  <tr key={session.id} data-session-row className="border-b border-line/60 last:border-0">
                    <td className="px-3 py-2">
                      <input
                        type="checkbox"
                        checked={Boolean(selected[session.id])}
                        disabled={!session.revocable}
                        aria-label={`Select the session of ${session.user_email}`}
                        onChange={(event) =>
                          setSelected((current) => ({ ...current, [session.id]: event.target.checked }))
                        }
                        className="size-4 rounded border-line"
                      />
                    </td>
                    <td className="px-3 py-2">
                      <span className="block text-ink">{session.user_email}</span>
                      <span className="text-[11.5px] text-muted">{session.user_display_name}</span>
                    </td>
                    <td className="px-3 py-2 font-mono text-[11.5px] text-muted">
                      {session.ip_address ?? "—"}
                    </td>
                    <td className="px-3 py-2 text-muted">{session.device_label ?? "unknown device"}</td>
                    <td className="px-3 py-2 text-muted">
                      {session.auth_methods.length > 0 ? session.auth_methods.join(" + ") : "—"}
                    </td>
                    <td className="px-3 py-2 text-muted">
                      {session.last_seen_at ? new Date(session.last_seen_at).toLocaleString() : "never"}
                    </td>
                    <td className="px-3 py-2 text-muted">
                      {new Date(session.expires_at).toLocaleDateString()}
                    </td>
                    <td className="px-3 py-2">
                      <span
                        className={`rounded-full border px-2 py-0.5 text-[11px] ${
                          STATE_CLASS[session.state] ?? "border-line bg-panel text-muted"
                        }`}
                      >
                        {STATE_LABEL[session.state] ?? session.state}
                        {session.current ? " · this one" : ""}
                      </span>
                    </td>
                    <td className="px-3 py-2">
                      <div className="flex items-center justify-end gap-1.5">
                        {/* `revocable` only says "not revoked yet" — the server refuses to end the
                            session you are calling from (409 `cannot_revoke_current_session`),
                            because the row is stamped before the response is written and the
                            operator loses the sign-in mid-click. Offering the button there is a
                            control that cannot work, so the row that says `current` shows
                            "Sign out all" alone. */}
                        {session.revocable && !session.current ? (
                          <button
                            type="button"
                            disabled={busy}
                            data-session-revoke
                            data-qa-guard="session-revoke"
                            aria-label={`Revoke the session of ${session.user_email}`}
                            onClick={() => void revoke(session)}
                            className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-ink transition hover:bg-panel"
                          >
                            <Ban className="size-3.5" aria-hidden />
                            Revoke
                          </button>
                        ) : null}
                        <button
                          type="button"
                          disabled={busy}
                          data-session-signout-all
                          data-qa-guard="sign-out-all"
                          aria-label={`Sign ${session.user_email} out everywhere`}
                          onClick={() => void signOutAll(session)}
                          className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted transition hover:bg-panel"
                        >
                          Sign out all
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <ul className="flex flex-col gap-2 lg:hidden" data-session-cards>
            {sessions.map((session) => (
              <li
                key={session.id}
                data-session-row
                className="flex flex-col gap-1.5 rounded-xl border border-line bg-surface p-3 text-[12.5px]"
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="font-medium text-ink">{session.user_email}</span>
                  <span
                    className={`rounded-full border px-2 py-0.5 text-[11px] ${
                      STATE_CLASS[session.state] ?? "border-line bg-panel text-muted"
                    }`}
                  >
                    {STATE_LABEL[session.state] ?? session.state}
                  </span>
                </div>
                <span className="text-[11.5px] text-muted">
                  {session.device_label ?? "unknown device"} · {session.ip_address ?? "no address"} ·{" "}
                  {session.auth_methods.join(" + ") || "—"}
                </span>
                <span className="text-[11.5px] text-muted">
                  Last seen{" "}
                  {session.last_seen_at ? new Date(session.last_seen_at).toLocaleString() : "never"}
                </span>
                <div className="flex items-center gap-2">
                  {session.revocable && !session.current ? (
                    <button
                      type="button"
                      disabled={busy}
                      data-session-revoke
                      data-qa-guard="session-revoke"
                      onClick={() => void revoke(session)}
                      className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-ink"
                    >
                      Revoke
                    </button>
                  ) : null}
                  <button
                    type="button"
                    disabled={busy}
                    data-session-signout-all
                    data-qa-guard="sign-out-all"
                    onClick={() => void signOutAll(session)}
                    className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted"
                  >
                    Sign out all
                  </button>
                </div>
              </li>
            ))}
          </ul>
        </>
      ) : null}
    </div>
  );
}
