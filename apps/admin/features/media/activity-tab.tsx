"use client";

/**
 * The activity tab of the file detail screen (docs/requests/REQ-010, slice 4).
 *
 * This is the answer to "who could see this file in March, and who deleted it" — so it is a
 * *trail*, and a trail has one rule that a list does not: **a hole in it is worse than an entry
 * somebody does not recognise.**
 *
 * Three consequences shape the screen:
 *
 * * **The action is rendered as a sentence, never as its token.** `media.share_revoked` is a
 *   database column; "A share link was revoked" is what happened. The sentence comes from the
 *   API beside the token, so the panel can never build a label out of a verb and disagree with
 *   the server about what it means — and an action the server has not seen yet is shown in its
 *   own words rather than dropped, because a trail that silently omits an entry is a trail
 *   nobody will rely on.
 * * **An unknown actor is a fact, not a gap.** A deleted account leaves `actor_user_id` null
 *   (`on delete set null`) and the row reads "the platform" for a system action. The screen says
 *   so rather than rendering an empty cell that reads as "we do not know who did this".
 * * **The detail is available but not compulsory.** Every action records structured metadata, and
 *   a reader who wants the exact filename or reason can open it — but the summary sentence
 *   already carries the part that matters, so the common case reads as a sentence rather than as
 *   a JSON blob.
 */
import { useCallback, useEffect, useState } from "react";

import { ChevronDown, ChevronRight, Clock, User, Wrench } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { fetchMediaActivity } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { MediaActivity, MediaActivityEntry } from "@/lib/types";

/**
 * Which kinds of detail an action records, and under what name.
 *
 * A whitelist rather than "render whatever is in the metadata": the metadata is the API's
 * structured detail and it will grow a field for something the panel has no rendering for, and a
 * table that prints an unknown key's value is showing somebody a database column they have to
 * decode. The two fields here are the two a reader actually asks for on a *file*.
 */
const DETAIL_KEYS: Record<string, string> = {
  filename: "File",
  reason: "Reason",
  site_id: "Site",
  filename_new: "New name",
};

/** Which icon an action's actor gets: a person, or the platform acting by itself. */
function ActorIcon({ actorType }: { actorType: string }) {
  if (actorType === "user") {
    return <User className="h-3 w-3 shrink-0" aria-hidden />;
  }
  return <Wrench className="h-3 w-3 shrink-0" aria-hidden />;
}

/** Who did it, in words that admit when there is nobody to name. */
function actorSentence(entry: MediaActivityEntry): string {
  if (entry.actor) {
    return entry.actor;
  }
  // `actor_type = 'user'` with no name means the account was deleted — `on delete set null` on
  // the audit table. Saying "the platform" there would be a false claim about a person who did
  // the work, so the two states are separated.
  return entry.actor_type === "user"
    ? "an account that has since been removed"
    : "the platform";
}

/** One row: the sentence, who, when, and the detail on demand. */
function ActivityRow({ entry }: { entry: MediaActivityEntry }) {
  const [open, setOpen] = useState(false);

  const details = Object.entries(DETAIL_KEYS)
    .map(([key, label]) => {
      const value = (entry.metadata as Record<string, unknown> | null)?.[key];
      return typeof value === "string" && value.trim() !== "" ? { label, value } : null;
    })
    .filter((item): item is { label: string; value: string } => item !== null);

  return (
    <li
      className="flex flex-col gap-1 py-3"
      data-testid="media-activity-row"
      data-action={entry.action}
    >
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <p className="text-[13px] text-ink">{entry.summary}</p>
        <span className="flex items-center gap-1 text-[11.5px] text-muted">
          <ActorIcon actorType={entry.actor_type} />
          {actorSentence(entry)}
        </span>
        <span className="flex items-center gap-1 text-[11.5px] text-muted">
          <Clock className="h-3 w-3 shrink-0" aria-hidden />
          {formatTimestamp(entry.occurred_at)}
        </span>
      </div>

      {details.length > 0 ? (
        <>
          <button
            type="button"
            onClick={() => setOpen((value) => !value)}
            className="inline-flex w-fit items-center gap-1 text-[11.5px] text-muted hover:text-ink"
            aria-expanded={open}
            data-testid="media-activity-toggle"
          >
            {open ? (
              <ChevronDown className="h-3 w-3" aria-hidden />
            ) : (
              <ChevronRight className="h-3 w-3" aria-hidden />
            )}
            {open ? "Hide detail" : "Show detail"}
          </button>
          {open ? (
            <dl className="flex flex-col gap-1 pl-1" data-testid="media-activity-detail">
              {details.map((detail) => (
                <div key={detail.label} className="flex gap-2 text-[11.5px]">
                  <dt className="text-muted">{detail.label}:</dt>
                  <dd className="break-words text-ink">{detail.value}</dd>
                </div>
              ))}
            </dl>
          ) : null}
        </>
      ) : null}
    </li>
  );
}

/** The activity tab: what has happened to this file, newest first. */
export function ActivityTab({ mediaId }: { mediaId: string }) {
  const [data, setData] = useState<MediaActivity | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setData(await fetchMediaActivity(mediaId));
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "the activity could not be read");
    }
  }, [mediaId]);

  useEffect(() => {
    void load();
  }, [load]);

  if (error) {
    return (
      <div data-testid="media-activity-tab">
        <p className="text-[11px] text-negative" role="alert" data-testid="media-activity-error">
          {error}
        </p>
      </div>
    );
  }

  if (!data) {
    return (
      <div data-testid="media-activity-tab">
        <LoadingTable columns={3} rows={3} />
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-3" data-testid="media-activity-tab">
      <p className="text-[12px] text-muted" data-testid="media-activity-note">
        Everything the platform recorded against this file. Changes to the folder it sits in, and
        to the site, are recorded against those instead.
      </p>

      {data.activity.length === 0 ? (
        <EmptyState
          title="Nothing has happened to this file yet"
          hint="An upload, a replacement, a share link and a deletion all land here. A file that arrived before this trail existed shows nothing until its next change."
        />
      ) : (
        <>
          {data.truncated ? (
            <p className="text-[11.5px] text-muted" data-testid="media-activity-truncated">
              Showing the {data.total} most recent entries.
            </p>
          ) : null}
          <ul className="flex flex-col divide-y divide-line" data-testid="media-activity-list">
            {data.activity.map((entry) => (
              <ActivityRow key={entry.id} entry={entry} />
            ))}
          </ul>
        </>
      )}
    </div>
  );
}
