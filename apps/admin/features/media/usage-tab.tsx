"use client";

/**
 * The usage tab of the file detail screen (docs/requests/REQ-010, slice 4).
 *
 * "Where is this file used?" is the question an editor asks before deleting it, and this screen
 * is built around the one thing that question cannot survive without: **a number and a list that
 * agree**.
 *
 * The trap is that the row count and the record count are different numbers. A page naming the
 * same hero in a list card, a header and a meta tag is *one* record and three rows, and a screen
 * that said "used in 3 places" beside one page is a number somebody deletes a page over. So the
 * summary sentence is the API's — not built here out of two integers — and it says which of the
 * two numbers it is reporting.
 *
 * The second half is the rows that point at nothing. A reference whose page was deleted in a
 * migration will refuse a purge for ever, and "cannot purge: still referenced" is a sentence an
 * operator meets and cannot act on. Those rows are shown, marked unresolved, and the fix — repoint
 * the record, or run the repair scan — is named on the row itself rather than left to a
 * knowledge article.
 *
 * There is deliberately no delete button. Removing a usage row here would make the library
 * *claim* a page does not point at this file while the page still does, which is worse than the
 * lie the repair scan removes: that one is about a record that no longer exists.
 */
import { useCallback, useEffect, useState } from "react";

import { AlertTriangle, ExternalLink, Link2, Unlink } from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { fetchMediaUsage } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { MediaUsage, MediaUsageEntry } from "@/lib/types";

/** How a referring record is drawn. Live is quiet; unresolvable is the loud one. */
function statusTone(status: string | null): string {
  switch (status) {
    case "published":
      return "bg-positive-soft text-positive";
    case "draft":
      return "bg-quiet-soft text-muted";
    default:
      return "bg-quiet-soft text-muted";
  }
}

/** A readable name for the kind of thing that points here. */
function kindLabel(kind: string): string {
  switch (kind) {
    case "page":
      return "Page";
    case "theme":
      return "Theme";
    case "form":
      return "Form";
    default:
      // An unknown kind is shown in its own words rather than hidden: a usage list that
      // silently drops records from a module that arrived last month is a list nobody trusts.
      return kind;
  }
}

/** The field of the record that points here, in a sentence. */
function fieldSentence(entry: MediaUsageEntry): string {
  if (!entry.field) {
    return `${kindLabel(entry.resource_kind)} ${entry.resource_id}`;
  }
  return `${kindLabel(entry.resource_kind)} · ${entry.field}`;
}

/**
 * The usage tab: the file's summary sentence and every record that points at it.
 */
export function UsageTab({ mediaId }: { mediaId: string }) {
  const [data, setData] = useState<MediaUsage | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setData(await fetchMediaUsage(mediaId));
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "the usage could not be read");
    }
  }, [mediaId]);

  useEffect(() => {
    void load();
  }, [load]);

  if (error) {
    return (
      <div data-testid="media-usage-tab">
        <p className="text-[11px] text-negative" role="alert" data-testid="media-usage-error">
          {error}
        </p>
      </div>
    );
  }

  if (!data) {
    return (
      <div data-testid="media-usage-tab">
        <LoadingTable columns={3} rows={3} />
      </div>
    );
  }

  const stale = data.usage.filter((entry) => !entry.resolved);

  return (
    <div className="flex flex-col gap-3" data-testid="media-usage-tab">
      {/* The summary is the answer to "can I delete this?", so it is a sentence and not two
          integers the reader has to combine themselves. */}
      <p className="text-[12px] text-muted" data-testid="media-usage-summary">
        {data.summary}
      </p>

      {data.usage.length === 0 ? (
        <EmptyState
          title="Nothing uses this file"
          hint="No page, theme or form on this site points at it, so deleting it breaks nothing here. A file that a module started using without recording the reference would not show up — the repair scan on the retention tab checks for that."
        />
      ) : (
        <>
          {stale.length > 0 ? (
            <p
              className="flex items-start gap-2 rounded-md bg-warn/10 px-3 py-2 text-[12px] text-warn"
              data-testid="media-usage-stale"
            >
              <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden />
              <span>
                {stale.length === 1
                  ? "One reference points at a record that no longer exists. It will refuse a "
                  : `${stale.length} references point at records that no longer exist. They will refuse a `}
                purge until they are dealt with — repoint the record, or run the repair scan on
                the retention tab.
              </span>
            </p>
          ) : null}

          {data.truncated ? (
            <p className="text-[11.5px] text-muted" data-testid="media-usage-truncated">
              Showing the first {data.usage.length} references. Narrow the library by moving this
              file or by running the repair scan to clear stale rows.
            </p>
          ) : null}

          <ul className="flex flex-col divide-y divide-line" data-testid="media-usage-list">
            {data.usage.map((entry) => (
              <li
                key={entry.id}
                className="flex flex-col gap-1 py-3 sm:flex-row sm:items-center sm:justify-between sm:gap-3"
                data-testid="media-usage-row"
                data-resolved={entry.resolved ? "1" : "0"}
              >
                <div className="flex min-w-0 flex-col gap-1">
                  <div className="flex items-center gap-2">
                    {entry.path ? (
                      <Link
                        href={entry.path}
                        className="inline-flex min-w-0 items-center gap-1.5 text-[13px] font-medium hover:underline"
                      >
                        <span className="truncate">{entry.label ?? entry.resource_id}</span>
                        <ExternalLink className="h-3 w-3 shrink-0 text-muted" aria-hidden />
                      </Link>
                    ) : (
                      <span className="inline-flex min-w-0 items-center gap-1.5 text-[13px]">
                        <Unlink className="h-3.5 w-3.5 shrink-0 text-warn" aria-hidden />
                        <span className="truncate text-muted">
                          {entry.label ?? entry.resource_id}
                        </span>
                      </span>
                    )}
                    {entry.status ? (
                      <span
                        className={`shrink-0 rounded px-1.5 py-0.5 text-[11px] ${statusTone(entry.status)}`}
                      >
                        {entry.status}
                      </span>
                    ) : null}
                    {!entry.resolved ? (
                      <span
                        className="shrink-0 rounded bg-warn/10 px-1.5 py-0.5 text-[11px] text-warn"
                        data-testid="media-usage-unresolved"
                      >
                        record not found
                      </span>
                    ) : null}
                  </div>
                  <p className="flex items-center gap-1.5 text-[11.5px] text-muted">
                    <Link2 className="h-3 w-3 shrink-0" aria-hidden />
                    <span className="truncate">{fieldSentence(entry)}</span>
                    <span aria-hidden>·</span>
                    <span>{formatTimestamp(entry.created_at)}</span>
                  </p>
                  {!entry.resolved ? (
                    <p className="text-[11.5px] text-warn">
                      This reference names {entry.resource_kind} {entry.resource_id}, which the
                      platform cannot find. Repoint it, or run the repair scan on the retention
                      tab to drop it.
                    </p>
                  ) : null}
                </div>
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}
