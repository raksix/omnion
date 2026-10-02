"use client";

/**
 * The approval inbox (REQ-052, slice 3): `/sales/approvals`.
 *
 * A manager's and a seller's view of the same table, separated by the four scopes the module
 * offers. Five decisions this screen makes, and each of them is a way it could have lied:
 *
 * * **The discount and the limit are printed together.** "Approve" on a row that says 22% while
 *   the policy says 15% is a decision made on a number; showing the threshold is what makes it a
 *   decision made on a *policy*.
 * * **Approve and Reject are per row, never bulk.** REQ-059's spec says it outright and this
 *   screen is where it would be broken: a bulk approve is a checkbox with a company's name on it.
 * * **A rejection cannot be submitted without a reason.** The comment box is required, the
 *   button says why, and a server refusal keeps what was typed — a seller whose comment vanished
 *   on a failed submit writes it twice and learns to write less.
 * * **A failed decision does not reload the list.** The row stays where it was with the error on
 *   it, because the alternative silently discards the reason somebody just typed.
 * * **You cannot approve your own request.** The buttons are simply not drawn for that row, and
 *   the refusal is still implemented on the server — hiding a button is a courtesy, not a rule.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";

import { Check, ExternalLink, Loader2, ShieldQuestion, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import { formatMoney } from "@/lib/sales";
import {
  approvalEmptyCopy,
  approvalStatusTone,
  cancelSalesApproval,
  decideSalesApproval,
  fetchSalesApprovals,
  type SalesApproval,
  type SalesApprovalScope,
  type SalesApprovalStatus,
} from "@/lib/sales-quotes";

import { SalesToolbar, useSales, useSalesKeyboard } from "./sales-parts";

/** How many columns the table draws, and what the header prints. */
const COLUMNS = 7;
const HEADERS = ["Quote", "Discount", "Amount", "Requested by", "Status", "Raised", ""];

/** The four tabs, with the module's own scope names. */
const SCOPES: { value: SalesApprovalScope; label: string }[] = [
  { value: "pending", label: "Waiting" },
  { value: "requested_by_me", label: "Mine" },
  { value: "decided", label: "Decided" },
  { value: "all", label: "All" },
];

/** The dialog a decision happens in — one at a time, so two comments cannot collide. */
type Pending = { approval: SalesApproval; verb: "approve" | "reject" } | null;

