"use client";

/**
 * `/hr/leave/{id}` — the request, its balance, its timeline and the decision panel (REQ-055, slice 2b).
 *
 * The decision panel is driven by `can_decide`, not by the status. That flag is the server's answer
 * to "may *this* caller still decide *this* request", and it accounts for the two facts a status
 * cannot: the caller's own request (nobody approves their own holiday) and a request already
 * decided. A screen that inferred the panel from `leave_status === "pending"` puts an Approve
 * button on your own request — the button then either 403s on click, or worse, succeeds.
 *
 * The panel also **disappears once a decision is made** rather than disabling itself, because a
 * greyed-out "Decide" is a question ("can I still?") with no answer on the page. The timeline
 * carries who decided, when and with what comment, so the record is on the screen rather than in a
 * history tab.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { ArrowLeft, Ban, Check, Loader2, XCircle } from "lucide-react";

import { ErrorState, ErrorStrip, toScreenError, type ScreenErrorValue } from "@/components/error-state";

import {
  cancelRequest,
  decideRequest,
  fetchRequest,
  type RequestDetail,
} from "@/lib/hr";

import { BalanceCardView, DaysCell, LeaveStatusBadge } from "./hr-parts";

/** The sentences a timeline step is read as. */
const STEP_LABELS: Record<string, string> = {
  requested: "requested",
  approved: "approved",
  rejected: "rejected",
  cancelled: "cancelled",
};

