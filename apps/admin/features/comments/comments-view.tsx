"use client";

/**
 * `/comments` — the moderation inbox and the per-site policy (REQ-064, slice 4a).
 *
 * A moderation queue is read in one moment — somebody wants to know whether the thing they are
 * reading has comments on it, and whether any of them are junk. Five decisions this screen
 * makes, each one a place the obvious version misleads somebody:
 *
 * 1. **A spam row is shown, and it says WHY.** The heuristic that marked it stores its reason,
 *    and the panel prints it. A queue that hides spam cannot be compared against a queue that
 *    does, and an owner who cannot see what the platform decided on their behalf cannot argue
 *    with it. This is the REQ's own risk note, in the place it applies.
 * 2. **The tab counts come from the same read as the rows.** Four tabs with four numbers and one
 *    table is one document, and a count that came from a second request is a count from a
 *    different moment — which on a busy queue is a number that disagrees with the table under
 *    it.
 * 3. **A bulk action reports what it skipped.** The API answers per comment, and the panel says
 *    "8 of 10 moved, 2 were already approved" rather than "10 moderated". The two that did not
 *    move are the ones a moderator needs to know about.
 * 4. **`Ban` never shows the address it stored for an IP.** The submission route fingerprints the
 *    sender, so the ban value is a fingerprint, and the panel copies it rather than pretending it
 *    can display something it deliberately does not have.
 * 5. **The policy form is the whole policy, and the server keeps what it was not sent.** Every
 *    control is visible at once because every control is a decision an owner makes once and then
 *    forgets; a screen that hides the link limit behind a "more" toggle hides it from the person
 *    whose site is about to be spammed.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  Ban as BanIcon,
  Check,
  Loader2,
  MessageSquare,
  RefreshCw,
  Search,
  Send,
  ShieldOff,
  Trash2,
  Undo2,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  addCommentBan,
  bulkModerateComments,
  deleteComment,
  fetchCommentInbox,
  fetchCommentSettings,
  moderateComment,
  removeCommentBan,
  replyToComment,
  saveCommentSettings,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { useSites } from "@/lib/sites";
import type { CommentInboxRow, CommentSettings, CommentStatus } from "@/lib/types";

/** The tabs, in the order the queue is worked. */
const TABS: { key: CommentStatus; label: string; empty: string }[] = [
  {
    key: "pending",
    label: "Pending",
    empty: "Nothing is waiting for a decision. New comments arrive here as soon as a visitor leaves one.",
  },
  {
    key: "approved",
    label: "Approved",
    empty: "No comment is published on this site yet. An approved comment appears on its page immediately.",
  },
  {
    key: "spam",
    label: "Spam",
    empty:
      "No comment has tripped a filter. Nothing is deleted automatically — anything the platform marks lands here for you to agree with.",
  },
  {
    key: "trash",
    label: "Trash",
    empty: "Nothing has been deleted. A trashed comment is recoverable; only the trash tab can remove it for good.",
  },
];

/** The tab the screen opens on. */
const DEFAULT_TAB: CommentStatus = "pending";

/** How a status is labelled on a row. */
const STATUS_LABEL: Record<CommentStatus, string> = {
  pending: "Pending",
  approved: "Published",
  spam: "Spam",
  trash: "Trashed",
};