export function ApprovalsView() {
  const { organizationId } = useSales();
  const params = useSearchParams();
  const router = useRouter();
  const searchRef = useRef<HTMLInputElement>(null);

  const scope = (params.get("scope") as SalesApprovalScope | null) ?? "pending";
  const search = params.get("search") ?? "";
  const statusFilter = (params.get("status") as SalesApprovalStatus | null) ?? "";

  const [rows, setRows] = useState<SalesApproval[] | null>(null);
  const [error, setError] = useState<ScreenErrorValue | null>(null);
  const [pending, setPending] = useState<Pending>(null);
  const [comment, setComment] = useState("");
  const [busy, setBusy] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);
  const [selected, setSelected] = useState(0);

  const load = useCallback(async () => {
    setError(null);
    try {
      const page = await fetchSalesApprovals({
        scope,
        search: search.trim() || undefined,
        status: statusFilter || undefined,
        organization_id: organizationId,
      });
      setRows(page.items);
      setSelected(0);
    } catch (cause) {
      setError(toScreenError(cause, "The approval inbox could not be read."));
      setRows([]);
    }
  }, [organizationId, scope, search, statusFilter]);

  useEffect(() => {
    void load();
  }, [load]);

  const setParam = useCallback(
    (name: string, value: string) => {
      const next = new URLSearchParams(params.toString());
      if (value === "") next.delete(name);
      else next.set(name, value);
      const text = next.toString();
      router.replace(text ? `?${text}` : "?", { scroll: false });
    },
    [params, router],
  );

  const open = useCallback((approval: SalesApproval, verb: "approve" | "reject") => {
    setPending({ approval, verb });
    setComment("");
    setFormError(null);
  }, []);

  const decide = useCallback(async () => {
    if (!pending) return;
    setBusy(true);
    setFormError(null);
    try {
      const decided = await decideSalesApproval(
        pending.approval.id,
        pending.verb,
        comment,
        organizationId,
      );
      setRows((current) =>
        (current ?? []).map((row) => (row.id === decided.id ? decided : row)),
      );
      setPending(null);
      setComment("");
      setRowError(null);
    } catch (cause) {
      // The comment stays in the box on purpose: a seller or manager who has explained a
      // decision once should not have to explain it again because the network blinked.
      setFormError(cause instanceof Error ? cause.message : "The decision could not be recorded.");
    } finally {
      setBusy(false);
    }
  }, [comment, organizationId, pending]);

  const withdraw = useCallback(
    async (approval: SalesApproval) => {
      setRowError(null);
      try {
        const cancelled = await cancelSalesApproval(approval.id, organizationId);
        setRows((current) =>
          (current ?? []).map((row) => (row.id === cancelled.id ? cancelled : row)),
        );
      } catch (cause) {
        setRowError(cause instanceof Error ? cause.message : "The request could not be withdrawn.");
      }
    },
    [organizationId],
  );

  // `a` approves and `r` rejects the focused row, matching the spec's inbox keys. The comment
  // step is where they land, so a keyboard decision is never a one-key accident.
  useSalesKeyboard(
    {
      count: rows?.length ?? 0,
      selected,
      onSelect: setSelected,
      onOpen: (index) => {
        const row = rows?.[index];
        if (row) router.push(row.subject_url);
      },
    },
    searchRef,
  );

  // `a` and `r` decide the focused row. They live in a window listener rather than in the shared
  // hook because that hook is a section contract shared by four lists, and this is the only one
  // of them with a decision to make.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target?.isContentEditable
      ) {
        return;
      }
      if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
        if (pending) {
          event.preventDefault();
          void decide();
        }
        return;
      }
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      const row = rows?.[selected];
      if (!row || row.status !== "pending") return;
      if (event.key === "a") {
        event.preventDefault();
        open(row, "approve");
      } else if (event.key === "r") {
        event.preventDefault();
        open(row, "reject");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [decide, open, pending, rows, selected]);

  const open_ = pending;
  const needsComment = open_?.verb === "reject";

  return (
    <section aria-label="Approval requests">
      <header className="mb-3 flex flex-wrap items-baseline justify-between gap-2">
        <div>
          <h1 className="text-[15px] font-medium">Approvals</h1>
          <p className="text-[12.5px] text-muted">
            A quote whose largest line discount is over the limit waits for a decision here.
          </p>
        </div>
        <div role="tablist" aria-label="Inbox scope" className="flex flex-wrap gap-1">
          {SCOPES.map((entry) => (
            <button
              key={entry.value}
              type="button"
              role="tab"
              aria-selected={scope === entry.value}
              data-qa-approval-scope={entry.value}
              onClick={() => setParam("scope", entry.value === "pending" ? "" : entry.value)}
              className={`rounded-md border px-2 py-1 text-[12px] ${
                scope === entry.value
                  ? "border-ink-soft bg-panel text-ink"
                  : "border-line text-muted hover:text-ink"
              }`}
            >
              {entry.label}
            </button>
          ))}
        </div>
      </header>

      <SalesToolbar
        search={search}
        onSearchChange={(value) => setParam("search", value)}
        searchRef={searchRef}
        count={rows?.length ?? 0}
        filtered={Boolean(search.trim() || statusFilter)}
      >
        <select
          aria-label="Filter by status"
          data-qa-approval-status
          value={statusFilter}
          onChange={(event) => setParam("status", event.target.value)}
          className="rounded-md border border-line bg-panel px-2 py-1.5 text-[12.5px]"
        >
          <option value="">Every status</option>
          <option value="pending">Waiting</option>
          <option value="approved">Approved</option>
          <option value="rejected">Rejected</option>
          <option value="cancelled">Withdrawn</option>
        </select>
      </SalesToolbar>

      {error ? <ErrorState error={error} onRetry={() => void load()} /> : null}
      {rowError ? (
        <p role="alert" className="mb-2 text-[12.5px] text-red" data-qa-approval-row-error>
          {rowError}
        </p>
      ) : null}

      {!error && rows === null ? <LoadingTable columns={COLUMNS} rows={6} /> : null}

      {rows && rows.length === 0 ? (
        <EmptyState
          title={approvalEmptyCopy(scope)}
          hint={
            search.trim() || statusFilter
              ? "No request matches these filters."
              : "Discounts inside the limit are sent without one."
          }
        />
      ) : null}

      {rows && rows.length > 0 ? (
        <div className="overflow-x-auto">
          <table className="w-full min-w-[46rem] border-collapse text-[13px]">
            <thead>
              <tr className="border-b border-line text-left text-[12px] text-muted">
                {HEADERS.map((column) => (
                  <th key={column} scope="col" className="py-2 pr-3 font-medium">
                    {column}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {rows.map((row, index) => {
                const isSelected = index === selected;
                const deciding = pending?.approval.id === row.id;
                return (
                  <tr
                    key={row.id}
                    data-qa-approval-row={row.quote_number}
                    aria-selected={isSelected}
                    onClick={() => setSelected(index)}
                    className={`border-b border-line/60 ${
                      isSelected ? "bg-panel" : "hover:bg-panel/60"
                    }`}
                  >
                    <td className="py-2 pr-3">
                      <Link
                        href={row.subject_url}
                        className="inline-flex items-center gap-1 font-medium hover:underline"
                      >
                        {row.quote_number}
                        <ExternalLink size={12} className="text-muted" aria-hidden />
                      </Link>
                      <span className="block text-[12px] text-muted">{row.quote_title}</span>
                    </td>
                    <td className="py-2 pr-3" data-qa-approval-discount>
                      <span className="font-medium">{row.discount_percent}%</span>
                      <span className="block text-[12px] text-muted">
                        limit {row.threshold_percent}%
                      </span>
                    </td>
                    <td className="py-2 pr-3">{formatMoney(row.grand_total, row.currency)}</td>
                    <td className="py-2 pr-3">{row.requester_name}</td>
                    <td className="py-2 pr-3">
                      <span
                        className={`rounded-md border px-1.5 py-0.5 text-[11.5px] ${approvalStatusTone(row.status)}`}
                      >
                        {row.status}
                      </span>
                      {row.decision?.comment ? (
                        <span className="mt-0.5 block max-w-[16rem] text-[12px] text-muted">
                          “{row.decision.comment}”
                        </span>
                      ) : null}
                    </td>
                    <td className="py-2 pr-3 text-[12px] text-muted">
                      {new Date(row.created_at).toISOString().slice(0, 10)}
                    </td>
                    <td className="py-2 pr-3 text-right">
                      {row.status === "pending" ? (
                        <div className="flex justify-end gap-1">
                          <button
                            type="button"
                            data-qa-approval-approve={row.quote_number}
                            onClick={(event) => {
                              event.stopPropagation();
                              open(row, "approve");
                            }}
                            className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-panel"
                          >
                            <Check size={13} aria-hidden /> Approve
                          </button>
                          <button
                            type="button"
                            data-qa-approval-reject={row.quote_number}
                            onClick={(event) => {
                              event.stopPropagation();
                              open(row, "reject");
                            }}
                            className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] text-red hover:bg-panel"
                          >
                            <X size={13} aria-hidden /> Reject
                          </button>
                        </div>
                      ) : deciding ? null : (
                        <span className="text-[12px] text-muted">decided</span>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      ) : null}

      {open_ ? (
        <div
          role="dialog"
          aria-modal="true"
          aria-label={`${open_.verb === "approve" ? "Approve" : "Reject"} ${open_.approval.quote_number}`}
          data-qa-approval-dialog
          className="fixed inset-0 z-50 flex items-end justify-center bg-black/40 p-4 sm:items-center"
        >
          <div className="w-full max-w-md rounded-lg border border-line bg-panel p-4 shadow-lg">
            <h2 className="flex items-center gap-2 text-[14px] font-medium">
              <ShieldQuestion size={15} aria-hidden />
              {open_.verb === "approve" ? "Approve" : "Reject"} {open_.approval.quote_number}
            </h2>
            <p className="mt-1 text-[12.5px] text-muted">
              {open_.approval.requester_name} is asking to send this quote with a{" "}
              <strong className="text-ink">{open_.approval.discount_percent}%</strong> discount,
              over the {open_.approval.threshold_percent}% limit.
            </p>
            {open_.approval.note ? (
              <p className="mt-2 rounded-md border border-line bg-canvas px-2 py-1.5 text-[12.5px]">
                “{open_.approval.note}”
              </p>
            ) : null}

            <label className="mt-3 block text-[12.5px] font-medium" htmlFor="approval-comment">
              {needsComment ? "Why are you rejecting it? (required)" : "Note (optional)"}
            </label>
            <textarea
              id="approval-comment"
              data-qa-approval-comment
              value={comment}
              onChange={(event) => setComment(event.target.value)}
              rows={3}
              className="mt-1 w-full rounded-md border border-line bg-canvas px-2 py-1.5 text-[13px] outline-none focus:border-ink-soft"
            />

            {formError ? (
              <p role="alert" className="mt-2 text-[12.5px] text-red" data-qa-approval-form-error>
                {formError}
              </p>
            ) : null}

            <div className="mt-3 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setPending(null)}
                className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
              >
                Keep it open
              </button>
              <button
                type="button"
                disabled={busy || (needsComment && comment.trim() === "")}
                data-qa-approval-confirm
                onClick={() => void decide()}
                className="inline-flex items-center gap-1.5 rounded-md border border-ink bg-ink px-3 py-1.5 text-[12.5px] text-canvas disabled:opacity-50"
              >
                {busy ? <Loader2 size={13} className="animate-spin" aria-hidden /> : null}
                {open_.verb === "approve" ? "Approve and allow sending" : "Reject the request"}
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </section>
  );
}

/** The row's own withdraw button, rendered by the detail screen rather than the inbox. */
export function WithdrawApprovalButton({
  approval,
  onDone,
  organizationId,
}: {
  approval: SalesApproval;
  onDone: (next: SalesApproval) => void;
  organizationId: string | null;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  return (
    <span className="inline-flex flex-col items-end gap-1">
      <button
        type="button"
        data-qa-approval-withdraw
        disabled={busy}
        onClick={async () => {
          setBusy(true);
          setError(null);
          try {
            onDone(await cancelSalesApproval(approval.id, organizationId));
          } catch (cause) {
            setError(cause instanceof Error ? cause.message : "It could not be withdrawn.");
          } finally {
            setBusy(false);
          }
        }}
        className="rounded-md border border-line px-2 py-1 text-[12px] hover:bg-panel disabled:opacity-50"
      >
        {busy ? <Loader2 size={13} className="animate-spin" aria-hidden /> : "Withdraw request"}
      </button>
      {error ? <span className="text-[12px] text-red">{error}</span> : null}
    </span>
  );
}
