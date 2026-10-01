"use client";

/**
 * `/hr/leave` — the request list and the absence calendar (REQ-055, slice 2b).
 *
 * One screen, two surfaces, because a person answering "who is out this week?" and a person
 * answering "is my holiday approved?" want the same screen at different zoom. The list is the
 * inbox; the calendar is the month.
 *
 * Three decisions that the screen could get wrong:
 *
 * - **The list opens on pending.** An approver's question is "what needs me?", and a screen that
 *   opens on every request ever filed buries the answer under history. `pending` is a chip, not the
 *   only filter, and the count in the header says how many are waiting.
 * - **The two surfaces share one window.** The calendar sends `from`/`to` only when the operator
 *   navigated; otherwise it takes the server's current month. The list, by contrast, defaults to
 *   the current *year* server-side. Keeping the window on the calendar rather than on the list is
 *   what makes "the month" mean one thing on this page instead of two.
 * - **A bar on the calendar opens the request.** The grid is a table of buttons, not a picture:
 *   a month grid nobody can click is the "coming soon" pattern in a costume.
 */
import { useCallback, useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import Link from "next/link";
import { ChevronLeft, ChevronRight, Plus, Search } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import { fetchCalendar, fetchRequests, type AbsenceCalendar, type LeaveRequest } from "@/lib/hr";

import { AbsenceCalendarGrid } from "./absence-calendar";
import { DaysCell, LeaveStatusBadge, LeaveTypeDot, LeaveWindowNote, NoAbsencesNote } from "./hr-parts";

/** The status chips. `pending` first because it is what an approver opens the screen for. */
const FILTERS = [
  { value: "pending", label: "Pending" },
  { value: "all", label: "All" },
  { value: "approved", label: "Approved" },
  { value: "rejected", label: "Rejected" },
  { value: "cancelled", label: "Cancelled" },
] as const;

export function LeaveView() {
  const router = useRouter();
  const [scope, setScope] = useState<string>("pending");
  const [search, setSearch] = useState("");
  const [rows, setRows] = useState<LeaveRequest[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);

  const [calendar, setCalendar] = useState<AbsenceCalendar | null>(null);
  const [calendarError, setCalendarError] = useState<ScreenErrorValue>(null);
  const [calendarLoading, setCalendarLoading] = useState(true);
  const [window, setWindow] = useState<{ from?: string; to?: string }>({});

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const page = await fetchRequests({
        // `pending_only` carries the chip and `status` carries a named status; the server owns the
        // difference, so the client does not have to know that "all" is not a status.
        status: scope === "pending" || scope === "all" ? undefined : scope,
        pending_only: scope === "pending" ? true : undefined,
        search: search.trim() || undefined,
        limit: 50,
      });
      setRows(page.items);
      setTotal(page.total_estimate);
    } catch (failure) {
      setError(toScreenError(failure, "The leave requests could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [scope, search]);

  const loadCalendar = useCallback(async () => {
    setCalendarLoading(true);
    setCalendarError(null);
    try {
      setCalendar(await fetchCalendar(window));
    } catch (failure) {
      setCalendarError(toScreenError(failure, "The absence calendar could not be loaded."));
    } finally {
      setCalendarLoading(false);
    }
  }, [window]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    void loadCalendar();
  }, [loadCalendar]);

  /** Step the window by a whole month, keeping the length the server handed us. */
  const step = async (months: number) => {
    if (!calendar) {
      return;
    }
    const from = shiftMonth(calendar.from, months);
    const to = shiftMonth(calendar.to, months);
    setWindow({ from, to });
  };

  const legend = legendOf(calendar);

  return (
    <div className="space-y-8">
      <header className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-xl font-semibold tracking-tight">Leave</h1>
          <p className="text-sm text-muted">
            Requests, balances and who is away.{" "}
            <span data-qa-hr-leave-total className="text-muted-foreground">
              {total} in view
            </span>
          </p>
        </div>
        <Link
          href="/hr/leave/new"
          data-qa-hr-leave-new
          className="inline-flex h-9 items-center gap-2 rounded-md bg-primary px-3 text-sm text-primary-foreground"
        >
          <Plus className="h-4 w-4" aria-hidden />
          Request leave
        </Link>
      </header>

      <section className="space-y-3" aria-labelledby="hr-leave-calendar-heading">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 id="hr-leave-calendar-heading" className="text-[15px] font-medium">
            Absence calendar
          </h2>
          <div className="flex items-center gap-2">
            {calendar ? <LeaveWindowNote from={calendar.from} to={calendar.to} today={calendar.today} /> : null}
            <div className="flex items-center gap-1">
              <button
                type="button"
                onClick={() => void step(-1)}
                disabled={!calendar}
                aria-label="Previous month"
                data-qa-hr-calendar-prev
                className="inline-flex h-8 w-8 items-center justify-center rounded-md border border-border disabled:opacity-50"
              >
                <ChevronLeft className="h-4 w-4" aria-hidden />
              </button>
              <button
                type="button"
                onClick={() => setWindow({})}
                disabled={!calendar}
                data-qa-hr-calendar-this-month
                className="h-8 rounded-md border border-border px-2.5 text-sm disabled:opacity-50"
              >
                This month
              </button>
              <button
                type="button"
                onClick={() => void step(1)}
                disabled={!calendar}
                aria-label="Next month"
                data-qa-hr-calendar-next
                className="inline-flex h-8 w-8 items-center justify-center rounded-md border border-border disabled:opacity-50"
              >
                <ChevronRight className="h-4 w-4" aria-hidden />
              </button>
            </div>
          </div>
        </div>

        {calendarLoading ? (
          <div className="rounded-lg border border-border p-4" aria-busy="true">
            <div className="h-4 w-40 animate-pulse rounded bg-quiet-soft" />
          </div>
        ) : calendarError ? (
          <ErrorState
            error={calendarError}
            onRetry={() => void loadCalendar()}
            qa="hr-calendar-error"
          />
        ) : calendar && calendar.bars.length === 0 ? (
          <div className="rounded-lg border border-border p-4">
            <NoAbsencesNote />
          </div>
        ) : calendar ? (
          <>
            <AbsenceCalendarGrid calendar={calendar} onOpenRequest={(id) => router.push(`/hr/leave/${id}`)} />
            <div className="flex flex-wrap items-center gap-3">
              {legend.map((entry) => (
                <LeaveTypeDot
                  key={entry.typeId}
                  typeId={entry.typeId}
                  name={entry.name}
                  count={entry.count}
                />
              ))}
            </div>
          </>
        ) : null}
      </section>

      <section className="space-y-3" aria-labelledby="hr-leave-list-heading">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 id="hr-leave-list-heading" className="text-[15px] font-medium">
            Requests
          </h2>
          <div className="flex flex-wrap items-center gap-2">
            {FILTERS.map((filter) => (
              <button
                key={filter.value}
                type="button"
                data-qa-hr-leave-filter={filter.value}
                aria-pressed={scope === filter.value}
                onClick={() => setScope(filter.value)}
                className={`h-8 rounded-full border px-3 text-sm ${
                  scope === filter.value
                    ? "border-primary bg-primary text-primary-foreground"
                    : "border-border text-muted hover:text-foreground"
                }`}
              >
                {filter.label}
              </button>
            ))}
            <label className="flex h-8 items-center gap-2 rounded-md border border-border px-2 text-sm">
              <Search className="h-4 w-4 text-muted" aria-hidden />
              <span className="sr-only">Search requests</span>
              <input
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder="Employee or reason"
                data-qa-hr-leave-search
                className="w-44 bg-transparent outline-none"
              />
            </label>
          </div>
        </div>

        {loading ? (
          <LoadingTable columns={7} />
        ) : error ? (
          <ErrorState error={error} onRetry={() => void load()} qa="hr-leave-error" />
        ) : rows.length === 0 ? (
          <div className="rounded-lg border border-border">
            <EmptyState
              title={
                scope === "pending"
                  ? "Nothing is waiting for a decision"
                  : search
                    ? "No request matches that search"
                    : "No leave request yet"
              }
              hint={
                scope === "pending"
                  ? "Requests appear here the moment somebody raises one."
                  : "Raise the first one and it will show up here with its balance."
              }
              action={
                <Link
                  href="/hr/leave/new"
                  className="inline-flex h-9 items-center gap-2 rounded-md bg-primary px-3 text-sm text-primary-foreground"
                >
                  <Plus className="h-4 w-4" aria-hidden />
                  Request leave
                </Link>
              }
            />
          </div>
        ) : (
          <div className="overflow-x-auto rounded-lg border border-border">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[12px] text-muted">
                  <th scope="col" className="px-4 py-2 font-medium">Employee</th>
                  <th scope="col" className="px-4 py-2 font-medium">Type</th>
                  <th scope="col" className="px-4 py-2 font-medium">From</th>
                  <th scope="col" className="px-4 py-2 font-medium">To</th>
                  <th scope="col" className="px-4 py-2 font-medium">Days</th>
                  <th scope="col" className="px-4 py-2 font-medium">Status</th>
                  <th scope="col" className="px-4 py-2 font-medium">Reason</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => (
                  <tr key={row.id} className="border-t border-line hover:bg-quiet-soft/40">
                    <td className="px-4 py-2.5">
                      <Link
                        href={`/hr/leave/${row.id}`}
                        data-qa-hr-leave-row={row.id}
                        className="font-medium hover:underline"
                      >
                        {row.employee_name}
                      </Link>
                    </td>
                    <td className="px-4 py-2.5 text-muted">{row.leave_type_name}</td>
                    <td className="px-4 py-2.5 whitespace-nowrap">{row.starts_on}</td>
                    <td className="px-4 py-2.5 whitespace-nowrap">{row.ends_on}</td>
                    <td className="px-4 py-2.5 whitespace-nowrap">
                      <DaysCell days={row.days} />
                      {row.half_day ? <span className="ml-1 text-[11px] text-muted">(half)</span> : null}
                    </td>
                    <td className="px-4 py-2.5">
                      <LeaveStatusBadge status={row.leave_status} />
                    </td>
                    <td className="max-w-64 truncate px-4 py-2.5 text-muted">{row.reason || "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>
    </div>
  );
}

/** One legend entry: a leave type and how many bars of it the window holds. */
function legendOf(calendar: AbsenceCalendar | null): { typeId: string; name: string; count: number }[] {
  if (!calendar) {
    return [];
  }
  const byType = new Map<string, { name: string; count: number }>();
  for (const bar of calendar.bars) {
    const entry = byType.get(bar.leave_type_id) ?? { name: bar.leave_type_name, count: 0 };
    entry.count += 1;
    byType.set(bar.leave_type_id, entry);
  }
  return [...byType.entries()].map(([typeId, entry]) => ({ typeId, ...entry }));
}

/** Move an ISO date by whole months, clamping the day into the target month's length. */
function shiftMonth(iso: string, months: number): string {
  const [year, month, day] = iso.split("-").map(Number);
  const total = (year * 12 + (month - 1)) + months;
  const nextYear = Math.floor(total / 12);
  const nextMonth = (total % 12 + 12) % 12;
  // The first of a month always exists, and every window the server sent starts on one, so the
  // clamp is for `to` rather than for `from` — a 31st walking into a 30-day month becomes the 28th
  // rather than rolling into the next month and shortening the window by a day.
  const lastDay = new Date(Date.UTC(nextYear, nextMonth + 1, 0)).getUTCDate();
  const clamped = Math.min(day, lastDay);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${nextYear}-${pad(nextMonth + 1)}-${pad(clamped)}`;
}
