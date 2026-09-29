"use client";

/**
 * `/forms/<id>/submissions` — the submission inbox (REQ-064, slice 2).
 *
 * An inbox is read far more often than it is written, and it is read in the one moment somebody
 * is anxious — "did anybody get my message?" Four things this screen refuses to do:
 *
 * 1. **The tabs carry real counts, and they are the same counts the list card shows.** A tab that
 *    says "Spam" with a number beside it that came from somewhere else is how an owner decides
 *    that submissions are being eaten.
 * 2. **A held-as-spam row is never shown as a row.** Submissions that tripped the protections are
 *    counted and dropped, so the inbox cannot display them; instead the empty states and the tab
 *    explain what the counter means. A screen that implies it can show spam it never received is
 *    lying about the one thing somebody opened it to find out.
 * 3. **The export is the *filtered* inbox.** The button passes exactly the filters the table is
 *    showing, and the sentence under it says so — a download that silently ignored the filter
 *    would be a way to export everything from a screen that says "Export 12".
 * 4. **Deleting a submission is permanent and says so.** There is no trash for a visitor's
 *    message; the confirmation is the last chance.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { useParams } from "next/navigation";
import { Download, Inbox as InboxIcon, Loader2, RefreshCw, Search, Trash2, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  bulkSubmissionStatus,
  deleteSubmission,
  exportSubmissionsCsv,
  fetchForm,
  fetchSubmissions,
  setSubmissionStatus,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { FormDetail, Inbox as InboxPage, Submission } from "@/lib/types";

/** The four inbox tabs, in the order they appear. */
const TABS = [
  { key: "new", label: "Unread" },
  { key: "read", label: "Read" },
  { key: "spam", label: "Spam" },
  { key: "archived", label: "Archived" },
] as const;

