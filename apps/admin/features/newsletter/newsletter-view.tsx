"use client";

/**
 * `/newsletter` — lists, subscribers, import/export and the sent archive (REQ-064, slice 4b).
 *
 * A mailing list screen is read by somebody asking four different questions, and the order the
 * screen answers them in is the whole design. Six decisions, each one a place the obvious
 * version misleads:
 *
 * 1. **`pending` is not a failure.** A signup that has not been confirmed yet is the correct
 *    state of a double opt-in, and an owner who reads "Pending" as "broken" will turn the
 *    confirmation off. The tab says how long the link has left instead of only how many rows
 *    are waiting, because "4 pending" and "4 pending, all older than 48 hours" are different
 *    situations with the same number over them.
 * 2. **The counts and the list come from ONE read.** Four numbers beside every list, and a
 *    second request for them is a second moment — a list that says 12 over a table that now
 *    shows 13 is an owner wondering whether they lost a subscriber.
 * 3. **A pending row's address is shown; a confirmed row's address is too, but the token never
 *    is.** The panel has `newsletter.read`, so the address is legitimately in front of it. The
 *    token digests are not on the wire at all: a digest is half of a credential, and a screen
 *    that could display one would be a screen that could be photographed.
 * 4. **The import report is shown in full.** "18 added" over a 40-row file has lost 22
 *    addresses somewhere, and the report names every address it skipped and the state it found
 *    it in — including the `unsubscribed` rows it refused to revive, which is the one the owner
 *    most needs to see and the one a count would hide.
 * 5. **The delete confirmation names the count.** Deleting a list cascades to its subscribers,
 *    so "Delete Weekly News" and "delete these 412 addresses, 9 of which are confirmed" are
 *    different decisions. A confirmation that does not say which one you are making is not a
 *    confirmation.
 * 6. **Unsubscribe is a state, not a delete, and the screen says so.** The row stays; a deleted
 *    row is how the next CSV import quietly re-adds somebody who left on purpose. The button
 *    that removes a row for good is called "Remove row", so it cannot be mistaken for the
 *    unsubscribe the recipient used.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  Archive,
  Check,
  Download,
  Loader2,
  Mail,
  Plus,
  RefreshCw,
  Search,
  Send,
  Trash2,
  Upload,
  UserPlus,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  addSubscriber,
  createNewsletterList,
  deleteNewsletterList,
  deleteSubscriber,
  exportSubscribers,
  fetchNewsletterIssues,
  fetchNewsletterLists,
  fetchSubscribers,
  importSubscribers,
  patchNewsletterList,
  sendNewsletterIssue,
  setSubscriberStatus,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { useSites } from "@/lib/sites";
import type {
  ImportReport,
  NewsletterIssue,
  NewsletterList,
  NewsletterSubscriber,
  SubscriberStatus,
} from "@/lib/types";

/** The four states, in the order an owner works them. */
const TABS: { key: SubscriberStatus; label: string; empty: string }[] = [
  {
    key: "confirmed",
    label: "Subscribed",
    empty:
      "Nobody is subscribed to this filter yet. A confirmed address is one that clicked the link we sent it.",
  },
  {
    key: "pending",
    label: "Awaiting confirmation",
    empty:
      "Nothing is waiting for a click. A signup lands here first and only becomes subscribable once the recipient confirms it.",
  },
  {
    key: "unsubscribed",
    label: "Unsubscribed",
    empty: "Nobody has left this list. Addresses that unsubscribe are kept and never re-added by an import.",
  },
  {
    key: "bounced",
    label: "Bounced",
    empty: "No address has hard-bounced. A bounced address cannot receive, which is a different fact from one that opted out.",
  },
];

/** How the four states are labelled on a row. */
const STATUS_LABEL: Record<SubscriberStatus, string> = {
  pending: "Awaiting confirmation",
  confirmed: "Subscribed",
  unsubscribed: "Unsubscribed",
  bounced: "Bounced",
};

/** The tone of a state badge. `pending` is neutral on purpose — it is not a problem. */
const STATUS_TONE: Record<SubscriberStatus, string> = {
  pending: "bg-surface-2 text-muted",
  confirmed: "bg-ok/10 text-ok",
  unsubscribed: "bg-surface-2 text-muted",
  bounced: "bg-danger/10 text-danger",
};

/** The tab the screen opens on: an owner opens a newsletter screen to see who receives. */
const DEFAULT_TAB: SubscriberStatus = "confirmed";

