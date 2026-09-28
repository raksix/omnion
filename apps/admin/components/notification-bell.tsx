"use client";

/**
 * The notification bell: the unread badge, the grouped panel and the last ten items.
 *
 * Three things this screen is careful about, each a way an inbox widget ends up lying:
 *
 * 1. **The badge and the grouped lines come from one request.** They are the same summary, so
 *    a bell that says 12 above four lines adding up to 9 cannot be built — which is the most
 *    damaging kind of wrong on a panel, because the reader trusts the badge and stops looking.
 * 2. **A failed load says so, in the panel.** An empty box after a network error is the one
 *    thing a reader cannot tell apart from "you are all caught up", and it is the state that
 *    makes people stop opening the bell at all.
 * 3. **The panel closes on `Esc` and returns focus to the bell.** A popover that traps the
 *    keyboard is worse than one that does not open, because the reader cannot get back.
 *
 * `Enter`/`Space` open it, which is why the trigger is a real `<button>` and not a `<div>` with
 * a click handler — the platform's own rule (no dead controls) applies to the a11y tree too.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { Bell, Check, RefreshCw, TriangleAlert, X } from "lucide-react";

import {
  fetchNotificationSummary,
  fetchNotifications,
  markAllNotificationsRead,
  type ApiError,
} from "@/lib/api";
import { NOTIFICATION_CATEGORIES, type NotificationRow, type NotificationSummary } from "@/lib/types";

/** How many rows the panel lists before it sends the reader to the full list. */
const PANEL_ROWS = 10;

/** How often the badge refreshes while the tab is visible (REQ-021: 30 s). */
const POLL_MS = 30_000;

/** A category and the line the panel shows for it. */
const CATEGORY_LINE: Record<string, string> = {
  approval: "awaiting approval",
  security: "security alerts",
  update: "plugin updates",
  ticket: "new tickets",
  system: "system messages",
  mention: "mentions",
};

type BellProps = {
  /** Called after a change so the list screen can reload too. */
  onChanged?: () => void;
};