export function FormInbox() {
  const params = useParams<{ id: string }>();
  const formId = params?.id as string | undefined;

  const [detail, setDetail] = useState<FormDetail | null>(null);
  const [page, setPage] = useState<InboxPage | null>(null);
  const [status, setStatus] = useState<string>("new");
  const [search, setSearch] = useState("");
  const [appliedSearch, setAppliedSearch] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [checked, setChecked] = useState<string[]>([]);
  const [open, setOpen] = useState<Submission | null>(null);

  const filters = useMemo(
    () => ({ status, search: appliedSearch || undefined, limit: 50 }),
    [status, appliedSearch],
  );

  const load = useCallback(async () => {
    if (!formId) return;
    setError(null);
    try {
      const [document, inbox] = await Promise.all([
        fetchForm(formId),
        fetchSubmissions(formId, filters),
      ]);
      setDetail(document);
      setPage(inbox);
      // Selection is cleared on every load: a bulk action against a selection that no longer
      // exists is a bulk action on somebody else's rows.
      setChecked([]);
    } catch (caught) {
      setError((caught as ApiError).message);
    }
  }, [formId, filters]);

  useEffect(() => {
    void load();
  }, [load]);

  const mark = useCallback(
    async (submission: Submission, next: string) => {
      if (!formId) return;
      setBusy(true);
      setError(null);
      try {
        await setSubmissionStatus(formId, submission.id, next);
        setNotice(
          next === "spam"
            ? "Marked as spam. It leaves the unread list; the count keeps it accounted for."
            : `Marked ${next === "new" ? "unread" : next}.`,
        );
        setOpen(null);
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [formId, load],
  );

  const bulk = useCallback(
    async (next: string) => {
      if (!formId || checked.length === 0) return;
      setBusy(true);
      setError(null);
      try {
        await bulkSubmissionStatus(formId, checked, next);
        setNotice(`${checked.length} submission${checked.length === 1 ? "" : "s"} moved to ${next}.`);
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [checked, formId, load],
  );

  const remove = useCallback(
    async (submission: Submission) => {
      if (!formId) return;
      setBusy(true);
      setError(null);
      try {
        await deleteSubmission(formId, submission.id);
        setNotice("Deleted. A visitor's message has no trash.");
        setOpen(null);
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [formId, load],
  );

  const exportCsv = useCallback(async () => {
    if (!formId) return;
    setError(null);
    try {
      // The filters, not the whole inbox: the button lives beside a filtered table and means
      // "export what I am looking at".
      await exportSubmissionsCsv(formId, { status, search: appliedSearch || undefined });
    } catch (caught) {
      setError((caught as ApiError).message);
    }
  }, [appliedSearch, formId, status]);

  if (error && page === null && detail === null) {
    return (
      <div className="space-y-3" data-inbox-state="error">
        <p className="text-[13px] text-red-700 dark:text-red-300">{error}</p>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-[13px]"
        >
          <RefreshCw className="h-4 w-4" aria-hidden />
          Retry
        </button>
      </div>
    );
  }
  if (!page || !detail) {
    return (
      <div className="space-y-3" data-inbox-state="loading" aria-busy="true">
        <div className="h-4 w-48 animate-pulse rounded bg-quiet-soft" />
        <div className="h-64 animate-pulse rounded-lg bg-quiet-soft" />
      </div>
    );
  }

  const rows = page.submissions;
  const isFiltered = status !== "new" || appliedSearch !== "";

  return (
    <div className="space-y-5" data-inbox data-inbox-tab={status}>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <p className="text-[13.5px] font-medium">{detail.name}</p>
          <p className="text-[12px] text-muted">
            <code className="font-mono">/{detail.key}</code> ·{" "}
            {detail.status === "published" ? "accepting submissions" : "a draft — nothing can arrive yet"}
            {detail.spam_count > 0 ? ` · ${detail.spam_count} held as spam` : ""}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            data-inbox-refresh
            onClick={() => void load()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <RefreshCw className="h-3.5 w-3.5" aria-hidden />
            Refresh
          </button>
          <button
            type="button"
            data-inbox-export
            onClick={() => void exportCsv()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <Download className="h-3.5 w-3.5" aria-hidden />
            Export{isFiltered ? " this filter" : ""}
          </button>
        </div>
      </div>

      <nav className="flex flex-wrap gap-1.5" data-inbox-tabs aria-label="Inbox states">
        {TABS.map((tab) => (
          <button
            key={tab.key}
            type="button"
            data-inbox-tab-button={tab.key}
            aria-pressed={status === tab.key}
            onClick={() => setStatus(tab.key)}
            className={`rounded-full border border-line px-3 py-1 text-[12px] ${
              status === tab.key ? "bg-quiet-soft font-medium" : "text-muted"
            }`}
          >
            {tab.label}
            <span data-inbox-count={tab.key} className="ml-1.5 tabular-nums">
              {page.counts[tab.key]}
            </span>
          </button>
        ))}
      </nav>

      <form
        className="flex flex-wrap gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          setAppliedSearch(search.trim());
        }}
      >
        <label htmlFor="inbox-search" className="sr-only">
          Search submissions
        </label>
        <div className="relative min-w-0 flex-1">
          <Search
            className="pointer-events-none absolute left-2.5 top-2.5 h-3.5 w-3.5 text-muted"
            aria-hidden
          />
          <input
            id="inbox-search"
            data-inbox-search
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Search the answers"
            className="w-full rounded-md border border-line py-1.5 pl-8 pr-2 text-[12.5px]"
          />
        </div>
        <button
          type="submit"
          data-inbox-search-submit
          className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
        >
          Search
        </button>
        {isFiltered ? (
          <button
            type="button"
            data-inbox-clear
            onClick={() => {
              setSearch("");
              setAppliedSearch("");
              setStatus("new");
            }}
            className="inline-flex items-center gap-1 rounded-md border border-line px-3 py-1.5 text-[12.5px]"
          >
            <X className="h-3.5 w-3.5" aria-hidden />
            Clear
          </button>
        ) : null}
      </form>

      {notice ? (
        <p data-inbox-notice className="text-[12.5px] text-muted">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p data-inbox-error className="text-[12.5px] text-red-700 dark:text-red-300">
          {error}
        </p>
      ) : null}

      {checked.length > 0 ? (
        /* The bulk bar is sticky on mobile and inline on desktop: a bar that scrolls out of
           reach while you scroll a long inbox is a bar that gets used once and then forgotten. */
        <div
          data-inbox-bulk
          className="sticky top-2 z-10 flex flex-wrap items-center gap-2 rounded-md border border-line bg-canvas px-3 py-2"
        >
          <span className="text-[12px]">
            {checked.length} selected
          </span>
          {(["read", "spam", "archived"] as const).map((next) => (
            <button
              key={next}
              type="button"
              data-inbox-bulk={next}
              disabled={busy}
              onClick={() => void bulk(next)}
              className="rounded-md border border-line px-2.5 py-1 text-[12px] disabled:opacity-50"
            >
              {next === "read" ? "Mark read" : next === "spam" ? "Spam" : "Archive"}
            </button>
          ))}
          <button
            type="button"
            data-inbox-bulk-clear
            onClick={() => setChecked([])}
            className="rounded-md border border-line px-2.5 py-1 text-[12px]"
          >
            Clear
          </button>
        </div>
      ) : null}

      {rows.length === 0 ? (
        <div className="rounded-lg border border-line" data-inbox-empty>
          <EmptyState
            title={emptyTitle(status, appliedSearch, detail.status)}
            hint={emptyHint(status, appliedSearch, detail.spam_count)}
          />
        </div>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full min-w-[720px] border-collapse text-left text-[12.5px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] text-muted">
                <th scope="col" className="w-8 py-2 pr-2">
                  <label className="sr-only" htmlFor="inbox-select-all">
                    Select every row
                  </label>
                  <input
                    id="inbox-select-all"
                    type="checkbox"
                    data-inbox-select-all
                    checked={checked.length === rows.length && rows.length > 0}
                    onChange={(event) =>
                      setChecked(event.target.checked ? rows.map((row) => row.id) : [])
                    }
                  />
                </th>
                <th scope="col" className="py-2 pr-3 font-medium">
                  Received
                </th>
                <th scope="col" className="py-2 pr-3 font-medium">
                  Name
                </th>
                <th scope="col" className="py-2 pr-3 font-medium">
                  E-mail
                </th>
                <th scope="col" className="py-2 pr-3 font-medium">
                  Summary
                </th>
                <th scope="col" className="py-2 pr-3 font-medium">
                  Status
                </th>
                <th scope="col" className="py-2 font-medium">
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {rows.map((submission) => (
                <tr key={submission.id} data-inbox-row={submission.id} className="border-b border-line/60">
                  <td className="py-2 pr-2">
                    <label className="sr-only" htmlFor={`select-${submission.id}`}>
                      Select this submission
                    </label>
                    <input
                      id={`select-${submission.id}`}
                      type="checkbox"
                      data-inbox-select={submission.id}
                      checked={checked.includes(submission.id)}
                      onChange={(event) =>
                        setChecked((current) =>
                          event.target.checked
                            ? [...current, submission.id]
                            : current.filter((id) => id !== submission.id),
                        )
                      }
                    />
                  </td>
                  <td className="py-2 pr-3 whitespace-nowrap text-muted">
                    {formatTimestamp(submission.created_at)}
                  </td>
                  <td className="py-2 pr-3">{submission.summary?.name ?? "—"}</td>
                  <td className="py-2 pr-3">{submission.summary?.email ?? "—"}</td>
                  <td className="max-w-[280px] truncate py-2 pr-3" title={submission.summary?.text ?? ""}>
                    {submission.summary?.text ?? ""}
                  </td>
                  <td className="py-2 pr-3">
                    <span data-inbox-row-status={submission.status} className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px]">
                      {submission.status}
                    </span>
                  </td>
                  <td className="py-2 text-right">
                    <button
                      type="button"
                      data-inbox-open={submission.id}
                      onClick={() => {
                        setOpen(submission);
                        if (submission.status === "new") void mark(submission, "read");
                      }}
                      className="rounded-md border border-line px-2.5 py-1 text-[12px]"
                    >
                      Open
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {open ? (
        <SubmissionDrawer
          submission={open}
          busy={busy}
          onClose={() => setOpen(null)}
          onMark={(next) => void mark(open, next)}
          onDelete={() => void remove(open)}
        />
      ) : null}
    </div>
  );
}

/** What an empty tab says — the draft case is named before anything else. */
function emptyTitle(status: string, search: string, formStatus: string): string {
  if (search) return `No submission matches “${search}”`;
  if (formStatus !== "published") return "This form is a draft, so nothing can arrive yet";
  if (status === "spam") return "Nothing marked as spam";
  if (status === "archived") return "Nothing archived";
  if (status === "read") return "Nothing read yet";
  return "No submissions yet";
}

function emptyHint(status: string, search: string, spamCount: number): string {
  if (search) return "The search runs over the stored answers, so a name or a phrase inside one will find it.";
  if (status === "spam") {
    // The one thing the spam tab has to explain: rows that were refused are not here, because
    // they were never stored. Silence here reads as "nothing was refused", which is the belief
    // that leads an owner to turn the protection off.
    return spamCount > 0
      ? `${spamCount} submission${spamCount === 1 ? " was" : "s were"} held and never stored — a filled honeypot, a fill under the minimum time, or the hourly limit. The count is the whole record.`
      : "A submission lands here only when somebody marks it. Nothing is ever filtered into this tab automatically.";
  }
  return "Submissions appear the moment somebody sends one. Nothing is filtered out of this tab.";
}

function SubmissionDrawer({
  submission,
  busy,
  onClose,
  onMark,
  onDelete,
}: {
  submission: Submission;
  busy: boolean;
  onClose: () => void;
  onMark: (status: string) => void;
  onDelete: () => void;
}) {
  const answers = Object.entries(submission.answers ?? {});
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="submission-drawer"
      data-inbox-drawer
      className="fixed inset-0 z-50 flex justify-end bg-black/40"
    >
      <div className="flex h-full w-full max-w-lg flex-col overflow-y-auto border-l border-line bg-canvas p-5">
        <div className="flex items-start justify-between gap-3">
          <div>
            <h2 id="submission-drawer" className="text-[14px] font-medium">
              Submission
            </h2>
            <p className="text-[12px] text-muted">{formatTimestamp(submission.created_at)}</p>
          </div>
          <button
            type="button"
            data-inbox-drawer-close
            onClick={onClose}
            aria-label="Close"
            className="rounded-md border border-line p-1"
          >
            <X className="h-3.5 w-3.5" aria-hidden />
          </button>
        </div>

        <dl className="mt-4 space-y-3">
          {answers.map(([key, value]) => (
            <div key={key} data-inbox-answer={key}>
              <dt className="text-[11.5px] font-medium text-muted">{key}</dt>
              <dd className="mt-0.5 whitespace-pre-wrap text-[12.5px]">
                {typeof value === "string" ? value : JSON.stringify(value)}
              </dd>
            </div>
          ))}
        </dl>

        {submission.consent_text ? (
          /* The consent text is shown with the answer rather than as a tick, because a row that
             says "agreed" cannot answer "agreed to what" — and that is the only question it is
             ever asked, usually later. */
          <div className="mt-4 rounded-md border border-line p-3" data-inbox-consent>
            <p className="text-[11.5px] font-medium text-muted">Accepted</p>
            <p className="mt-1 text-[12.5px]">{submission.consent_text}</p>
          </div>
        ) : null}

        <p className="mt-3 text-[11.5px] text-muted">
          From {submission.source_path ?? "an unknown path"} · spam score {submission.spam_score}/100
        </p>

        <div className="mt-4 flex flex-wrap gap-2">
          <button
            type="button"
            data-inbox-drawer-spam
            disabled={busy}
            onClick={() => onMark("spam")}
            className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            Mark spam
          </button>
          <button
            type="button"
            data-inbox-drawer-archive
            disabled={busy}
            onClick={() => onMark("archived")}
            className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            Archive
          </button>
          <button
            type="button"
            data-inbox-drawer-delete
            disabled={busy}
            onClick={() => onDelete()}
            className="inline-flex items-center gap-1.5 rounded-md border border-red-300 px-2.5 py-1.5 text-[12.5px] text-red-700 disabled:opacity-50 dark:border-red-800 dark:text-red-300"
          >
            {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <Trash2 className="h-3.5 w-3.5" aria-hidden />}
            Delete permanently
          </button>
        </div>
        <p className="mt-2 text-[11px] text-muted">
          A visitor's message has no trash. Deleting is the end of the record.
        </p>
      </div>
    </div>
  );
}
