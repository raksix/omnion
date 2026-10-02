"use client";

/**
 * The activity feed and a record's merged timeline (REQ-051, slice 4).
 *
 * An activity is the only part of the CRM a person *adds* — the rest is a shape somebody else
 * derived — so this screen is the one place in the module where the validation is worth showing
 * in full: every refusal arrives as a sentence under the field it belongs to, because the person
 * who just wrote a note is the person who has to fix it.
 *
 * Two shapes share this file because they answer the same question with the same rows:
 *
 * * `/crm/activities` — every activity the caller may see, newest first, with the log form.
 * * `<Record>Timeline` — the same list, narrowed to one record and **merged** with the deal stage
 *   changes and the archive markers, so "what happened to this contact" is one ordered stream and
 *   not three lists a person has to reconcile.
 *
 * The filter state lives in the URL for the same reason the contact list's does: a filtered feed
 * is a link, so the back button, a bookmark and a shared URL all land on the same rows.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  Check,
  CircleDot,
  Clock,
  FileText,
  Loader2,
  Phone,
  Plus,
  TriangleAlert,
  Video,
  X,
} from "lucide-react";
import { useRouter } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, ErrorStrip, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { ApiError } from "@/lib/api";
import {
  CRM_ACTIVITY_KINDS,
  createCrmActivity,
  fetchCrmActivities,
  relativeTime,
  setCrmActivityDone,
  type CrmActivity,
  type CrmActivityKind,
  type CrmTimelineEntry,
  type CrmTimelineSource,
} from "@/lib/crm";
import { CRM_NAV, CrmShortcutSheet, crmListItemCursor, useCrmKeyboard } from "./crm-parts";
import { useCrmTenant } from "./crm-tenant";

/** The icon and the word each kind is shown with, so a kind is never a bare string in a list. */
const KIND_ICON: Record<string, typeof Phone> = {
  call: Phone,
  meeting: Video,
  note: FileText,
  task: Check,
};

/** The word each kind is written with. */
const KIND_LABEL: Record<string, string> = {
  call: "Call",
  meeting: "Meeting",
  note: "Note",
  task: "Task",
};

/** The tone each timeline source is drawn in, so the three arms are distinguishable at a glance. */
const SOURCE_LABEL: Record<CrmTimelineSource, string> = {
  activity: "Activity",
  stage_change: "Stage change",
  archived: "Archived",
};

/** The attach-target the form offers, in the order the picker lists them. */
const ATTACHMENTS = [
  { value: "contact", label: "Contact" },
  { value: "company", label: "Company" },
  { value: "deal", label: "Deal" },
] as const;

// ---------------------------------------------------------------------------------------------
// The feed
// ---------------------------------------------------------------------------------------------

/** The log form's state. One screen, so it is local rather than a context. */
type FormState = {
  kind: CrmActivityKind;
  subject: string;
  body: string;
  attachment: (typeof ATTACHMENTS)[number]["value"];
  /** The identifier of the chosen record; free text so the walkthrough can type one. */
  recordId: string;
  dueAt: string;
};

const EMPTY_FORM: FormState = {
  kind: "note",
  subject: "",
  body: "",
  attachment: "contact",
  recordId: "",
  dueAt: "",
};