export function LeaveDetailView({ requestId }: { requestId: string }) {
  const router = useRouter();
  const [detail, setDetail] = useState<RequestDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);

  const [comment, setComment] = useState("");
  const [working, setWorking] = useState<"approved" | "rejected" | "cancelled" | null>(null);
  const [actionError, setActionError] = useState<ScreenErrorValue>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setDetail(await fetchRequest(requestId));
    } catch (failure) {
      setError(toScreenError(failure, "The leave request could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [requestId]);

  useEffect(() => {
    void load();
  }, [load]);

  const decide = async (decision: "approved" | "rejected") => {
    setWorking(decision);
    setActionError(null);
    try {
      await decideRequest(requestId, { decision, comment: comment.trim() || undefined });
      setComment("");
      await load();
    } catch (failure) {
      setActionError(toScreenError(failure, "The decision could not be saved."));
    } finally {
      setWorking(null);
    }
  };

  const cancel = async () => {
    setWorking("cancelled");
    setActionError(null);
    try {
      await cancelRequest(requestId);
      await load();
    } catch (failure) {
      setActionError(toScreenError(failure, "The request could not be cancelled."));
    } finally {
      setWorking(null);
    }
  };

  if (loading) {
    return (
      <div className="space-y-3" aria-busy="true">
        <div className="h-5 w-48 animate-pulse rounded bg-quiet-soft" />
        <div className="h-24 w-full max-w-md animate-pulse rounded bg-quiet-soft" />
      </div>
    );
  }

  if (error || !detail) {
    return (
      <ErrorState
        error={error ?? "The request is not available."}
        onRetry={() => void load()}
        qa="hr-leave-detail-error"
      />
    );
  }

  return (
    <div className="max-w-3xl space-y-6">
      <header className="space-y-2">
        <Link
          href="/hr/leave"
          className="inline-flex items-center gap-1.5 text-[13px] text-muted hover:text-foreground"
        >
          <ArrowLeft className="h-3.5 w-3.5" aria-hidden />
          Leave
        </Link>
        <div className="flex flex-wrap items-center gap-3">
          <h1 className="text-xl font-semibold tracking-tight" data-qa-hr-detail-name>
            {detail.employee_name}
          </h1>
          <LeaveStatusBadge status={detail.leave_status} />
        </div>
        <p className="text-sm text-muted">
          {detail.leave_type_name} · {detail.starts_on} → {detail.ends_on} ·{" "}
          <DaysCell days={detail.days} className="inline" />
          {detail.half_day ? " · half day" : ""}
        </p>
        {detail.reason ? (
          <p className="rounded-md border border-border px-3 py-2 text-[13px]" data-qa-hr-detail-reason>
            {detail.reason}
          </p>
        ) : null}
      </header>

      <section aria-labelledby="hr-detail-balance">
        <h2 id="hr-detail-balance" className="mb-2 text-[15px] font-medium">
          Balance · {detail.balance.balance_year}
        </h2>
        <BalanceCardView card={detail.balance} />
      </section>

      <section aria-labelledby="hr-detail-timeline">
        <h2 id="hr-detail-timeline" className="mb-2 text-[15px] font-medium">
          Timeline
        </h2>
        <ol className="space-y-2" data-qa-hr-detail-timeline>
          {detail.timeline.map((step, index) => (
            <li key={`${step.kind}-${step.at}-${index}`} className="flex gap-3 text-[13px]">
              <span
                aria-hidden
                className={`mt-1.5 h-2 w-2 shrink-0 rounded-full ${
                  step.kind === "approved"
                    ? "bg-emerald-500"
                    : step.kind === "rejected"
                      ? "bg-destructive"
                      : step.kind === "cancelled"
                        ? "bg-muted"
                        : "bg-primary"
                }`}
              />
              <div className="min-w-0">
                <p className="font-medium">
                  {STEP_LABELS[step.kind] ?? step.kind}
                  <span className="ml-2 font-normal text-muted">{step.at.slice(0, 16).replace("T", " ")}</span>
                </p>
                {step.comment ? (
                  <p className="text-muted" data-qa-hr-detail-comment={step.kind}>
                    {step.comment}
                  </p>
                ) : null}
              </div>
            </li>
          ))}
        </ol>
      </section>

      {actionError ? <ErrorStrip error={actionError} onRetry={() => setActionError(null)} qa="hr-detail-action-error" /> : null}

      {detail.can_decide ? (
        <section
          className="space-y-3 rounded-lg border border-border p-4"
          aria-labelledby="hr-detail-decide"
          data-qa-hr-detail-decision
        >
          <h2 id="hr-detail-decide" className="text-[15px] font-medium">
            Decision
          </h2>
          <label htmlFor="hr-detail-comment" className="block text-[13px] font-medium">
            Comment <span className="font-normal text-muted">(shown on the timeline)</span>
          </label>
          <textarea
            id="hr-detail-comment"
            rows={2}
            value={comment}
            onChange={(event) => setComment(event.target.value)}
            data-qa-hr-detail-comment-input
            className="w-full rounded-md border border-border bg-transparent px-2.5 py-2 text-sm"
          />
          <div className="flex flex-wrap gap-2">
            <button
              type="button"
              onClick={() => void decide("approved")}
              disabled={working !== null}
              data-qa-hr-detail-approve
              className="inline-flex h-9 items-center gap-2 rounded-md bg-primary px-3 text-sm text-primary-foreground disabled:opacity-60"
            >
              {working === "approved" ? (
                <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
              ) : (
                <Check className="h-4 w-4" aria-hidden />
              )}
              Approve
            </button>
            <button
              type="button"
              onClick={() => void decide("rejected")}
              disabled={working !== null}
              data-qa-hr-detail-reject
              className="inline-flex h-9 items-center gap-2 rounded-md border border-border px-3 text-sm disabled:opacity-60"
            >
              {working === "rejected" ? (
                <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
              ) : (
                <XCircle className="h-4 w-4" aria-hidden />
              )}
              Reject
            </button>
          </div>
        </section>
      ) : detail.leave_status === "pending" ? (
        <section className="space-y-2 rounded-lg border border-border p-4" data-qa-hr-detail-cancel>
          <h2 className="text-[15px] font-medium">Withdraw</h2>
          <p className="text-[13px] text-muted">
            This request is pending and cannot be decided by you. Withdrawing it releases the days it
            was holding.
          </p>
          <button
            type="button"
            onClick={() => void cancel()}
            disabled={working !== null}
            data-qa-hr-detail-cancel-button
            className="inline-flex h-9 items-center gap-2 rounded-md border border-border px-3 text-sm disabled:opacity-60"
          >
            {working === "cancelled" ? (
              <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
            ) : (
              <Ban className="h-4 w-4" aria-hidden />
            )}
            Withdraw the request
          </button>
        </section>
      ) : (
        <p className="text-[13px] text-muted" data-qa-hr-detail-closed>
          This request is closed. The timeline above is the record.
        </p>
      )}

      <p className="text-[12px] text-muted">
        Raised {detail.created_at.slice(0, 16).replace("T", " ")}
        {detail.decided_by_name ? ` · decided by ${detail.decided_by_name}` : ""}
        <button
          type="button"
          onClick={() => router.refresh()}
          className="ml-2 underline decoration-dotted"
        >
          Refresh
        </button>
      </p>
    </div>
  );
}
