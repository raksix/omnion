"use client";

/**
 * The clock (REQ-055, slice 2d) — `/hr/me/attendance` and `/hr/attendance`.
 *
 * Attendance is the half of the module leave does not answer: leave says when somebody is *away*,
 * this says when they were *here* and how much of that day can be defended. Two surfaces, and
 * the reason they are two rather than one tab:
 *
 * - **The self-service clock** (`/hr/me/attendance`) carries **no `hr.*` key**, for the same
 *   reason the rest of `/hr/me/*` carries none: the person pressing the button is an employee,
 *   and an employee is exactly who holds no `hr.attendance.*` permission. The two punches are
 *   the only writes on the screen, and the month grid below them is the receipt.
 * - **The roster** (`/hr/attendance`) is one organization's *day* — who is in, who is out, who is
 *   away — and it needs `hr.attendance.read`, because it answers for everybody at once.
 *
 * Three things the screen could get wrong, and what it does instead:
 *
 * 1. **The button is the day's state, not a constant.** A day with a check-in and no check-out
 *    offers "Check out"; a day already punched offers neither. Two always-visible buttons is the
 *    double-punch pattern, and the second click is a 409 whose message the user has to read to
 *    understand why their day did not change.
 * 2. **A refusal shows what the server found.** The 409 body carries the punch it found, so the
 *    error line reads "already clocked in, at 09:02" instead of a bare conflict code — the
 *    difference between an error a person can act on and one they have to read a log for.
 * 3. **The minutes are never computed here.** The grid cell, the footer total and the CSV are
 *    three readers of one server-side projection. A screen that added its own arithmetic would be
 *    a fourth reader, and the acceptance criterion is that the CSV matches the grid.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import { AlertTriangle, Clock, Download, LogIn, LogOut, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ErrorState,
  describeError,
  toScreenError,
  type ScreenErrorValue,
} from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  checkInSelf,
  checkOutSelf,
  fetchMyAttendance,
  fetchRoster,
  formatMinutes,
  formatPunch,
  type AttendanceDay,
  type AttendanceException,
  type AttendanceMonth,
  type Roster,
} from "@/lib/hr";

/** The three exceptions, as a person reads them. Never colour alone. */
const EXCEPTION_LABEL: Record<AttendanceException, string> = {
  missing_checkout: "No check-out",
  overtime: "Over 10h",
  under_hours: "Under 4h",
};

/**
 * The exception as a badge.
 *
 * Overtime and under-hours are the *reading* of a day and are amber; a missing check-out is a
 * *mistake* and is red. The word carries the meaning either way — the tone is only a reading aid,
 * and a grid that distinguished them by hue alone would be unreadable to a colour-blind operator
 * and unreadable in print.
 */