export function NotificationBell({ onChanged }: BellProps) {
  const router = useRouter();
  const [open, setOpen] = useState(false);
  const [summary, setSummary] = useState<NotificationSummary | null>(null);
  const [recent, setRecent] = useState<NotificationRow[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      // The panel needs both, and they are two reads by design: the summary is what the
      // header shows on every route, and the rows are only worth fetching when the panel is
      // open. Fetching rows eagerly would put a list request behind every page load.
      const [counts, page] = await Promise.all([
        fetchNotificationSummary(),
        fetchNotifications({ limit: PANEL_ROWS }),
      ]);
      setSummary(counts);
      setRecent(page.notifications);
    } catch (caught) {
      const failure = caught as ApiError;
      setError(failure.message);
    }
  }, []);

  // The first load, and a poll while the tab is visible. A hidden tab keeps its badge honest
  // without asking the server 30 times an hour for a window nobody is looking at.
  useEffect(() => {
    void load();
    const timer = setInterval(() => {
      if (document.visibilityState === "visible") void load();
    }, POLL_MS);
    return () => clearInterval(timer);
  }, [load]);

  // `Esc` closes and hands focus back to the bell — the reader must land where they started.
  useEffect(() => {
    if (!open) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setOpen(false);
        trigger.current?.focus();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open]);

  // A click outside closes the panel, the way every other popover on the platform behaves.
  useEffect(() => {
    if (!open) return;
    const onPointer = (event: MouseEvent) => {
      if (root.current && !root.current.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onPointer);
    return () => document.removeEventListener("mousedown", onPointer);
  }, [open]);

  const unread = summary?.unread ?? 0;

  const clearAll = async () => {
    setLoading(true);
    try {
      setSummary(await markAllNotificationsRead());
      await load();
      onChanged?.();
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setLoading(false);
    }
  };

  // A grouped line is shown when it has a count, and the whole group is hidden when the
  // platform has nothing to say. The alternative — six lines of zero — is a panel that
  // spends its space on absence.
  const groups = (summary?.by_category ?? []).filter((entry) => entry.count > 0);

  return (
    <div ref={root} className="relative">
      <button
        ref={trigger}
        type="button"
        aria-label={unread > 0 ? `Notifications, ${unread} unread` : "Notifications"}
        aria-expanded={open}
        aria-haspopup="dialog"
        data-bell
        onClick={() => setOpen((value) => !value)}
        className="relative rounded-lg border border-line bg-surface p-2 text-muted transition hover:text-ink"
      >
        <Bell className="size-4" aria-hidden />
        {unread > 0 ? (
          <span
            data-bell-badge
            aria-hidden
            className="absolute -top-1.5 -right-1.5 flex min-w-4.5 items-center justify-center rounded-full bg-accent px-1 text-[10px] font-semibold text-white tabular-nums"
          >
            {unread > 99 ? "99+" : unread}
          </span>
        ) : null}
      </button>

      {open ? (
        <div
          role="dialog"
          aria-label="Notifications"
          data-bell-panel
          className="absolute right-0 z-40 mt-2 w-[min(420px,calc(100vw-2rem))] overflow-hidden rounded-xl border border-line bg-surface shadow-xl"
        >
          <div className="flex items-center justify-between border-b border-line px-4 py-3">
            <h2 className="text-[13px] font-semibold">Notifications</h2>
            <div className="flex items-center gap-1">
              <button
                type="button"
                onClick={clearAll}
                disabled={loading || unread === 0}
                className="rounded-md px-2 py-1 text-[12px] text-muted transition hover:bg-quiet-soft hover:text-ink disabled:opacity-40"
              >
                <span className="inline-flex items-center gap-1">
                  <Check className="size-3.5" aria-hidden />
                  Mark all read
                </span>
              </button>
              <button
                type="button"
                aria-label="Close notifications"
                onClick={() => {
                  setOpen(false);
                  trigger.current?.focus();
                }}
                className="rounded-md p-1.5 text-muted transition hover:bg-quiet-soft hover:text-ink"
              >
                <X className="size-4" aria-hidden />
              </button>
            </div>
          </div>

          {groups.length > 0 ? (
            <ul data-bell-groups className="border-b border-line py-1">
              {groups.map((group) => (
                <li key={group.category}>
                  <Link
                    href={`/notifications?category=${encodeURIComponent(group.category)}`}
                    data-bell-group={group.category}
                    onClick={() => setOpen(false)}
                    className="flex items-center justify-between px-4 py-2 text-[12.5px] transition hover:bg-quiet-soft"
                  >
                    <span className="text-muted">
                      {group.count} {CATEGORY_LINE[group.category] ?? group.category}
                    </span>
                    <span className="font-medium tabular-nums">{group.count}</span>
                  </Link>
                </li>
              ))}
            </ul>
          ) : null}

          {/* The error state is its own line rather than an empty list: "could not load" and
              "you are all caught up" are different facts and must not look the same. */}
          {error ? (
            <div
              data-bell-error
              className="flex items-start gap-2 border-b border-line px-4 py-3 text-[12px] text-muted"
            >
              <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-amber-600" aria-hidden />
              <span className="flex-1">Could not load notifications — retry</span>
              <button
                type="button"
                onClick={() => void load()}
                className="inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-[11.5px] text-accent-strong hover:underline"
              >
                <RefreshCw className="size-3" aria-hidden />
                Retry
              </button>
            </div>
          ) : null}

          <div className="max-h-80 overflow-y-auto">
            {recent.length === 0 && !error ? (
              <p data-bell-empty className="px-4 py-8 text-center text-[12.5px] text-muted">
                You&apos;re all caught up.
              </p>
            ) : null}
            {recent.map((row) => (
              <button
                key={row.id}
                type="button"
                data-bell-row={row.id}
                onClick={() => {
                  setOpen(false);
                  router.push(row.url ?? `/notifications?category=${encodeURIComponent(row.category)}`);
                }}
                className={`flex w-full flex-col gap-0.5 border-b border-line/60 px-4 py-2.5 text-left transition last:border-0 hover:bg-quiet-soft ${
                  row.read_at ? "opacity-60" : ""
                }`}
              >
                <span className="flex items-center justify-between gap-2">
                  <span className="truncate text-[12.5px] font-medium">{row.title}</span>
                  {!row.read_at ? (
                    <span
                      aria-label="Unread"
                      className="size-1.5 shrink-0 rounded-full bg-accent"
                    />
                  ) : null}
                </span>
                <span className="truncate text-[11.5px] text-muted">
                  {CATEGORY_LINE[row.category] ?? row.category}
                </span>
              </button>
            ))}
          </div>

          <div className="border-t border-line px-4 py-2">
            <Link
              href="/notifications"
              data-bell-view-all
              onClick={() => setOpen(false)}
              className="text-[12.5px] text-accent-strong hover:underline"
            >
              View all
            </Link>
          </div>
        </div>
      ) : null}
    </div>
  );
}

/** The categories the panel's own vocabulary covers — exported so the list screen agrees. */
export { NOTIFICATION_CATEGORIES };
