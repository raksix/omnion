"use client";

/**
 * `/settings/iam/approvals` — the permission request inbox (REQ-006, slice 4b).
 *
 * Two halves on one screen, because they are two ends of one conversation: the **inbox** an
 * approver works through (pending first, counts per tab), and the **ask** form anybody can use
 * to request a permission for a window. An approval is not a checkbox — it names the minutes it
 * grants, and the screen shows the moment it runs out.
 */
import { useCallback, useEffect, useState } from "react";

import { Check, Clock, Inbox, RefreshCw, Send, X } from "lucide-react";

import { useSession } from "@/lib/session";
import {
  ApiError,
  createIamRequest,
  decideIamApproval,
  fetchIamApprovals,
  fetchMyIamRequests,
  fetchOrganizations,
  type IamApprovalCounts,
  type IamApprovalRequest,
} from "@/lib/api";

/** The windows the approve dialog offers, in minutes. */
const WINDOWS: { label: string; minutes: number }[] = [
  { label: "30 minutes", minutes: 30 },
  { label: "1 hour", minutes: 60 },
  { label: "8 hours", minutes: 480 },
  { label: "24 hours", minutes: 1440 },
  { label: "7 days", minutes: 10080 },
  { label: "30 days", minutes: 43200 },
];

const TABS = ["pending", "approved", "rejected", "expired", "all"] as const;

/**
 * Shape of a permission key (`iam.policies.read`): the field refuses anything else before the
 * request leaves the browser, so a mistyped ask never becomes a `400` in the console — the API's
 * own refusals (an unknown key, an unusable window) are proven over HTTP in
 * `apps/api/tests/iam_approvals.rs`.
 */
const KEY_SHAPE = /^[a-z][a-z0-9_-]*(\.[a-z0-9_*-]+)*$/;

/** `true` when the API refused because the caller may not read the inbox. */
function isForbidden(cause: unknown): boolean {
  return cause instanceof ApiError && cause.status === 403;
}

function statusBadge(status: IamApprovalRequest["status"]) {
  const styles: Record<IamApprovalRequest["status"], string> = {
    pending: "border-amber-500/40 bg-amber-500/10 text-amber-700",
    approved: "border-emerald-500/40 bg-emerald-500/10 text-emerald-700",
    rejected: "border-line bg-panel text-muted",
    expired: "border-line bg-panel text-muted",
  };
  return `rounded-full border px-2 py-0.5 text-[11px] ${styles[status]}`;
}

