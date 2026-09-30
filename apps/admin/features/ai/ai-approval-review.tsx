"use client";

/**
 * `/ai/approvals/[id]` — the review screen (docs/requests/REQ-101, slice 1).
 *
 * This is the screen the whole request exists for, and it has one job: let a person decide, in
 * one reading and one click, **on exactly what the agent said it would do**. Four decisions make
 * that possible, and each of them is a place where the obvious implementation lies:
 *
 * 1. **The frozen preview is the only diff shown.** The panel never re-computes a preview from
 *    live state — a preview the client derived is a preview nobody approved. What the reviewer
 *    sees is the object the request was frozen with, and the hash beside it is what an approve
 *    is checked against.
 *
 * 2. **The phrase is sent only when it was typed.** `approveAiApproval` omits the field entirely
 *    when it is blank, because the API answers `confirmation_mismatch` to a wrong phrase rather
 *    than to an absent one — sending the field unconditionally would make every irreversible
 *    class un-approvable by typing anything, which is the opposite of the guard.
 *
 * 3. **A race is not an error.** `changed: false` with a code means somebody else got there
 *    first, or the row expired while it was open. The screen says which, and re-reads the row —
 *    it does not paint a red banner for something the reviewer did not cause.
 *
 * 4. **A viewer without the decision key sees the buttons disabled and the key named**, exactly
 *    as the inbox does, and the API refuses the same call with a 403 naming it.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useParams, useRouter } from "next/navigation";

import {
  ArrowLeft,
  CircleCheck,
  CircleSlash,
  Clock,
  Loader2,
  RotateCcw,
  ShieldAlert,
  TriangleAlert,
} from "lucide-react";

import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiApproval,
  type AiApprovalAuditRow,
  approveAiApproval,
  fetchAiApproval,
  rejectAiApproval,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

const DECIDE_KEY = "ai.approvals.act";

const STATUS_TONE: Record<string, string> = {
  pending: "bg-accent-soft text-accent-strong",
  approved: "bg-positive-soft text-positive",
  applied: "bg-positive-soft text-positive",
  rejected: "bg-quiet-soft text-muted",
  expired: "bg-quiet-soft text-muted",
  stale: "bg-caution-soft text-caution",
  failed: "bg-danger-soft text-danger",
};

/** One field row of the frozen diff. */
type DiffField = { field: string; old: unknown; new: unknown };

/** One operation card of the frozen diff. */
type DiffOperation = {
  action: string;
  resource: string;
  fields: DiffField[];
  /** The cascade count a delete preview carries ("1 page, 4 revisions"). */
  cascades?: string[];
};

/**
 * Read the frozen preview into the shape the screen renders.
 *
 * Every field is tolerated rather than assumed: a preview the runtime wrote before an operation
 * grew a `cascades` key must still render the operations it does have, and a preview with no
 * `fields` must render an empty card rather than crash the screen — a crash here would lose the
 * decision the reviewer was in the middle of.
 */
function readPreview(preview: unknown): DiffOperation[] {
  if (typeof preview !== "object" || preview === null) return [];
  const operations = (preview as { operations?: unknown }).operations;
  if (!Array.isArray(operations)) return [];
  return operations.flatMap((entry) => {
    if (typeof entry !== "object" || entry === null) return [];
    const row = entry as Record<string, unknown>;
    const fields = Array.isArray(row.fields)
      ? (row.fields as unknown[]).flatMap((field) =>
          typeof field === "object" && field !== null
            ? [field as DiffField]
            : [],
        )
      : [];
    const cascades = Array.isArray(row.cascades) ? (row.cascades as unknown[]).map(String) : undefined;
    return [
      {
        action: String(row.action ?? "update"),
        resource: String(row.resource ?? "resource"),
        fields,
        ...(cascades?.length ? { cascades } : {}),
      },
    ];
  });
}

/** A value rendered as text. Long values wrap; nothing is silently truncated. */
function valueText(value: unknown): string {
  if (value === null || value === undefined) return "—";
  if (typeof value === "string") return value.length ? value : "(empty)";
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return JSON.stringify(value);
}

const ACTION_TONE: Record<string, string> = {
  create: "bg-positive-soft text-positive",
  update: "bg-accent-soft text-accent-strong",
  delete: "bg-danger-soft text-danger",
};

/**
 * The diff of one operation: OLD and NEW side by side, unchanged fields collapsed.
 *
 * A changed field carries a marker and the word `changed`, not only a colour — the visual check
 * for this screen requires that highlighting never be the sole carrier of the distinction.
 */
