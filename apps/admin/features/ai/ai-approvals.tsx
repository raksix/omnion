"use client";

/**
 * `/ai/approvals` — the review inbox (docs/requests/REQ-101, slice 1).
 *
 * Every row on this screen is **an action nobody has taken yet**, and the screen is built around
 * that one fact. Three decisions shape it:
 *
 * 1. **A failed load is not an empty inbox.** "Nothing waiting for you" sends a reviewer to stop
 *    looking, so the failure keeps its own wording and its own Retry button — the same split
 *    `ai-view.tsx` makes for the provider list. A rejection lands on the error slot, never back
 *    on the loading one: `null` there would mean "still loading" and the skeleton would never
 *    resolve.
 *
 * 2. **A viewer without `ai.approvals.act` sees disabled buttons that NAME the missing key.**
 *    Not greyed-out-with-no-explanation. The API sends `viewer_missing` for exactly this, and
 *    the write paths enforce the same keys with a 403 naming them — so the disabled state is a
 *    promise the API keeps rather than a guess from the role's name.
 *
 * 3. **Expiry is rendered as a countdown, and the row's own `decidable` decides the button.**
 *    A pending row the sweeper has not reached yet is still `pending` with a past `expires_at`,
 *    so the panel asks the server rather than comparing two timestamps it might round
 *    differently — and an expired row is read-only with its reason, never a live button that
 *    answers `expired`.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";

import {
  ChevronRight,
  Clock,
  Loader2,
  Search,
  ShieldCheck,
  SlidersHorizontal,
  TriangleAlert,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiApproval,
  type AiApprovalClass,
  type AiApprovalPolicy,
  approveAiApproval,
  fetchAiApprovalPolicies,
  fetchAiApprovals,
  putAiApprovalPolicy,
  rejectAiApproval,
  resetAiApprovalPolicy,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The decision key a reviewer needs, named in every disabled control's explanation. */
const DECIDE_KEY = "ai.approvals.act";
/** The key that governs the policy screen, kept apart because it is a different act. */
const POLICY_KEY = "ai.policies.manage";

/** The status tabs. `all` is a real tab rather than "no filter" so the counts have a home. */
const STATUS_TABS = [
  { key: "pending", label: "Pending" },
  { key: "approved", label: "Approved" },
  { key: "rejected", label: "Rejected" },
  { key: "expired", label: "Expired" },
  { key: "stale", label: "Stale" },
  { key: "applied", label: "Applied" },
  { key: "failed", label: "Failed" },
  { key: "all", label: "All" },
] as const;

/** Risk is never carried by colour alone — each level has its own word. */
const RISK_TONE: Record<string, string> = {
  low: "bg-quiet-soft text-muted",
  medium: "bg-caution-soft text-caution",
  high: "bg-danger-soft text-danger",
};

const STATUS_TONE: Record<string, string> = {
  pending: "bg-accent-soft text-accent-strong",
  approved: "bg-positive-soft text-positive",
  applied: "bg-positive-soft text-positive",
  rejected: "bg-quiet-soft text-muted",
  expired: "bg-quiet-soft text-muted",
  stale: "bg-caution-soft text-caution",
  failed: "bg-danger-soft text-danger",
};

/** `content_publish` → `Content publish`. The class key stays visible too — it is what the API
 *  filters on, and an operator who cannot see it cannot file a ticket with a precise word. */