/** `/settings/iam/approvals`. */
export function ApprovalsView() {
  const { user } = useSession();
  const [tab, setTab] = useState<(typeof TABS)[number]>("pending");
  const [requests, setRequests] = useState<IamApprovalRequest[]>([]);
  const [counts, setCounts] = useState<IamApprovalCounts | null>(null);
  const [mine, setMine] = useState<IamApprovalRequest[]>([]);
  const [mineBlocked, setMineBlocked] = useState(false);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // A platform account (the first-run owner) runs the platform, not one tenant, so it names the
  // organization it is working in; an account with its own organization never sees the picker.
  const platformAccount = user ? user.organization_id === null : false;
  const [organizations, setOrganizations] = useState<{ id: string; name: string }[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string>("");
  const activeOrg = platformAccount ? (selectedOrg || null) : (user?.organization_id ?? null);

  // The ask form.
  const [askOpen, setAskOpen] = useState(false);
  const [askPermission, setAskPermission] = useState("");
  const [askJustification, setAskJustification] = useState("");
  const [askResource, setAskResource] = useState("");

  // The approve / reject dialog.
  const [deciding, setDeciding] = useState<{ request: IamApprovalRequest; mode: "approve" | "reject" } | null>(null);
  const [windowMinutes, setWindowMinutes] = useState(480);
  const [decisionNote, setDecisionNote] = useState("");

  const load = useCallback(
    async (which: (typeof TABS)[number], organizationId: string | null) => {
      setStatus("loading");
      setLoadError(null);
      try {
        const body = await fetchIamApprovals({ status: which, organizationId });
        setRequests(body.requests);
        setCounts(body.counts);
        setStatus("ready");
      } catch (cause) {
        if (isForbidden(cause)) {
          setRequests([]);
          setStatus("ready");
          setLoadError({ code: "forbidden", message: "You do not hold `iam.approvals.read`." });
          return;
        }
        setStatus("error");
        setLoadError(
          cause instanceof ApiError
            ? { code: cause.code, message: cause.message }
            : { code: "unknown_error", message: "The inbox could not be read." },
        );
      }
    },
    [],
  );

  const loadMine = useCallback(async (organizationId: string | null) => {
    try {
      const body = await fetchMyIamRequests(organizationId);
      setMine(body.requests);
      setMineBlocked(false);
    } catch {
      setMineBlocked(true);
    }
  }, []);

  useEffect(() => {
    if (!user) return;
    if (!platformAccount) {
      void load(tab, null);
      void loadMine(null);
      return;
    }
    if (organizations === null) {
      void fetchOrganizations()
        .then((list) => {
          setOrganizations(list.map((organization) => ({ id: organization.id, name: organization.name })));
          if (list.length > 0) setSelectedOrg((current) => current || list[0].id);
        })
        .catch(() => setOrganizations([]));
      return;
    }
    if (!selectedOrg) return;
    void load(tab, selectedOrg);
  }, [user, tab, load, loadMine, platformAccount, organizations, selectedOrg]);

  useEffect(() => {
    if (!user || !platformAccount || !selectedOrg) return;
    void loadMine(selectedOrg);
  }, [user, platformAccount, selectedOrg, loadMine]);

  const submitAsk = async () => {
    const permission = askPermission.trim();
    if (!KEY_SHAPE.test(permission)) {
      setError(
        "A permission key looks like `iam.policies.read` — lowercase letters, dots and dashes.",
      );
      return;
    }

    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const resource = askResource.trim();
      const created = await createIamRequest({
        permissionKey: permission,
        justification: askJustification.trim(),
        resourceType: resource ? "path" : null,
        resourceId: resource || null,
        organizationId: activeOrg,
      });
      setNotice(
        `Requested ${created.permission_key} — it is pending until an approver decides.`,
      );
      setAskPermission("");
      setAskJustification("");
      setAskResource("");
      setAskOpen(false);
      await load(tab, activeOrg);
      await loadMine(activeOrg);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? cause.message : "The request could not be created.",
      );
    } finally {
      setBusy(false);
    }
  };

  const submitDecision = async () => {
    if (!deciding) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const result = await decideIamApproval(deciding.request.id, {
        decision: deciding.mode,
        grantMinutes: deciding.mode === "approve" ? windowMinutes : undefined,
        note: decisionNote,
      });
      setNotice(
        deciding.mode === "approve"
          ? `${result.permission_key} is granted for ${result.grant_minutes} minutes (until ${
              result.grant_expires_at ? new Date(result.grant_expires_at).toLocaleString() : "—"
            }).`
          : `${result.permission_key} was refused.`,
      );
      setDeciding(null);
      setDecisionNote("");
      await load(tab, activeOrg);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The decision could not be saved.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-col gap-4" data-approvals-view>
      <div className="flex flex-wrap items-center gap-2" data-approvals-tabs>
        {TABS.map((value) => {
          const count = counts && value !== "all" ? counts[value] : null;
          const active = tab === value;
          return (
            <button
              key={value}
              type="button"
              data-approval-tab={value}
              aria-pressed={active}
              onClick={() => setTab(value)}
              className={`flex h-8 items-center gap-1.5 rounded-lg border px-3 text-[12.5px] capitalize transition ${
                active
                  ? "border-accent bg-accent-soft font-medium text-accent-strong"
                  : "border-line bg-surface text-muted hover:text-ink"
              }`}
            >
              {value}
              {count !== null ? (
                <span className="rounded-full bg-panel px-1.5 text-[11px] text-muted">{count}</span>
              ) : null}
            </button>
          );
        })}
        {platformAccount && organizations && organizations.length > 0 ? (
          <label className="ml-auto flex flex-col">
            <span className="sr-only">Organization</span>
            <select
              value={selectedOrg}
              data-approvals-organization
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
        ) : null}
        <button
          type="button"
          data-approvals-reload
          onClick={() => {
            void load(tab, activeOrg);
            void loadMine(activeOrg);
          }}
          className="flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-[12.5px] text-ink transition hover:bg-panel"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Reload
        </button>
        <button
          type="button"
          data-request-new
          onClick={() => setAskOpen((open) => !open)}
          className="flex h-8 items-center gap-1.5 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
        >
          <Send className="size-3.5" aria-hidden />
          Request access
        </button>
      </div>

      {askOpen ? (
        <form
          data-request-form
          className="flex flex-col gap-2 rounded-xl border border-line bg-surface p-3"
          onSubmit={(event) => {
            event.preventDefault();
            void submitAsk();
          }}
        >
          <div className="flex flex-wrap gap-2">
            <label className="flex min-w-64 flex-1 flex-col gap-1">
              <span className="text-[11.5px] text-muted">Permission key</span>
              <input
                value={askPermission}
                data-request-permission
                onChange={(event) => setAskPermission(event.target.value)}
                placeholder="e.g. iam.policies.read"
                className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <label className="flex min-w-56 flex-1 flex-col gap-1">
              <span className="text-[11.5px] text-muted">Limit to a path (optional)</span>
              <input
                value={askResource}
                data-request-resource
                onChange={(event) => setAskResource(event.target.value)}
                placeholder="e.g. /blog/*"
                className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
          </div>
          <label className="flex flex-col gap-1">
            <span className="text-[11.5px] text-muted">Justification</span>
            <textarea
              value={askJustification}
              data-request-justification
              onChange={(event) => setAskJustification(event.target.value)}
              rows={2}
              placeholder="Why is the window needed?"
              className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
          </label>
          <div className="flex items-center gap-2">
            <button
              type="submit"
              data-request-submit
              disabled={busy || askPermission.trim() === ""}
              className="h-8 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
            >
              Submit request
            </button>
            <button
              type="button"
              onClick={() => setAskOpen(false)}
              className="h-8 rounded-lg border border-line px-3 text-[12.5px] text-muted"
            >
              Cancel
            </button>
          </div>
        </form>
      ) : null}

      {notice ? (
        <span data-approvals-notice className="text-[12.5px] text-muted">
          {notice}
        </span>
      ) : null}

      {error ? (
        <p
          role="alert"
          data-approvals-error
          className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
        >
          {error}
        </p>
      ) : null}

      {status === "loading" ? (
        <div className="flex flex-col gap-2" data-approvals-loading>
          {[0, 1, 2].map((index) => (
            <div key={index} className="h-12 animate-pulse rounded-lg bg-panel" />
          ))}
        </div>
      ) : null}

      {status === "error" && loadError ? (
        <div
          role="alert"
          data-approvals-load-error
          className="flex flex-col gap-2 rounded-xl border border-danger/40 bg-danger-soft p-4 text-[12.5px] text-caution"
        >
          <span>{loadError.message}</span>
          <button
            type="button"
            onClick={() => void load(tab, activeOrg)}
            className="w-fit rounded-lg border border-caution/40 px-3 py-1 text-[12px]"
          >
            Try again
          </button>
        </div>
      ) : null}

      {status === "ready" && requests.length === 0 ? (
        <div
          data-approvals-empty
          className="flex flex-col items-center gap-2 rounded-xl border border-dashed border-line bg-surface p-8 text-center"
        >
          <Inbox className="size-5 text-muted" aria-hidden />
          <p className="text-[13px] font-medium text-ink">
            {tab === "pending" ? "Nothing waiting for a decision" : `No ${tab} requests`}
          </p>
          <p className="max-w-md text-[12px] text-muted">
            A request appears here the moment somebody asks for a permission they do not hold —
            approve it with a window, or refuse it with a note.
          </p>
        </div>
      ) : null}

      {status === "ready" && requests.length > 0 ? (
        <ul className="flex flex-col gap-2" data-approval-rows>
          {requests.map((request) => (
            <li
              key={request.id}
              data-approval-row
              data-approval-status={request.status}
              className="flex flex-col gap-2 rounded-xl border border-line bg-surface p-3 text-[12.5px]"
            >
              <div className="flex flex-wrap items-center gap-2">
                <span className="font-mono text-[12px] text-ink" data-approval-permission>
                  {request.permission_key}
                </span>
                <span className={statusBadge(request.status)} data-approval-status-badge>
                  {request.status}
                </span>
                {request.resource_id ? (
                  <span className="rounded-full border border-line bg-panel px-2 py-0.5 text-[11px] text-muted">
                    on {request.resource_id}
                  </span>
                ) : null}
                {request.status === "approved" && request.grant_expires_at ? (
                  <span
                    data-approval-window
                    className="flex items-center gap-1 rounded-full border border-emerald-500/40 bg-emerald-500/10 px-2 py-0.5 text-[11px] text-emerald-700"
                  >
                    <Clock className="size-3" aria-hidden />
                    {request.grant_active ? "expires" : "ended"}{" "}
                    {new Date(request.grant_expires_at).toLocaleString()} · {request.grant_minutes}m
                  </span>
                ) : null}
                <span className="ml-auto text-[11.5px] text-muted">
                  {new Date(request.created_at).toLocaleString()}
                </span>
              </div>

              <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11.5px] text-muted">
                <span data-approval-requester>
                  {request.requester.name || request.requester.email} · {request.requester.email}
                </span>
                {request.justification ? <span>“{request.justification}”</span> : null}
                {request.decided_by ? (
                  <span>
                    decided by {request.decided_by.email ?? "—"}
                    {request.decision_note ? ` — “${request.decision_note}”` : ""}
                  </span>
                ) : null}
              </div>

              {request.status === "pending" ? (
                <div className="flex items-center gap-2">
                  <button
                    type="button"
                    disabled={busy}
                    data-approval-approve
                    onClick={() => {
                      setDeciding({ request, mode: "approve" });
                      setWindowMinutes(480);
                      setDecisionNote("");
                    }}
                    className="flex h-7 items-center gap-1 rounded-lg bg-accent px-2.5 text-[11.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
                  >
                    <Check className="size-3.5" aria-hidden />
                    Approve
                  </button>
                  <button
                    type="button"
                    disabled={busy}
                    data-approval-reject
                    onClick={() => {
                      setDeciding({ request, mode: "reject" });
                      setDecisionNote("");
                    }}
                    className="flex h-7 items-center gap-1 rounded-lg border border-line px-2.5 text-[11.5px] text-caution transition hover:bg-panel disabled:opacity-50"
                  >
                    <X className="size-3.5" aria-hidden />
                    Reject
                  </button>
                </div>
              ) : null}
            </li>
          ))}
        </ul>
      ) : null}

      {mine.length > 0 && !mineBlocked ? (
        <section className="flex flex-col gap-2 rounded-xl border border-line bg-surface p-3" data-my-requests>
          <h2 className="text-[12.5px] font-medium text-ink">Your requests</h2>
          <ul className="flex flex-col gap-1.5">
            {mine.map((request) => (
              <li key={request.id} data-my-request-row className="flex flex-wrap items-center gap-2 text-[12px]">
                <span className="font-mono text-muted">{request.permission_key}</span>
                <span className={statusBadge(request.status)}>{request.status}</span>
                {request.grant_expires_at ? (
                  <span className="text-[11.5px] text-muted">
                    until {new Date(request.grant_expires_at).toLocaleString()}
                  </span>
                ) : null}
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      {deciding ? (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-ink/40 p-4">
          <div
            role="dialog"
            aria-modal
            aria-label={deciding.mode === "approve" ? "Approve request" : "Reject request"}
            data-approval-dialog={deciding.mode}
            className="w-full max-w-md rounded-xl border border-line bg-surface p-4 shadow-xl"
          >
            <h2 className="text-[13.5px] font-semibold text-ink">
              {deciding.mode === "approve" ? "Grant a window" : "Refuse the request"}
            </h2>
            <p className="mt-1 text-[12px] text-muted">
              {deciding.request.requester.email} asked for{" "}
              <span className="font-mono">{deciding.request.permission_key}</span>
              {deciding.request.resource_id ? ` on ${deciding.request.resource_id}` : ""}.
            </p>

            {deciding.mode === "approve" ? (
              <label className="mt-3 flex flex-col gap-1">
                <span className="text-[11.5px] text-muted">Window</span>
                <select
                  value={windowMinutes}
                  data-approval-window-select
                  onChange={(event) => setWindowMinutes(Number(event.target.value))}
                  className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
                >
                  {WINDOWS.map((window) => (
                    <option key={window.minutes} value={window.minutes}>
                      {window.label}
                    </option>
                  ))}
                </select>
              </label>
            ) : null}

            <label className="mt-3 flex flex-col gap-1">
              <span className="text-[11.5px] text-muted">Note (optional)</span>
              <input
                value={decisionNote}
                data-approval-note
                onChange={(event) => setDecisionNote(event.target.value)}
                className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>

            <div className="mt-4 flex items-center gap-2">
              <button
                type="button"
                disabled={busy}
                data-approval-decide-confirm
                onClick={() => void submitDecision()}
                className="h-8 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
              >
                {deciding.mode === "approve" ? "Approve" : "Reject"}
              </button>
              <button
                type="button"
                onClick={() => setDeciding(null)}
                className="h-8 rounded-lg border border-line px-3 text-[12.5px] text-muted"
              >
                Cancel
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}