function OperationCard({
  operation,
  index,
  showUnchanged,
  onToggleUnchanged,
}: {
  operation: DiffOperation;
  index: number;
  showUnchanged: boolean;
  onToggleUnchanged: () => void;
}) {
  const changed = operation.fields.filter(
    (field) => valueText(field.old) !== valueText(field.new),
  );
  const unchanged = operation.fields.length - changed.length;
  const rows = showUnchanged ? operation.fields : changed;

  return (
    <article
      data-diff-operation={index}
      data-diff-action={operation.action}
      className="flex flex-col gap-2 rounded-lg border border-line bg-surface p-3"
    >
      <header className="flex flex-wrap items-center gap-2">
        <span
          data-diff-action-badge
          className={`inline-flex items-center rounded-full px-2 py-0.5 text-[10.5px] font-medium ${
            ACTION_TONE[operation.action] ?? "bg-quiet-soft text-muted"
          }`}
        >
          {operation.action}
        </span>
        <span className="font-mono text-[12px] break-all">{operation.resource}</span>
      </header>

      {operation.cascades?.length ? (
        <p data-diff-cascades className="rounded-lg bg-danger-soft px-2.5 py-1.5 text-[12px] text-danger">
          This also removes: {operation.cascades.join(", ")}.
        </p>
      ) : null}

      {rows.length === 0 ? (
        <p data-diff-no-changes className="text-[12px] text-muted">
          {operation.fields.length === 0
            ? "The previewed operation names no field."
            : "Every field is unchanged — nothing would be written."}
        </p>
      ) : (
        <ul className="flex flex-col gap-1.5">
          {rows.map((field, fieldIndex) => {
            const isChanged = valueText(field.old) !== valueText(field.new);
            return (
              <li
                key={`${field.field}-${fieldIndex}`}
                data-diff-field={field.field}
                data-diff-changed={isChanged ? "true" : "false"}
                className={`rounded-lg border px-2.5 py-1.5 ${
                  isChanged ? "border-accent/40 bg-accent-soft/40" : "border-line bg-canvas"
                }`}
              >
                <div className="flex items-center gap-2">
                  <span className="font-mono text-[11.5px]">{field.field}</span>
                  {isChanged ? (
                    <span data-diff-changed-marker className="text-[10.5px] font-medium text-accent-strong">
                      changed
                    </span>
                  ) : null}
                </div>
                {/* Two columns on a wide screen, stacked below the 1024 px breakpoint with both
                    labels kept — a diff whose OLD and NEW columns collapse into one another on a
                    phone is a diff nobody can read on a phone. */}
                <div className="mt-1 grid gap-1.5 text-[12px] sm:grid-cols-2">
                  <div data-diff-old className="rounded border border-line bg-canvas px-2 py-1">
                    <span className="block text-[10.5px] uppercase tracking-wide text-muted">Old</span>
                    <span className="break-words whitespace-pre-wrap">{valueText(field.old)}</span>
                  </div>
                  <div data-diff-new className="rounded border border-line bg-canvas px-2 py-1">
                    <span className="block text-[10.5px] uppercase tracking-wide text-muted">New</span>
                    <span className="break-words whitespace-pre-wrap">{valueText(field.new)}</span>
                  </div>
                </div>
              </li>
            );
          })}
        </ul>
      )}

      {unchanged > 0 ? (
        <button
          type="button"
          onClick={onToggleUnchanged}
          data-diff-toggle-unchanged
          className="self-start rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas"
        >
          {showUnchanged ? "Hide unchanged" : `Show unchanged (${unchanged})`}
        </button>
      ) : null}
    </article>
  );
}

/** The audit trail, oldest first: a reviewer reads a request as a story. */
function Timeline({ rows }: { rows: AiApprovalAuditRow[] }) {
  return (
    <ol data-approval-timeline className="flex flex-col gap-2">
      {rows.map((row) => (
        <li
          key={row.id}
          data-audit-row={row.action}
          className="flex flex-wrap items-baseline gap-2 border-l-2 border-line pl-3 text-[12px]"
        >
          <span className="font-medium">{row.action}</span>
          <span className="text-muted">
            {row.actor_type === "agent" ? "the agent" : "a person"}
            {row.actor_user_id ? ` (${row.actor_user_id.slice(0, 8)})` : ""}
          </span>
          <span className="ml-auto text-[11.5px] text-muted">{formatTimestamp(row.created_at)}</span>
          {typeof row.metadata?.reason === "string" ? (
            <span data-audit-reason className="w-full text-[12px] text-muted">
              {row.metadata.reason}
            </span>
          ) : null}
        </li>
      ))}
    </ol>
  );
}

