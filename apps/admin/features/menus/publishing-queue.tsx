"use client";

/**
 * `/publishing/queue` — every promise the platform made about when a page appears (REQ-064,
 * slice 1).
 *
 * Four things this screen refuses to do:
 *
 * 1. **"Publish now" does not publish here.** It moves the instant into the past and the worker
 *    does the work, exactly as a timed entry will. A button with its own lighter publish would
 *    leave two definitions of "published" in one platform, and they would disagree within a week
 *    — the button would succeed where the runner refused, and nobody would know which was right.
 * 2. **Only a pending row is reschedulable or cancellable.** The server refuses the others, and
 *    the buttons are not drawn on them: a button that is present-but-dead teaches people to
 *    distrust the row it sits on.
 * 3. **A failed row shows the reason, not a red pill.** The error is the platform's own words and
 *    it is the only thing that tells an editor whether retrying can possibly help.
 * 4. **The timezone label sits beside the instant, in the author's wall clock.** The instant is
 *    stored in UTC and the label is a display convention; converting one into the other here would
 *    make "9:00" mean two different things in two columns of the same table.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { CalendarClock, Loader2, Play, RefreshCw, RotateCcw, X } from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  cancelEntry,
  fetchPublishingQueue,
  publishEntryNow,
  rescheduleEntry,
  retryEntry,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { useSites } from "@/lib/sites";
import { PUBLISHING_STATUSES, type PublishingEntry } from "@/lib/types";

const STATUS_CLASS: Record<string, string> = {
  pending: "text-caution",
  done: "text-positive",
  failed: "text-red-700 dark:text-red-300",
  cancelled: "text-muted",
};

export function PublishingQueueView() {
  const [rows, setRows] = useState<PublishingEntry[] | null>(null);
  const [status, setStatus] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [rescheduling, setRescheduling] = useState<PublishingEntry | null>(null);

  const { selectedSite } = useSites();

  // The queue is the site switcher's screen, exactly like `/pages`: an entry schedules a PAGE, and
  // a page belongs to a site. Without the site in the query the server has to guess the
  // organization from the account, which refuses the platform owner outright (an owner has no
  // primary organization by design) and would list a second site's rows under this one's name.
  const load = useCallback(async () => {
    setError(null);
    if (!selectedSite) {
      setRows([]);
      return;
    }
    try {
      const base = { site_id: selectedSite.id, limit: 200 };
      setRows(await fetchPublishingQueue(status ? { ...base, status } : base));
    } catch (caught) {
      setError((caught as ApiError).message);
    }
  }, [status, selectedSite]);

  useEffect(() => {
    void load();
  }, [load]);

  const act = useCallback(
    async (row: PublishingEntry, verb: "cancel" | "now" | "retry", label: string) => {
      setBusyId(row.id);
      setNotice(null);
      setError(null);
      try {
        if (verb === "cancel") await cancelEntry(row.id);
        if (verb === "now") await publishEntryNow(row.id);
        if (verb === "retry") await retryEntry(row.id);
        setNotice(label);
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusyId(null);
      }
    },
    [load],
  );

  const counts = useMemo(() => {
    const tally: Record<string, number> = {};
    for (const row of rows ?? []) {
      tally[row.status] = (tally[row.status] ?? 0) + 1;
    }
    return tally;
  }, [rows]);

  if (rows === null && !error) return <QueueSkeleton />;

  return (
    <div className="space-y-6" data-queue-state="ready">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-wrap gap-2" role="group" aria-label="Filter by state">
          {PUBLISHING_STATUSES.map((entry) => (
            <button
              key={entry.value || "all"}
              type="button"
              data-queue-chip={entry.value || "all"}
              aria-pressed={status === entry.value}
              onClick={() => setStatus(entry.value)}
              className={`rounded-full border px-3 py-1 text-[12px] ${
                status === entry.value ? "border-line bg-quiet-soft" : "border-line"
              }`}
            >
              {entry.label}
              {/* The count is the *loaded* page's, so it is only drawn for a state this response
                  actually carries. A chip counting a filtered-out state would answer a question
                  nobody asked, and a "0" above a filter that hides the rows is worse than none. */}
              {entry.value && entry.value in counts ? ` (${counts[entry.value]})` : ""}
            </button>
          ))}
        </div>
        <button
          type="button"
          data-queue-refresh
          onClick={() => void load()}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
        >
          <RefreshCw className="h-3.5 w-3.5" aria-hidden />
          Refresh
        </button>
      </div>

      {notice ? (
        <p data-queue-notice className="text-[12.5px] text-muted">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p data-queue-error className="text-[12.5px] text-red-700 dark:text-red-300">
          {error}
        </p>
      ) : null}

      {rescheduling ? (
        <RescheduleForm
          entry={rescheduling}
          onCancel={() => setRescheduling(null)}
          onError={setError}
          onSaved={async () => {
            setRescheduling(null);
            setNotice("Moved. The worker will pick it up at the new time.");
            await load();
          }}
        />
      ) : null}

      {rows && rows.length === 0 ? (
        <div className="rounded-lg border border-line" data-queue-empty>
          <EmptyState
            title={status ? `No ${status} entries` : "Nothing is scheduled"}
            hint={
              status
                ? "No entry is in the state you picked. Clear the filter to see the rest."
                : "Schedule a publish or unpublish from a page's screen and it lands here, with the instant it will fire."
            }
          />
        </div>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-line">
          <table className="w-full min-w-[760px] border-collapse text-[13px]">
            <caption className="sr-only">Every scheduled publish and unpublish</caption>
            <thead>
              <tr className="border-b border-line bg-quiet-soft">
                <th scope="col" className="px-3 py-2 text-left font-medium">Page</th>
                <th scope="col" className="px-3 py-2 text-left font-medium">Action</th>
                <th scope="col" className="px-3 py-2 text-left font-medium">When</th>
                <th scope="col" className="px-3 py-2 text-left font-medium">State</th>
                <th scope="col" className="px-3 py-2 text-right font-medium">Actions</th>
              </tr>
            </thead>
            <tbody>
              {(rows ?? []).map((row) => (
                <tr key={row.id} data-queue-row={row.id} className="border-b border-line last:border-0">
                  <td className="px-3 py-2">
                    <Link
                      href={`/pages/${row.page_id}/edit`}
                      className="underline decoration-dotted"
                      data-queue-page={row.page_id}
                    >
                      {row.page_title}
                    </Link>
                    <span className="block text-[11.5px] text-muted">
                      /{row.page_slug} · {row.page_type}
                    </span>
                  </td>
                  <td className="px-3 py-2">{row.action}</td>
                  <td className="px-3 py-2">
                    {formatTimestamp(row.scheduled_at)}
                    {/* The author's own timezone, printed rather than converted: the instant is UTC
                        and the label is a wall clock, and collapsing the two is how a 9:00 post
                        goes out at 6:00. */}
                    <span className="block text-[11.5px] text-muted">{row.timezone}</span>
                  </td>
                  <td className={`px-3 py-2 ${STATUS_CLASS[row.status] ?? ""}`}>
                    <span className="font-medium">{row.status}</span>
                    {row.result ? (
                      <span className="block text-[11.5px] text-muted">{row.result}</span>
                    ) : null}
                    {/* The reason, in the platform's words. A red pill on its own tells an editor
                        nothing about whether a retry can help. */}
                    {row.error ? (
                      <span data-queue-error-text className="block text-[11.5px] text-red-700 dark:text-red-300">
                        {row.error}
                      </span>
                    ) : null}
                  </td>
                  <td className="px-3 py-2 text-right">
                    <span className="flex flex-wrap justify-end gap-1.5">
                      {row.status === "pending" ? (
                        <>
                          <button
                            type="button"
                            data-queue-reschedule={row.id}
                            onClick={() => setRescheduling(row)}
                            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px]"
                          >
                            <CalendarClock className="h-3.5 w-3.5" aria-hidden />
                            Reschedule
                          </button>
                          <button
                            type="button"
                            data-queue-publish-now={row.id}
                            disabled={busyId === row.id}
                            onClick={() =>
                              void act(row, "now", "Made due. The worker publishes it on its next pass.")
                            }
                            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px] disabled:opacity-50"
                          >
                            {busyId === row.id ? (
                              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                            ) : (
                              <Play className="h-3.5 w-3.5" aria-hidden />
                            )}
                            Publish now
                          </button>
                          <button
                            type="button"
                            data-queue-cancel={row.id}
                            disabled={busyId === row.id}
                            onClick={() => void act(row, "cancel", "Cancelled.")}
                            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px] disabled:opacity-50"
                          >
                            <X className="h-3.5 w-3.5" aria-hidden />
                            Cancel
                          </button>
                        </>
                      ) : null}
                      {row.status === "failed" ? (
                        <button
                          type="button"
                          data-queue-retry={row.id}
                          disabled={busyId === row.id}
                          onClick={() =>
                            void act(row, "retry", "Back in the queue, one minute out.")
                          }
                          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px] disabled:opacity-50"
                        >
                          {busyId === row.id ? (
                            <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                          ) : (
                            <RotateCcw className="h-3.5 w-3.5" aria-hidden />
                          )}
                          Retry
                        </button>
                      ) : null}
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

/** A datetime-local field, converted to the RFC 3339 instant the store compares. */
function RescheduleForm({
  entry,
  onCancel,
  onSaved,
  onError,
}: {
  entry: PublishingEntry;
  onCancel: () => void;
  onSaved: () => Promise<void>;
  onError: (message: string | null) => void;
}) {
  const [value, setValue] = useState(() => {
    const parsed = new Date(entry.scheduled_at);
    if (Number.isNaN(parsed.getTime())) return "";
    // `datetime-local` takes a *local* wall clock with no zone suffix, so the ISO string has to
    // be sliced rather than passed whole: `toISOString` would hand the field UTC and the editor
    // would move the entry by their own offset.
    const offset = parsed.getTimezoneOffset() * 60_000;
    return new Date(parsed.getTime() - offset).toISOString().slice(0, 16);
  });
  const [busy, setBusy] = useState(false);

  const submit = useCallback(async () => {
    const parsed = new Date(value);
    if (Number.isNaN(parsed.getTime())) {
      onError("That is not a date this browser can read.");
      return;
    }
    setBusy(true);
    onError(null);
    try {
      await rescheduleEntry(entry.id, parsed.toISOString());
      await onSaved();
    } catch (caught) {
      onError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  }, [value, entry.id, onSaved, onError]);

  return (
    <form
      data-queue-reschedule-form={entry.id}
      className="space-y-3 rounded-lg border border-line p-4"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <p className="text-[13px] font-medium">
        Move “{entry.page_title}” ({entry.action})
      </p>
      <label className="flex flex-col gap-1 text-[12px]">
        <span className="text-muted">New time — your browser's clock ({entry.timezone} on the row)</span>
        <input
          type="datetime-local"
          required
          data-queue-reschedule-input
          value={value}
          onChange={(event) => setValue(event.target.value)}
          className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
        />
      </label>
      <div className="flex gap-2">
        <button
          type="submit"
          data-queue-reschedule-save
          disabled={busy || !value}
          className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-50"
        >
          {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
          Move it
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
        >
          Cancel
        </button>
      </div>
    </form>
  );
}

function QueueSkeleton() {
  return (
    <div className="space-y-3" data-queue-state="loading" aria-busy="true">
      <div className="h-7 w-80 animate-pulse rounded-full bg-quiet-soft" />
      {Array.from({ length: 5 }, (_, index) => (
        <div key={index} className="h-12 animate-pulse rounded bg-quiet-soft" />
      ))}
    </div>
  );
}
