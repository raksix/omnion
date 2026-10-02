"use client";

/**
 * `/crm/leads/{id}` — one lead, in full (docs/requests/REQ-117, slice 1).
 *
 * The detail is three columns on a wide screen and one column on a phone, and each column has
 * a job that the others cannot do:
 *
 * * **Left — the evidence.** The payload as submitted, the attribution split into first touch
 *   and the most recent visit, the consent quote and the spam verdict. None of it is editable,
 *   and that is not a missing feature: `PATCH /api/v1/crm/leads/{id}` has no `payload`, no
 *   `received_at` and no `spam_score`, because a verdict its own subject can rewrite is not a
 *   verdict. The screen says so once, in the raw-payload toggle's own line, instead of leaving
 *   the reader to wonder why there is no edit button here.
 * * **Right — the work.** `Mark responded` (idempotent on the first instant, so a double click
 *   cannot rewrite the measurement an SLA report rests on), `Reject` with a required reason,
 *   and the match panel with the key the dedupe passed actually matched on.
 * * **The conversion stepper reports what the server knows.** `Convert` is a real call to
 *   `crm.leads.convert` and the four documented steps come back *computed* from the lead row
 *   and from which modules this deployment actually has — so "the sales module is not
 *   installed here" and "nobody pressed the button yet" are different sentences, each with
 *   its own note. The previous build hard-coded four strings saying the buttons did not
 *   exist; a panel that says "waiting" for a module that will never arrive is a bug report
 *   against a working platform, and a dead button is the one thing a panel must not ship.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { useParams, useRouter } from "next/navigation";
import {
  ArrowLeft,
  ArrowRight,
  Ban,
  Check,
  FileWarning,
  Loader2,
  ShieldAlert,
  Trash2,
  TriangleAlert,
  UserRoundCheck,
} from "lucide-react";

import { LoadingTable } from "@/components/loading-table";
import { ApiError } from "@/lib/api";
import {
  AUTORESPONDER_STATE_LABEL,
  DECISION_LABEL,
  LEAD_EVENT_LABEL,
  LEAD_STATUSES,
  LEAD_STATUS_LABEL,
  LEAD_STATUS_TONE,
  SLA_STATE_LABEL,
  SLA_STATE_TONE,
  contactLabel,
  absoluteInstant,
  countdown,
  ownerLabel,
  slaState,
} from "@/lib/crm-intake";
import {
  convertLead,
  deleteLead,
  fetchLead,
  markLeadResponded,
  markLeadSpam,
  rejectLead,
  patchLead,
  type Lead,
  type LeadConversion,
  type LeadDetail as LeadDetailBody,
  type LeadStep,
  assignLead,
  fetchLeadOwners,
  type LeadOwner,
} from "@/lib/crm-intake-api";

type Draft = {
  first_name: string;
  last_name: string;
  email: string;
  phone: string;
  company_name: string;
  product_interest: string;
  message: string;
  status: string;
};

function draftOf(lead: Lead): Draft {
  return {
    first_name: lead.first_name ?? "",
    last_name: lead.last_name ?? "",
    email: lead.email ?? "",
    phone: lead.phone ?? "",
    company_name: lead.company_name ?? "",
    product_interest: lead.product_interest ?? "",
    message: lead.message ?? "",
    status: lead.status,
  };
}

export function LeadDetail() {
  const params = useParams<{ id: string }>();
  const router = useRouter();
  const id = params?.id ?? "";

  const [detail, setDetail] = useState<LeadDetailBody | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [showRaw, setShowRaw] = useState(false);
  const [rejectReason, setRejectReason] = useState("");
  const [rejecting, setRejecting] = useState(false);
  // The hand-over panel is *closed* until asked for, because the reason is a required field
  // and an always-open pair of inputs sitting above the other actions makes the panel read as
  // an assignment form when it is mostly not.
  const [assigning, setAssigning] = useState(false);
  const [assignOwner, setAssignOwner] = useState("");
  const [assignReason, setAssignReason] = useState("");
  // The roster is fetched once per detail read, not per panel open, and a failure is
  // *degraded* rather than fatal: the panel still works with a typed id, it just says why the
  // list is empty. A roster that fails to load must not take the lead page down with it.
  const [owners, setOwners] = useState<LeadOwner[] | null>(null);
  const [ownersError, setOwnersError] = useState<string | null>(null);
  // What the last `Convert` actually produced, kept apart from `notice` because the
  // interesting half is the `deal_skipped` sentence — "the CRM module is not installed" is
  // information the operator needs after the toast has gone.
  const [conversion, setConversion] = useState<LeadConversion | null>(null);

  const load = useCallback(async () => {
    if (!id) return;
    setLoading(true);
    setError(null);
    // The roster is a separate call that must not decide whether the lead renders: it is
    // beside the answer, not part of it.
    void fetchLeadOwners()
      .then((rows) => {
        setOwners(rows);
        setOwnersError(null);
      })
      .catch(() => {
        setOwners([]);
        setOwnersError("The list of owners could not be read, so the picker is showing nothing.");
      });
    try {
      const answer = await fetchLead(id);
      setDetail(answer);
      setDraft(draftOf(answer.lead));
    } catch (caught) {
      setDetail(null);
      setError(
        caught instanceof ApiError
          ? caught.message
          : "This lead could not be read.",
      );
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  /**
   * Convert, and reload.
   *
   * It reloads rather than patching the row in place, and the reason is that the step plan
   * *changes* — the opportunity becomes done and the quotation becomes reachable — so a
   * local merge of the returned lead would leave the stepper showing the pre-conversion
   * state next to a post-conversion lead. A stale stepper is worse than a re-read.
   */
  const convert = async () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const answer = await convertLead(id);
      setConversion(answer);
      setNotice(
        answer.deal_skipped
          ? "The contact is ready. The opportunity is not."
          : "The lead is now a contact and an opportunity.",
      );
      await load();
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The lead could not be converted.",
      );
    } finally {
      setBusy(false);
    }
  };

  const run = async (action: () => Promise<Lead>, message: string) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const updated = await action();
      setNotice(message);
      setDetail((previous) => (previous ? { ...previous, lead: updated } : previous));
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The action could not be completed.");
    } finally {
      setBusy(false);
    }
  };

  const save = async () => {
    if (!draft || !detail) return;
    // A cleared e-mail *and* a cleared phone is refused by the store, so the screen refuses it
    // first with the same rule in the panel's words — a validation message that arrives after
    // a round trip is a worse experience than one that arrives before it.
    if (!draft.email.trim() && !draft.phone.trim()) {
      setError("A lead needs an e-mail or a phone — the edit cannot clear both.");
      return;
    }
    await run(
      () =>
        patchLead(detail.lead.id, {
          first_name: draft.first_name.trim() || null,
          last_name: draft.last_name.trim() || null,
          email: draft.email.trim() || null,
          phone: draft.phone.trim() || null,
          company_name: draft.company_name.trim() || null,
          product_interest: draft.product_interest.trim() || null,
          message: draft.message.trim() || null,
          status: draft.status,
        }),
      "The lead was saved.",
    );
  };

  const onDelete = async () => {
    if (!detail) return;
    if (!confirm("Delete this lead and its payload? The deletion is recorded in the audit log.")) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await deleteLead(detail.lead.id);
      router.replace("/crm/leads");
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The lead could not be deleted.");
      setBusy(false);
    }
  };

  if (loading && !detail) {
    return (
      <div className="rounded-xl border border-line bg-surface" data-lead-detail-loading>
        <LoadingTable columns={4} rows={5} />
      </div>
    );
  }

  if (!detail) {
    return (
      <div className="flex flex-col items-start gap-3" data-lead-detail-missing>
        <Link
          href="/crm/leads"
          className="inline-flex items-center gap-1.5 text-[12.5px] text-accent-strong hover:underline"
        >
          <ArrowLeft className="size-3.5" aria-hidden />
          Back to the inbox
        </Link>
        <p role="alert" className="text-[13px] text-muted">
          {error ?? "This lead could not be found."}
        </p>
      </div>
    );
  }

  const lead = detail.lead;
  const state = slaState(lead);
  // Ids to names, built once per roster. The header chip, the trail and the picker all read
  // this one map, which is the only way the three can be guaranteed to agree — three separate
  // lookups is three chances for the header to say one thing and the timeline another.
  const ownersById = new Map((owners ?? []).map((owner) => [owner.id, owner.label]));
  const remaining = countdown(lead.first_response_due_at);
  const payloadEntries = Object.entries(detail.payload ?? {});

  return (
    <div className="flex flex-col gap-4" data-testid="crm-lead-detail" data-lead-id={lead.id}>
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <Link
            href="/crm/leads"
            className="inline-flex items-center gap-1.5 text-[12.5px] text-accent-strong hover:underline"
          >
            <ArrowLeft className="size-3.5" aria-hidden />
            Back to the inbox
          </Link>
          <h2 className="mt-1.5 text-[15px] font-semibold text-ink" data-lead-name>
            {contactLabel(lead)}
          </h2>
          <p className="mt-0.5 text-[12px] text-muted">
            Received {absoluteInstant(lead.received_at)}
            {lead.product_interest ? ` · ${lead.product_interest}` : ""}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <span
            data-lead-status={lead.status}
            className={`inline-flex items-center rounded-full px-2.5 py-1 text-[11.5px] font-medium ${LEAD_STATUS_TONE[lead.status] ?? "bg-quiet-soft text-muted"}`}
          >
            {LEAD_STATUS_LABEL[lead.status] ?? lead.status}
          </span>
          <span
            data-sla={state}
            className={`inline-flex items-center gap-1.5 rounded-full px-2.5 py-1 text-[11.5px] ${SLA_STATE_TONE[state]}`}
          >
            {SLA_STATE_LABEL[state]}
            {remaining && state !== "none" && state !== "met" ? ` · ${remaining}` : ""}
          </span>
          {/* The owner reads as a name or a badge, never as a boolean. "Assigned" was the
              truth and useless: the whole question on this screen is *who*, and an answer
              that contains no name makes the header a place to look and find nothing. */}
          <span
            data-lead-owner-chip
            className={
              lead.owner_user_id
                ? "inline-flex items-center gap-1.5 rounded-full border border-line bg-surface px-2.5 py-1 text-[11.5px] text-muted"
                : "inline-flex items-center gap-1.5 rounded-full bg-caution-soft px-2.5 py-1 text-[11.5px] text-caution"
            }
          >
            <UserRoundCheck className="size-3.5" aria-hidden />
            {ownerLabel(ownersById, lead.owner_user_id)}
          </span>
        </div>
      </header>

      {notice ? (
        <p
          role="status"
          data-lead-notice
          className="rounded-lg border border-positive/40 bg-positive-soft px-3 py-2 text-[12.5px] text-positive"
        >
          {notice}
        </p>
      ) : null}

      {error ? (
        <div
          role="alert"
          data-lead-error
          className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
        >
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
          <span className="flex-1">{error}</span>
          <button type="button" onClick={() => void load()} className="text-accent-strong hover:underline">
            Reload
          </button>
        </div>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
        {/* The evidence column. */}
        <section className="flex flex-col gap-4" aria-label="Submission">
          <div className="rounded-xl border border-line bg-surface">
            <h3 className="border-b border-line px-4 py-2.5 text-[12.5px] font-semibold">
              What they submitted
            </h3>
            <dl className="grid grid-cols-[9rem_minmax(0,1fr)] gap-x-3 gap-y-2 px-4 py-3 text-[12.5px]">
              {payloadEntries.length === 0 ? (
                <p className="col-span-2 text-muted">The payload is empty.</p>
              ) : (
                payloadEntries.map(([key, value]) => (
                  <div key={key} className="contents">
                    <dt className="truncate text-muted">{key}</dt>
                    <dd className="min-w-0 break-words whitespace-pre-wrap">
                      {typeof value === "string" ? value : JSON.stringify(value)}
                    </dd>
                  </div>
                ))
              )}
              <dt className="text-muted">Payload size</dt>
              <dd className="tabular-nums">{detail.payload_bytes} bytes</dd>
            </dl>
            <div className="border-t border-line px-4 py-2.5">
              <button
                type="button"
                data-lead-toggle-raw
                aria-expanded={showRaw}
                onClick={() => setShowRaw((open) => !open)}
                className="text-[12px] text-accent-strong hover:underline"
              >
                {showRaw ? "Hide the raw payload" : "Show the raw payload"}
              </button>
              <p className="mt-1 text-[11.5px] text-muted">
                The answers above are what the visitor sent. They cannot be edited here on purpose:
                a lead edit fixes what a person can see, never the evidence the verdicts rest on.
              </p>
              {showRaw ? (
                <pre
                  data-lead-raw
                  className="mt-2 max-h-64 overflow-auto rounded-lg bg-quiet-soft p-3 text-[11.5px]"
                >
                  {JSON.stringify(detail.payload, null, 2)}
                </pre>
              ) : null}
            </div>
          </div>

          <div className="rounded-xl border border-line bg-surface">
            <h3 className="border-b border-line px-4 py-2.5 text-[12.5px] font-semibold">Attribution</h3>
            <div className="grid gap-4 px-4 py-3 text-[12.5px] sm:grid-cols-2">
              <div>
                <p className="mb-1.5 text-[11.5px] font-medium text-muted">First touch</p>
                <dl className="grid grid-cols-[6.5rem_minmax(0,1fr)] gap-x-2 gap-y-1">
                  <dt className="text-muted">Source</dt>
                  <dd className="truncate">{lead.attribution.utm_source ?? "—"}</dd>
                  <dt className="text-muted">Medium</dt>
                  <dd className="truncate">{lead.attribution.utm_medium ?? "—"}</dd>
                  <dt className="text-muted">Campaign</dt>
                  <dd className="truncate">{lead.attribution.utm_campaign ?? "—"}</dd>
                  <dt className="text-muted">Click id</dt>
                  <dd className="truncate">{lead.attribution.click_id ?? "—"}</dd>
                </dl>
              </div>
              <div>
                <p className="mb-1.5 text-[11.5px] font-medium text-muted">Most recent visit</p>
                <dl className="grid grid-cols-[6.5rem_minmax(0,1fr)] gap-x-2 gap-y-1">
                  <dt className="text-muted">Referrer</dt>
                  <dd className="truncate">{lead.attribution.referrer_host ?? "—"}</dd>
                  <dt className="text-muted">Landing</dt>
                  <dd className="truncate">{lead.attribution.landing_path ?? "—"}</dd>
                  <dt className="text-muted">Form on</dt>
                  <dd className="truncate">{lead.attribution.source_path ?? "—"}</dd>
                </dl>
              </div>
            </div>
          </div>

          <div className="rounded-xl border border-line bg-surface">
            <h3 className="border-b border-line px-4 py-2.5 text-[12.5px] font-semibold">Verdicts</h3>
            <dl className="grid grid-cols-[9rem_minmax(0,1fr)] gap-x-3 gap-y-2 px-4 py-3 text-[12.5px]">
              <dt className="text-muted">Dedupe</dt>
              <dd data-lead-decision>
                {lead.decision ? (DECISION_LABEL[lead.decision] ?? lead.decision) : "No match attempted"}
                {lead.dedupe_key ? (
                  <span className="ml-2 text-[11.5px] text-muted">matched on {lead.dedupe_key}</span>
                ) : null}
              </dd>
              <dt className="text-muted">Spam score</dt>
              <dd className="tabular-nums" data-lead-spam-score>
                {lead.spam_score}
              </dd>
              {lead.rejection_reason ? (
                <>
                  <dt className="text-muted">Reason</dt>
                  <dd className="text-caution">{lead.rejection_reason}</dd>
                </>
              ) : null}
              <dt className="text-muted">Consent</dt>
              <dd data-lead-consent>
                {lead.consent_given ? "Given" : "Not given"}
                {lead.consent_text ? (
                  <span className="mt-0.5 block text-[11.5px] italic text-muted">
                    “{lead.consent_text}”
                  </span>
                ) : null}
              </dd>
              {lead.duplicate_of ? (
                <>
                  <dt className="text-muted">Duplicates</dt>
                  <dd className="text-muted">An existing contact (linked below)</dd>
                </>
              ) : null}
            </dl>
          </div>
        </section>

        {/* The work column. */}
        <section className="flex flex-col gap-4" aria-label="Actions">
          <div className="rounded-xl border border-line bg-surface">
            <h3 className="border-b border-line px-4 py-2.5 text-[12.5px] font-semibold">Edit</h3>
            {draft ? (
              <form
                data-lead-edit
                className="grid gap-3 px-4 py-3 sm:grid-cols-2"
                onSubmit={(event) => {
                  event.preventDefault();
                  void save();
                }}
              >
                <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                  <span className="font-medium">First name</span>
                  <input
                    id="lead-first-name"
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
                    value={draft.first_name}
                    onChange={(event) => setDraft({ ...draft, first_name: event.target.value })}
                  />
                </label>
                <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                  <span className="font-medium">Last name</span>
                  <input
                    id="lead-last-name"
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
                    value={draft.last_name}
                    onChange={(event) => setDraft({ ...draft, last_name: event.target.value })}
                  />
                </label>
                <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                  <span className="font-medium">E-mail</span>
                  <input
                    id="lead-email"
                    type="email"
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
                    value={draft.email}
                    onChange={(event) => setDraft({ ...draft, email: event.target.value })}
                  />
                </label>
                <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                  <span className="font-medium">Phone</span>
                  <input
                    id="lead-phone"
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
                    value={draft.phone}
                    onChange={(event) => setDraft({ ...draft, phone: event.target.value })}
                  />
                </label>
                <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                  <span className="font-medium">Company</span>
                  <input
                    id="lead-company"
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
                    value={draft.company_name}
                    onChange={(event) => setDraft({ ...draft, company_name: event.target.value })}
                  />
                </label>
                <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                  <span className="font-medium">Product interest</span>
                  <input
                    id="lead-product"
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
                    value={draft.product_interest}
                    onChange={(event) => setDraft({ ...draft, product_interest: event.target.value })}
                  />
                </label>
                <label className="flex flex-col gap-1 text-[11.5px] text-muted sm:col-span-2">
                  <span className="font-medium">Message</span>
                  <textarea
                    id="lead-message"
                    rows={3}
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
                    value={draft.message}
                    onChange={(event) => setDraft({ ...draft, message: event.target.value })}
                  />
                </label>
                <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                  <span className="font-medium">Status</span>
                  <select
                    id="lead-status"
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
                    value={draft.status}
                    onChange={(event) => setDraft({ ...draft, status: event.target.value })}
                  >
                    {LEAD_STATUSES.map((status) => (
                      <option key={status} value={status}>
                        {LEAD_STATUS_LABEL[status] ?? status}
                      </option>
                    ))}
                  </select>
                </label>
                <div className="flex items-end sm:col-span-2">
                  <button
                    type="submit"
                    disabled={busy}
                    data-lead-save
                    className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent-soft px-3 py-1.5 text-[12.5px] text-accent-strong disabled:opacity-50"
                  >
                    {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <Check className="size-3.5" aria-hidden />}
                    Save changes
                  </button>
                </div>
              </form>
            ) : null}
          </div>

          <div className="rounded-xl border border-line bg-surface">
            <h3 className="border-b border-line px-4 py-2.5 text-[12.5px] font-semibold">Work this lead</h3>
            <div className="flex flex-col gap-2.5 px-4 py-3">
              {/* The hand-over. `crm.leads.assign` is a different power from `crm.leads.manage`,
                  which is why this is its own call and not an edit of the draft above — an
                  operator who may correct a lead's fields is not automatically the person who
                  decides whose work it is. */}
              {!assigning ? (
                <button
                  type="button"
                  data-lead-assign-open
                  data-qa-guard="crm-intake-depth"
                  disabled={busy || lead.status === "converted"}
                  onClick={() => {
                    // Pre-fill with the current owner so "hand it to the same person" is visible
                    // as what it is, and so pressing save without changing anything is a no-op
                    // the server can record honestly rather than a blank assignment.
                    setAssignOwner(lead.owner_user_id ?? "");
                    setAssignReason("");
                    setAssigning(true);
                  }}
                  className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] transition hover:text-ink disabled:opacity-50"
                >
                  <UserRoundCheck className="size-3.5" aria-hidden />
                  {lead.owner_user_id ? "Change owner" : "Assign an owner"}
                </button>
              ) : (
                <div
                  data-lead-assign
                  className="flex flex-col gap-2 rounded-lg border border-line bg-canvas p-2.5"
                >
                  {/* A picker, not a uuid box. The store wants an id; an operator knows a
                      colleague's name, and the roster is the only place in the panel where the
                      two meet. The load is on the same row because handing the tenth lead to
                      somebody who already has nine is a decision somebody has to *see* they
                      are making. */}
                  <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                    <span className="font-medium">Owner</span>
                    <select
                      data-lead-assign-owner
                      className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] text-ink"
                      value={assignOwner}
                      onChange={(event) => setAssignOwner(event.target.value)}
                    >
                      <option value="">Unassigned queue</option>
                      {(owners ?? []).map((owner) => (
                        <option
                          key={owner.id}
                          value={owner.id}
                          data-owner-option={owner.id}
                        >
                          {owner.label} · {owner.open_leads} open
                          {owner.status === "disabled" ? " · disabled" : ""}
                        </option>
                      ))}
                    </select>
                  </label>
                  {owners && owners.length === 0 && ownersError ? (
                    <p className="text-[11.5px] text-caution" data-lead-assign-roster-error>
                      {ownersError}
                    </p>
                  ) : null}
                  {owners && owners.length === 0 && !ownersError ? (
                    <p className="text-[11.5px] text-muted" data-lead-assign-roster-empty>
                      Nobody else is in this organization yet, so this lead can only sit in the
                      unassigned queue. Invite people under Settings → IAM to share the queue.
                    </p>
                  ) : null}
                  <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                    <span className="font-medium">Why</span>
                    <input
                      data-lead-assign-reason
                      className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] text-ink"
                      placeholder="Wrong region · asked for it"
                      value={assignReason}
                      onChange={(event) => setAssignReason(event.target.value)}
                    />
                  </label>
                  <p className="text-[11.5px] text-muted">
                    The unassigned queue is a choice, not an absence, and the reason is recorded
                    either way. The first-response deadline does not move: handing a lead to the
                    right person is not a new promise to the person who wrote in.
                  </p>
                  <div className="flex flex-wrap gap-2">
                    <button
                      type="button"
                      data-lead-assign-save
                      disabled={busy || assignReason.trim() === ""}
                      onClick={() =>
                        void run(
                          () =>
                            assignLead(
                              lead.id,
                              assignOwner === "" ? null : assignOwner,
                              assignReason.trim(),
                            ),
                          "The lead has a new owner.",
                        ).then(() => setAssigning(false))
                      }
                      className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent-soft px-3 py-1.5 text-[12.5px] text-accent-strong disabled:opacity-50"
                    >
                      {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <Check className="size-3.5" aria-hidden />}
                      Save owner
                    </button>
                    <button
                      type="button"
                      onClick={() => setAssigning(false)}
                      className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted"
                    >
                      Cancel
                    </button>
                  </div>
                  {assignReason.trim() === "" ? (
                    <p className="text-[11.5px] text-muted">
                      A reason is required — a lead that changed hands with no explanation cannot
                      be explained to the person who had it.
                    </p>
                  ) : null}
                </div>
              )}

              <button
                type="button"
                data-lead-respond
                data-qa-guard="crm-intake-depth"
                disabled={busy || lead.first_response_at !== null}
                onClick={() => void run(() => markLeadResponded(lead.id), "The first response is recorded.")}
                className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] transition hover:text-ink disabled:opacity-50"
              >
                <Check className="size-3.5" aria-hidden />
                {lead.first_response_at
                  ? `Responded ${absoluteInstant(lead.first_response_at)}`
                  : "Mark responded"}
              </button>
              <p className="text-[11.5px] text-muted">
                Stops the first-response clock. It records the first instant only, so pressing it
                twice cannot rewrite the measurement.
              </p>

              <button
                type="button"
                data-lead-spam
                data-qa-guard="crm-intake-depth"
                disabled={busy}
                onClick={() => void run(() => markLeadSpam(lead.id), "The lead is filed as spam.")}
                className="inline-flex items-center gap-1.5 self-start rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted transition hover:text-ink disabled:opacity-50"
              >
                <ShieldAlert className="size-3.5" aria-hidden />
                Mark as spam
              </button>

              <div className="flex flex-col gap-1.5">
                {rejecting ? (
                  <>
                    <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                      <span className="font-medium">Why is this lead refused?</span>
                      <input
                        id="lead-reject-reason"
                        value={rejectReason}
                        onChange={(event) => setRejectReason(event.target.value)}
                        placeholder="Not a real quote request"
                        className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
                      />
                    </label>
                    <div className="flex gap-2">
                      <button
                        type="button"
                        data-lead-reject-confirm
                        data-qa-guard="crm-intake-depth"
                        disabled={busy || !rejectReason.trim()}
                        onClick={() =>
                          void run(async () => {
                            const answer = await rejectLead(lead.id, rejectReason.trim());
                            setRejecting(false);
                            setRejectReason("");
                            return answer;
                          }, "The lead is rejected, and the reason is kept on the row.")
                        }
                        className="inline-flex items-center gap-1.5 rounded-lg border border-caution bg-caution-soft px-3 py-1.5 text-[12.5px] text-caution disabled:opacity-50"
                      >
                        <Ban className="size-3.5" aria-hidden />
                        Reject with this reason
                      </button>
                      <button
                        type="button"
                        onClick={() => setRejecting(false)}
                        className="rounded-lg px-2.5 py-1.5 text-[12.5px] text-muted hover:underline"
                      >
                        Cancel
                      </button>
                    </div>
                  </>
                ) : (
                  <button
                    type="button"
                    data-lead-reject
                    data-qa-guard="crm-intake-depth"
                    disabled={busy}
                    onClick={() => setRejecting(true)}
                    className="inline-flex items-center gap-1.5 self-start rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted transition hover:text-ink disabled:opacity-50"
                  >
                    <Ban className="size-3.5" aria-hidden />
                    Reject
                  </button>
                )}
                <p className="text-[11.5px] text-muted">
                  A rejected lead keeps its row and its reason — a refusal nobody can explain is
                  one an operator undoes without thinking.
                </p>
              </div>

              <button
                type="button"
                data-lead-delete
                data-qa-guard="crm-intake-depth"
                disabled={busy}
                onClick={() => void onDelete()}
                className="inline-flex items-center gap-1.5 self-start rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-red-600 transition hover:border-red-500/40 disabled:opacity-50"
              >
                <Trash2 className="size-3.5" aria-hidden />
                Delete
              </button>
            </div>
          </div>

          <div className="rounded-xl border border-line bg-surface">
            <h3 className="border-b border-line px-4 py-2.5 text-[12.5px] font-semibold">
              Conversion
            </h3>
            <ol data-conversion-stepper className="flex flex-col gap-2.5 px-4 py-3 text-[12.5px]">
              {detail.steps.map((step, index) => (
                <li
                  key={step.key}
                  data-step={step.key}
                  data-state={step.state}
                  className="flex items-start gap-2.5"
                >
                  <span
                    className={`mt-0.5 flex size-4 shrink-0 items-center justify-center rounded-full text-[10px] ${
                      step.state === "done"
                        ? "bg-positive text-white"
                        : step.state === "current"
                          ? "bg-accent text-white"
                          : "bg-quiet-soft text-muted"
                    }`}
                    aria-hidden
                  >
                    {step.state === "done" ? "✓" : index + 1}
                  </span>
                  <span className="flex flex-col">
                    <span className="font-medium">{STEP_LABEL[step.key]}</span>
                    <span className="text-[11.5px] text-muted">{step.note}</span>
                  </span>
                </li>
              ))}
            </ol>
            {conversion ? (
              <p
                data-conversion-result
                className="mx-4 mb-3 rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[11.5px] text-muted"
              >
                {conversion.deal_skipped
                  ? `Contact ready. ${conversion.deal_skipped}.`
                  : `Contact and opportunity ready${
                      conversion.contact_created ? " (contact created)" : " (existing contact reused)"
                    }.`}
              </p>
            ) : null}
            <div className="flex flex-wrap gap-2 border-t border-line px-4 py-3">
              <button
                type="button"
                data-lead-convert
                data-qa-guard="crm-intake-depth"
                disabled={busy || lead.deal_id !== null}
                onClick={() => void convert()}
                className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent-soft px-3 py-1.5 text-[12.5px] text-accent-strong disabled:opacity-50"
              >
                <ArrowRight className="size-3.5" aria-hidden />
                {lead.deal_id ? "Opportunity created" : "Convert"}
              </button>
              {lead.deal_id ? (
                <span className="inline-flex items-center gap-1.5 self-center text-[11.5px] text-muted">
                  <Check className="size-3.5" aria-hidden />
                  Deal linked
                </span>
              ) : null}
            </div>
          </div>

          <div className="rounded-xl border border-line bg-surface">
            <h3 className="border-b border-line px-4 py-2.5 text-[12.5px] font-semibold">Timeline</h3>
            {detail.timeline.length === 0 ? (
              <p className="px-4 py-4 text-[12.5px] text-muted" data-lead-timeline-empty>
                Nothing has happened to this lead yet.
              </p>
            ) : (
              <ol data-lead-timeline className="flex flex-col gap-2.5 px-4 py-3 text-[12.5px]">
                {detail.timeline.map((event) => (
                  <li key={event.id} data-event={event.kind} className="flex flex-col">
                    <span className="font-medium">{LEAD_EVENT_LABEL[event.kind] ?? event.kind}</span>
                    <span className="text-[11.5px] text-muted">
                      {absoluteInstant(event.created_at)}
                      {event.actor_user_id ? " · by an operator" : ""}
                    </span>
                    {/* A hand-over line names both people it moved between. Rendering only the
                        new one leaves "Owner changed" answering no question, and rendering
                        neither leaves the trail unable to explain a lead that reached the
                        wrong desk. */}
                    {typeof event.detail?.owner_user_id === "string" ? (
                      <span className="text-[11.5px] text-muted" data-lead-trail-owner>
                        {"→ "}
                        {ownerLabel(ownersById, String(event.detail.owner_user_id))}
                        {typeof event.detail.previous_owner_user_id === "string"
                          ? ` from ${ownerLabel(ownersById, String(event.detail.previous_owner_user_id))}`
                          : " from the unassigned queue"}
                      </span>
                    ) : null}
                    {typeof event.detail?.reason === "string" ? (
                      <span className="text-[11.5px] text-muted">{event.detail.reason}</span>
                    ) : null}
                    {typeof event.detail?.status === "string" ? (
                      <span className="text-[11.5px] text-muted">
                        → {LEAD_STATUS_LABEL[String(event.detail.status)] ?? String(event.detail.status)}
                      </span>
                    ) : null}
                    {Array.isArray(event.detail?.changed) ? (
                      <span className="text-[11.5px] text-muted">
                        Changed {String((event.detail.changed as string[]).join(", "))}
                      </span>
                    ) : null}
                    {/* The request id (REQ-117, acceptance 17). It is shown, not just stored,
                        because the reader of a trail line is the one who has to quote it: the id
                        in the panel's error state and the id on this line are the same string,
                        which is the only reason either is worth having. A line with none — a
                        worker sweep, which belongs to no exchange — says so rather than showing
                        an empty chip that reads as a broken lookup. */}
                    {typeof event.detail?.request_id === "string" ? (
                      <span className="text-[11.5px] text-muted" data-lead-trail-request-id>
                        request <code className="font-mono">{event.detail.request_id}</code>
                      </span>
                    ) : null}
                    {/* The autoresponder's own state, and — for the two states where the
                        message has not gone out — the INSTANT it goes out. The reason string on
                        this line was the word "delayed" and the editor renders a minute count,
                        so neither named the moment a lead that reads "answered" is not answered
                        until. It is read from the stored row rather than recomputed from the
                        source's configuration, because the configuration may have changed since
                        the claim was taken and the row is what the send will actually honour. */}
                    {event.autoresponder_state ? (
                      <span
                        className="text-[11.5px] text-muted"
                        data-lead-trail-autoresponder
                        data-autoresponder-state={event.autoresponder_state}
                      >
                        {AUTORESPONDER_STATE_LABEL[event.autoresponder_state]}
                        {event.autoresponder_state === "reserved" &&
                        typeof event.detail?.due_at === "string" ? (
                          <span data-lead-trail-autoresponder-due>
                            {` · goes out around ${absoluteInstant(event.detail.due_at)}`}
                          </span>
                        ) : null}
                      </span>
                    ) : null}
                    {/* An abandoned claim is the one state that is a defect rather than a
                        waiting room, so it is named in the operator's terms rather than as a
                        state name: nothing is holding this reply and the worker will pick it up
                        on its next pass. */}
                    {event.autoresponder_state === "abandoned" ? (
                      <span
                        className="text-[11.5px] text-caution"
                        data-lead-trail-autoresponder-abandoned
                      >
                        No delivery was recorded and nobody is holding it, so the worker will send
                        this on its next pass. If it does not, this lead was never answered.
                      </span>
                    ) : null}
                  </li>
                ))}
              </ol>
            )}
          </div>
        </section>
      </div>

      {/* The sticky action bar. It exists below `sm` only: on a wide screen the actions are
          already in the right column, and a second copy of the same buttons is two places to
          keep in step. On a phone the column is below the fold, so without this the "mark
          responded" the SLA is about is unreachable. */}
      <div
        data-lead-sticky-bar
        className="sticky bottom-0 z-20 -mx-4 flex items-center gap-2 border-t border-line bg-canvas/95 px-4 py-2.5 backdrop-blur sm:hidden"
      >
        <button
          type="button"
          data-lead-respond-sticky
          data-qa-guard="crm-intake-depth"
          disabled={busy || lead.first_response_at !== null}
          onClick={() => void run(() => markLeadResponded(lead.id), "The first response is recorded.")}
          className="inline-flex flex-1 items-center justify-center gap-1.5 rounded-lg border border-accent bg-accent-soft px-3 py-2 text-[12.5px] text-accent-strong disabled:opacity-50"
        >
          <Check className="size-3.5" aria-hidden />
          Mark responded
        </button>
        <Link
          href="/crm/leads"
          className="rounded-lg border border-line px-3 py-2 text-[12.5px] text-muted"
        >
          Inbox
        </Link>
      </div>

      {lead.status === "duplicate" || lead.duplicate_of ? (
        <p className="flex items-start gap-2 rounded-lg border border-caution/40 bg-caution-soft px-3 py-2.5 text-[12.5px] text-caution">
          <FileWarning className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          This lead was filed as a duplicate of an existing contact. The
          <Link href="/crm/leads/duplicates" className="mx-1 underline">
            duplicate queue
          </Link>
          is where that decision can be reversed.
        </p>
      ) : null}
    </div>
  );
}

/** The four documented steps, in order. The *state* of each comes from the server. */
const STEP_LABEL: Record<LeadStep["key"], string> = {
  lead: "Lead",
  opportunity: "Opportunity",
  quotation: "Quotation",
  customer: "Customer",
};