/** The review screen. */
export function AiApprovalReviewScreen() {
  const params = useParams<{ id: string }>();
  const router = useRouter();
  const id = params.id;

  const [approval, setApproval] = useState<AiApproval | null>(null);
  const [audit, setAudit] = useState<AiApprovalAuditRow[]>([]);
  const [missing, setMissing] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);

  const [confirmation, setConfirmation] = useState("");
  const [reason, setReason] = useState("");
  const [showUnchanged, setShowUnchanged] = useState<Record<number, boolean>>({});

  const load = useCallback(() => {
    setError(null);
    fetchAiApproval(id)
      .then((detail) => {
        setApproval(detail.approval);
        setAudit(detail.audit);
      })
      .catch((cause: unknown) => {
        setApproval(null);
        setError(cause instanceof ApiError ? cause.message : "This request could not be loaded.");
      });
  }, [id]);

  useEffect(load, [load, reloadToken]);

  // The inbox knows the viewer's missing keys and the review screen has to agree with it: a
  // button the inbox called disabled that is live here would be the API refusing in the one
  // place the reviewer expects to act.
  useEffect(() => {
    let cancelled = false;
    import("@/lib/api")
      .then(({ fetchAiApprovals }) => fetchAiApprovals({ status: "all", limit: 200 }))
      .then((inbox) => {
        if (!cancelled) setMissing(inbox.viewer_missing);
      })
      .catch(() => {
        // A failure here must not hide the screen: the button stays enabled and the API is the
        // authority anyway. Silently treating "unknown" as "forbidden" would take the decision
        // away from somebody who has it.
        if (!cancelled) setMissing([]);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const operations = useMemo(
    () => (approval ? readPreview(approval.preview) : []),
    [approval],
  );

  const canDecide = !missing.includes(DECIDE_KEY);
  const decided = approval ? !approval.decidable : false;
  const expired =
    approval?.status === "expired" || (approval?.status === "pending" && !approval.decidable);

  const decide = async (kind: "approve" | "reject") => {
    if (!approval) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const result =
        kind === "approve"
          ? await approveAiApproval(approval.id, {
              // The phrase is sent ONLY when the reviewer actually typed it. Sending an empty or
              // placeholder string is how a checkbox becomes a confirmation.
              confirmation,
              currentRevision: approval.base_revision ?? undefined,
            })
          : await rejectAiApproval(approval.id, reason.trim());
      setApproval(result.approval);
      if (result.changed) {
        setNotice(
          kind === "approve"
            ? "Approved. The parked run resumes and applies exactly what was previewed."
            : "Rejected. The run ends without the effect, and the reason is in the audit trail.",
        );
        setConfirmation("");
        setReason("");
      } else {
        // A race, not an error: somebody else decided it, or it expired while this screen was
        // open. Say which, and show the row as it now stands.
        setNotice(
          result.code === "expired"
            ? "Nothing changed: this request expired while it was open."
            : `Nothing changed: somebody else already decided this (${result.code ?? "already decided"}).`,
        );
      }
      setReloadToken((token) => token + 1);
    } catch (cause: unknown) {
      if (cause instanceof ApiError) {
        // `stale` is its own branch: the resource moved under the preview, and the screen's
        // answer is to say so rather than to show a generic failure.
        setError(
          cause.code === "stale"
            ? "The resource changed since this preview was taken. Re-read it before deciding."
            : cause.message,
        );
      } else {
        setError("The decision did not go through.");
      }
    } finally {
      setBusy(false);
    }
  };

  if (approval === null && error) {
    return (
      <div className="flex flex-col items-center gap-2 px-6 py-12 text-center">
        <p className="text-[13.5px] font-medium">This request could not be loaded</p>
        <p className="max-w-sm text-[12.5px] text-muted" data-approval-detail-error>
          {error}
        </p>
        <div className="mt-2 flex items-center gap-2">
          <button
            type="button"
            onClick={load}
            data-approval-detail-retry
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Try again
          </button>
          <Link
            href="/ai/approvals"
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Back to the inbox
          </Link>
        </div>
      </div>
    );
  }

  if (approval === null) {
    return <LoadingTable columns={4} rows={3} />;
  }

  return (
    <div className="flex flex-col gap-4 pb-24 lg:pb-0">
      <button
        type="button"
        onClick={() => router.push("/ai/approvals")}
        data-approval-back
        className="flex w-fit items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas"
      >
        <ArrowLeft className="size-3.5" aria-hidden />
        Back to the inbox
      </button>

      <section className="rounded-xl border border-line bg-surface">
        <header className="flex flex-col gap-2 border-b border-line px-4 py-3">
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="text-[14px] font-semibold">{approval.title}</h2>
            <span
              data-approval-detail-status={approval.status}
              className={`inline-flex items-center rounded-full px-2 py-0.5 text-[10.5px] font-medium ${
                STATUS_TONE[approval.status] ?? "bg-quiet-soft text-muted"
              }`}
            >
              {approval.status}
            </span>
            {approval.decidable ? null : (
              <span className="inline-flex items-center gap-1 text-[11.5px] text-muted">
                <CircleSlash className="size-3" aria-hidden />
                read-only
              </span>
            )}
          </div>
          <p className="text-[12.5px]">{approval.summary}</p>
          <dl className="grid gap-x-6 gap-y-1 text-[12px] sm:grid-cols-2 lg:grid-cols-3">
            <div>
              <dt className="text-muted">Tool</dt>
              <dd className="font-mono">
                {approval.tool_key} · {approval.tool_class}
              </dd>
            </div>
            <div>
              <dt className="text-muted">Resource</dt>
              <dd className="break-words">
                {approval.resource_type ?? "resource"}
                {approval.resource_label ? ` · ${approval.resource_label}` : ""}
              </dd>
            </div>
            <div>
              <dt className="text-muted">Model</dt>
              <dd className="font-mono break-all">
                {approval.model_id ? approval.model_id.slice(0, 8) : "the default model"}
              </dd>
            </div>
            <div>
              <dt className="text-muted">Requested</dt>
              <dd>{formatTimestamp(approval.created_at)}</dd>
            </div>
            <div>
              <dt className="text-muted">Expires</dt>
              <dd data-approval-expiry className="flex items-center gap-1">
                <Clock className="size-3" aria-hidden />
                {formatTimestamp(approval.expires_at)}
              </dd>
            </div>
            <div>
              <dt className="text-muted">Preview hash</dt>
              <dd className="font-mono break-all text-[11px]">{approval.preview_hash}</dd>
            </div>
            <div>
              <dt className="text-muted">Run</dt>
              <dd className="font-mono break-all">
                {approval.run_id ? (
                  <Link href={`/ai/runs/${approval.run_id}`} className="underline">
                    {approval.run_id.slice(0, 8)}
                  </Link>
                ) : (
                  "no run"
                )}
              </dd>
            </div>
            <div>
              <dt className="text-muted">Operations</dt>
              <dd className="tabular-nums">{approval.operation_count}</dd>
            </div>
            <div>
              <dt className="text-muted">Decision</dt>
              <dd data-approval-decided>
                {approval.decided_at
                  ? `${approval.decided_by ? approval.decided_by.slice(0, 8) : "someone"} · ${formatTimestamp(approval.decided_at)}`
                  : "not decided yet"}
              </dd>
            </div>
          </dl>
          {approval.decision_note ? (
            <p data-approval-note className="text-[12px] text-muted">
              Reason: {approval.decision_note}
            </p>
          ) : null}
        </header>

        {notice ? (
          <p
            data-approval-detail-notice
            className="flex items-center gap-1.5 border-b border-line bg-positive-soft px-4 py-2 text-[12.5px] text-positive"
          >
            <CircleCheck className="size-3.5" aria-hidden />
            {notice}
          </p>
        ) : null}
        {error ? (
          <p
            role="alert"
            data-approval-detail-error-banner
            className="flex items-center gap-1.5 border-b border-line bg-danger-soft px-4 py-2 text-[12.5px] text-danger"
          >
            <TriangleAlert className="size-3.5" aria-hidden />
            {error}
          </p>
        ) : null}
        {expired ? (
          <p
            data-approval-expired-banner
            className="flex items-center gap-1.5 border-b border-line bg-quiet-soft px-4 py-2 text-[12.5px] text-muted"
          >
            <Clock className="size-3.5" aria-hidden />
            This request expired before anybody decided it. It is read-only, and the parked run
            was released without the effect.
          </p>
        ) : null}

        {/* The proposed operations: the frozen diff, never a client-side re-preview. */}
        <div className="flex flex-col gap-2 px-4 py-4">
          <h3 className="text-[13px] font-semibold">Proposed operations</h3>
          {operations.length === 0 ? (
            <p data-diff-empty className="text-[12.5px] text-muted">
              The frozen preview names no operation. Nothing would be written, so approving changes
              nothing.
            </p>
          ) : (
            operations.map((operation, index) => (
              <OperationCard
                key={`${operation.action}-${operation.resource}-${index}`}
                operation={operation}
                index={index}
                showUnchanged={showUnchanged[index] ?? false}
                onToggleUnchanged={() =>
                  setShowUnchanged((open) => ({ ...open, [index]: !open[index] }))
                }
              />
            ))
          )}
        </div>

        {/* The danger zone. The consequence is stated in plain language and the phrase is a real
            text field showing what it must contain — never a checkbox. */}
        {approval.requires_confirmation || approval.irreversible ? (
          <div
            data-approval-danger-zone
            className="flex flex-col gap-2 border-t border-line bg-danger-soft/40 px-4 py-3"
          >
            <p className="flex items-center gap-1.5 text-[12.5px] font-medium text-danger">
              <ShieldAlert className="size-4" aria-hidden />
              This operation cannot be undone
            </p>
            <p className="text-[12.5px]">
              Approving applies the change immediately and permanently. There is no undo for this
              class, and the platform will not offer a &quot;don&apos;t ask again&quot; shortcut.
            </p>
            {approval.requires_confirmation && approval.confirmation_phrase ? (
              <label className="flex flex-col gap-1 sm:max-w-sm">
                <span className="text-[12px] font-medium">
                  Type <code className="font-mono">{approval.confirmation_phrase}</code> to confirm
                </span>
                <input
                  value={confirmation}
                  disabled={!canDecide || decided || busy}
                  onChange={(event) => setConfirmation(event.target.value)}
                  placeholder={approval.confirmation_phrase}
                  data-approval-confirmation
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15 disabled:opacity-50"
                />
                <span className="text-[11px] text-muted">
                  The API checks this phrase itself — an empty field is sent as no phrase at all,
                  and the request is refused with `confirmation_required`.
                </span>
              </label>
            ) : null}
          </div>
        ) : null}

        <div className="border-t border-line px-4 py-4">
          <h3 className="text-[13px] font-semibold">Timeline</h3>
          {audit.length === 0 ? (
            <p className="text-[12.5px] text-muted">No audit row has been written for this request.</p>
          ) : (
            <div className="mt-2">
              <Timeline rows={audit} />
            </div>
          )}
        </div>
      </section>

      {/* The decision bar. Sticky at the bottom on a phone so Approve and Reject are reachable
          without scrolling back up past the whole diff. */}
      <div
        data-approval-decision-bar
        className="fixed inset-x-0 bottom-0 z-30 flex flex-col gap-2 border-t border-line bg-surface px-4 py-3 lg:static lg:inset-auto lg:z-auto lg:rounded-xl lg:border"
      >
        {!canDecide ? (
          <p data-approval-decision-readonly className="text-[12px] text-caution">
            You are missing {DECIDE_KEY} — these buttons are disabled for your account, and the API
            refuses the same call with a 403 naming it.
          </p>
        ) : null}
        {decided && !expired ? (
          <p data-approval-already-decided className="flex items-center gap-1.5 text-[12px] text-muted">
            <CircleSlash className="size-3.5" aria-hidden />
            This request is already decided. A second decision changes nothing.
          </p>
        ) : null}
        <label className="flex flex-col gap-1">
          <span className="text-[12px] font-medium">Rejection reason (required to reject)</span>
          <input
            value={reason}
            disabled={!canDecide || decided || busy}
            onChange={(event) => setReason(event.target.value)}
            maxLength={500}
            placeholder="Why this should not happen."
            data-approval-reason
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15 disabled:opacity-50"
          />
        </label>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            disabled={!canDecide || decided || busy}
            onClick={() => void decide("approve")}
            data-approval-approve
            className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-quiet-soft disabled:text-muted"
          >
            {busy ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <CircleCheck className="size-3.5" aria-hidden />
            )}
            Approve and apply
          </button>
          <button
            type="button"
            disabled={!canDecide || decided || busy}
            onClick={() => void decide("reject")}
            data-approval-reject
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-danger transition hover:bg-danger-soft disabled:cursor-not-allowed disabled:opacity-50"
          >
            <CircleSlash className="size-3.5" aria-hidden />
            Reject
          </button>
          <button
            type="button"
            onClick={load}
            data-approval-reload
            className="ml-auto flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            <RotateCcw className="size-3.5" aria-hidden />
            Re-read
          </button>
        </div>
      </div>
    </div>
  );
}