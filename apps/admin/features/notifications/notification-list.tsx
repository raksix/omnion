"use client";

/**
 * The notification list: filters, bulk actions, keyset pagination and a detail drawer.
 *
 * Four claims this screen makes, each one a way a list lies to the person reading it:
 *
 * 1. **A bulk action reports what it changed, not what was selected.** Five rows selected, two
 *    already archived, and the honest message is "2 archived" — not "5 archived" (a lie) and
 *    not an error (the other three are a legitimate no-op). The number comes from the API, so
 *    the panel is not guessing on the server's behalf.
 * 2. **The filter lives in the query string.** A filtered view that cannot be pasted to a
 *    colleague is a view that has to be rebuilt by hand, and the rebuild is where the
 *    mistakes are.
 * 3. **The empty state offers a way out.** "You're all caught up" with a *Show read
 *    notifications* button, because the reader's next question is almost always "where did
 *    they go?" and a dead end cannot answer it.
 * 4. **The keyboard path is real.** `j`/`k` move a cursor, `Enter` opens, `e` toggles read,
 *    `x` selects, `/` focuses the filter, `Esc` closes the drawer. Every one of them is
 *    asserted in the walkthrough, because a documented shortcut that does not work is worse
 *    than no shortcut.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import {
  Archive,
  BellOff,
  Check,
  Inbox,
  RefreshCw,
  Search,
  Settings2,
  Square,
  SquareCheckBig,
  Trash2,
  TriangleAlert,
} from "lucide-react";

import {
  bulkNotifications,
  deleteNotification,
  fetchNotification,
  fetchNotifications,
  setNotificationRead,
  type ApiError,
} from "@/lib/api";
import {
  NOTIFICATION_CATEGORIES,
  NOTIFICATION_PRIORITIES,
  type NotificationFilters,
  type NotificationRow,
} from "@/lib/types";

const PAGE = 25;

const CATEGORY_LINE: Record<string, string> = {
  approval: "Approval",
  security: "Security",
  update: "Update",
  ticket: "Ticket",
  system: "System",
  mention: "Mention",
};

const PRIORITY_TONE: Record<string, string> = {
  critical: "bg-red-500/10 text-red-700 dark:text-red-300",
  high: "bg-amber-500/10 text-amber-700 dark:text-amber-300",
  normal: "bg-quiet-soft text-muted",
  low: "bg-quiet-soft text-muted",
};

/** Read the filters out of the query string — the same shape the API takes. */
function filtersFrom(params: URLSearchParams): NotificationFilters {
  const filters: NotificationFilters = {};
  const category = params.get("category");
  if (category) filters.category = category;
  const read = params.get("read");
  if (read === "unread" || read === "read") filters.read = read;
  const priority = params.get("priority");
  if (priority) filters.priority = priority;
  if (params.get("archived") === "1") filters.archived = true;
  if (params.get("with_read") === "1") filters.with_read = true;
  return filters;
}