export function CommentsView() {
  const { selectedSite, status: siteStatus, error: siteError } = useSites();
  const siteId = selectedSite?.id ?? null;

  const [tab, setTab] = useState<CommentStatus>(DEFAULT_TAB);
  const [search, setSearch] = useState("");
  const [appliedSearch, setAppliedSearch] = useState("");
  const [inbox, setInbox] = useState<Awaited<ReturnType<typeof fetchCommentInbox>> | null>(null);
  const [document, setDocument] = useState<Awaited<ReturnType<typeof fetchCommentSettings>> | null>(
    null,
  );
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [checked, setChecked] = useState<string[]>([]);
  const [open, setOpen] = useState<CommentInboxRow | null>(null);
  const [banning, setBanning] = useState<CommentInboxRow | null>(null);
  const [savingPolicy, setSavingPolicy] = useState(false);

  const filters = useMemo(
    () => ({ site_id: siteId ?? "", status: tab, search: appliedSearch || undefined, limit: 50 }),
    [siteId, tab, appliedSearch],
  );

  const load = useCallback(async () => {
    if (!siteId) return;
    setError(null);
    try {
      const [next, settings] = await Promise.all([
        fetchCommentInbox(filters),
        fetchCommentSettings(siteId),
      ]);
      setInbox(next);
      setDocument(settings);
      // Selection clears on every load: a bulk action against a selection that no longer exists
      // is a bulk action on somebody else's rows.
      setChecked([]);
    } catch (caught) {
      setError((caught as ApiError).message);
    }
  }, [siteId, filters]);

  useEffect(() => {
    void load();
  }, [load]);

  // A site change invalidates everything on screen, including a drawer that was open against
  // the previous site — a comment detail that survives a site switch is a comment from a
  // different tenant.
  useEffect(() => {
    setOpen(null);
    setBanning(null);
    setNotice(null);
  }, [siteId]);

  const counts = useMemo(() => {
    const map = new Map<string, number>();
    for (const row of inbox?.counts ?? []) map.set(row.status, row.count);
    return map;
  }, [inbox]);

  const move = useCallback(
    async (comment: CommentInboxRow, next: CommentStatus, reason?: string) => {
      if (!siteId) return;
      setBusy(true);
      setError(null);
      try {
        await moderateComment(comment.id, siteId, next, reason);
        setNotice(
          next === "approved"
            ? "Published. It is on the page now."
            : next === "spam"
              ? "Marked as spam. It leaves the published list and stays here for you to check."
              : next === "trash"
                ? "Moved to trash. A trashed comment is recoverable from the Trash tab."
                : "Back in the queue.",
        );
        setOpen(null);
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [siteId, load],
  );

  const bulk = useCallback(
    async (next: CommentStatus) => {
      if (!siteId || checked.length === 0) return;
      setBusy(true);
      setError(null);
      try {
        const result = await bulkModerateComments(siteId, checked, next);
        // The report is the point: "8 of 10" tells a moderator two rows did not move and why
        // they might look again, and "10 of 10" when two rows were already in that state is a
        // claim the panel cannot support.
        if (result.complete) {
          setNotice(`${result.updated.length} moved to ${STATUS_LABEL[next].toLowerCase()}.`);
        } else {
          const parts = [`${result.updated.length} of ${result.requested} moved`];
          if (result.refused.length > 0) {
            parts.push(
              `${result.refused.length} already in that state`,
            );
          }
          if (result.missing.length > 0) parts.push(`${result.missing.length} no longer exist`);
          setNotice(`${parts.join(", ")}.`);
        }
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [siteId, checked, load],
  );

  const purge = useCallback(
    async (comment: CommentInboxRow) => {
      if (!siteId) return;
      setBusy(true);
      setError(null);
      try {
        await deleteComment(comment.id, siteId);
        setNotice("Removed for good. This is the only irreversible action in comments.");
        setOpen(null);
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [siteId, load],
  );

  if (siteStatus === "loading" && !inbox) {
    return (
      <div className="space-y-3" data-comments-state="loading">
        <div className="h-8 w-64 animate-pulse rounded-md bg-surface" />
        <div className="h-64 animate-pulse rounded-md bg-surface" />
      </div>
    );
  }

  if (!siteId) {
    return (
      <div data-comments-state="no-site">
        <EmptyState
          title="Pick a site"
          hint="Comments belong to a site. Choose one from the header to open its queue."
        />
      </div>
    );
  }

  const rows = inbox?.comments ?? [];
  const allChecked = rows.length > 0 && checked.length === rows.length;

  return (
    <div className="space-y-6" data-comments-state="ready" data-comments-site={siteId}>
      <PolicyPanel
        document={document}
        saving={savingPolicy}
        onSave={async (next) => {
          setSavingPolicy(true);
          setError(null);
          try {
            const saved = await saveCommentSettings(siteId, next);
            setDocument(saved);
            setNotice(
              saved.settings.comments_enabled
                ? "Comments are on for this site."
                : "Comments are off. The public form accepts nothing and existing comments stay where they are.",
            );
          } catch (caught) {
            setError((caught as ApiError).message);
          } finally {
            setSavingPolicy(false);
          }
        }}
      />

      {siteError ? <ErrorStrip message={siteError} onRetry={() => void load()} /> : null}
      {error ? <ErrorStrip message={error} onRetry={() => void load()} /> : null}
      {notice ? (
        <div
          role="status"
          data-comments-notice
          className="flex items-start gap-2 rounded-md border border-line bg-surface px-3 py-2 text-[12.5px]"
        >
          <Check className="mt-0.5 h-3.5 w-3.5 shrink-0 text-ok" aria-hidden />
          <span className="flex-1">{notice}</span>
          <button type="button" onClick={() => setNotice(null)} aria-label="Dismiss">
            <X className="h-3.5 w-3.5" aria-hidden />
          </button>
        </div>
      ) : null}

      <section className="space-y-3" aria-label="Moderation queue">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex flex-wrap gap-1" role="tablist" aria-label="Moderation states">
            {TABS.map((entry) => {
              const count = counts.get(entry.key) ?? 0;
              const active = entry.key === tab;
              return (
                <button
                  key={entry.key}
                  type="button"
                  role="tab"
                  aria-selected={active}
                  data-comment-tab={entry.key}
                  onClick={() => {
                    setTab(entry.key);
                    setError(null);
                    setNotice(null);
                  }}
                  className={[
                    "inline-flex items-center gap-1.5 rounded-md border px-2.5 py-1.5 text-[12.5px]",
                    active
                      ? "border-accent bg-accent/10 text-ink"
                      : "border-line text-muted hover:text-ink",
                  ].join(" ")}
                >
                  {entry.label}
                  <span
                    data-comment-tab-count={entry.key}
                    className={[
                      "rounded px-1.5 py-0.5 text-[11px] tabular-nums",
                      count > 0 ? "bg-surface-2 text-ink" : "text-muted",
                    ].join(" ")}
                  >
                    {count}
                  </span>
                </button>
              );
            })}
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <form
              data-comment-search-form
              onSubmit={(event) => {
                event.preventDefault();
                setAppliedSearch(search.trim());
              }}
              className="flex items-center gap-1.5"
            >
              <label htmlFor="comment-search" className="sr-only">
                Search comments
              </label>
              <input
                id="comment-search"
                data-comment-search
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder="Name, address or text"
                className="w-56 rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
              />
              <button
                type="submit"
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
              >
                <Search className="h-3.5 w-3.5" aria-hidden />
                Search
              </button>
            </form>
            <button
              type="button"
              data-comments-refresh
              onClick={() => void load()}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
            >
              <RefreshCw className="h-3.5 w-3.5" aria-hidden />
              Refresh
            </button>
          </div>
        </div>

        {checked.length > 0 ? (
          <div
            data-comment-bulk-bar
            className="sticky top-12 z-10 flex flex-wrap items-center gap-2 rounded-md border border-line bg-surface px-3 py-2 text-[12.5px] shadow-sm"
          >
            <span className="tabular-nums">
              {checked.length} selected of {rows.length}
            </span>
            <BulkButton
              label="Approve"
              icon={<Check className="h-3.5 w-3.5" aria-hidden />}
              disabled={busy}
              onClick={() => void bulk("approved")}
            />
            <BulkButton
              label="Spam"
              icon={<ShieldOff className="h-3.5 w-3.5" aria-hidden />}
              disabled={busy}
              onClick={() => void bulk("spam")}
            />
            <BulkButton
              label="Trash"
              icon={<Trash2 className="h-3.5 w-3.5" aria-hidden />}
              disabled={busy}
              onClick={() => void bulk("trash")}
            />
            {tab === "trash" ? (
              <BulkButton
                label="Restore"
                icon={<Undo2 className="h-3.5 w-3.5" aria-hidden />}
                disabled={busy}
                onClick={() => void bulk("pending")}
              />
            ) : null}
            <button
              type="button"
              onClick={() => setChecked([])}
              className="ml-auto rounded-md border border-line px-2.5 py-1.5"
            >
              Clear
            </button>
          </div>
        ) : null}

        {inbox === null ? (
          <div className="space-y-2" aria-busy="true">
            {[0, 1, 2].map((row) => (
              <div key={row} className="h-14 animate-pulse rounded-md bg-surface" />
            ))}
          </div>
        ) : rows.length === 0 ? (
          <EmptyState
            title={`${STATUS_LABEL[tab]} is empty`}
            hint={TABS.find((entry) => entry.key === tab)?.empty}
            action={
              appliedSearch ? (
                <button
                  type="button"
                  onClick={() => {
                    setSearch("");
                    setAppliedSearch("");
                  }}
                  className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
                >
                  Clear the search
                </button>
              ) : undefined
            }
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[720px] border-collapse text-left text-[12.5px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                  <th scope="col" className="w-8 py-2">
                    <label className="sr-only" htmlFor="comment-check-all">
                      Select every comment in this tab
                    </label>
                    <input
                      id="comment-check-all"
                      data-comment-check-all
                      type="checkbox"
                      checked={allChecked}
                      onChange={(event) =>
                        setChecked(event.target.checked ? rows.map((row) => row.id) : [])
                      }
                    />
                  </th>
                  <th scope="col" className="py-2">Author</th>
                  <th scope="col" className="py-2">Comment</th>
                  <th scope="col" className="py-2">Page</th>
                  <th scope="col" className="py-2">Submitted</th>
                  <th scope="col" className="py-2">Why</th>
                  <th scope="col" className="py-2 text-right">Actions</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((comment) => (
                  <tr
                    key={comment.id}
                    data-comment-row={comment.id}
                    data-comment-status={comment.status}
                    className="border-b border-line/60 align-top"
                  >
                    <td className="py-2.5">
                      <label className="sr-only" htmlFor={`comment-check-${comment.id}`}>
                        Select the comment by {comment.author_name}
                      </label>
                      <input
                        id={`comment-check-${comment.id}`}
                        data-comment-check={comment.id}
                        type="checkbox"
                        checked={checked.includes(comment.id)}
                        onChange={(event) =>
                          setChecked((current) =>
                            event.target.checked
                              ? [...current, comment.id]
                              : current.filter((id) => id !== comment.id),
                          )
                        }
                      />
                    </td>
                    <td className="py-2.5 pr-3">
                      <div className="font-medium">{comment.author_name}</div>
                      <div className="text-muted">{comment.author_email}</div>
                      {comment.is_staff_reply ? (
                        <span className="mt-1 inline-block rounded bg-surface-2 px-1.5 py-0.5 text-[11px]">
                          Site reply
                        </span>
                      ) : null}
                    </td>
                    <td className="py-2.5 pr-3">
                      <button
                        type="button"
                        data-comment-open={comment.id}
                        onClick={() => setOpen(comment)}
                        className="text-left hover:underline"
                      >
                        {excerpt(comment.body)}
                      </button>
                    </td>
                    <td className="py-2.5 pr-3 text-muted">
                      {comment.page_title ?? "—"}
                    </td>
                    <td className="py-2.5 pr-3 text-muted whitespace-nowrap">
                      {formatTimestamp(comment.created_at)}
                    </td>
                    <td className="py-2.5 pr-3">
                      {comment.spam_reason ? (
                        <span
                          data-comment-reason={comment.id}
                          className="inline-flex items-start gap-1 text-[11.5px] text-warn"
                        >
                          <AlertTriangle className="mt-0.5 h-3 w-3 shrink-0" aria-hidden />
                          {comment.spam_reason}
                        </span>
                      ) : (
                        <span className="text-[11.5px] text-muted">
                          {comment.approved_at
                            ? `Published ${formatTimestamp(comment.approved_at)}`
                            : "—"}
                        </span>
                      )}
                    </td>
                    <td className="py-2.5 text-right">
                      <RowActions
                        comment={comment}
                        onOpen={() => setOpen(comment)}
                        onMove={(next) => void move(comment, next)}
                        onBan={() => setBanning(comment)}
                        onPurge={() => void purge(comment)}
                        onRestore={() => void move(comment, "pending")}
                      />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>

      {open ? (
        <CommentDrawer
          comment={open}
          siteId={siteId}
          busy={busy}
          onClose={() => setOpen(null)}
          onReplied={async () => {
            setOpen(null);
            setNotice("Your reply is published on the page.");
            await load();
          }}
          onMove={(next) => void move(open, next)}
          onBan={() => {
            setBanning(open);
            setOpen(null);
          }}
          onPurge={() => void purge(open)}
        />
      ) : null}

      {banning && document ? (
        <BanDialog
          comment={banning}
          busy={busy}
          onClose={() => setBanning(null)}
          onPlace={async (kind, value, reason) => {
            setBusy(true);
            setError(null);
            try {
              await addCommentBan(siteId, { kind, value, reason });
              const fresh = await fetchCommentSettings(siteId);
              setDocument(fresh);
              setBanning(null);
              setNotice(
                kind === "ip"
                  ? "Address banned. Only its fingerprint is stored, not the address itself."
                  : "Address banned. Its comments are refused and no new ones are accepted.",
              );
            } catch (caught) {
              setError((caught as ApiError).message);
              setBanning(null);
            } finally {
              setBusy(false);
            }
          }}
        />
      ) : null}

      {document ? (
        <BanList
          bans={document.bans}
          busy={busy}
          onRemove={async (id) => {
            setBusy(true);
            setError(null);
            try {
              await removeCommentBan(siteId, id);
              const fresh = await fetchCommentSettings(siteId);
              setDocument(fresh);
              setNotice("Ban lifted. That address can comment again.");
            } catch (caught) {
              setError((caught as ApiError).message);
            } finally {
              setBusy(false);
            }
          }}
        />
      ) : null}
    </div>
  );
}

/** The four actions a row offers, chosen by its state. */
function RowActions({
  comment,
  onOpen,
  onMove,
  onBan,
  onPurge,
  onRestore,
}: {
  comment: CommentInboxRow;
  onOpen: () => void;
  onMove: (next: CommentStatus) => void;
  onBan: () => void;
  onPurge: () => void;
  onRestore: () => void;
}) {
  const ghost =
    "inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px]";

  return (
    <div className="flex flex-wrap justify-end gap-1.5">
      {comment.status === "pending" ? (
        <>
          <button
            type="button"
            data-comment-approve={comment.id}
            onClick={() => onMove("approved")}
            className={ghost}
          >
            <Check className="h-3 w-3" aria-hidden />
            Approve
          </button>
          <button
            type="button"
            data-comment-spam={comment.id}
            onClick={() => onMove("spam")}
            className={ghost}
          >
            <ShieldOff className="h-3 w-3" aria-hidden />
            Spam
          </button>
        </>
      ) : null}
      {comment.status === "approved" ? (
        <button
          type="button"
          data-comment-trash={comment.id}
          onClick={() => onMove("trash")}
          className={ghost}
        >
          <Trash2 className="h-3 w-3" aria-hidden />
          Unpublish
        </button>
      ) : null}
      {comment.status === "trash" ? (
        <>
          <button
            type="button"
            data-comment-restore={comment.id}
            onClick={onRestore}
            className={ghost}
          >
            <Undo2 className="h-3 w-3" aria-hidden />
            Restore
          </button>
          <button
            type="button"
            data-comment-delete={comment.id}
            onClick={onPurge}
            className={ghost}
          >
            <Trash2 className="h-3 w-3" aria-hidden />
            Delete
          </button>
        </>
      ) : null}
      {comment.status === "spam" ? (
        <button
          type="button"
          data-comment-approve={comment.id}
          onClick={() => onMove("approved")}
          className={ghost}
        >
          {/* A spam row that was NOT spam is the mistake this button undoes, and it is the
              mistake a heuristic makes: the row carries its reason so a moderator can judge. */}
          <Check className="h-3 w-3" aria-hidden />
          Not spam
        </button>
      ) : null}
      <button
        type="button"
        data-comment-detail={comment.id}
        onClick={onOpen}
        className={ghost}
      >
        <MessageSquare className="h-3 w-3" aria-hidden />
        Open
      </button>
      <button
        type="button"
        data-comment-ban={comment.id}
        onClick={onBan}
        className={ghost}
      >
        <BanIcon className="h-3 w-3" aria-hidden />
        Ban
      </button>
    </div>
  );
}

/** The detail drawer: the whole body, the replies, and a reply box. */
function CommentDrawer({
  comment,
  siteId,
  busy,
  onClose,
  onMove,
  onBan,
  onPurge,
  onReplied,
}: {
  comment: CommentInboxRow;
  siteId: string;
  busy: boolean;
  onClose: () => void;
  onMove: (next: CommentStatus) => void;
  onBan: () => void;
  onPurge: () => void;
  onReplied: () => void;
}) {
  const [replyBody, setReplyBody] = useState("");
  const [replyName, setReplyName] = useState("The site");
  const [error, setError] = useState<string | null>(null);

  const isReply = comment.parent_id !== null;

  return (
    <div
      data-comment-drawer={comment.id}
      className="fixed inset-0 z-30 flex justify-end bg-black/30"
      role="dialog"
      aria-modal="true"
      aria-label={`Comment by ${comment.author_name}`}
      onKeyDown={(event) => {
        if (event.key === "Escape") onClose();
      }}
    >
      <div
        className="flex h-full w-full max-w-lg flex-col gap-4 overflow-y-auto border-l border-line bg-panel p-5"
        onClick={(event) => event.stopPropagation()}
      >
        <div className="flex items-start justify-between gap-3">
          <div>
            <h2 className="text-[15px] font-medium">{comment.author_name}</h2>
            <p className="text-[12px] text-muted">
              {comment.author_email}
              {comment.ip_hint ? ` · client ${comment.ip_hint.slice(0, 12)}…` : ""}
            </p>
            <p className="text-[12px] text-muted">
              on {comment.page_title ?? "a page"} · {formatTimestamp(comment.created_at)}
            </p>
          </div>
          <button type="button" onClick={onClose} aria-label="Close" data-comment-drawer-close>
            <X className="h-4 w-4" aria-hidden />
          </button>
        </div>

        {comment.spam_reason ? (
          <p
            data-comment-drawer-reason
            className="flex items-start gap-2 rounded-md border border-line px-3 py-2 text-[12.5px] text-warn"
          >
            <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden />
            <span>
              The platform marked this as spam: {comment.spam_reason}. Nothing was deleted, and
              you can publish it anyway if the filter is wrong.
            </span>
          </p>
        ) : null}

        <p
          data-comment-drawer-body
          className="whitespace-pre-wrap rounded-md border border-line bg-surface px-3 py-2.5 text-[13px]"
        >
          {comment.body}
        </p>

        {isReply ? (
          <p className="text-[12px] text-muted">This is a reply, so it cannot itself be replied to.</p>
        ) : (
          <form
            data-comment-reply-form
            className="space-y-2"
            onSubmit={async (event) => {
              event.preventDefault();
              if (!siteId || replyBody.trim() === "") return;
              setError(null);
              try {
                await replyToComment(comment.id, {
                  site_id: siteId,
                  author_name: replyName.trim() || "The site",
                  body: replyBody.trim(),
                });
                setReplyBody("");
                onReplied();
              } catch (caught) {
                setError((caught as ApiError).message);
              }
            }}
          >
            <p className="text-[12.5px] font-medium">Reply as the site</p>
            <div className="flex flex-wrap items-center gap-2">
              <label htmlFor="comment-reply-name" className="text-[12px] text-muted">
                Name
              </label>
              <input
                id="comment-reply-name"
                data-comment-reply-name
                value={replyName}
                onChange={(event) => setReplyName(event.target.value)}
                className="w-40 rounded-md border border-line bg-surface px-2 py-1 text-[12.5px]"
              />
            </div>
            <label htmlFor="comment-reply-body" className="block text-[12px] text-muted">
              Your answer
            </label>
            <textarea
              id="comment-reply-body"
              data-comment-reply-body
              value={replyBody}
              onChange={(event) => setReplyBody(event.target.value)}
              rows={3}
              className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
            />
            {error ? <p className="text-[12px] text-danger">{error}</p> : null}
            <button
              type="submit"
              data-comment-reply-send
              disabled={busy || replyBody.trim() === ""}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
            >
              {busy ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
              ) : (
                <Send className="h-3.5 w-3.5" aria-hidden />
              )}
              Publish the reply
            </button>
          </form>
        )}

        <div className="mt-auto flex flex-wrap gap-1.5 border-t border-line pt-3">
          {comment.status !== "approved" ? (
            <ActionButton
              label="Approve"
              icon={<Check className="h-3.5 w-3.5" aria-hidden />}
              hook="comment-drawer-approve"
              onClick={() => onMove("approved")}
            />
          ) : null}
          {comment.status !== "spam" ? (
            <ActionButton
              label="Spam"
              icon={<ShieldOff className="h-3.5 w-3.5" aria-hidden />}
              hook="comment-drawer-spam"
              onClick={() => onMove("spam")}
            />
          ) : null}
          {comment.status !== "trash" ? (
            <ActionButton
              label="Trash"
              icon={<Trash2 className="h-3.5 w-3.5" aria-hidden />}
              hook="comment-drawer-trash"
              onClick={() => onMove("trash")}
            />
          ) : null}
          <ActionButton
            label="Ban this address"
            icon={<BanIcon className="h-3.5 w-3.5" aria-hidden />}
            hook="comment-drawer-ban"
            onClick={onBan}
          />
          {comment.status === "trash" ? (
            <ActionButton
              label="Delete for good"
              icon={<Trash2 className="h-3.5 w-3.5" aria-hidden />}
              hook="comment-drawer-delete"
              onClick={onPurge}
            />
          ) : null}
        </div>
      </div>
    </div>
  );
}

/** One drawer action. */
function ActionButton({
  label,
  icon,
  hook,
  onClick,
}: {
  label: string;
  icon: React.ReactNode;
  hook: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      data-comment-drawer-action={hook}
      onClick={onClick}
      className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
    >
      {icon}
      {label}
    </button>
  );
}

/** One bulk-bar action. */
function BulkButton({
  label,
  icon,
  disabled,
  onClick,
}: {
  label: string;
  icon: React.ReactNode;
  disabled: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      data-comment-bulk={label.toLowerCase()}
      disabled={disabled}
      onClick={onClick}
      className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1 text-[12px] disabled:opacity-50"
    >
      {icon}
      {label}
    </button>
  );
}

/** The per-site policy, every control visible at once. */
function PolicyPanel({
  document,
  saving,
  onSave,
}: {
  document: { settings: CommentSettings; bans: unknown[] } | null;
  saving: boolean;
  onSave: (next: Partial<CommentSettings> & { comments_enabled: boolean }) => void;
}) {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [autoApprove, setAutoApprove] = useState<number | null>(null);
  const [linkLimit, setLinkLimit] = useState<number | null>(null);
  const [fillSeconds, setFillSeconds] = useState<number | null>(null);
  const [perHour, setPerHour] = useState<number | null>(null);
  const [words, setWords] = useState<string | null>(null);
  const [notify, setNotify] = useState<boolean | null>(null);

  // Seed the controls from the stored document once it arrives. A control that shows a default
  // before the read finishes is a control that reports a setting nobody chose.
  useEffect(() => {
    if (!document) return;
    setEnabled(document.settings.comments_enabled);
    setAutoApprove(document.settings.auto_approve_after_comments);
    setLinkLimit(document.settings.max_links_per_comment);
    setFillSeconds(document.settings.min_fill_seconds);
    setPerHour(document.settings.per_ip_per_hour);
    setWords(document.settings.blocked_words.join("\n"));
    setNotify(document.settings.notify_on_comment);
  }, [document]);

  if (!document || enabled === null) {
    return (
      <section
        aria-label="Comment policy"
        data-comment-policy="loading"
        className="h-40 animate-pulse rounded-md bg-surface"
      />
    );
  }

  const dirty =
    enabled !== document.settings.comments_enabled ||
    autoApprove !== document.settings.auto_approve_after_comments ||
    linkLimit !== document.settings.max_links_per_comment ||
    fillSeconds !== document.settings.min_fill_seconds ||
    perHour !== document.settings.per_ip_per_hour ||
    words !== document.settings.blocked_words.join("\n") ||
    notify !== document.settings.notify_on_comment;

  return (
    <section
      aria-label="Comment policy"
      data-comment-policy="ready"
      data-comment-policy-enabled={enabled ? "on" : "off"}
      className="space-y-3 rounded-md border border-line bg-panel p-4"
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-[14px] font-medium">Comment policy</h2>
          <p className="text-[12px] text-muted">
            Nothing is deleted automatically. Every rule here decides which tab a comment lands
            in, and a comment that lands in Spam can be published anyway.
          </p>
        </div>
        <button
          type="button"
          data-comment-policy-save
          disabled={!dirty || saving}
          onClick={() =>
            onSave({
              comments_enabled: enabled,
              auto_approve_after_comments: autoApprove ?? 0,
              max_links_per_comment: linkLimit ?? 0,
              min_fill_seconds: fillSeconds ?? 0,
              per_ip_per_hour: perHour ?? 1,
              blocked_words: (words ?? "")
                .split("\n")
                .map((word) => word.trim())
                .filter((word) => word !== ""),
              notify_on_comment: notify ?? true,
            })
          }
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
        >
          {saving ? (
            <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
          ) : (
            <Check className="h-3.5 w-3.5" aria-hidden />
          )}
          Save the policy
        </button>
      </div>

      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex items-start gap-2 text-[12.5px]">
          <input
            type="checkbox"
            data-comment-policy-enabled
            checked={enabled}
            onChange={(event) => setEnabled(event.target.checked)}
            className="mt-0.5"
          />
          <span>
            Accept comments
            <span className="block text-muted">
              Off by default. Turning it off stops new comments; existing ones stay where they
              are.
            </span>
          </span>
        </label>

        <label className="flex items-start gap-2 text-[12.5px]">
          <input
            type="checkbox"
            data-comment-policy-notify
            checked={notify ?? false}
            onChange={(event) => setNotify(event.target.checked)}
            className="mt-0.5"
          />
          <span>
            Notify a moderator
            <span className="block text-muted">Sends a notification when a comment arrives.</span>
          </span>
        </label>

        <NumberField
          id="comment-policy-auto-approve"
          label="Trust an address after"
          hint="Approved comments from that address skip the queue. 0 trusts nobody."
          value={autoApprove ?? 0}
          onChange={setAutoApprove}
        />
        <NumberField
          id="comment-policy-per-hour"
          label="Comments per client per hour"
          hint="Counted against a fingerprint of the address, never the address itself."
          value={perHour ?? 5}
          min={1}
          onChange={setPerHour}
        />
        <NumberField
          id="comment-policy-max-links"
          label="Links allowed in one comment"
          hint="Two is generous for a person and below what a link farm needs."
          value={linkLimit ?? 2}
          onChange={setLinkLimit}
        />
        <NumberField
          id="comment-policy-fill-seconds"
          label="Minimum seconds on the form"
          hint="A form filled in faster than this was filled in by a script."
          value={fillSeconds ?? 3}
          onChange={setFillSeconds}
        />
      </div>

      <div className="space-y-1.5">
        <label htmlFor="comment-policy-words" className="block text-[12.5px]">
          Blocked words
          <span className="block text-muted">
            One per line. A comment containing one lands in Spam with the reason recorded.
          </span>
        </label>
        <textarea
          id="comment-policy-words"
          data-comment-policy-words
          rows={3}
          value={words ?? ""}
          onChange={(event) => setWords(event.target.value)}
          className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
        />
      </div>
    </section>
  );
}

/** One numeric policy control, with its label above it rather than in a placeholder. */
function NumberField({
  id,
  label,
  hint,
  value,
  onChange,
  min = 0,
}: {
  id: string;
  label: string;
  hint: string;
  value: number;
  onChange: (value: number) => void;
  min?: number;
}) {
  return (
    <div className="space-y-1.5">
      <label htmlFor={id} className="block text-[12.5px]">
        {label}
        <span className="block text-muted">{hint}</span>
      </label>
      <input
        id={id}
        type="number"
        min={min}
        value={value}
        onChange={(event) => onChange(Number(event.target.value))}
        className="w-28 rounded-md border border-line bg-surface px-2 py-1 text-[12.5px]"
      />
    </div>
  );
}

/** The ban form, opened from a row. */
function BanDialog({
  comment,
  busy,
  onClose,
  onPlace,
}: {
  comment: CommentInboxRow;
  busy: boolean;
  onClose: () => void;
  onPlace: (kind: "email" | "ip", value: string, reason?: string) => void;
}) {
  const [kind, setKind] = useState<"email" | "ip">("email");
  const [reason, setReason] = useState("");

  // The value is whatever the comment carries: the address for an e-mail ban, the fingerprint
  // for a network ban. A field the moderator has to retype is a field typed wrong.
  const value = kind === "email" ? comment.author_email : (comment.ip_hint ?? "");

  return (
    <div
      data-comment-ban-dialog
      className="fixed inset-0 z-30 flex items-center justify-center bg-black/30 p-4"
      role="dialog"
      aria-modal="true"
      aria-label="Ban this address"
    >
      <div className="w-full max-w-md space-y-3 rounded-md border border-line bg-panel p-5">
        <h2 className="text-[14px] font-medium">Ban from commenting</h2>
        <p className="text-[12.5px] text-muted">
          A ban refuses new comments from this address on this site. Comments already left stay
          where they are.
        </p>

        <div className="flex gap-2">
          {(["email", "ip"] as const).map((option) => (
            <button
              key={option}
              type="button"
              data-comment-ban-kind={option}
              onClick={() => setKind(option)}
              className={[
                "rounded-md border px-2.5 py-1.5 text-[12.5px]",
                kind === option ? "border-accent bg-accent/10" : "border-line",
              ].join(" ")}
            >
              {option === "email" ? "E-mail" : "Network"}
            </button>
          ))}
        </div>

        <div className="space-y-1.5">
          <span className="block text-[12.5px]">{kind === "email" ? "Address" : "Fingerprint"}</span>
          <p
            data-comment-ban-value
            className="rounded-md border border-line bg-surface px-2.5 py-1.5 font-mono text-[12px]"
          >
            {value || "this comment carries no client hint"}
          </p>
          <span className="block text-[11.5px] text-muted">
            {kind === "ip"
              ? "The network is stored as a fingerprint, so the address itself is never kept."
              : "The address is stored as typed, lower-cased, which is how a submission is compared."}
          </span>
        </div>

        <div className="space-y-1.5">
          <label htmlFor="comment-ban-reason" className="block text-[12.5px]">
            Why
            <span className="block text-muted">
              Stored with the ban, and shown to whoever is refused.
            </span>
          </label>
          <input
            id="comment-ban-reason"
            data-comment-ban-reason
            value={reason}
            onChange={(event) => setReason(event.target.value)}
            className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
        </div>

        <div className="flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            Cancel
          </button>
          <button
            type="button"
            data-comment-ban-place
            disabled={busy || value === ""}
            onClick={() => onPlace(kind, value, reason.trim() || undefined)}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            {busy ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
            ) : (
              <BanIcon className="h-3.5 w-3.5" aria-hidden />
            )}
            Ban
          </button>
        </div>
      </div>
    </div>
  );
}

/** The bans this site has placed, so a ban is not a write-only action. */
function BanList({
  bans,
  busy,
  onRemove,
}: {
  bans: { id: string; kind: string; value: string; reason: string | null; active: boolean }[];
  busy: boolean;
  onRemove: (id: string) => void;
}) {
  if (bans.length === 0) return null;

  return (
    <section aria-label="Bans" data-comment-bans="ready" className="space-y-2">
      <h2 className="text-[13.5px] font-medium">Banned addresses</h2>
      <ul className="divide-y divide-line rounded-md border border-line">
        {bans.map((ban) => (
          <li key={ban.id} data-comment-ban-row={ban.id} className="flex flex-wrap items-center gap-2 px-3 py-2 text-[12.5px]">
            <span className="rounded bg-surface-2 px-1.5 py-0.5 text-[11px] uppercase">
              {ban.kind}
            </span>
            <span className="font-mono">{ban.value}</span>
            {ban.reason ? <span className="text-muted">{ban.reason}</span> : null}
            {!ban.active ? <span className="text-muted">expired</span> : null}
            <button
              type="button"
              data-comment-ban-remove={ban.id}
              disabled={busy}
              onClick={() => onRemove(ban.id)}
              className="ml-auto inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px] disabled:opacity-50"
            >
              <Undo2 className="h-3 w-3" aria-hidden />
              Lift
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}

/** An error with a retry, because every panel on this screen can fail. */
function ErrorStrip({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div
      role="alert"
      data-comments-error
      className="flex flex-wrap items-center gap-2 rounded-md border border-danger/40 bg-danger/5 px-3 py-2 text-[12.5px] text-danger"
    >
      <AlertTriangle className="h-3.5 w-3.5 shrink-0" aria-hidden />
      <span className="flex-1">{message}</span>
      <button type="button" onClick={onRetry} className="rounded-md border border-line px-2 py-1">
        Retry
      </button>
    </div>
  );
}

/** The first line of a body, bounded. */
function excerpt(body: string): string {
  const flat = body.replace(/\s+/g, " ").trim();
  return flat.length <= 120 ? flat : `${flat.slice(0, 117)}…`;
}