export function NewsletterView() {
  const { selectedSite, status: siteStatus, error: siteError } = useSites();
  const siteId = selectedSite?.id ?? null;

  const [lists, setLists] = useState<NewsletterList[] | null>(null);
  const [listId, setListId] = useState<string | null>(null);
  const [tab, setTab] = useState<SubscriberStatus>(DEFAULT_TAB);
  const [search, setSearch] = useState("");
  const [appliedSearch, setAppliedSearch] = useState("");
  const [subscribers, setSubscribers] = useState<NewsletterSubscriber[] | null>(null);
  const [total, setTotal] = useState(0);
  const [issues, setIssues] = useState<NewsletterIssue[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [creating, setCreating] = useState(false);
  const [importing, setImporting] = useState<NewsletterList | null>(null);
  const [adding, setAdding] = useState<NewsletterList | null>(null);
  const [editing, setEditing] = useState<NewsletterList | null>(null);
  const [deleting, setDeleting] = useState<NewsletterList | null>(null);
  const [bouncing, setBouncing] = useState<NewsletterSubscriber | null>(null);
  const [purging, setPurging] = useState<NewsletterSubscriber | null>(null);
  const [sending, setSending] = useState(false);
  const [report, setReport] = useState<ImportReport | null>(null);
  const { checkedIds, setCheckedIds } = useCheckedIds(`${siteId}:${listId}:${tab}:${appliedSearch}`);

  // The list selection belongs to a site: a list id carried across a site switch addresses a
  // list the caller may not see, and the API answers 404 for it rather than a stranger's rows.
  useEffect(() => {
    setListId(null);
    setCreating(false);
    setEditing(null);
    setDeleting(null);
    setImporting(null);
    setAdding(null);
    setReport(null);
    setNotice(null);
  }, [siteId]);

  // Default to the first list once they arrive, because "no list selected" on a site that has
  // one is a state the owner has to be told how to leave.
  useEffect(() => {
    if (listId || !lists || lists.length === 0) return;
    setListId(lists[0].id);
  }, [lists, listId]);

  const loadLists = useCallback(async () => {
    if (!siteId) return;
    const next = await fetchNewsletterLists(siteId);
    setLists(next);
    return next;
  }, [siteId]);

  const loadSubscribers = useCallback(async () => {
    if (!siteId) return;
    setSubscribers(null);
    const page = await fetchSubscribers({
      site_id: siteId,
      list_id: listId ?? undefined,
      status: tab,
      search: appliedSearch || undefined,
      limit: 50,
    });
    setSubscribers(page.subscribers);
    setTotal(page.total);
  }, [siteId, listId, tab, appliedSearch]);

  const loadIssues = useCallback(async () => {
    if (!siteId) return;
    setIssues(await fetchNewsletterIssues({ site_id: siteId, limit: 20 }));
  }, [siteId]);

  const load = useCallback(async () => {
    if (!siteId) return;
    setError(null);
    try {
      await loadLists();
      await Promise.all([loadSubscribers(), loadIssues()]);
    } catch (caught) {
      setError((caught as ApiError).message);
      // A failed read must not leave a spinner over an empty table: an owner reads "no
      // subscribers" as a fact, and "the request failed" is a different fact.
      setSubscribers([]);
    }
  }, [siteId, loadLists, loadSubscribers, loadIssues]);

  useEffect(() => {
    void load();
  }, [load]);

  const selected = useMemo(
    () => lists?.find((entry) => entry.id === listId) ?? null,
    [lists, listId],
  );

  // The counts for the four tabs, from the SAME read as the list. A per-state count call would
  // be four requests that can each land on a different moment than the list beside them.
  const tabCounts = useMemo(() => {
    const map = new Map<SubscriberStatus, number>();
    for (const entry of TABS) map.set(entry.key, 0);
    if (!selected?.counts) return map;
    map.set("pending", selected.counts.pending);
    map.set("confirmed", selected.counts.confirmed);
    map.set("unsubscribed", selected.counts.unsubscribed);
    map.set("bounced", selected.counts.bounced);
    return map;
  }, [selected]);

  const move = useCallback(
    async (row: NewsletterSubscriber, next: SubscriberStatus, reason?: string) => {
      if (!siteId) return;
      setBusy(true);
      setError(null);
      try {
        await setSubscriberStatus(row.id, siteId, next, reason);
        setNotice(
          next === "confirmed"
            ? "Subscribed. This address will receive the next issue."
            : next === "unsubscribed"
              ? "Unsubscribed. The row is kept, so an import cannot re-add it by accident."
              : next === "bounced"
                ? "Marked as bounced. It will not be sent to again."
                : "Moved back to awaiting confirmation.",
        );
        setBouncing(null);
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [siteId, load],
  );

  const purge = useCallback(
    async (row: NewsletterSubscriber) => {
      if (!siteId) return;
      setBusy(true);
      setError(null);
      try {
        await deleteSubscriber(row.id, siteId);
        setNotice("Row removed. This is not the same as unsubscribing — nobody can be told about it.");
        setPurging(null);
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [siteId, load],
  );

  const dropList = useCallback(async () => {
    if (!siteId || !deleting) return;
    setBusy(true);
    setError(null);
    try {
      await deleteNewsletterList(deleting.id, siteId);
      setNotice(`Deleted "${deleting.name}" and every address on it.`);
      setDeleting(null);
      setListId(null);
      await load();
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  }, [siteId, deleting, load]);

  const runExport = useCallback(async () => {
    if (!siteId) return;
    setBusy(true);
    setError(null);
    try {
      const csv = await exportSubscribers({
        site_id: siteId,
        list_id: listId ?? undefined,
        status: tab,
        search: appliedSearch || undefined,
      });
      // Built in the browser rather than fetched as a file: the panel authenticates with a
      // cookie, and a plain `<a download>` to the API would arrive without it.
      const blob = new Blob([csv], { type: "text/csv;charset=utf-8" });
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = `subscribers-${STATUS_LABEL[tab].toLowerCase().replace(/\s+/g, "-")}.csv`;
      anchor.click();
      URL.revokeObjectURL(url);
      setNotice(`Exported ${total} address${total === 1 ? "" : "es"} as CSV.`);
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  }, [siteId, listId, tab, appliedSearch, total]);

  if (siteStatus === "loading" && !lists) {
    return (
      <div className="space-y-3" data-newsletter-state="loading">
        <div className="h-8 w-64 animate-pulse rounded-md bg-surface" />
        <div className="h-64 animate-pulse rounded-md bg-surface" />
      </div>
    );
  }

  if (!siteId) {
    return (
      <div data-newsletter-state="no-site">
        <EmptyState
          title="Pick a site"
          hint="A mailing list belongs to a site. Choose one from the header to open its subscribers."
        />
      </div>
    );
  }

  const rows = subscribers ?? [];
  const rowsLoading = subscribers === null;
  const allSelected = rows.length > 0 && rows.every((row) => checkedIds.has(row.id));
  const counts = selected?.counts;

  return (
    <div className="space-y-6" data-newsletter-state="ready" data-newsletter-site={siteId}>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-wrap items-center gap-2">
          <label htmlFor="newsletter-list" className="text-[12.5px]">
            List
          </label>
          <select
            id="newsletter-list"
            data-newsletter-list-select
            value={listId ?? ""}
            onChange={(event) => setListId(event.target.value || null)}
            className="rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          >
            {(lists ?? []).length === 0 ? <option value="">No lists yet</option> : null}
            {(lists ?? []).map((entry) => (
              <option key={entry.id} value={entry.id}>
                {entry.name} · {entry.counts?.confirmed ?? 0} subscribed
              </option>
            ))}
          </select>
          {selected ? (
            <span
              data-newsletter-list-key
              className="rounded bg-surface-2 px-2 py-1 font-mono text-[11.5px] text-muted"
              title="The key a theme's signup form posts to"
            >
              {selected.key}
            </span>
          ) : null}
          {selected && !selected.double_opt_in ? (
            <span className="rounded bg-surface-2 px-2 py-1 text-[11.5px] text-muted">
              Single opt-in
            </span>
          ) : null}
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            data-newsletter-new-list
            onClick={() => setCreating(true)}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <Plus className="h-3.5 w-3.5" aria-hidden />
            New list
          </button>
          <button
            type="button"
            data-newsletter-refresh
            onClick={() => void load()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <RefreshCw className="h-3.5 w-3.5" aria-hidden />
            Refresh
          </button>
        </div>
      </div>

      {siteError ? <ErrorStrip message={siteError} onRetry={() => void load()} /> : null}
      {error ? <ErrorStrip message={error} onRetry={() => void load()} /> : null}
      {notice ? (
        <div
          role="status"
          data-newsletter-notice
          className="flex items-start gap-2 rounded-md border border-line bg-surface px-3 py-2 text-[12.5px]"
        >
          <Check className="mt-0.5 h-3.5 w-3.5 shrink-0 text-ok" aria-hidden />
          <span className="flex-1">{notice}</span>
          <button type="button" onClick={() => setNotice(null)} aria-label="Dismiss">
            <X className="h-3.5 w-3.5" aria-hidden />
          </button>
        </div>
      ) : null}

      {lists && lists.length === 0 ? (
        <div data-newsletter-lists="empty" className="rounded-md border border-line">
          <EmptyState
            title="No list on this site yet"
            hint="A list is one signup form and the addresses it collects. Create one, then point a theme's signup form at its key."
            action={
              <button
                type="button"
                onClick={() => setCreating(true)}
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
              >
                <Plus className="h-3.5 w-3.5" aria-hidden />
                New list
              </button>
            }
          />
        </div>
      ) : null}

      {lists && lists.length > 0 ? (
        <section aria-label="Subscribers" className="space-y-3">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex flex-wrap gap-1" role="tablist" aria-label="Subscriber states">
              {TABS.map((entry) => {
                const count = tabCounts.get(entry.key) ?? 0;
                const active = entry.key === tab;
                return (
                  <button
                    key={entry.key}
                    type="button"
                    role="tab"
                    aria-selected={active}
                    data-newsletter-tab={entry.key}
                    onClick={() => {
                      setTab(entry.key);
                      setError(null);
                      setNotice(null);
                    }}
                    className={[
                      "inline-flex items-center gap-1.5 rounded-md border px-2.5 py-1.5 text-[12.5px]",
                      active ? "border-accent bg-accent/10 text-ink" : "border-line text-muted hover:text-ink",
                    ].join(" ")}
                  >
                    {entry.label}
                    <span
                      data-newsletter-tab-count={entry.key}
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
                data-newsletter-search-form
                onSubmit={(event) => {
                  event.preventDefault();
                  setAppliedSearch(search.trim());
                }}
                className="flex items-center gap-1.5"
              >
                <label htmlFor="newsletter-search" className="sr-only">
                  Search subscribers
                </label>
                <input
                  id="newsletter-search"
                  data-newsletter-search
                  value={search}
                  onChange={(event) => setSearch(event.target.value)}
                  placeholder="Address, name or source"
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
                data-newsletter-export
                disabled={busy}
                onClick={() => void runExport()}
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
              >
                <Download className="h-3.5 w-3.5" aria-hidden />
                Export CSV
              </button>
              {selected ? (
                <>
                  <button
                    type="button"
                    data-newsletter-add-subscriber
                    onClick={() => setAdding(selected)}
                    className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
                  >
                    <UserPlus className="h-3.5 w-3.5" aria-hidden />
                    Add address
                  </button>
                  <button
                    type="button"
                    data-newsletter-import
                    onClick={() => setImporting(selected)}
                    className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
                  >
                    <Upload className="h-3.5 w-3.5" aria-hidden />
                    Import CSV
                  </button>
                  <button
                    type="button"
                    data-newsletter-send-issue
                    onClick={() => setSending(true)}
                    className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
                  >
                    <Send className="h-3.5 w-3.5" aria-hidden />
                    Write an issue
                  </button>
                  <button
                    type="button"
                    data-newsletter-edit-list
                    onClick={() => setEditing(selected)}
                    className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
                  >
                    Edit list
                  </button>
                  <button
                    type="button"
                    data-newsletter-delete-list
                    onClick={() => setDeleting(selected)}
                    className="inline-flex items-center gap-1.5 rounded-md border border-danger/40 px-2.5 py-1.5 text-[12.5px] text-danger"
                  >
                    <Trash2 className="h-3.5 w-3.5" aria-hidden />
                    Delete list
                  </button>
                </>
              ) : null}
            </div>
          </div>

          {rowsLoading ? (
            <div className="rounded-md border border-line" data-newsletter-rows="loading">
              <div className="space-y-2 p-4">
                {Array.from({ length: 3 }, (_, index) => (
                  <div key={index} className="h-8 animate-pulse rounded bg-surface" />
                ))}
              </div>
            </div>
          ) : rows.length === 0 ? (
            <div className="rounded-md border border-line" data-newsletter-rows="empty">
              <EmptyState title={TABS.find((entry) => entry.key === tab)?.empty ?? "Nothing here."} />
            </div>
          ) : (
            <div className="overflow-x-auto rounded-md border border-line">
              <table className="w-full min-w-[720px] border-collapse text-left text-[12.5px]">
                <thead className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                  <tr>
                    <th scope="col" className="w-8 px-3 py-2">
                      <label htmlFor="newsletter-select-all" className="sr-only">
                        Select every row on this page
                      </label>
                      <input
                        id="newsletter-select-all"
                        data-newsletter-select-all
                        type="checkbox"
                        checked={allSelected}
                        onChange={(event) =>
                          setCheckedIds(
                            event.target.checked ? rows.map((row) => row.id) : [],
                          )
                        }
                      />
                    </th>
                    <th scope="col" className="px-3 py-2">
                      Address
                    </th>
                    <th scope="col" className="px-3 py-2">
                      State
                    </th>
                    <th scope="col" className="px-3 py-2">
                      Since
                    </th>
                    <th scope="col" className="px-3 py-2">
                      Actions
                    </th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-line">
                  {rows.map((row) => (
                    <tr key={row.id} data-newsletter-row={row.id} data-newsletter-row-status={row.status}>
                      <td className="px-3 py-2">
                        <label htmlFor={`newsletter-pick-${row.id}`} className="sr-only">
                          Select {row.email}
                        </label>
                        <input
                          id={`newsletter-pick-${row.id}`}
                          data-newsletter-pick={row.id}
                          type="checkbox"
                          checked={checkedIds.has(row.id)}
                          onChange={(event) => {
                            const next = new Set(checkedIds);
                            if (event.target.checked) next.add(row.id);
                            else next.delete(row.id);
                            setCheckedIds(Array.from(next));
                          }}
                        />
                      </td>
                      <td className="px-3 py-2">
                        <span className="font-mono">{row.email}</span>
                        {row.name ? <span className="ml-2 text-muted">{row.name}</span> : null}
                        {row.source ? (
                          <span className="ml-2 text-[11px] text-muted">via {row.source}</span>
                        ) : null}
                      </td>
                      <td className="px-3 py-2">
                        <span
                          data-newsletter-row-badge={row.status}
                          className={`rounded px-1.5 py-0.5 text-[11px] ${STATUS_TONE[row.status]}`}
                        >
                          {STATUS_LABEL[row.status]}
                        </span>
                        {/* WHY, not just what: a subscriber table with no reason column cannot
                            answer "why is this address not receiving the issue". */}
                        {row.status_reason ? (
                          <span className="ml-2 text-[11px] text-muted">{row.status_reason}</span>
                        ) : null}
                        {row.status === "pending" && row.confirm_expires_at ? (
                          <span
                            data-newsletter-row-expires={row.id}
                            className="ml-2 text-[11px] text-muted"
                          >
                            link expires {formatTimestamp(row.confirm_expires_at)}
                          </span>
                        ) : null}
                      </td>
                      <td className="px-3 py-2 text-muted">{formatTimestamp(row.created_at)}</td>
                      <td className="px-3 py-2">
                        <div className="flex flex-wrap gap-1.5">
                          {row.status !== "confirmed" ? (
                            <RowAction
                              label="Confirm"
                              icon={<Check className="h-3 w-3" aria-hidden />}
                              disabled={busy}
                              testId={`newsletter-confirm-${row.id}`}
                              onClick={() => void move(row, "confirmed")}
                            />
                          ) : null}
                          {row.status !== "unsubscribed" ? (
                            <RowAction
                              label="Unsubscribe"
                              icon={<X className="h-3 w-3" aria-hidden />}
                              disabled={busy}
                              testId={`newsletter-unsubscribe-${row.id}`}
                              onClick={() => void move(row, "unsubscribed")}
                            />
                          ) : null}
                          {row.status !== "bounced" ? (
                            <RowAction
                              label="Bounce"
                              icon={<AlertTriangle className="h-3 w-3" aria-hidden />}
                              disabled={busy}
                              testId={`newsletter-bounce-${row.id}`}
                              onClick={() => setBouncing(row)}
                            />
                          ) : null}
                          <RowAction
                            label="Remove row"
                            icon={<Trash2 className="h-3 w-3" aria-hidden />}
                            disabled={busy}
                            testId={`newsletter-purge-${row.id}`}
                            onClick={() => setPurging(row)}
                          />
                        </div>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}

          <p className="text-[11.5px] text-muted" data-newsletter-total>
            {total} address{total === 1 ? "" : "es"} in this state
            {counts ? ` · ${counts.confirmed} will receive the next issue` : null}
          </p>
        </section>
      ) : null}

      <IssueArchive
        issues={issues}
        sending={sending}
        busy={busy}
        onSend={() => setSending(true)}
        onClose={() => setSending(false)}
        onSent={async (message) => {
          setSending(false);
          setNotice(message);
          await load();
        }}
        onError={setError}
      />

      {creating ? (
        <ListDialog
          mode="create"
          busy={busy}
          onClose={() => setCreating(false)}
          onSubmit={async (values) => {
            setBusy(true);
            setError(null);
            try {
              const created = await createNewsletterList({ site_id: siteId, ...values });
              setCreating(false);
              setListId(created.id);
              setNotice(
                `Created "${created.name}". Its signup form posts to key "${created.key}".`,
              );
              await load();
            } catch (caught) {
              setError((caught as ApiError).message);
            } finally {
              setBusy(false);
            }
          }}
        />
      ) : null}

      {editing ? (
        <ListDialog
          mode="edit"
          list={editing}
          busy={busy}
          onClose={() => setEditing(null)}
          onSubmit={async (values) => {
            setBusy(true);
            setError(null);
            try {
              await patchNewsletterList(editing.id, siteId, values);
              setEditing(null);
              setNotice(
                values.double_opt_in === false
                  ? "List saved. Signups are now single opt-in: an address is subscribed the moment it submits the form."
                  : "List saved.",
              );
              await load();
            } catch (caught) {
              setError((caught as ApiError).message);
            } finally {
              setBusy(false);
            }
          }}
        />
      ) : null}

      {deleting ? (
        <ConfirmDialog
          testId="newsletter-delete-dialog"
          title={`Delete "${deleting.name}"?`}
          body={
            deleting.counts && deleting.counts.total > 0 ? (
              <>
                This also deletes its {deleting.counts.total} address
                {deleting.counts.total === 1 ? "" : "es"} — {deleting.counts.confirmed} of them
                subscribed. The rows cannot be recovered; an address that unsubscribed on its own
                is deleted with them.
              </>
            ) : (
              "This list has no addresses yet, so nothing else goes with it."
            )
          }
          confirmLabel="Delete the list"
          danger
          busy={busy}
          onClose={() => setDeleting(null)}
          onConfirm={() => void dropList()}
        />
      ) : null}

      {adding ? (
        <AddSubscriberDialog
          list={adding}
          busy={busy}
          onClose={() => setAdding(null)}
          onSubmit={async (values) => {
            setBusy(true);
            setError(null);
            try {
              const created = await addSubscriber(adding.id, siteId, values);
              setAdding(null);
              setNotice(
                created.status === "pending"
                  ? "Added, awaiting their confirmation. They will not receive anything until they click the link."
                  : "Subscribed. This list is single opt-in, so the address is live now.",
              );
              await load();
            } catch (caught) {
              setError((caught as ApiError).message);
            } finally {
              setBusy(false);
            }
          }}
        />
      ) : null}

      {importing ? (
        <ImportDialog
          list={importing}
          busy={busy}
          report={report}
          onClose={() => {
            setImporting(null);
            setReport(null);
          }}
          onSubmit={async (csv) => {
            setBusy(true);
            setError(null);
            try {
              const next = await importSubscribers(importing.id, siteId, csv, "csv import");
              setReport(next);
              await load();
            } catch (caught) {
              setError((caught as ApiError).message);
            } finally {
              setBusy(false);
            }
          }}
        />
      ) : null}

      {bouncing ? (
        <ReasonDialog
          testId="newsletter-bounce-dialog"
          title="Mark as bounced"
          body={`This address hard-bounced, so it cannot receive. It is kept, so it is not re-added by a later import, and it will not be sent to again.`}
          confirmLabel="Mark as bounced"
          busy={busy}
          onClose={() => setBouncing(null)}
          onConfirm={(reason) => void move(bouncing, "bounced", reason)}
        />
      ) : null}

      {purging ? (
        <ConfirmDialog
          testId="newsletter-purge-dialog"
          title={`Remove ${purging.email}?`}
          body="This deletes the row itself, which is not what unsubscribing does. Nothing tells this address anything, and a later import can add it back."
          confirmLabel="Remove the row"
          danger
          busy={busy}
          onClose={() => setPurging(null)}
          onConfirm={() => void purge(purging)}
        />
      ) : null}
    </div>
  );
}

/**
 * Selection state, as a stable `Set`.
 *
 * The `Set` is memoised rather than rebuilt on every render: it is passed to row components as a
 * value, and a fresh `Set` each render is a new object identity each render — which is how a
 * `useMemo` above it silently stops memoising. Selection also clears whenever the filter it was
 * made under changes, because a bulk action against rows that are no longer visible is a bulk
 * action on somebody else's addresses.
 */
function useCheckedIds(resetKey: string) {
  const [ids, setIds] = useState<string[]>([]);
  const checkedIds = useMemo(() => new Set(ids), [ids]);

  useEffect(() => {
    setIds([]);
  }, [resetKey]);

  return { checkedIds, setCheckedIds: setIds };
}

/** One row action button. */
function RowAction({
  label,
  icon,
  disabled,
  testId,
  onClick,
}: {
  label: string;
  icon: React.ReactNode;
  disabled: boolean;
  testId: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      data-testid={testId}
      disabled={disabled}
      onClick={onClick}
      className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px] disabled:opacity-50"
    >
      {icon}
      {label}
    </button>
  );
}

/** Create / edit a list. The key is optional on create and immutable afterwards. */
function ListDialog({
  mode,
  list,
  busy,
  onClose,
  onSubmit,
}: {
  mode: "create" | "edit";
  list?: NewsletterList;
  busy: boolean;
  onClose: () => void;
  onSubmit: (values: {
    name: string;
    description?: string;
    double_opt_in?: boolean;
  }) => void | Promise<void>;
}) {
  const [name, setName] = useState(list?.name ?? "");
  const [description, setDescription] = useState(list?.description ?? "");
  const [doubleOptIn, setDoubleOptIn] = useState(list?.double_opt_in ?? true);
  const [touched, setTouched] = useState(false);

  const nameError = touched && name.trim() === "" ? "Give the list a name." : null;

  return (
    <div
      data-newsletter-list-dialog={mode}
      className="fixed inset-0 z-30 flex items-center justify-center bg-black/30 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={mode === "create" ? "Create a list" : "Edit the list"}
    >
      <form
        className="w-full max-w-md space-y-3 rounded-md border border-line bg-panel p-5"
        onSubmit={(event) => {
          event.preventDefault();
          setTouched(true);
          if (name.trim() === "") return;
          void onSubmit({
            name: name.trim(),
            description: description.trim() || undefined,
            double_opt_in: doubleOptIn,
          });
        }}
      >
        <h2 className="text-[14px] font-medium">
          {mode === "create" ? "New newsletter list" : "Edit list"}
        </h2>

        <div className="space-y-1.5">
          <label htmlFor="newsletter-name" className="block text-[12.5px]">
            Name
            <span className="block text-muted">What the signup form and the archive call it.</span>
          </label>
          <input
            id="newsletter-name"
            data-newsletter-name
            value={name}
            onChange={(event) => setName(event.target.value)}
            aria-invalid={nameError ? true : undefined}
            aria-describedby={nameError ? "newsletter-name-error" : undefined}
            className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
          {nameError ? (
            <p id="newsletter-name-error" data-newsletter-name-error className="text-[11.5px] text-danger">
              {nameError}
            </p>
          ) : null}
        </div>

        <div className="space-y-1.5">
          <label htmlFor="newsletter-description" className="block text-[12.5px]">
            Description
            <span className="block text-muted">
              Shown on the signup form, so a visitor knows what they are agreeing to.
            </span>
          </label>
          <textarea
            id="newsletter-description"
            data-newsletter-description
            value={description}
            onChange={(event) => setDescription(event.target.value)}
            rows={2}
            className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
        </div>

        <div className="flex items-start gap-2">
          <input
            id="newsletter-double-opt-in"
            data-newsletter-double-opt-in
            type="checkbox"
            checked={doubleOptIn}
            onChange={(event) => setDoubleOptIn(event.target.checked)}
            className="mt-1"
          />
          <label htmlFor="newsletter-double-opt-in" className="text-[12.5px]">
            Require confirmation
            <span className="block text-muted">
              The address is only subscribed after somebody clicks the link we send it. Turning
              this off subscribes every form submission immediately.
            </span>
          </label>
        </div>

        {mode === "create" ? (
          <p className="text-[11.5px] text-muted">
            The signup key is derived from the name and shown after creation. Changing the name
            later does not change the key, because published themes already point at it.
          </p>
        ) : null}

        <div className="flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            Cancel
          </button>
          <button
            type="submit"
            data-newsletter-list-save
            disabled={busy}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <Check className="h-3.5 w-3.5" aria-hidden />}
            {mode === "create" ? "Create list" : "Save"}
          </button>
        </div>
      </form>
    </div>
  );
}

/** Add one address by hand. */
function AddSubscriberDialog({
  list,
  busy,
  onClose,
  onSubmit,
}: {
  list: NewsletterList;
  busy: boolean;
  onClose: () => void;
  onSubmit: (values: { email: string; name?: string }) => void | Promise<void>;
}) {
  const [email, setEmail] = useState("");
  const [name, setName] = useState("");
  const [touched, setTouched] = useState(false);

  // Deliberately the simplest shape that is still an error: a server-side validator that says
  // "that is not an e-mail address" is the one that counts, and a client rule that disagrees
  // with it is a second place to be wrong.
  const emailError = touched && !/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email.trim())
    ? "Enter an e-mail address."
    : null;

  return (
    <div
      data-newsletter-add-dialog
      className="fixed inset-0 z-30 flex items-center justify-center bg-black/30 p-4"
      role="dialog"
      aria-modal="true"
      aria-label="Add an address"
    >
      <form
        className="w-full max-w-md space-y-3 rounded-md border border-line bg-panel p-5"
        onSubmit={(event) => {
          event.preventDefault();
          setTouched(true);
          if (emailError) return;
          void onSubmit({ email: email.trim(), name: name.trim() || undefined });
        }}
      >
        <h2 className="text-[14px] font-medium">Add an address to {list.name}</h2>
        <p className="text-[12.5px] text-muted">
          {list.double_opt_in
            ? "This list requires confirmation, so the address stays unsubscribed until whoever owns it clicks the link."
            : "This list is single opt-in, so the address is subscribed as soon as you save."}
        </p>

        <div className="space-y-1.5">
          <label htmlFor="newsletter-add-email" className="block text-[12.5px]">
            Address
          </label>
          <input
            id="newsletter-add-email"
            data-newsletter-add-email
            type="email"
            value={email}
            onChange={(event) => setEmail(event.target.value)}
            aria-invalid={emailError ? true : undefined}
            aria-describedby={emailError ? "newsletter-add-email-error" : undefined}
            className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
          {emailError ? (
            <p id="newsletter-add-email-error" data-newsletter-add-email-error className="text-[11.5px] text-danger">
              {emailError}
            </p>
          ) : null}
        </div>

        <div className="space-y-1.5">
          <label htmlFor="newsletter-add-name" className="block text-[12.5px]">
            Name
            <span className="block text-muted">Optional.</span>
          </label>
          <input
            id="newsletter-add-name"
            data-newsletter-add-name
            value={name}
            onChange={(event) => setName(event.target.value)}
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
            type="submit"
            data-newsletter-add-save
            disabled={busy}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <UserPlus className="h-3.5 w-3.5" aria-hidden />}
            Add
          </button>
        </div>
      </form>
    </div>
  );
}

/**
 * Import a CSV.
 *
 * The report is the screen's real output. "18 added" over a 40-row file has lost 22 addresses
 * somewhere, and the addresses it skipped are named — including the `unsubscribed` ones it
 * refused to revive, which is the one an owner most needs to see.
 */
function ImportDialog({
  list,
  busy,
  report,
  onClose,
  onSubmit,
}: {
  list: NewsletterList;
  busy: boolean;
  report: ImportReport | null;
  onClose: () => void;
  onSubmit: (csv: string) => void | Promise<void>;
}) {
  const [csv, setCsv] = useState("");

  return (
    <div
      data-newsletter-import-dialog
      className="fixed inset-0 z-30 flex items-center justify-center bg-black/30 p-4"
      role="dialog"
      aria-modal="true"
      aria-label="Import addresses"
    >
      <div className="w-full max-w-lg space-y-3 rounded-md border border-line bg-panel p-5">
        <h2 className="text-[14px] font-medium">Import into {list.name}</h2>

        {report ? (
          <div data-newsletter-import-report className="space-y-2">
            <p className="text-[12.5px]">
              <strong className="tabular-nums">{report.added}</strong> added
              {report.blank > 0 ? (
                <>
                  {" · "}
                  <span className="tabular-nums">{report.blank}</span> rows held no usable address
                </>
              ) : null}
              {report.skipped.length > 0 ? (
                <>
                  {" · "}
                  <span className="tabular-nums">{report.skipped.length}</span> already on the list
                </>
              ) : null}
            </p>
            {report.skipped.length > 0 ? (
              <>
                <p className="text-[11.5px] text-muted">
                  Skipped, and why. An address that unsubscribed on its own is never revived by an
                  import — that is the difference between a list and a spreadsheet.
                </p>
                <ul className="max-h-40 divide-y divide-line overflow-y-auto rounded-md border border-line">
                  {report.skipped.map((entry) => (
                    <li
                      key={`${entry.email}-${entry.status}`}
                      data-newsletter-import-skip={entry.email}
                      className="flex items-center gap-2 px-3 py-1.5 text-[11.5px]"
                    >
                      <span className="font-mono">{entry.email}</span>
                      <span className="ml-auto text-muted">{entry.status}</span>
                    </li>
                  ))}
                </ul>
              </>
            ) : null}
            <div className="flex justify-end">
              <button
                type="button"
                onClick={onClose}
                data-newsletter-import-close
                className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
              >
                Close
              </button>
            </div>
          </div>
        ) : (
          <>
            <p className="text-[12.5px] text-muted">
              One address per line, or a CSV with an address in the first column. A header row is
              ignored. Imported addresses are always{" "}
              <strong>awaiting confirmation</strong>, even on a single opt-in list — the platform
              cannot confirm an address it did not hear from.
            </p>
            <div className="space-y-1.5">
              <label htmlFor="newsletter-import-csv" className="block text-[12.5px]">
                Addresses
              </label>
              <textarea
                id="newsletter-import-csv"
                data-newsletter-import-csv
                value={csv}
                onChange={(event) => setCsv(event.target.value)}
                rows={7}
                spellCheck={false}
                className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 font-mono text-[12px]"
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
                data-newsletter-import-run
                disabled={busy || csv.trim() === ""}
                onClick={() => void onSubmit(csv)}
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
              >
                {busy ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                ) : (
                  <Upload className="h-3.5 w-3.5" aria-hidden />
                )}
                Import
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

/** The sent-issue archive, and the form that writes the next one. */
function IssueArchive({
  issues,
  sending,
  busy,
  onSend,
  onClose,
  onSent,
  onError,
}: {
  issues: NewsletterIssue[];
  sending: boolean;
  busy: boolean;
  onSend: () => void;
  onClose: () => void;
  onSent: (message: string) => void | Promise<void>;
  onError: (message: string | null) => void;
}) {
  return (
    <section aria-label="Sent issues" className="space-y-2">
      <div className="flex items-center justify-between">
        <h2 className="text-[13.5px] font-medium">Sent issues</h2>
        <button
          type="button"
          data-newsletter-archive-new
          onClick={onSend}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
        >
          <Mail className="h-3.5 w-3.5" aria-hidden />
          Write an issue
        </button>
      </div>

      {issues.length === 0 ? (
        <div className="rounded-md border border-line" data-newsletter-archive="empty">
          <EmptyState
            title="No issue has been sent yet"
            hint="An archived issue keeps its own public permalink, so readers who missed it can still read it."
          />
        </div>
      ) : (
        <ul className="divide-y divide-line rounded-md border border-line" data-newsletter-archive="ready">
          {issues.map((issue) => (
            <li
              key={issue.id}
              data-newsletter-issue={issue.id}
              className="flex flex-wrap items-center gap-2 px-3 py-2 text-[12.5px]"
            >
              <Archive className="h-3.5 w-3.5 shrink-0 text-muted" aria-hidden />
              <span className="font-medium">{issue.subject}</span>
              <span className="text-muted">
                {issue.recipient_count} recipient{issue.recipient_count === 1 ? "" : "s"}
              </span>
              <span className="ml-auto text-[11.5px] text-muted">
                {formatTimestamp(issue.sent_at)}
              </span>
              <span className="rounded bg-surface-2 px-1.5 py-0.5 font-mono text-[11px] text-muted">
                /{issue.archive_slug}
              </span>
            </li>
          ))}
        </ul>
      )}

      {sending ? (
        <SendIssueDialog
          busy={busy}
          onClose={onClose}
          onSubmit={async (values) => {
            try {
              const sent = await sendNewsletterIssue(values);
              await onSent(
                `Archived "${values.subject}" for ${sent.recipient_count} recipient${sent.recipient_count === 1 ? "" : "s"}.`,
              );
            } catch (caught) {
              onError((caught as ApiError).message);
              onClose();
            }
          }}
        />
      ) : null}
    </section>
  );
}

/** Compose and archive an issue. */
function SendIssueDialog({
  busy,
  onClose,
  onSubmit,
}: {
  busy: boolean;
  onClose: () => void;
  onSubmit: (values: {
    site_id: string;
    list_id: string;
    subject: string;
    body_html: string;
    archive_slug?: string;
  }) => void | Promise<void>;
}) {
  const [siteId, setSiteId] = useState("");
  const [listId, setListId] = useState("");
  const [lists, setLists] = useState<NewsletterList[]>([]);
  const [subject, setSubject] = useState("");
  const [body, setBody] = useState("");
  const [touched, setTouched] = useState(false);
  const { selectedSite } = useSites();

  useEffect(() => {
    if (selectedSite?.id) setSiteId(selectedSite.id);
  }, [selectedSite?.id]);

  useEffect(() => {
    if (!siteId) return;
    void fetchNewsletterLists(siteId).then(setLists).catch(() => setLists([]));
  }, [siteId]);

  const subjectError = touched && subject.trim() === "" ? "Give the issue a subject." : null;
  const bodyError = touched && body.trim() === "" ? "The issue has no body." : null;

  return (
    <div
      data-newsletter-send-dialog
      className="fixed inset-0 z-30 flex items-center justify-center bg-black/30 p-4"
      role="dialog"
      aria-modal="true"
      aria-label="Write an issue"
    >
      <form
        className="w-full max-w-lg space-y-3 rounded-md border border-line bg-panel p-5"
        onSubmit={(event) => {
          event.preventDefault();
          setTouched(true);
          if (subject.trim() === "" || body.trim() === "" || !listId) return;
          void onSubmit({
            site_id: siteId,
            list_id: listId,
            subject: subject.trim(),
            body_html: body,
          });
        }}
      >
        <h2 className="text-[14px] font-medium">Write an issue</h2>
        <p className="text-[12.5px] text-muted">
          The platform has no mail transport configured on this installation, so this archives the
          issue and records how many addresses it would have reached. The public archive page is
          what readers get.
        </p>

        <div className="space-y-1.5">
          <label htmlFor="newsletter-issue-list" className="block text-[12.5px]">
            List
          </label>
          <select
            id="newsletter-issue-list"
            data-newsletter-issue-list
            value={listId}
            onChange={(event) => setListId(event.target.value)}
            className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          >
            <option value="">Choose a list</option>
            {lists.map((entry) => (
              <option key={entry.id} value={entry.id}>
                {entry.name} · {entry.counts?.confirmed ?? 0} subscribed
              </option>
            ))}
          </select>
        </div>

        <div className="space-y-1.5">
          <label htmlFor="newsletter-issue-subject" className="block text-[12.5px]">
            Subject
          </label>
          <input
            id="newsletter-issue-subject"
            data-newsletter-issue-subject
            value={subject}
            onChange={(event) => setSubject(event.target.value)}
            aria-invalid={subjectError ? true : undefined}
            className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
          {subjectError ? (
            <p data-newsletter-issue-subject-error className="text-[11.5px] text-danger">
              {subjectError}
            </p>
          ) : null}
        </div>

        <div className="space-y-1.5">
          <label htmlFor="newsletter-issue-body" className="block text-[12.5px]">
            Body
            <span className="block text-muted">
              HTML is allowed and sanitised on save, because the archive page is a platform
              surface and renders it.
            </span>
          </label>
          <textarea
            id="newsletter-issue-body"
            data-newsletter-issue-body
            value={body}
            onChange={(event) => setBody(event.target.value)}
            rows={6}
            className="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
          {bodyError ? (
            <p data-newsletter-issue-body-error className="text-[11.5px] text-danger">
              {bodyError}
            </p>
          ) : null}
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
            type="submit"
            data-newsletter-issue-send
            disabled={busy}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            {busy ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
            ) : (
              <Send className="h-3.5 w-3.5" aria-hidden />
            )}
            Archive the issue
          </button>
        </div>
      </form>
    </div>
  );
}

/** A confirmation that names what is about to happen. */
function ConfirmDialog({
  testId,
  title,
  body,
  confirmLabel,
  danger,
  busy,
  onClose,
  onConfirm,
}: {
  testId: string;
  title: string;
  body: React.ReactNode;
  confirmLabel: string;
  danger?: boolean;
  busy: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  return (
    <div
      data-testid={testId}
      className="fixed inset-0 z-30 flex items-center justify-center bg-black/30 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={title}
    >
      <div className="w-full max-w-md space-y-3 rounded-md border border-line bg-panel p-5">
        <h2 className="text-[14px] font-medium">{title}</h2>
        <p className="text-[12.5px] text-muted">{body}</p>
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
            data-testid={`${testId}-confirm`}
            disabled={busy}
            onClick={onConfirm}
            className={[
              "inline-flex items-center gap-1.5 rounded-md border px-2.5 py-1.5 text-[12.5px] disabled:opacity-50",
              danger ? "border-danger/40 text-danger" : "border-line",
            ].join(" ")}
          >
            {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}

/** A confirmation that asks for a stored reason. */
function ReasonDialog({
  testId,
  title,
  body,
  confirmLabel,
  busy,
  onClose,
  onConfirm,
}: {
  testId: string;
  title: string;
  body: string;
  confirmLabel: string;
  busy: boolean;
  onClose: () => void;
  onConfirm: (reason: string) => void;
}) {
  const [reason, setReason] = useState("");

  return (
    <div
      data-testid={testId}
      className="fixed inset-0 z-30 flex items-center justify-center bg-black/30 p-4"
      role="dialog"
      aria-modal="true"
      aria-label={title}
    >
      <div className="w-full max-w-md space-y-3 rounded-md border border-line bg-panel p-5">
        <h2 className="text-[14px] font-medium">{title}</h2>
        <p className="text-[12.5px] text-muted">{body}</p>
        <div className="space-y-1.5">
          <label htmlFor={`${testId}-reason`} className="block text-[12.5px]">
            Reason
            <span className="block text-muted">
              Stored on the row and shown beside it. A subscriber table with no reason cannot
              answer "why is this address not receiving".
            </span>
          </label>
          <input
            id={`${testId}-reason`}
            data-testid={`${testId}-reason`}
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
            data-testid={`${testId}-confirm`}
            disabled={busy}
            onClick={() => onConfirm(reason.trim())}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}

/** An error with a retry, because every panel on this screen can fail. */
function ErrorStrip({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div
      role="alert"
      data-newsletter-error
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