export function NotificationList() {
  const router = useRouter();
  const params = useSearchParams();
  const filters = useMemo(() => filtersFrom(new URLSearchParams(params?.toString() ?? "")), [params]);

  const [rows, setRows] = useState<NotificationRow[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [drawer, setDrawer] = useState<NotificationRow | null>(null);
  const [cursorIndex, setCursorIndex] = useState(-1);
  const filterInput = useRef<HTMLInputElement>(null);
  const tableRef = useRef<HTMLTableElement>(null);

  const load = useCallback(
    async (append = false) => {
      setLoading(true);
      setError(null);
      try {
        const page = await fetchNotifications({
          ...filters,
          limit: PAGE,
          before: append ? (cursor ?? undefined) : undefined,
        } as NotificationFilters);
        setRows((previous) => (append ? [...previous, ...page.notifications] : page.notifications));
        setCursor(page.next_before);
        setHasMore(page.has_more);
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setLoading(false);
      }
    },
    [filters, cursor],
  );

  useEffect(() => {
    void load(false);
    setSelected(new Set());
    setCursorIndex(-1);
  }, [load]);

  // A filter change rewrites the query string, so the URL is always the state and the back
  // button walks the filters rather than the rows.
  const setFilter = (key: keyof NotificationFilters, value: string | boolean | undefined) => {
    const next = new URLSearchParams(params?.toString() ?? "");
    if (value === undefined || value === false || value === "") {
      next.delete(key);
    } else if (value === true) {
      next.set(key, "1");
    } else {
      next.set(key, String(value));
    }
    const query = next.toString();
    router.replace(query ? `/notifications?${query}` : "/notifications");
  };

  const reset = () => router.replace("/notifications");

  const runBulk = async (action: "read" | "unread" | "archive" | "delete") => {
    if (selected.size === 0) return;
    await applyBulk(action, [...selected], selected.size);
  };

  /**
   * Run one bulk action over an explicit id list.
   *
   * Split out of `runBulk` so the keyboard's `Shift+E` can act on *what is on screen* rather
   * than on a selection the reader did not make. The notice still says "N of M", because the
   * reader's question after a bulk action is always "did that do what I asked" and a bare "5"
   * cannot answer it — some of those five were already read, and the honest number is lower.
   */
  const applyBulk = async (
    action: "read" | "unread" | "archive" | "delete",
    ids: string[],
    of: number,
  ) => {
    if (ids.length === 0) return;
    if (action === "delete" && !confirm(`Delete ${of} notifications? This cannot be undone.`)) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await bulkNotifications(action, ids);
      setNotice(
        action === "delete"
          ? `Deleted ${result.changed} of ${of} selected.`
          : `${result.changed} of ${of} marked ${action === "read" ? "read" : action === "unread" ? "unread" : "archived"}.`,
      );
      setSelected(new Set());
      await load(false);
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  };

  /** `Shift+E` — mark every row currently on screen read, without selecting it first. */
  const markVisibleRead = async () => {
    await applyBulk(
      "read",
      rows.map((row) => row.id),
      rows.length,
    );
  };

  const toggleRead = async (row: NotificationRow) => {
    setBusy(true);
    try {
      await setNotificationRead(row.id, row.read_at === null);
      await load(false);
      if (drawer?.id === row.id) setDrawer({ ...row, read_at: row.read_at ? null : new Date().toISOString() });
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  };

  const openRow = async (row: NotificationRow) => {
    setDrawer(row);
    if (row.read_at === null) {
      // Opening a notification is what marks it read, the way every inbox behaves — but the
      // server's answer is what the badge uses, so the count never comes from a guess.
      try {
        await setNotificationRead(row.id, true);
        await load(false);
        setDrawer({ ...row, read_at: new Date().toISOString() });
      } catch (caught) {
        setError((caught as ApiError).message);
      }
    }
  };

  // The keyboard path. It is bound on the list rather than on the document, so a shortcut
  // cannot fire while the reader is typing in the filter box.
  const onKeyDown = (event: React.KeyboardEvent<HTMLTableSectionElement>) => {
    if (event.key === "/" && filterInput.current) {
      event.preventDefault();
      filterInput.current.focus();
      return;
    }
    // `Shift+E` is checked *before* `e`, because the two share a key and the shift is what
    // separates them — testing `event.key === "e"` first would make the upper case one
    // unreachable on keyboards that report a shifted letter as "E" and unreachable on the
    // ones that report it as "e", depending on the platform. `event.key` is compared
    // case-insensitively here rather than trusting the platform to pick a convention.
    if (event.key.toLowerCase() === "e" && event.shiftKey) {
      event.preventDefault();
      void markVisibleRead();
      return;
    }
    if (event.key === "j" || event.key === "ArrowDown") {
      event.preventDefault();
      setCursorIndex((index) => Math.min(index + 1, rows.length - 1));
      return;
    }
    if (event.key === "k" || event.key === "ArrowUp") {
      event.preventDefault();
      setCursorIndex((index) => Math.max(index - 1, 0));
      return;
    }
    if (event.key === "Escape") {
      // `Escape` is answered *before* the cursor is read. The drawer is a panel over the
      // list, and the list is where this handler lives, so a reader who opens a row and
      // presses Escape is asking the drawer to go — whether or not a row happens to be under
      // the cursor at that moment. Reading the cursor first would mean Escape works on an
      // empty list and silently does nothing on a list that has rows, which is the one
      // combination in which a shortcut is worse than no shortcut: it looks broken, and it
      // is broken *only* in the case people notice.
      if (drawer) {
        event.preventDefault();
        setDrawer(null);
      }
      return;
    }
    const row = rows[cursorIndex];
    if (!row) return;
    if (event.key === "Enter") {
      event.preventDefault();
      void openRow(row);
    } else if (event.key === "e") {
      event.preventDefault();
      void toggleRead(row);
    } else if (event.key === "x") {
      event.preventDefault();
      setSelected((previous) => {
        const next = new Set(previous);
        if (next.has(row.id)) next.delete(row.id);
        else next.add(row.id);
        return next;
      });
    }
  };

  const openDetail = async (row: NotificationRow) => {
    setDrawer(row);
    try {
      // The list's copy is the summary; the detail is the record with its payload, so a
      // drawer that showed only the list row would be a second, thinner answer to the same
      // question.
      setDrawer(await fetchNotification(row.id));
    } catch {
      /* the list row is already on screen — a failed re-read is not a reason to blank it */
    }
  };

  const activeFilters =
    (filters.category ? 1 : 0) +
    (filters.read ? 1 : 0) +
    (filters.priority ? 1 : 0) +
    (filters.archived ? 1 : 0) +
    (filters.with_read ? 1 : 0);

  return (
    <div className="flex flex-col gap-4">
      {/* Filters. Every control is a real form control with a visible label, because a filter
          the reader cannot name is a filter they will not trust. */}
      <section
        aria-label="Filters"
        data-notification-filters
        className="flex flex-wrap items-end gap-3 rounded-xl border border-line bg-surface p-3"
      >
        <label className="flex flex-col gap-1 text-[11.5px] text-muted">
          <span className="font-medium">Search titles</span>
          <input
            ref={filterInput}
            id="notification-search"
            type="search"
            placeholder="Press / to focus"
            className="w-48 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
            onChange={(event) => {
              const next = new URLSearchParams(params?.toString() ?? "");
              if (event.target.value) next.set("q", event.target.value);
              else next.delete("q");
              router.replace(next.toString() ? `/notifications?${next}` : "/notifications");
            }}
          />
        </label>

        <label className="flex flex-col gap-1 text-[11.5px] text-muted">
          <span className="font-medium">Category</span>
          <select
            id="notification-category"
            value={filters.category ?? ""}
            onChange={(event) => setFilter("category", event.target.value)}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
          >
            <option value="">All categories</option>
            {NOTIFICATION_CATEGORIES.map((category) => (
              <option key={category} value={category}>
                {CATEGORY_LINE[category] ?? category}
              </option>
            ))}
          </select>
        </label>

        <label className="flex flex-col gap-1 text-[11.5px] text-muted">
          <span className="font-medium">State</span>
          <select
            id="notification-read"
            value={filters.read ?? ""}
            onChange={(event) => setFilter("read", event.target.value)}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
          >
            <option value="">Unread and read</option>
            <option value="unread">Unread only</option>
            <option value="read">Read only</option>
          </select>
        </label>

        <label className="flex flex-col gap-1 text-[11.5px] text-muted">
          <span className="font-medium">Priority</span>
          <select
            id="notification-priority"
            value={filters.priority ?? ""}
            onChange={(event) => setFilter("priority", event.target.value)}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
          >
            <option value="">Any priority</option>
            {NOTIFICATION_PRIORITIES.map((priority) => (
              <option key={priority} value={priority}>
                {priority}
              </option>
            ))}
          </select>
        </label>

        <button
          type="button"
          onClick={() => void load(false)}
          data-notification-refresh
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted transition hover:text-ink"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Refresh
        </button>

        {activeFilters > 0 ? (
          <button
            type="button"
            onClick={reset}
            data-notification-reset
            className="rounded-lg px-2.5 py-1.5 text-[12.5px] text-accent-strong hover:underline"
          >
            Reset filters
          </button>
        ) : null}

        {/* The link to the settings screen lives here rather than behind the bell, because
            the reader who has just worked out that they want fewer notifications is standing
            on this page, not looking at the header. */}
        <Link
          href="/notifications/settings"
          data-notification-settings-link
          className="ml-auto inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted transition hover:text-ink"
        >
          <Settings2 className="size-3.5" aria-hidden />
          Settings
        </Link>
      </section>

      {notice ? (
        <p
          data-notification-notice
          role="status"
          className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] text-muted"
        >
          {notice}
        </p>
      ) : null}

      {error ? (
        <div
          data-notification-error
          role="alert"
          className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
        >
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
          <span className="flex-1">{error}</span>
          <button
            type="button"
            onClick={() => void load(false)}
            className="text-accent-strong hover:underline"
          >
            Retry
          </button>
        </div>
      ) : null}

      {/* The bulk bar only exists when something is selected — a row of disabled buttons is
          a worse empty state than no row at all. */}
      {selected.size > 0 ? (
        <div
          data-notification-bulk
          role="toolbar"
          aria-label="Bulk actions"
          className="flex flex-wrap items-center gap-2 rounded-xl border border-accent/30 bg-accent-soft px-3 py-2"
        >
          <span className="text-[12.5px] font-medium">{selected.size} selected</span>
          <button
            type="button"
            disabled={busy}
            onClick={() => void runBulk("read")}
            data-bulk="read"
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            <Check className="size-3.5" aria-hidden />
            Mark read
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() => void runBulk("unread")}
            data-bulk="unread"
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            <Inbox className="size-3.5" aria-hidden />
            Mark unread
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() => void runBulk("archive")}
            data-bulk="archive"
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            <Archive className="size-3.5" aria-hidden />
            Archive
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() => void runBulk("delete")}
            data-bulk="delete"
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] text-red-600 disabled:opacity-50"
          >
            <Trash2 className="size-3.5" aria-hidden />
            Delete
          </button>
          <button
            type="button"
            onClick={() => setSelected(new Set())}
            className="ml-auto text-[12.5px] text-muted hover:underline"
          >
            Clear selection
          </button>
        </div>
      ) : null}

      <div className="overflow-hidden rounded-xl border border-line bg-surface">
        {loading && rows.length === 0 ? (
          <div aria-busy="true" data-notification-skeleton className="flex flex-col gap-2 p-4">
            {Array.from({ length: 5 }, (_, index) => (
              <div key={index} className="h-9 animate-pulse rounded bg-quiet-soft" />
            ))}
          </div>
        ) : rows.length === 0 ? (
          <div data-notification-empty className="flex flex-col items-center gap-3 px-4 py-16 text-center">
            <BellOff className="size-6 text-muted" aria-hidden />
            <div>
              <p className="text-[13.5px] font-medium">You&apos;re all caught up.</p>
              <p className="text-[12.5px] text-muted">
                Nothing here{activeFilters > 0 ? " matches these filters" : " yet"}.
              </p>
            </div>
            {filters.read === "unread" ? (
              <button
                type="button"
                data-notification-show-read
                onClick={() => setFilter("with_read", true)}
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-accent-strong hover:underline"
              >
                Show read notifications
              </button>
            ) : null}
            {activeFilters > 0 ? (
              <button
                type="button"
                onClick={reset}
                className="text-[12.5px] text-muted hover:underline"
              >
                Reset filters
              </button>
            ) : null}
          </div>
        ) : (
          <div className="overflow-x-auto">
            <table
              ref={tableRef}
              data-notification-table
              className="w-full border-collapse text-left text-[13px]"
            >
              <thead>
                <tr className="border-b border-line text-[11.5px] text-muted">
                  <th scope="col" className="w-9 px-3 py-2.5">
                    <span className="sr-only">Select</span>
                  </th>
                  <th scope="col" className="px-3 py-2.5 font-medium">Title</th>
                  <th scope="col" className="px-3 py-2.5 font-medium">Category</th>
                  <th scope="col" className="px-3 py-2.5 font-medium">Priority</th>
                  <th scope="col" className="px-3 py-2.5 font-medium">Created</th>
                  <th scope="col" className="px-3 py-2.5 font-medium">State</th>
                </tr>
              </thead>
              <tbody onKeyDown={onKeyDown} tabIndex={0} aria-label="Notifications">
                {rows.map((row, index) => {
                  const isCursor = index === cursorIndex;
                  const isSelected = selected.has(row.id);
                  return (
                    <tr
                      key={row.id}
                      data-notification-row={row.id}
                      data-cursor={isCursor ? "true" : undefined}
                      // The read state is on the row as a data attribute rather than only as
                      // a visual difference, so the QA pass can assert what the reader sees
                      // and a row that looks identical to another for a colour reason is not
                      // mistaken for one that is.
                      data-read={row.read_at ? "true" : "false"}
                      onClick={() => void openDetail(row)}
                      className={`cursor-pointer border-b border-line/60 transition last:border-0 hover:bg-quiet-soft ${
                        isCursor ? "bg-accent-soft" : ""
                      }`}
                    >
                      <td className="px-3 py-2.5">
                        <button
                          type="button"
                          aria-label={isSelected ? `Deselect ${row.title}` : `Select ${row.title}`}
                          aria-pressed={isSelected}
                          data-select={row.id}
                          onClick={(event) => {
                            event.stopPropagation();
                            setSelected((previous) => {
                              const next = new Set(previous);
                              if (next.has(row.id)) next.delete(row.id);
                              else next.add(row.id);
                              return next;
                            });
                          }}
                          className="rounded p-0.5 text-muted hover:text-ink"
                        >
                          {isSelected ? (
                            <SquareCheckBig className="size-4" aria-hidden />
                          ) : (
                            <Square className="size-4" aria-hidden />
                          )}
                        </button>
                      </td>
                      <td className="px-3 py-2.5">
                        <span className="flex items-center gap-2">
                          {!row.read_at ? (
                            <span aria-label="Unread" className="size-1.5 shrink-0 rounded-full bg-accent" />
                          ) : null}
                          <span className="truncate font-medium">{row.title}</span>
                        </span>
                      </td>
                      <td className="px-3 py-2.5 text-muted">
                        {CATEGORY_LINE[row.category] ?? row.category}
                      </td>
                      <td className="px-3 py-2.5">
                        <span
                          className={`rounded-md px-1.5 py-0.5 text-[11.5px] ${
                            PRIORITY_TONE[row.priority] ?? PRIORITY_TONE.normal
                          }`}
                        >
                          {row.priority}
                        </span>
                      </td>
                      <td className="px-3 py-2.5 text-muted tabular-nums">
                        {new Date(row.created_at).toLocaleString()}
                      </td>
                      <td className="px-3 py-2.5">
                        <button
                          type="button"
                          data-toggle-read={row.id}
                          onClick={(event) => {
                            event.stopPropagation();
                            void toggleRead(row);
                          }}
                          className="rounded-md border border-line px-2 py-1 text-[11.5px] text-muted transition hover:text-ink"
                        >
                          {row.read_at ? "Mark unread" : "Mark read"}
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {hasMore ? (
        <div className="flex justify-center">
          <button
            type="button"
            data-notification-more
            disabled={loading}
            onClick={() => void load(true)}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] text-muted transition hover:text-ink disabled:opacity-50"
          >
            <Search className="size-3.5" aria-hidden />
            {loading ? "Loading…" : "Load older"}
          </button>
        </div>
      ) : null}

      {/* The drawer. It does not change the route — the reader keeps their filters in the URL
          and their scroll position, which is what a side panel is for. */}
      {drawer ? (
        <div className="fixed inset-0 z-50 flex justify-end" role="dialog" aria-modal="true" aria-label="Notification detail">
          <button
            type="button"
            aria-label="Close detail"
            onClick={() => setDrawer(null)}
            className="absolute inset-0 bg-ink/40"
          />
          <div
            data-notification-drawer={drawer.id}
            className="relative flex h-full w-[min(440px,100vw)] flex-col border-l border-line bg-surface shadow-xl"
          >
            <header className="flex items-start justify-between gap-3 border-b border-line px-4 py-3">
              <div className="min-w-0">
                <h2 className="text-[13.5px] font-semibold">{drawer.title}</h2>
                <p className="text-[11.5px] text-muted">
                  {CATEGORY_LINE[drawer.category] ?? drawer.category} · {drawer.priority}
                </p>
              </div>
              <button
                type="button"
                aria-label="Close detail"
                onClick={() => setDrawer(null)}
                className="rounded-md p-1.5 text-muted hover:bg-quiet-soft hover:text-ink"
              >
                <BellOff className="size-4" aria-hidden />
              </button>
            </header>

            <div className="flex-1 overflow-y-auto px-4 py-4 text-[13px]">
              {drawer.body ? <p className="mb-4 whitespace-pre-wrap">{drawer.body}</p> : null}

              {drawer.url ? (
                <a
                  href={drawer.url}
                  data-notification-link
                  className="mb-4 inline-flex items-center gap-1 text-[12.5px] text-accent-strong hover:underline"
                >
                  Go to the record
                </a>
              ) : null}

              <dl className="grid grid-cols-[7rem_1fr] gap-x-3 gap-y-1.5 text-[12.5px]">
                <dt className="text-muted">Created</dt>
                <dd className="tabular-nums">{new Date(drawer.created_at).toLocaleString()}</dd>
                <dt className="text-muted">Read</dt>
                <dd>{drawer.read_at ? new Date(drawer.read_at).toLocaleString() : "Not read"}</dd>
                {drawer.source_type ? (
                  <>
                    <dt className="text-muted">Source</dt>
                    <dd className="truncate">
                      {drawer.source_type}
                      {drawer.source_id ? ` · ${drawer.source_id}` : ""}
                    </dd>
                  </>
                ) : null}
              </dl>

              {drawer.payload && Object.keys(drawer.payload as object).length > 0 ? (
                <pre
                  data-notification-payload
                  className="mt-4 overflow-x-auto rounded-lg bg-quiet-soft p-3 text-[11.5px]"
                >
                  {JSON.stringify(drawer.payload, null, 2)}
                </pre>
              ) : null}
            </div>

            <footer className="flex items-center gap-2 border-t border-line px-4 py-3">
              <button
                type="button"
                onClick={() => void toggleRead(drawer)}
                data-drawer-toggle-read
                className="rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] transition hover:text-ink"
              >
                {drawer.read_at ? "Mark unread" : "Mark read"}
              </button>
              <button
                type="button"
                onClick={async () => {
                  await deleteNotification(drawer.id);
                  setDrawer(null);
                  await load(false);
                }}
                data-drawer-delete
                className="rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-red-600 transition hover:border-red-500/40"
              >
                <span className="inline-flex items-center gap-1">
                  <Trash2 className="size-3.5" aria-hidden />
                  Delete
                </span>
              </button>
            </footer>
          </div>
        </div>
      ) : null}
    </div>
  );
}