/** `/crm/activities`: the feed, the filters and the form that fills it. */
export function ActivitiesView() {
  const [rows, setRows] = useState<CrmActivity[] | null>(null);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [search, setSearch] = useState("");
  const [kind, setKind] = useState("");
  const [done, setDone] = useState("");
  const [reloadToken, setReloadToken] = useState(0);

  const [form, setForm] = useState<FormState>(EMPTY_FORM);
  const [formError, setFormError] = useState<{ field: string; message: string } | null>(null);
  const [saving, setSaving] = useState(false);
  const subjectRef = useRef<HTMLInputElement | null>(null);
  const searchRef = useRef<HTMLInputElement | null>(null);

  const filtered = search !== "" || kind !== "" || done !== "";

  // This screen is a form with a feed under it rather than a `CrmShell` list, so it used to have
  // **none** of the keyboard contract while the module's shortcut sheet still promised it `/`, `j`,
  // `k`, `Enter`, `e` and `?`. A sheet is a claim about the whole section, so a screen that draws
  // its own rows calls the same hook the shell does (`useCrmKeyboard`).
  //
  // `Enter` and `e` both go to the activity's **record** — the contact, company or deal the activity
  // is about — because an activity has no view of its own; it is the note on somebody else's
  // record. An activity with no subject record (a free-standing note) therefore has nowhere to open,
  // and the cursor does not pretend otherwise: those rows still take the cursor, and pressing a key
  // on one does nothing rather than navigating somewhere unrelated.
  const rowIds = useMemo(() => (rows ?? []).map((row) => row.id), [rows]);
  const recordHref = useCallback((id: string) => {
    const row = (rows ?? []).find((entry) => entry.id === id);
    if (!row) return null;
    if (row.contact_id) return `/crm/contacts?focus=${row.contact_id}`;
    if (row.company_id) return `/crm/companies?focus=${row.company_id}`;
    if (row.deal_id) return `/crm/deals?focus=${row.deal_id}`;
    return null;
  }, [rows]);
  const openRecord = useCallback(
    (id: string) => {
      const href = recordHref(id);
      if (href) {
        router.push(href);
        return;
      }
      // No subject record: say so rather than opening the wrong thing. This is the honest answer
      // for a free-standing note, and it is a sentence instead of silence.
      const row = (rows ?? []).find((entry) => entry.id === id);
      setNotice(
        row
          ? `“${row.subject}” is a note with no contact, company or deal attached, so there is no record to open.`
          : "That activity is no longer in the list.",
      );
    },
    [recordHref, rows],
  );
  const { selectedIndex, setSelectedIndex, showShortcuts } = useCrmKeyboard({
    rowIds,
    searchRef,
    onOpen: openRecord,
    onEdit: openRecord,
    onCreate: () => subjectRef.current?.focus(),
  });

  // The organization the panel is reading (REQ-051): a platform account has no primary one.
  const { organizationId } = useCrmTenant();
  const router = useRouter();

  const load = useCallback(async () => {
    setError(null);
    try {
      const page = await fetchCrmActivities({
        search: search || undefined,
        kind: kind || undefined,
        done: (done || undefined) as "open" | "done" | undefined,
        organization_id: organizationId ?? undefined,
        limit: 50,
      });
      setRows(page.items ?? []);
    } catch (failure) {
      setError(toScreenError(failure, "The activity feed could not be read."));
      setRows([]);
    }
  }, [search, kind, done, reloadToken, organizationId]);

  useEffect(() => {
    void load();
  }, [load]);

  /** Log one activity, and put the fresh row at the top of the feed. */
  const submit = useCallback(async () => {
    setFormError(null);
    setNotice(null);
    if (form.subject.trim().length === 0) {
      setFormError({ field: "subject", message: "an activity needs a subject — what happened" });
      subjectRef.current?.focus();
      return;
    }
    if (form.recordId.trim().length === 0) {
      // The form cannot look the record up for the person, so it says which one is missing
      // rather than sending a request the API will refuse with the same words.
      setFormError({
        field: "recordId",
        message: `an activity hangs off a ${form.attachment} — paste its id`,
      });
      return;
    }
    if (form.kind === "task" && form.dueAt === "") {
      setFormError({
        field: "dueAt",
        message: "a task needs a due date, or a mark that it is already done",
      });
      return;
    }

    setSaving(true);
    try {
      const attachment = { [`${form.attachment}_id`]: form.recordId.trim() };
      const created = await createCrmActivity({
        kind: form.kind,
        subject: form.subject.trim(),
        body: form.body.trim() || undefined,
        ...attachment,
        due_at: form.kind === "task" && form.dueAt ? form.dueAt : undefined,
      } as Parameters<typeof createCrmActivity>[0]);
      setRows((current) => [created, ...(current ?? [])]);
      setForm({ ...EMPTY_FORM, attachment: form.attachment });
      setNotice("Logged.");
      subjectRef.current?.focus();
    } catch (failure) {
      // The API's refusal names the field it belongs to; that is what the form shows.
      const field =
        failure instanceof ApiError ? ((failure as ApiError & { field?: string }).field ?? "subject") : "subject";
      const message =
        failure instanceof Error ? failure.message : "the activity could not be logged";
      setFormError({ field: field === "contact_id" ? "recordId" : field, message });
    } finally {
      setSaving(false);
    }
  }, [form]);

  /** Close a task, or open it again — the same row, the opposite state. */
  const toggleDone = useCallback(
    async (activity: CrmActivity) => {
      setError(null);
      setRows((current) =>
        (current ?? []).map((row) =>
          row.id === activity.id
            ? { ...row, done_at: row.done_at ? null : new Date().toISOString() }
            : row,
        ),
      );
      try {
        const fresh = await setCrmActivityDone(activity.id, !activity.done_at);
        setRows((current) => (current ?? []).map((row) => (row.id === fresh.id ? fresh : row)));
      } catch (failure) {
        // Roll the optimistic change back: a row that says "done" and is not is worse than no row.
        setRows((current) => (current ?? []).map((row) => (row.id === activity.id ? activity : row)));
        setError(failure instanceof Error ? failure.message : "the task could not be updated.");
      }
    },
    [],
  );

  return (
    <div className="space-y-4">
      <nav aria-label="CRM sections" className="flex flex-wrap items-center gap-1 border-b border-line pb-2">
        {CRM_NAV.map((item) => (
          <a
            key={item.href}
            href={item.href}
            aria-current={item.href === "/crm/activities" ? "page" : undefined}
            className="rounded-md px-2.5 py-1 text-[12.5px] text-muted transition hover:bg-canvas"
          >
            {item.label}
          </a>
        ))}
      </nav>

      {/* `?` toggles this sheet through `useCrmKeyboard`, and this screen called the hook and
          read the flag into a binding nothing rendered — so the key worked, the state flipped, and
          no sheet appeared. The module's own shortcut contract advertises `?` on this screen, so a
          binding that produces no visible answer is a dead control. `/crm/leads` already draws it
          this way; the omission was this screen alone. */}
      {showShortcuts ? <CrmShortcutSheet /> : null}

      {error ? (
        <ErrorStrip
          error={error}
          onRetry={() => setReloadToken((token) => token + 1)}
          qa="crm-activities-error"
        />
      ) : null}
      {notice ? (
        <p role="status" className="border-b border-line bg-success-soft px-4 py-2.5 text-[12px] text-success">
          {notice}
        </p>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_20rem]">
        <section className="min-w-0 space-y-3" aria-label="Activity feed">
          <div className="flex flex-wrap items-center gap-2">
            <label className="flex items-center gap-1.5 text-[12px] text-muted">
              <span className="sr-only">Search activities</span>
              <input
                ref={searchRef}
                data-qa="activity-search"
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder="Search subjects and notes"
                aria-label="Search activities"
                className="w-56 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
              />
            </label>
            <label className="flex items-center gap-1.5 text-[12px] text-muted">
              <span className="sr-only">Filter by kind</span>
              <select
                data-qa="activity-kind"
                value={kind}
                onChange={(event) => setKind(event.target.value)}
                aria-label="Filter by kind"
                className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px]"
              >
                <option value="">All kinds</option>
                {CRM_ACTIVITY_KINDS.map((value) => (
                  <option key={value} value={value}>
                    {KIND_LABEL[value]}
                  </option>
                ))}
              </select>
            </label>
            <label className="flex items-center gap-1.5 text-[12px] text-muted">
              <span className="sr-only">Filter by state</span>
              <select
                data-qa="activity-done"
                value={done}
                onChange={(event) => setDone(event.target.value)}
                aria-label="Filter by state"
                className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px]"
              >
                <option value="">Open and done</option>
                <option value="open">Open only</option>
                <option value="done">Done only</option>
              </select>
            </label>
            {(search || kind || done) && (
              <button
                type="button"
                data-qa="activity-clear"
                onClick={() => {
                  setSearch("");
                  setKind("");
                  setDone("");
                }}
                className="inline-flex items-center gap-1 rounded-lg border border-line px-2.5 py-1.5 text-[12px] transition hover:bg-canvas"
              >
                <X aria-hidden className="h-3.5 w-3.5" />
                Clear
              </button>
            )}
          </div>

          {error ? (
            <ErrorState
              error={error}
              onRetry={() => setReloadToken((token) => token + 1)}
              qa="crm-activities-body-error"
              action={
                <button
                  type="button"
                  onClick={() => subjectRef.current?.focus()}
                  className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
                >
                  <Plus aria-hidden className="h-3.5 w-3.5" />
                  Log an activity
                </button>
              }
            />
          ) : rows === null ? (
            <ActivitySkeleton />
          ) : rows.length === 0 ? (
            <EmptyState
              title={filtered ? "No activity matches these filters" : "No activity yet"}
              hint={
                filtered
                  ? "The feed is filtered — clear the filters to see everything you may read."
                  : "Log the first call, meeting, note or task and it appears here, newest first."
              }
              action={
                filtered ? null : (
                  <button
                    type="button"
                    data-qa="activity-empty-action"
                    onClick={() => subjectRef.current?.focus()}
                    className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
                  >
                    <Plus aria-hidden className="h-3.5 w-3.5" />
                    Log an activity
                  </button>
                )
              }
            />
          ) : (
            <ul className="divide-y divide-line border-y border-line">
              {rows.map((activity, index) => {
                const Icon = KIND_ICON[activity.kind] ?? CircleDot;
                // The cursor is drawn by the row, exactly as `CrmRow` does for a table: a shortcut
                // nobody can see is indistinguishable from a broken one.
                const cursor = crmListItemCursor(index === selectedIndex);
                return (
                  <li
                    key={activity.id}
                    {...cursor}
                    data-qa="activity-row"
                    data-kind={activity.kind}
                    data-done={activity.done_at ? "true" : "false"}
                    onClick={() => setSelectedIndex(index)}
                    className={`flex flex-wrap items-start gap-3 py-2.5 transition ${cursor.className}`}
                  >
                    <span className="mt-0.5 inline-flex h-6 w-6 items-center justify-center rounded-md border border-line text-muted">
                      <Icon aria-hidden className="h-3.5 w-3.5" />
                    </span>
                    <div className="min-w-0 flex-1">
                      <p className="truncate text-[13px] font-medium">{activity.subject}</p>
                      {activity.body ? (
                        <p className="mt-0.5 line-clamp-2 text-[12px] text-muted">{activity.body}</p>
                      ) : null}
                      <p className="mt-1 flex flex-wrap items-center gap-2 text-[11.5px] text-muted">
                        <span>{KIND_LABEL[activity.kind] ?? activity.kind}</span>
                        <span aria-hidden>·</span>
                        <span title={activity.occurred_at}>{relativeTime(activity.occurred_at)}</span>
                        {activity.due_at && !activity.done_at ? (
                          <>
                            <span aria-hidden>·</span>
                            <span className="inline-flex items-center gap-1 text-warning">
                              <Clock aria-hidden className="h-3 w-3" />
                              due {relativeTime(activity.due_at)}
                            </span>
                          </>
                        ) : null}
                      </p>
                    </div>
                    {activity.kind === "task" ? (
                      <button
                        type="button"
                        data-qa="activity-toggle-done"
                        onClick={() => void toggleDone(activity)}
                        className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas"
                      >
                        <Check aria-hidden className="h-3.5 w-3.5" />
                        {activity.done_at ? "Reopen" : "Close"}
                      </button>
                    ) : null}
                  </li>
                );
              })}
            </ul>
          )}
        </section>

        {/* The form. A field's refusal renders under the field, never in a banner.

            This is a `<form>` and not another `<section>`, for two reasons that are the same
            reason. A section is a box; a form is a form: pressing Enter in the record-id field has
            to log the activity, because the alternative is a data-entry screen where the only way
            to submit is to find the button with the mouse. And the mobile gate measures
            `form label` geometry — this screen's fields sat in a section, so the measurement found
            zero fields and the one form in the module that carried the two-up pair was the one
            form nobody measured. The filter labels above stay outside it: they are three controls
            on one toolbar row, not fields of this form, and letting them in would make the
            single-column assertion measure the toolbar instead. */}
        <form
          aria-label="Log an activity"
          className="h-fit space-y-3 rounded-xl border border-line p-3"
          onSubmit={(event) => {
            event.preventDefault();
            void submit();
          }}
        >
          <h2 className="text-[13px] font-medium">Log an activity</h2>
          <label className="block text-[12px] text-muted">
            Kind
            <select
              data-qa="activity-form-kind"
              value={form.kind}
              onChange={(event) => setForm({ ...form, kind: event.target.value as CrmActivityKind })}
              className="mt-1 w-full rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] text-ink"
            >
              {CRM_ACTIVITY_KINDS.map((value) => (
                <option key={value} value={value}>
                  {KIND_LABEL[value]}
                </option>
              ))}
            </select>
          </label>
          <label className="block text-[12px] text-muted">
            Subject
            <input
              ref={subjectRef}
              data-qa="activity-form-subject"
              value={form.subject}
              onChange={(event) => setForm({ ...form, subject: event.target.value })}
              aria-invalid={formError?.field === "subject" || undefined}
              className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] text-ink"
            />
            {formError?.field === "subject" ? (
              <span role="alert" className="mt-1 block text-[11.5px] text-danger">
                {formError.message}
              </span>
            ) : null}
          </label>
          <label className="block text-[12px] text-muted">
            Notes
            <textarea
              data-qa="activity-form-body"
              value={form.body}
              onChange={(event) => setForm({ ...form, body: event.target.value })}
              rows={3}
              className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] text-ink"
            />
          </label>
          {/* One column on a phone, two from `sm`. Without the prefix this pair — "Hangs off" and
              "Record id" — stays side by side at 390px, which is the form the mobile box says must
              be single-column. A form whose fields are technically two-up is a form nobody can fill
              on a phone, so the layout has to be stated rather than assumed to fall back. */}
          <div className="grid gap-2 sm:grid-cols-2">
            <label className="block text-[12px] text-muted">
              Hangs off
              <select
                data-qa="activity-form-attachment"
                value={form.attachment}
                onChange={(event) =>
                  setForm({ ...form, attachment: event.target.value as FormState["attachment"] })
                }
                className="mt-1 w-full rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] text-ink"
              >
                {ATTACHMENTS.map((option) => (
                  <option key={option.value} value={option.value}>
                    {option.label}
                  </option>
                ))}
              </select>
            </label>
            <label className="block text-[12px] text-muted">
              Record id
              <input
                data-qa="activity-form-record"
                value={form.recordId}
                onChange={(event) => setForm({ ...form, recordId: event.target.value })}
                aria-invalid={formError?.field === "recordId" || undefined}
                className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] text-ink"
              />
            </label>
          </div>
          {formError?.field === "recordId" ? (
            <p role="alert" className="text-[11.5px] text-danger">
              {formError.message}
            </p>
          ) : null}
          {form.kind === "task" ? (
            <label className="block text-[12px] text-muted">
              Due
              <input
                data-qa="activity-form-due"
                type="date"
                value={form.dueAt}
                onChange={(event) => setForm({ ...form, dueAt: event.target.value })}
                className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] text-ink"
              />
              {formError?.field === "dueAt" ? (
                <span role="alert" className="mt-1 block text-[11.5px] text-danger">
                  {formError.message}
                </span>
              ) : null}
            </label>
          ) : null}
          <button
            type="submit"
            data-qa="activity-form-submit"
            disabled={saving}
            className="inline-flex w-full items-center justify-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-60"
          >
            {saving ? <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" /> : <Plus aria-hidden className="h-3.5 w-3.5" />}
            Log it
          </button>
        </form>
      </div>
    </div>
  );
}

