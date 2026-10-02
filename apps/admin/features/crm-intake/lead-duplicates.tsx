"use client";

/**
 * `/crm/leads/duplicates` — the verdicts an operator can still reverse
 * (docs/requests/REQ-117, slice 1).
 *
 * A source's `reject_duplicate` policy files a submission as a duplicate instead of linking it,
 * and the point of this screen is that **the verdict is reversible and reversible in one place**:
 * the same endpoint the detail screen uses (`PATCH /api/v1/crm/leads/{id}` with a `contact_id`
 * or a status) links the row to a contact or keeps it separate, and the reason is on the row.
 *
 * It shows the key that matched, because "duplicate of somebody" is not a decision anybody can
 * check — "duplicate because `ayse@company.com` is already a contact" is. The score is shown
 * beside it, since the store keeps it on the trail.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { ArrowLeft, Link2, Loader2, RefreshCw, Split, TriangleAlert } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError } from "@/lib/api";
import { contactLabel, relativeInstant } from "@/lib/crm-intake";
import {
  fetchLeadDuplicates,
  resolveLeadDuplicate,
  type Lead,
} from "@/lib/crm-intake-api";

export function LeadDuplicates() {
  const [rows, setRows] = useState<Lead[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setRows(await fetchLeadDuplicates());
    } catch (caught) {
      setRows(null);
      setError(caught instanceof ApiError ? caught.message : "The queue could not be read.");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  /**
   * Reverse the verdict.
   *
   * A named endpoint rather than a `patchLead` here, and the difference is the whole point of
   * this screen: the panel does not know which contact the dedupe matched — the store does, and
   * only the store can attach it. The `Link` button used to send `PATCH { status: "assigned" }`,
   * which changed the status and nothing else; the row left the queue, the notice below said it
   * was "linked to the contact it matched", and no contact had been touched. The panel named an
   * outcome it could not produce.
   *
   * A refusal is a *saying*, not an error: the request was fine and the answer is "not this
   * row", so it reads as the reason rather than as a failure banner.
   */
  const decide = async (lead: Lead, keepSeparate: boolean) => {
    setBusy(lead.id);
    setError(null);
    setNotice(null);
    try {
      const answer = await resolveLeadDuplicate(
        lead.id,
        keepSeparate ? "keep_separate" : "link",
      );
      if (!answer.applied) {
        setError(answer.reason ?? "This duplicate verdict could not be reversed.");
        return;
      }
      setNotice(
        keepSeparate
          ? `${contactLabel(lead)} is kept as its own lead.`
          : `${contactLabel(lead)} is linked to the contact it matched.`,
      );
      await load();
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The decision could not be saved.");
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="flex flex-col gap-4" data-testid="crm-lead-duplicates">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <Link
            href="/crm/leads"
            className="inline-flex items-center gap-1.5 text-[12.5px] text-accent-strong hover:underline"
          >
            <ArrowLeft className="size-3.5" aria-hidden />
            Back to the inbox
          </Link>
          <h2 className="mt-1.5 text-[15px] font-semibold text-ink">Duplicate queue</h2>
          <p className="mt-0.5 max-w-2xl text-[12.5px] text-muted">
            Submissions a source&apos;s dedupe policy filed against an existing contact. Each row
            names the key that matched, so the verdict can be checked instead of trusted.
          </p>
        </div>
        <button
          type="button"
          onClick={() => void load()}
          data-duplicates-refresh
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted transition hover:text-ink"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Refresh
        </button>
      </header>

      {notice ? (
        <p
          role="status"
          data-duplicates-notice
          className="rounded-lg border border-positive/40 bg-positive-soft px-3 py-2 text-[12.5px] text-positive"
        >
          {notice}
        </p>
      ) : null}

      {error ? (
        <div
          role="alert"
          data-duplicates-error
          className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
        >
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
          <span className="flex-1">{error}</span>
          <button type="button" onClick={() => void load()} className="text-accent-strong hover:underline">
            Retry
          </button>
        </div>
      ) : null}

      <div className="overflow-hidden rounded-xl border border-line bg-surface">
        {loading && rows === null ? (
          <LoadingTable columns={5} rows={4} />
        ) : rows === null || rows.length === 0 ? (
          <EmptyState
            title="No duplicates waiting."
            hint="A submission only lands here when its source's dedupe policy is set to file duplicates instead of linking them. Sources that link keep their verdict on the lead itself."
            action={
              <Link
                href="/crm/leads"
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-accent-strong hover:underline"
              >
                Open the inbox
              </Link>
            }
          />
        ) : (
          <div className="overflow-x-auto">
            <table data-duplicates-table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] text-muted">
                  <th scope="col" className="px-3 py-2.5 font-medium">New lead</th>
                  <th scope="col" className="px-3 py-2.5 font-medium">Matched on</th>
                  <th scope="col" className="px-3 py-2.5 font-medium">Received</th>
                  <th scope="col" className="px-3 py-2.5 font-medium">Decision</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((lead) => (
                  <tr key={lead.id} data-duplicate-row={lead.id} className="border-b border-line/60 last:border-0">
                    <td className="px-3 py-2.5">
                      <Link
                        href={`/crm/leads/${lead.id}`}
                        data-duplicate-open={lead.id}
                        className="block max-w-56 truncate font-medium text-accent-strong hover:underline"
                      >
                        {contactLabel(lead)}
                      </Link>
                      {lead.email ? (
                        <span className="block max-w-56 truncate text-[11.5px] text-muted">{lead.email}</span>
                      ) : null}
                    </td>
                    <td className="px-3 py-2.5">
                      {/*
                        The key AND the score, and the reason a refusal shows one and the other
                        hides: "duplicate of somebody" is not a decision an operator can check,
                        while "`ayse@company.com` already exists, matched on e-mail at 0.95" is.
                        A score with no key beside it is a number nobody can argue with, and a key
                        with no score is a claim. The score was not stored at all before this
                        tick, so a duplicate filed last month could not be re-examined today.
                      */}
                      <span className="block max-w-48 truncate font-mono text-[11.5px] text-muted">
                        {lead.dedupe_key ?? "—"}
                        {lead.dedupe_score !== null ? (
                          <span
                            data-duplicate-score={lead.id}
                            className="ml-1.5 text-muted/70"
                            title={`matched with confidence ${lead.dedupe_score.toFixed(2)}`}
                          >
                            {lead.dedupe_score.toFixed(2)}
                          </span>
                        ) : null}
                      </span>
                      {lead.rejection_reason ? (
                        <span className="block max-w-48 truncate text-[11.5px] text-muted">
                          {lead.rejection_reason}
                        </span>
                      ) : null}
                      {/*
                        A row with no recorded contact cannot be linked, and the button that
                        would fail is replaced by a sentence that says so. "Link" on such a row
                        is the ambiguous verdict (several contacts matched) — the detail screen
                        lists them, and a disabled control without a reason is the one thing
                        this REQ forbids.
                      */}
                      {lead.dedupe_contact_id === null ? (
                        <span
                          data-duplicate-nomatch={lead.id}
                          className="mt-0.5 block max-w-56 text-[11px] text-muted"
                        >
                          No single contact was recorded for this match — open the lead to choose one.
                        </span>
                      ) : null}
                    </td>
                    <td className="px-3 py-2.5 text-muted tabular-nums">{relativeInstant(lead.received_at)}</td>
                    <td className="px-3 py-2.5">
                      <div className="flex flex-wrap items-center gap-1.5">
                        <button
                          type="button"
                          data-duplicate-link={lead.id}
                          data-qa-guard="crm-intake-depth"
                          // Two reasons it cannot be pressed, and neither is "the network is
                          // slow": a decision in flight, and a match with no single contact.
                          // The second is explained in the cell above, so the disabled state is
                          // never a shrug.
                          disabled={busy === lead.id || lead.dedupe_contact_id === null}
                          onClick={() => void decide(lead, false)}
                          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:text-ink disabled:opacity-50"
                        >
                          {busy === lead.id ? (
                            <Loader2 className="size-3.5 animate-spin" aria-hidden />
                          ) : (
                            <Link2 className="size-3.5" aria-hidden />
                          )}
                          Link
                        </button>
                        <button
                          type="button"
                          data-duplicate-keep={lead.id}
                          data-qa-guard="crm-intake-depth"
                          disabled={busy === lead.id}
                          onClick={() => void decide(lead, true)}
                          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted transition hover:text-ink disabled:opacity-50"
                        >
                          <Split className="size-3.5" aria-hidden />
                          Keep separate
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}