function ExceptionBadge({ exception }: { exception: AttendanceException }) {
  const tone =
    exception === "overtime"
      ? "bg-warning-soft text-warning-ink"
      : "bg-danger-soft text-danger-ink";
  return (
    <span
      data-qa-hr-attendance-exception={exception}
      className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[11.5px] font-medium ${tone}`}
    >
      <AlertTriangle className="h-3 w-3" aria-hidden />
      {EXCEPTION_LABEL[exception]}
    </span>
  );
}

/** The badge for one day, or the neutral cell when the day is plain. */
function DayFlag({ exception }: { exception: AttendanceException | null | undefined }) {
  if (!exception) {
    return <span className="text-[12.5px] text-muted">—</span>;
  }
  return <ExceptionBadge exception={exception} />;
}

/** The month switcher. Reads and writes the URL, so a shared link keeps its month. */
function MonthPicker({ month, onChange }: { month: string; onChange: (next: string) => void }) {
  const shift = (delta: number) => {
    const [year, mon] = month.split("-").map(Number);
    // Walking the year rather than `month + 1`: December has to roll into January, and a
    // switcher that renders January when the viewer clicked "December, next" is worse than no
    // switcher at all.
    const zeroBased = year * 12 + (mon - 1) + delta;
    const nextYear = Math.floor(zeroBased / 12);
    const nextMonth = (zeroBased % 12 + 12) % 12 + 1;
    onChange(`${nextYear}-${String(nextMonth).padStart(2, "0")}`);
  };
  const [year, mon] = month.split("-");
  return (
    <div className="flex items-center gap-1" role="group" aria-label="Month">
      <button
        type="button"
        onClick={() => shift(-1)}
        data-qa-hr-attendance-prev
        className="inline-flex h-8 w-8 items-center justify-center rounded-md border border-line text-[13.5px] hover:bg-quiet-soft"
        aria-label="Previous month"
      >
        ‹
      </button>
      <span data-qa-hr-attendance-month className="min-w-24 text-center text-[13.5px] font-medium">
        {year}-{mon}
      </span>
      <button
        type="button"
        onClick={() => shift(1)}
        data-qa-hr-attendance-next
        className="inline-flex h-8 w-8 items-center justify-center rounded-md border border-line text-[13.5px] hover:bg-quiet-soft"
        aria-label="Next month"
      >
        ›
      </button>
    </div>
  );
}

/** The clock card: the two punches, the current state and the refusal. */
function ClockCard({
  day,
  onPunch,
  busy,
  refusal,
}: {
  day: AttendanceDay | undefined;
  onPunch: (kind: "in" | "out") => void;
  busy: boolean;
  refusal: ScreenErrorValue;
}) {
  const isOpen = day?.check_in != null && day?.check_out == null;
  const worked = day?.minutes_worked ?? null;

  return (
    <section
      aria-label="Clock"
      data-qa-hr-attendance-card
      className="rounded-xl border border-line bg-surface p-4"
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="flex items-center gap-2 text-[13.5px] font-semibold text-ink">
            <Clock className="h-4 w-4" aria-hidden />
            Today
          </h2>
          {day ? (
            <p data-qa-hr-attendance-today className="mt-1 text-[12.5px] text-muted">
              {day.check_in ? `In at ${formatPunch(day.check_in)}` : "Clocked in"}
              {day.check_out ? ` · out at ${formatPunch(day.check_out)}` : ""}
              {worked !== null ? ` · ${formatMinutes(worked)}` : ""}
            </p>
          ) : (
            <p className="mt-1 text-[12.5px] text-muted">No punch recorded today.</p>
          )}
        </div>
        <div className="flex gap-2">
          <button
            type="button"
            onClick={() => onPunch("in")}
            disabled={busy || day?.check_in != null}
            data-qa-hr-attendance-check-in
            className="inline-flex h-8 items-center gap-1.5 rounded-md bg-accent px-3 text-[13px] font-medium text-on-accent disabled:opacity-40"
          >
            <LogIn className="h-4 w-4" aria-hidden />
            Check in
          </button>
          <button
            type="button"
            onClick={() => onPunch("out")}
            disabled={busy || !isOpen}
            data-qa-hr-attendance-check-out
            className="inline-flex h-8 items-center gap-1.5 rounded-md border border-line px-3 text-[13px] font-medium disabled:opacity-40"
          >
            <LogOut className="h-4 w-4" aria-hidden />
            Check out
          </button>
        </div>
      </div>
      {refusal ? (
        <p
          role="alert"
          data-qa-hr-attendance-refusal
          className="mt-3 rounded-md bg-danger-soft px-3 py-2 text-[12.5px] text-danger-ink"
        >
          {describeError(refusal).message}
        </p>
      ) : null}
    </section>
  );
}

/** The month's totals. The footer's job is to be the number the CSV's last row carries. */
function SummaryStrip({ summary }: { summary: AttendanceMonth["summary"] }) {
  const cells: Array<{ label: string; value: number | string; qa: string }> = [
    { label: "Days", value: summary.days_present, qa: "days" },
    { label: "Worked", value: formatMinutes(summary.minutes_worked), qa: "worked" },
    { label: "Over 10h", value: summary.overtime_days, qa: "overtime" },
    { label: "Under 4h", value: summary.under_hours_days, qa: "under" },
    { label: "No check-out", value: summary.missing_checkout_days, qa: "missing" },
    { label: "Open", value: summary.open_days, qa: "open" },
  ];
  return (
    <div
      data-qa-hr-attendance-summary
      className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-6"
    >
      {cells.map((cell) => (
        <div key={cell.qa} className="rounded-xl border border-line bg-surface px-3 py-2">
          <p className="text-[11.5px] text-muted">{cell.label}</p>
          <p data-qa-hr-attendance-total={cell.qa} className="text-[17px] font-semibold text-ink">
            {cell.value}
          </p>
        </div>
      ))}
    </div>
  );
}

/**
 * `/hr/me/attendance` — the caller's own clock and month.
 *
 * The whole screen is keyless. An employee pressing "Check in" is doing the most ordinary act in
 * the module, and requiring a permission for it would switch the feature off for exactly the
 * people it exists for.
 */
export function MyAttendanceView() {
  const router = useRouter();
  const search = useSearchParams();
  const month = search.get("month") ?? new Date().toISOString().slice(0, 7);

  const [data, setData] = useState<AttendanceMonth | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [busy, setBusy] = useState(false);
  const [refusal, setRefusal] = useState<ScreenErrorValue>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setData(await fetchMyAttendance(month));
    } catch (caught) {
      setError(toScreenError(caught, "Your attendance could not be read."));
    } finally {
      setLoading(false);
    }
  }, [month]);

  useEffect(() => {
    void load();
  }, [load]);

  const punch = async (kind: "in" | "out") => {
    setBusy(true);
    setRefusal(null);
    try {
      // No `work_date`: the server stamps today itself, so a phone with the wrong clock cannot
      // write yesterday's worked hours.
      if (kind === "in") {
        await checkInSelf();
      } else {
        await checkOutSelf();
      }
      await load();
    } catch (caught) {
      // The 409 body carries the punch it found, so this reads "already clocked in, at 09:02"
      // rather than a conflict code with no information in it.
      setRefusal(toScreenError(caught, "The clock did not accept that punch."));
    } finally {
      setBusy(false);
    }
  };

  const today = useMemo(
    () => data?.days.find((day) => day.work_date === new Date().toISOString().slice(0, 10)),
    [data],
  );

  if (loading && !data) {
    return <LoadingTable columns={4} />;
  }
  if (error) {
    return <ErrorState error={error} onRetry={() => void load()} />;
  }
  if (!data) {
    return null;
  }

  return (
    <div className="space-y-4" data-qa-hr-attendance>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <MonthPicker
          month={month}
          onChange={(next) => router.push(`/hr/me/attendance?month=${next}`)}
        />
        <a
          href={`/api/v1/hr/me/attendance/export?month=${month}`}
          data-qa-hr-attendance-export
          className="inline-flex h-8 items-center gap-1.5 rounded-md border border-line px-2.5 text-[13px]"
        >
          <Download className="h-4 w-4" aria-hidden />
          Export CSV
        </a>
      </div>

      <ClockCard day={today} onPunch={(kind) => void punch(kind)} busy={busy} refusal={refusal} />
      <SummaryStrip summary={data.summary} />

      {data.days.length === 0 ? (
        <EmptyState
          title="No attendance recorded"
          hint={`Nothing was punched in ${month}. Use the clock above and the day appears here.`}
        />
      ) : (
        <div className="overflow-x-auto rounded-xl border border-line">
          <table className="w-full border-collapse text-[13px]">
            <thead className="border-b border-line text-left text-muted">
              <tr>
                <th className="px-4 py-3 font-medium">Day</th>
                <th className="px-4 py-3 font-medium">In</th>
                <th className="px-4 py-3 font-medium">Out</th>
                <th className="px-4 py-3 font-medium">Worked</th>
                <th className="px-4 py-3 font-medium">Source</th>
                <th className="px-4 py-3 font-medium">Flags</th>
              </tr>
            </thead>
            <tbody>
              {data.days.map((day) => (
                <tr
                  key={day.id}
                  className="border-t border-line"
                  data-qa-hr-attendance-row={day.work_date}
                >
                  <td className="px-4 py-3.5 font-medium text-ink">{day.work_date}</td>
                  <td className="px-4 py-3.5">{formatPunch(day.check_in)}</td>
                  <td className="px-4 py-3.5">{formatPunch(day.check_out)}</td>
                  <td className="px-4 py-3.5" data-qa-hr-attendance-minutes>
                    {formatMinutes(day.minutes_worked)}
                  </td>
                  <td className="px-4 py-3.5 text-muted">
                    {day.source}
                    {day.corrected ? " · corrected" : ""}
                  </td>
                  <td className="px-4 py-3.5">
                    {/*
                      The exception is decided by the server against its own clock. A client
                      recomputing "is this day in the past" is a second answer to a question the
                      grid, the summary and the CSV must all give the same way.
                    */}
                    <DayFlag exception={day.exception} />
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

/**
 * `/hr/attendance` — one organization's day: who is in, who is out, who is away.
 *
 * Needs `hr.attendance.read`, because it answers for everybody at once. The absence half is
 * joined from approved leave rather than re-derived here, so the roster cannot say somebody is
 * working on a day their own leave says they are away.
 */
export function RosterView() {
  const search = useSearchParams();
  const day = search.get("day") ?? new Date().toISOString().slice(0, 10);

  const [roster, setRoster] = useState<Roster | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setRoster(await fetchRoster(day));
    } catch (caught) {
      setError(toScreenError(caught, "The roster could not be read."));
    } finally {
      setLoading(false);
    }
  }, [day]);

  useEffect(() => {
    void load();
  }, [load]);

  if (loading && !roster) {
    return <LoadingTable columns={5} />;
  }
  if (error) {
    return <ErrorState error={error} onRetry={() => void load()} />;
  }
  if (!roster) {
    return null;
  }

  const present = roster.days.filter((entry) => entry.check_in !== null).length;
  const away = roster.days.filter((entry) => entry.on_leave).length;

  return (
    <div className="space-y-4" data-qa-hr-attendance-roster>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <label className="flex items-center gap-2 text-[13px]">
          <span className="text-muted">Day</span>
          <input
            type="date"
            defaultValue={day}
            data-qa-hr-attendance-roster-day
            onChange={(event) => {
              router_push(`/hr/attendance?day=${event.target.value}`);
            }}
            className="h-8 rounded-md border border-line bg-surface px-2 text-[13px]"
          />
        </label>
        <p data-qa-hr-attendance-roster-counts className="text-[12.5px] text-muted">
          {present} in · {away} on leave · {roster.days.length} on the roster
        </p>
      </div>

      {roster.days.length === 0 ? (
        <EmptyState
          title="Nobody is on the roster"
          hint="No employee has a record for this day yet. Punch the clock from My workspace and they appear here."
        />
      ) : (
        <div className="overflow-x-auto rounded-xl border border-line">
          <table className="w-full border-collapse text-[13px]">
            <thead className="border-b border-line text-left text-muted">
              <tr>
                <th className="px-4 py-3 font-medium">Employee</th>
                <th className="px-4 py-3 font-medium">In</th>
                <th className="px-4 py-3 font-medium">Out</th>
                <th className="px-4 py-3 font-medium">Worked</th>
                <th className="px-4 py-3 font-medium">Status</th>
              </tr>
            </thead>
            <tbody>
              {roster.days.map((entry) => (
                <tr
                  key={entry.employee_id}
                  className="border-t border-line"
                  data-qa-hr-attendance-roster-row={entry.employee_name}
                >
                  <td className="px-4 py-3.5 font-medium text-ink">{entry.employee_name}</td>
                  <td className="px-4 py-3.5">{formatPunch(entry.check_in)}</td>
                  <td className="px-4 py-3.5">{formatPunch(entry.check_out)}</td>
                  <td className="px-4 py-3.5">{formatMinutes(entry.minutes_worked)}</td>
                  <td className="px-4 py-3.5">
                    {entry.on_leave ? (
                      <span className="text-muted">On leave</span>
                    ) : (
                      <DayFlag exception={entry.exception} />
                    )}
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

/** `window.location` navigation, kept in one place so the date input's handler stays a line. */
function router_push(href: string) {
  window.location.assign(href);
}

/** The close affordance the correction drawer reuses. */
export { X };