/** Eight placeholder rows while the first page is on its way. */
function ActivitySkeleton() {
  return (
    <ul aria-hidden className="divide-y divide-line border-y border-line">
      {Array.from({ length: 8 }).map((_, index) => (
        <li key={index} className="flex items-center gap-3 py-2.5">
          <span className="h-6 w-6 animate-pulse rounded-md bg-canvas" />
          <span className="flex-1">
            <span className="block h-3 w-1/2 animate-pulse rounded bg-canvas" />
            <span className="mt-1.5 block h-2.5 w-1/3 animate-pulse rounded bg-canvas" />
          </span>
        </li>
      ))}
    </ul>
  );
}

// ---------------------------------------------------------------------------------------------
// The merged timeline
// ---------------------------------------------------------------------------------------------

/**
 * One record's merged timeline.
 *
 * Reused by the contact, company and deal detail screens: the three ask the same question and the
 * answer has the same shape, so the three share this component rather than three near-copies that
 * would drift apart the first time an arm is added.
 */
export function RecordTimeline(props: {
  /** Which record's timeline this is. */
  record: "contact" | "company" | "deal";
  /** Its identifier. */
  id: string;
  /** How many entries to ask for. */
  limit?: number;
}) {
  const { record, id, limit = 25 } = props;
  const [entries, setEntries] = useState<CrmTimelineEntry[] | null>(null);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [reloadToken, setReloadToken] = useState(0);

  useEffect(() => {
    let cancelled = false;
    setError(null);
    const plural = record === "company" ? "companies" : `${record}s`;
    fetch(`/api/v1/crm/${plural}/${id}/timeline?limit=${limit}`, {
      credentials: "same-origin",
      headers: { accept: "application/json" },
    })
      .then(async (response) => {
        if (!response.ok) {
          const body = (await response.json().catch(() => null)) as {
            error?: { message?: string };
          } | null;
          throw new Error(body?.error?.message ?? `The timeline answered with ${response.status}.`);
        }
        return (await response.json()) as { items?: CrmTimelineEntry[] };
      })
      .then((page) => {
        if (!cancelled) setEntries(page.items ?? []);
      })
      .catch((failure: unknown) => {
        if (!cancelled) {
          setError(toScreenError(failure, "The timeline could not be read."));
          setEntries([]);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [record, id, limit, reloadToken]);

  const grouped = useMemo(() => groupByDay(entries ?? []), [entries]);

  if (error) {
    return (
      <ErrorState
        error={error}
        onRetry={() => setReloadToken((token) => token + 1)}
        qa="timeline-error"
      />
    );
  }

  if (entries === null) {
    return (
      <ul aria-hidden className="space-y-2">
        {Array.from({ length: 4 }).map((_, index) => (
          <li key={index} className="h-8 animate-pulse rounded-lg bg-canvas" />
        ))}
      </ul>
    );
  }

  if (entries.length === 0) {
    return (
      <EmptyState
        title="Nothing has happened yet"
        hint="Calls, meetings, notes and tasks logged against this record — and the deals it is in — appear here, newest first."
      />
    );
  }

  return (
    <div className="space-y-3" data-qa="timeline">
      {grouped.map(([day, dayEntries]) => (
        <section key={day} aria-label={day}>
          <h3 className="sticky top-0 z-[1] bg-surface py-1 text-[11.5px] font-medium uppercase tracking-wide text-muted">
            {day}
          </h3>
          <ul className="mt-1 space-y-1.5">
            {dayEntries.map((entry) => (
              <li
                key={entry.id}
                data-qa="timeline-entry"
                data-source={entry.source}
                className="flex flex-wrap items-start gap-2.5 rounded-lg border border-line px-2.5 py-2"
              >
                <span className="mt-0.5 text-[11.5px] tabular-nums text-muted">
                  {new Date(entry.occurred_at).toLocaleTimeString(undefined, {
                    hour: "2-digit",
                    minute: "2-digit",
                  })}
                </span>
                <div className="min-w-0 flex-1">
                  <p className="truncate text-[12.5px] font-medium">
                    {entry.subject ?? SOURCE_LABEL[entry.source]}
                  </p>
                  {entry.body ? (
                    <p className="mt-0.5 text-[12px] text-muted">{entry.body}</p>
                  ) : null}
                  <p className="mt-1 flex flex-wrap items-center gap-2 text-[11px] text-muted">
                    <span className="rounded border border-line px-1.5 py-0.5">
                      {entry.kind ? (KIND_LABEL[entry.kind] ?? entry.kind) : SOURCE_LABEL[entry.source]}
                    </span>
                    {entry.due_at && !entry.done_at ? (
                      <span className="inline-flex items-center gap-1 text-warning">
                        <TriangleAlert aria-hidden className="h-3 w-3" />
                        due {relativeTime(entry.due_at)}
                      </span>
                    ) : null}
                    {entry.done_at ? <span>closed {relativeTime(entry.done_at)}</span> : null}
                  </p>
                </div>
              </li>
            ))}
          </ul>
        </section>
      ))}
    </div>
  );
}

/** Group the entries under a day heading, keeping the newest day first. */
function groupByDay(entries: CrmTimelineEntry[]): [string, CrmTimelineEntry[]][] {
  const groups = new Map<string, CrmTimelineEntry[]>();
  for (const entry of entries) {
    const day = new Date(entry.occurred_at).toLocaleDateString(undefined, {
      weekday: "short",
      day: "numeric",
      month: "short",
      year: "numeric",
    });
    const bucket = groups.get(day);
    if (bucket) bucket.push(entry);
    else groups.set(day, [entry]);
  }
  return [...groups.entries()];
}