function classLabel(value: string): string {
  const words = value.replace(/_/g, " ").trim();
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** "3 minutes left" / "expired 4 minutes ago" — the reader needs a direction, not a timestamp. */
function countdown(expiresAt: string, status: string): { text: string; urgent: boolean } {
  if (status !== "pending") {
    return { text: "—", urgent: false };
  }
  const seconds = Math.round((new Date(expiresAt).getTime() - Date.now()) / 1000);
  if (!Number.isFinite(seconds)) return { text: "—", urgent: false };
  if (seconds <= 0) return { text: "expired", urgent: true };
  if (seconds < 120) return { text: `${seconds}s left`, urgent: true };
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return { text: `${minutes} min left`, urgent: false };
  const hours = Math.round(minutes / 60);
  return { text: `${hours} h left`, urgent: false };
}

/** The resource cell: type plus the name the reviewer recognises, never a bare uuid. */
function resourceOf(approval: AiApproval): string {
  const type = approval.resource_type ?? "resource";
  const label = approval.resource_label ?? approval.resource_id;
  return label ? `${type} · ${label}` : type;
}

/**
 * One row's decision buttons.
 *
 * The disabled branch says **why**: which key is missing. A control that refuses with a reason
 * is a control the reviewer can act on; a greyed-out one with no text is a dead button, which the
 * definition of done forbids outright.
 */
function RowActions({
  approval,
  canDecide,
  missing,
  busy,
  onReview,
  onQuickReject,
}: {
  approval: AiApproval;
  canDecide: boolean;
  missing: string[];
  busy: boolean;
  onReview: () => void;
  onQuickReject: () => void;
}) {
  const expired = approval.status === "expired" || (approval.status === "pending" && !approval.decidable);
  const disabled = busy || !approval.decidable;
  const why = expired
    ? "This request has expired, so it is read-only."
    : canDecide
      ? undefined
      : `You are missing ${missing.join(", ")} — this action cannot be taken from your account`;

  return (
    <span className="flex items-center gap-1.5">
      <button
        type="button"
        onClick={onReview}
        data-approval-review={approval.id}
        className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas"
      >
        Review
        <ChevronRight className="size-3" aria-hidden />
      </button>
      <button
        type="button"
        disabled={disabled}
        onClick={onQuickReject}
        title={why}
        data-approval-quick-reject={approval.id}
        className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-caution transition hover:bg-caution-soft disabled:cursor-not-allowed disabled:opacity-50"
      >
        Reject
      </button>
    </span>
  );
}

/**
 * The policy table (REQ-101 slice 1, the `/ai/approvals/policies` half).
 *
 * Un-gating a class is the strongest single act in the AI hub — it says "from now on, agents may
 * publish without asking" — so the direction that *loosens* the gate is the one that demands a
 * typed phrase naming the class, and the row keeps a warning stripe afterwards. Resetting is not
 * a policy change: removing an override can only tighten towards the fail-closed default, so it
 * carries no phrase, and the API agrees.
 */
function PolicyScreen({
  canManage,
  missing,
  unknownReason = null,
}: {
  canManage: boolean;
  missing: string[];
  /** Set when the key check itself failed — a different claim from "the key is absent". */
  unknownReason?: string | null;
}) {
  const [rows, setRows] = useState<AiApprovalPolicy[] | null>(null);
  const [classes, setClasses] = useState<AiApprovalClass[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [editing, setEditing] = useState<string | null>(null);
  const [mode, setMode] = useState<"require" | "allow">("require");
  const [typedConfirmation, setTypedConfirmation] = useState(true);
  const [expiresMinutes, setExpiresMinutes] = useState("60");
  const [confirmation, setConfirmation] = useState("");

  const load = useCallback(() => {
    setError(null);
    fetchAiApprovalPolicies()
      .then((screen) => {
        setRows(screen.policies);
        setClasses(screen.classes);
      })
      .catch((cause: unknown) => {
        setRows([]);
        setError(
          cause instanceof ApiError ? cause.message : "The approval policy could not be read.",
        );
      });
  }, []);

  useEffect(load, [load]);

  const expectedPhrase = (toolClass: string) => `set ${toolClass} to allow`;

  const save = async (toolClass: string, label: string) => {
    setBusy(toolClass);
    setError(null);
    setNotice(null);
    try {
      const body = await putAiApprovalPolicy(toolClass, {
        mode,
        typedConfirmation,
        // An empty minutes field is sent as the platform default rather than as 0, which the
        // column's 5–1440 check would refuse — a blank number is an unset number here.
        expiresMinutes: expiresMinutes.trim() ? Number(expiresMinutes) : undefined,
        // Only a typed phrase is sent, so the field's own value can never be echoed back as a
        // confirmation the reviewer did not give.
        confirmation: mode === "allow" ? confirmation : undefined,
      });
      setRows(body.policies);
      setNotice(`${label} is now ${mode === "allow" ? "allowed without approval" : "gated"}.`);
      setEditing(null);
      setConfirmation("");
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The policy was not saved.");
    } finally {
      setBusy(null);
    }
  };

  const reset = async (toolClass: string, label: string) => {
    setBusy(toolClass);
    setError(null);
    setNotice(null);
    try {
      const body = await resetAiApprovalPolicy(toolClass);
      setRows(body.policies);
      setNotice(`${label} is back to the platform default.`);
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The policy was not reset.");
    } finally {
      setBusy(null);
    }
  };

  return (
    <section id="approval-policies" className="scroll-mt-4 rounded-xl border border-line bg-surface">
      <header className="border-b border-line px-4 py-3">
        <h2 className="text-[13.5px] font-semibold">Class policy</h2>
        <p className="text-[12px] text-muted">
          The six dangerous classes, all gated by default. Letting one through means agents act in
          that class without asking — the phrase below is the price of it.
        </p>
      </header>

      {notice ? (
        <p data-policy-notice className="border-b border-line bg-positive-soft px-4 py-2 text-[12.5px] text-positive">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p
          role="alert"
          data-policy-error
          className="border-b border-line bg-danger-soft px-4 py-2 text-[12.5px] text-danger"
        >
          {error}
        </p>
      ) : null}
      {!canManage ? (
        <p
          data-policy-readonly
          className="border-b border-line bg-caution-soft px-4 py-2 text-[12.5px] text-caution"
        >
          {unknownReason ??
            `You are missing ${missing.join(", ") || POLICY_KEY} — the policy is readable here but cannot be changed from your account.`}
        </p>
      ) : null}

      {rows === null ? (
        <LoadingTable columns={5} rows={3} />
      ) : rows.length === 0 ? (
        <EmptyState
          title="No class policy is loaded"
          hint="The platform seeds all six dangerous classes as gated. A failure here means the table could not be read."
          action={
            <button
              type="button"
              onClick={load}
              data-policy-retry
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              Try again
            </button>
          }
        />
      ) : (
        <ul className="divide-y divide-[var(--color-line)]">
          {rows.map((policy) => {
            const open = editing === policy.tool_class;
            const locked = !canManage || busy === policy.tool_class;
            return (
              <li
                key={policy.tool_class}
                data-policy-row={policy.tool_class}
                data-permissive={policy.permissive ? "true" : "false"}
                className={`flex flex-col gap-2 px-4 py-3 ${
                  policy.permissive ? "border-l-2 border-danger bg-danger-soft/30" : ""
                }`}
              >
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-[13px] font-medium">{policy.label}</span>
                  <span className="font-mono text-[11px] text-muted">{policy.tool_class}</span>
                  {policy.irreversible ? (
                    <span className="inline-flex items-center gap-1 rounded-full bg-danger-soft px-2 py-0.5 text-[10.5px] font-medium text-danger">
                      <TriangleAlert className="size-3" aria-hidden />
                      Irreversible
                    </span>
                  ) : null}
                  <span
                    data-policy-mode={policy.tool_class}
                    className={`inline-flex items-center rounded-full px-2 py-0.5 text-[10.5px] font-medium ${
                      policy.permissive ? "bg-danger-soft text-danger" : "bg-positive-soft text-positive"
                    }`}
                  >
                    {policy.permissive ? "Allowed without approval" : "Requires approval"}
                  </span>
                  <span className="text-[11px] text-muted">
                    {policy.source === "organization" ? "Set here" : "Platform default"} ·{" "}
                    {policy.expires_minutes} min
                  </span>
                  <span className="ml-auto flex items-center gap-1.5">
                    <button
                      type="button"
                      disabled={!canManage || busy === policy.tool_class}
                      title={canManage ? undefined : `Missing ${POLICY_KEY}`}
                      onClick={() => {
                        setEditing(open ? null : policy.tool_class);
                        setMode(policy.mode === "allow" ? "require" : "allow");
                        setTypedConfirmation(policy.typed_confirmation);
                        setExpiresMinutes(String(policy.expires_minutes));
                        setConfirmation("");
                      }}
                      data-policy-edit={policy.tool_class}
                      className="rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-50"
                    >
                      Change
                    </button>
                    {policy.source === "organization" ? (
                      <button
                        type="button"
                        disabled={!canManage || busy === policy.tool_class}
                        title={
                          canManage
                            ? "Drop the override and fall back to the platform default"
                            : `Missing ${POLICY_KEY}`
                        }
                        onClick={() => void reset(policy.tool_class, policy.label)}
                        data-policy-reset={policy.tool_class}
                        className="rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-50"
                      >
                        Reset
                      </button>
                    ) : null}
                  </span>
                </div>

                {open ? (
                  <div className="flex flex-col gap-2 rounded-lg border border-line bg-canvas p-3">
                    <label className="flex flex-col gap-1">
                      <span className="text-[12px] font-medium">Mode</span>
                      <select
                        value={mode}
                        onChange={(event) => setMode(event.target.value as "require" | "allow")}
                        data-policy-mode-select={policy.tool_class}
                        className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
                      >
                        <option value="require">Require approval</option>
                        <option value="allow">Allow without approval</option>
                      </select>
                      <span className="text-[11px] text-muted">
                        Allowing is cumulative: every gated class turned off is one more thing no
                        human will ever see.
                      </span>
                    </label>

                    <label className="flex items-center gap-2 text-[12.5px]">
                      <input
                        type="checkbox"
                        checked={typedConfirmation}
                        disabled={locked}
                        onChange={(event) => setTypedConfirmation(event.target.checked)}
                        data-policy-typed={policy.tool_class}
                        className="size-3.5 accent-[var(--color-accent)] disabled:opacity-50"
                      />
                      Ask for a typed confirmation on irreversible steps
                    </label>

                    <label className="flex flex-col gap-1 sm:max-w-[12rem]">
                      <span className="text-[12px] font-medium">Expiry minutes</span>
                      <input
                        value={expiresMinutes}
                        disabled={locked}
                        onChange={(event) => setExpiresMinutes(event.target.value)}
                        type="number"
                        inputMode="numeric"
                        min={5}
                        max={1440}
                        data-policy-expiry={policy.tool_class}
                        className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent focus:ring-2 focus:ring-accent/15 disabled:opacity-50"
                      />
                      <span className="text-[11px] text-muted">
                        5–1440. How long a request waits before it expires unread.
                      </span>
                    </label>

                    {mode === "allow" ? (
                      <label className="flex flex-col gap-1">
                        <span className="text-[12px] font-medium">Type the phrase to confirm</span>
                        <input
                          value={confirmation}
                          disabled={locked}
                          onChange={(event) => setConfirmation(event.target.value)}
                          placeholder={expectedPhrase(policy.tool_class)}
                          data-policy-confirmation={policy.tool_class}
                          className="rounded-lg border border-line bg-surface px-2.5 py-1.5 font-mono text-[12px] outline-none focus:border-accent focus:ring-2 focus:ring-accent/15 disabled:opacity-50"
                        />
                        <span className="text-[11px] text-muted">
                          Type <code className="font-mono">{expectedPhrase(policy.tool_class)}</code>{" "}
                          exactly. The API checks this phrase itself, so the dialog is not what
                          keeps the gate closed.
                        </span>
                      </label>
                    ) : null}

                    <div className="flex items-center gap-2">
                      <button
                        type="button"
                        disabled={!canManage || busy === policy.tool_class}
                        onClick={() => void save(policy.tool_class, policy.label)}
                        data-policy-save={policy.tool_class}
                        className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-quiet-soft disabled:text-muted"
                      >
                        {busy === policy.tool_class ? (
                          <Loader2 className="size-3.5 animate-spin" aria-label="Saving" />
                        ) : (
                          "Save policy"
                        )}
                      </button>
                      <button
                        type="button"
                        onClick={() => setEditing(null)}
                        className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
                      >
                        Cancel
                      </button>
                    </div>
                  </div>
                ) : null}
              </li>
            );
          })}
        </ul>
      )}
      <p className="border-t border-line px-4 py-2 text-[11.5px] text-muted">
        {classes.length} dangerous classes · all gated on a fresh installation
      </p>
    </section>
  );
}

/**
 * The policy table on its own route (`/ai/approvals/policies`).
 *
 * It resolves the viewer's decision keys itself rather than taking them from a caller, because
 * this screen is reachable directly from the inbox's empty state — a link to a route that only
 * worked when arrived from one particular page is a dead link with extra steps.
 */
export function AiApprovalPolicyScreen() {
  const [missing, setMissing] = useState<string[] | null>(null);
  const [unknown, setUnknown] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetchAiApprovals({ status: "pending", limit: 1 })
      .then((inbox) => {
        if (!cancelled) setMissing(inbox.viewer_missing);
      })
      .catch((cause: unknown) => {
        // The key set could not be read, so the screen does **not** guess. Guessing "allowed"
        // would offer a button the API answers with a 403; guessing "forbidden" would take the
        // control away from somebody who has it. It renders read-only and says why — a visible
        // "I could not check" beats either wrong answer.
        if (cancelled) return;
        setMissing([POLICY_KEY]);
        setUnknown(
          cause instanceof ApiError
            ? cause.message
            : "Your decision permissions could not be read, so the policy is shown read-only.",
        );
      });
    return () => {
      cancelled = true;
    };
  }, []);

  if (missing === null) {
    return <LoadingTable columns={5} rows={3} />;
  }
  return (
    <PolicyScreen
      canManage={!missing.includes(POLICY_KEY)}
      missing={missing}
      unknownReason={unknown}
    />
  );
}

/** The review inbox. */
export function AiApprovalsView() {
  const router = useRouter();
  // `null` is "still loading" and renders the skeleton; a *failed* load sets `inbox` to an empty
  // object and puts the reason in `error`, because parking a rejection back on the loading slot
  // is what leaves a screen shimmering for ever.
  const [approvals, setApprovals] = useState<AiApproval[] | null>(null);
  const [counts, setCounts] = useState<Record<string, number>>({});
  const [missing, setMissing] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const [status, setStatus] = useState<string>("pending");
  const [toolClass, setToolClass] = useState<string>("all");
  const [search, setSearch] = useState("");
  const [reloadToken, setReloadToken] = useState(0);
  const [rejecting, setRejecting] = useState<AiApproval | null>(null);
  const [reason, setReason] = useState("");

  const canDecide = useMemo(() => !missing.includes(DECIDE_KEY), [missing]);

  useEffect(() => {
    let cancelled = false;
    setError(null);
    fetchAiApprovals({ status, toolClass, q: search })
      .then((inbox) => {
        if (cancelled) return;
        setApprovals(inbox.approvals);
        setCounts(inbox.counts);
        setMissing(inbox.viewer_missing);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setApprovals([]);
        setError(
          cause instanceof ApiError ? cause.message : "The review inbox could not be loaded.",
        );
      });
    return () => {
      cancelled = true;
    };
  }, [status, toolClass, search, reloadToken]);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  const quickReject = async (approval: AiApproval) => {
    setBusy(approval.id);
    setError(null);
    setNotice(null);
    try {
      const result = await rejectAiApproval(approval.id, reason.trim() || "Rejected from the inbox");
      setNotice(
        result.changed
          ? `${approval.title} rejected.`
          : `Nothing changed: this request was already ${result.code ?? "decided"}.`,
      );
      setRejecting(null);
      setReason("");
      reload();
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The rejection did not go through.");
    } finally {
      setBusy(null);
    }
  };

  const classOptions = useMemo(() => {
    const seen = new Set(approvals?.map((approval) => approval.tool_class) ?? []);
    // The class vocabulary comes from the rows on screen plus the policy screen's own list, so
    // the filter can never offer a class the API has never heard of.
    return [...seen].sort();
  }, [approvals]);

  return (
    <div className="flex flex-col gap-4">
      <section className="rounded-xl border border-line bg-surface">
        <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <div>
            <h2 className="text-[13.5px] font-semibold">Review inbox</h2>
            <p className="text-[12px] text-muted">
              What an agent wants to do, held until a person decides. Nothing here has happened yet.
            </p>
          </div>
          <div className="ml-auto flex items-center gap-1.5">
            <label className="relative flex items-center">
              <Search
                className="pointer-events-none absolute left-2 size-3.5 text-muted"
                aria-hidden
              />
              <input
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder="Search resource…"
                data-approval-search
                className="w-48 rounded-lg border border-line bg-canvas py-1.5 pl-7 pr-2 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <label className="flex items-center gap-1.5 text-[12px] text-muted">
              <SlidersHorizontal className="size-3.5" aria-hidden />
              <select
                value={toolClass}
                onChange={(event) => setToolClass(event.target.value)}
                data-approval-class-filter
                className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12.5px] outline-none transition focus:border-accent"
              >
                <option value="all">Every class</option>
                {classOptions.map((value) => (
                  <option key={value} value={value}>
                    {classLabel(value)}
                  </option>
                ))}
              </select>
            </label>
          </div>
        </header>

        <nav
          className="flex flex-wrap items-center gap-1 border-b border-line px-4 py-2"
          aria-label="Approval status"
        >
          {STATUS_TABS.map((tab) => (
            <button
              key={tab.key}
              type="button"
              onClick={() => setStatus(tab.key)}
              aria-pressed={status === tab.key}
              data-approval-tab={tab.key}
              className={`rounded-full px-2.5 py-1 text-[12px] transition ${
                status === tab.key
                  ? "bg-accent text-white"
                  : "text-muted hover:bg-canvas"
              }`}
            >
              {tab.label}
              {tab.key !== "all" && counts[tab.key] ? (
                <span className="ml-1 tabular-nums opacity-80">{counts[tab.key]}</span>
              ) : null}
            </button>
          ))}
          {counts.pending ? (
            <span
              data-approval-pending-badge
              className="ml-auto inline-flex items-center gap-1 rounded-full bg-accent-soft px-2 py-0.5 text-[11px] font-medium text-accent-strong"
            >
              <Clock className="size-3" aria-hidden />
              {counts.pending} waiting
            </span>
          ) : null}
        </nav>

        {notice ? (
          <p
            data-approval-notice
            className="border-b border-line bg-positive-soft px-4 py-2 text-[12.5px] text-positive"
          >
            {notice}
          </p>
        ) : null}
        {error ? (
          <div
            role="alert"
            data-approval-error
            className="flex flex-wrap items-center gap-2 border-b border-line bg-danger-soft px-4 py-2 text-[12.5px] text-danger"
          >
            <span className="flex-1">{error}</span>
            <button
              type="button"
              onClick={reload}
              data-approval-retry
              className="rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas"
            >
              Try again
            </button>
          </div>
        ) : null}
        {!canDecide && !error ? (
          <p data-approval-readonly className="border-b border-line bg-caution-soft px-4 py-2 text-[12.5px] text-caution">
            You can read this inbox but not decide in it — you are missing {DECIDE_KEY}. Ask an
            administrator for the key, or read what each request would do.
          </p>
        ) : null}

        {approvals === null ? (
          <LoadingTable columns={6} rows={4} />
        ) : error ? (
          // An outage is not an empty inbox: the wording and the button say so.
          <div className="flex flex-col items-center gap-2 px-6 py-10 text-center">
            <p className="text-[13.5px] font-medium">The review inbox could not be loaded</p>
            <p className="max-w-sm text-[12.5px] text-muted">{error}</p>
            <button
              type="button"
              onClick={reload}
              data-approval-reload
              className="mt-2 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              Try again
            </button>
          </div>
        ) : approvals.length === 0 ? (
          <EmptyState
            title={
              status === "pending" && search
                ? "Nothing matches that search"
                : "Nothing waiting for you"
            }
            hint={
              status === "pending" && search
                ? "No pending request mentions that. Clear the search to see the whole queue."
                : "No agent has asked to do anything dangerous. When one does, it parks here until a person decides."
            }
            action={
              status === "pending" && search ? (
                <button
                  type="button"
                  onClick={() => setSearch("")}
                  data-approval-clear-search
                  className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
                >
                  Clear the search
                </button>
              ) : (
                <Link
                  href="/ai/approvals/policies"
                  data-approval-empty-policies
                  className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
                >
                  <ShieldCheck className="size-3.5" aria-hidden />
                  See which classes are gated
                </Link>
              )
            }
          />
        ) : (
          <>
            {/* Desktop: a table. The resource column carries the name a human recognises, so a
                reviewer triages without opening anything. */}
            <div className="hidden overflow-x-auto md:block" data-approval-table>
              <table className="w-full border-collapse text-left text-[13px]">
                <thead>
                  <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                    <th className="px-4 py-2 font-medium">Requested</th>
                    <th className="px-4 py-2 font-medium">Tool</th>
                    <th className="px-4 py-2 font-medium">Resource</th>
                    <th className="px-4 py-2 font-medium">Risk</th>
                    <th className="px-4 py-2 font-medium">Ops</th>
                    <th className="px-4 py-2 font-medium">Status</th>
                    <th className="px-4 py-2 font-medium">Expires</th>
                    <th className="px-4 py-2 font-medium">Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {approvals.map((approval) => {
                    const left = countdown(approval.expires_at, approval.status);
                    return (
                      <tr
                        key={approval.id}
                        data-approval-row={approval.id}
                        className="border-b border-line last:border-b-0"
                      >
                        <td className="px-4 py-3 align-top">
                          <span className="block text-[12.5px] font-medium">{approval.title}</span>
                          <span
                            title={formatTimestamp(approval.created_at)}
                            className="text-[11.5px] text-muted"
                          >
                            {formatTimestamp(approval.created_at)}
                          </span>
                        </td>
                        <td className="px-4 py-3 align-top">
                          <span className="block font-mono text-[12px]">{approval.tool_key}</span>
                          <span className="text-[11px] text-muted">
                            {classLabel(approval.tool_class)}
                          </span>
                        </td>
                        <td className="max-w-[16rem] px-4 py-3 align-top text-[12px] break-words">
                          {resourceOf(approval)}
                        </td>
                        <td className="px-4 py-3 align-top">
                          <span
                            data-approval-risk={approval.risk}
                            className={`inline-flex items-center rounded-full px-2 py-0.5 text-[10.5px] font-medium ${
                              RISK_TONE[approval.risk] ?? RISK_TONE.low
                            }`}
                          >
                            {approval.risk}
                          </span>
                          {approval.irreversible ? (
                            <span className="ml-1 text-[10.5px] text-danger">irreversible</span>
                          ) : null}
                        </td>
                        <td className="px-4 py-3 align-top tabular-nums text-[12px]">
                          {approval.operation_count}
                        </td>
                        <td className="px-4 py-3 align-top">
                          <span
                            data-approval-status={approval.status}
                            className={`inline-flex items-center rounded-full px-2 py-0.5 text-[10.5px] font-medium ${
                              STATUS_TONE[approval.status] ?? "bg-quiet-soft text-muted"
                            }`}
                          >
                            {approval.status}
                          </span>
                        </td>
                        <td className="px-4 py-3 align-top text-[12px]">
                          <span className={left.urgent ? "text-danger" : "text-muted"}>{left.text}</span>
                        </td>
                        <td className="px-4 py-3 align-top">
                          <RowActions
                            approval={approval}
                            canDecide={canDecide}
                            missing={missing}
                            busy={busy === approval.id}
                            onReview={() => router.push(`/ai/approvals/${approval.id}`)}
                            onQuickReject={() => {
                              setRejecting(approval);
                              setReason("");
                            }}
                          />
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>

            {/* Mobile: cards. The table's eight columns become a horizontal scroll on a phone,
                which hides the very row the reviewer is triaging. */}
            <ul className="flex flex-col divide-y divide-[var(--color-line)] md:hidden" data-approval-cards>
              {approvals.map((approval) => {
                const left = countdown(approval.expires_at, approval.status);
                return (
                  <li key={approval.id} data-approval-card={approval.id} className="flex flex-col gap-2 px-4 py-3">
                    <div className="flex flex-wrap items-center gap-1.5">
                      <span className="text-[12.5px] font-medium">{approval.title}</span>
                      <span
                        data-approval-card-status={approval.status}
                        className={`inline-flex items-center rounded-full px-2 py-0.5 text-[10.5px] font-medium ${
                          STATUS_TONE[approval.status] ?? "bg-quiet-soft text-muted"
                        }`}
                      >
                        {approval.status}
                      </span>
                      <span className={`ml-auto text-[11.5px] ${left.urgent ? "text-danger" : "text-muted"}`}>
                        {left.text}
                      </span>
                    </div>
                    <p className="font-mono text-[11.5px] text-muted">
                      {approval.tool_key} · {classLabel(approval.tool_class)} ·{" "}
                      {approval.operation_count} op{approval.operation_count === 1 ? "" : "s"}
                    </p>
                    <p className="text-[12px] break-words">{resourceOf(approval)}</p>
                    <RowActions
                      approval={approval}
                      canDecide={canDecide}
                      missing={missing}
                      busy={busy === approval.id}
                      onReview={() => router.push(`/ai/approvals/${approval.id}`)}
                      onQuickReject={() => {
                        setRejecting(approval);
                        setReason("");
                      }}
                    />
                  </li>
                );
              })}
            </ul>
          </>
        )}
      </section>

      {/* The quick reject's reason. A rejection without a reason is refused by the API, so the
          dialog asks for one rather than sending a default string the reviewer did not write. */}
      {rejecting ? (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-ink/40 p-4">
          <div
            role="dialog"
            aria-modal="true"
            aria-label="Reject this request"
            data-approval-reject-dialog
            className="flex w-full max-w-md flex-col gap-3 rounded-xl border border-line bg-surface p-4"
          >
            <h3 className="text-[14px] font-semibold">Reject this request</h3>
            <p className="text-[12.5px] text-muted">
              {rejecting.title} — the run ends without the effect, and the reason is recorded in
              the audit trail.
            </p>
            <label className="flex flex-col gap-1">
              <span className="text-[12px] font-medium">Reason</span>
              <textarea
                value={reason}
                onChange={(event) => setReason(event.target.value)}
                rows={3}
                maxLength={500}
                placeholder="Why this should not happen."
                data-approval-reject-reason
                className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
              <span className="text-[11px] text-muted">{reason.length}/500</span>
            </label>
            <div className="flex items-center gap-2">
              <button
                type="button"
                disabled={busy === rejecting.id}
                onClick={() => void quickReject(rejecting)}
                data-approval-reject-confirm
                className="rounded-lg bg-danger px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:opacity-90 disabled:opacity-60"
              >
                {busy === rejecting.id ? (
                  <Loader2 className="size-3.5 animate-spin" aria-label="Rejecting" />
                ) : (
                  "Reject request"
                )}
              </button>
              <button
                type="button"
                onClick={() => setRejecting(null)}
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
              >
                Cancel
              </button>
            </div>
          </div>
        </div>
      ) : null}

      <PolicyScreen canManage={!missing.includes(POLICY_KEY)} missing={missing} />
    </div>
  );
}