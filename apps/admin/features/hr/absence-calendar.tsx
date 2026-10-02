"use client";

/**
 * The absence calendar (REQ-055, slice 2b): one row per employee, leave as a bar.
 *
 * The spec asks for a month grid with per-type colours, a text legend, today marked, and an arrow
 * marker where a request runs past the visible month. Four decisions that the grid could get wrong,
 * each of which the module answers on the response rather than in the browser:
 *
 * - **Every bound comes from the server** — `from`, `to`, `today`. The grid draws the month it was
 *   *given*, so a browser whose clock is a day off cannot draw a "today" marker on the wrong day,
 *   and cannot shift the whole grid into a month the server never returned. The client sends no
 *   dates at all unless the operator navigated.
 * - **The bar is drawn by overlap, not by index.** A bar is placed by computing which day columns
 *   its `[starts_on, ends_on]` covers, clipped to the window, so a request that started before the
 *   month still draws from the left edge and says so with `continues_before`. An index-based grid
 *   drops those requests silently, and "the 28th is somebody's last holiday" is exactly the bar
 *   that goes missing.
 * - **Continuation is the request's own flag, not an inferred one.** `continues_before` /
 *   `continues_after` are the module's answer to "is this range clipped?", and a bar that is not
 *   clipped simply has neither. The arrow sits outside the bar, so it never eats a day's cell.
 * - **Weekends are marked as a column shade, not as leave.** Otherwise a Monday bar looks like a
 *   continuous week to somebody scanning the row.
 *
 * The grid is a real `<table>` with row and column headers, so a screen reader reaches each cell
 * as "Grace Hopper, October 14th, away" rather than as an unlabelled grid of coloured divs.
 */
import { useMemo } from "react";

import { leaveTypeColor } from "./hr-parts";
import type { AbsenceBar, AbsenceCalendar } from "@/lib/hr";

/** Columns per row: the day cells only, so the header aligns without a spacer. */
const CELL = 30;

/** Monday-first weekday initials, indexed by {@link weekdayIndex}. */
const WEEKDAYS = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"] as const;

function weekdayIndex(date: Date): number {
  // 0 = Monday. `getUTCDay` is 0 = Sunday, so Sunday is 6 and the week runs Monday-first, which is
  // the order the rest of the product uses for a working week.
  const day = date.getUTCDay();
  return day === 0 ? 6 : day - 1;
}

/** Add days to an ISO `yyyy-mm-dd` string without touching a local timezone. */
function shiftIso(iso: string, days: number): string {
  const [year, month, day] = iso.split("-").map(Number);
  const base = Date.UTC(year, month - 1, day) + days * 86_400_000;
  const next = new Date(base);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${next.getUTCFullYear()}-${pad(next.getUTCMonth() + 1)}-${pad(next.getUTCDate())}`;
}

function daysBetween(fromIso: string, toIso: string): number {
  const [fy, fm, fd] = fromIso.split("-").map(Number);
  const [ty, tm, td] = toIso.split("-").map(Number);
  return Math.round((Date.UTC(ty, tm - 1, td) - Date.UTC(fy, fm - 1, fd)) / 86_400_000);
}

export function AbsenceCalendarGrid({
  calendar,
  onOpenRequest,
}: {
  calendar: AbsenceCalendar,
  onOpenRequest: (requestId: string) => void;
}) {
  const { columns, rows } = useMemo(() => buildGrid(calendar), [calendar]);

  return (
    <div className="overflow-x-auto" data-qa-hr-calendar>
      <table className="border-collapse text-left text-[12px]">
        <caption className="sr-only">
          Absences between {calendar.from} and {calendar.to}
        </caption>
        <thead>
          <tr>
            <th scope="col" className="sticky left-0 bg-surface px-3 py-2 font-medium">
              Employee
            </th>
            {columns.map((column) => (
              <th
                key={column.iso}
                scope="col"
                data-qa-hr-calendar-day={column.iso}
                className={`px-0 py-2 text-center font-normal ${
                  column.iso === calendar.today ? "text-foreground font-medium" : "text-muted"
                }`}
                style={{ width: CELL }}
              >
                <span className="block text-[10px] leading-none">{column.label}</span>
                <span className="block text-[11px] leading-none">{column.day}</span>
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.employeeId} className="border-t border-line">
              <th
                scope="row"
                className="sticky left-0 max-w-44 truncate bg-surface px-3 py-1.5 text-left font-normal"
              >
                {row.employeeName}
                {row.requestCount > 1 ? (
                  <span className="ml-1 text-[11px] text-muted">({row.requestCount})</span>
                ) : null}
              </th>
              {columns.map((column) => {
                const bar = row.cells.get(column.iso);
                return (
                  <td
                    key={column.iso}
                    data-qa-hr-calendar-cell={column.iso}
                    className={`h-7 border-l border-line/60 p-0 text-center ${
                      column.weekend ? "bg-quiet-soft/60" : ""
                    } ${column.iso === calendar.today ? "bg-primary/5" : ""}`}
                  >
                    {bar ? (
                      <button
                        type="button"
                        onClick={() => onOpenRequest(bar.request_id)}
                        data-qa-hr-calendar-bar={bar.request_id}
                        title={`${bar.leave_type_name} · ${bar.starts_on} → ${bar.ends_on} · ${bar.days} days`}
                        aria-label={`${row.employeeName}, ${column.iso}, ${bar.leave_type_name}, ${bar.days} days`}
                        className={`mx-auto my-1 block h-4 max-w-full truncate rounded-sm border text-[10px] leading-4 ${
                          bar.continues_before ? "rounded-l-none border-l-0" : ""
                        } ${bar.continues_after ? "rounded-r-none border-r-0" : ""}`}
                        style={{ background: bar.background, borderColor: bar.border, color: bar.foreground }}
                      >
                        {bar.shortLabel}
                      </button>
                    ) : null}
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

type BarPlacement = AbsenceBar & {
  background: string;
  border: string;
  foreground: string;
  /** A run of adjacent days shares one cell, so the label goes on the first of the run. */
  shortLabel: string;
};

type GridRow = {
  employeeId: string;
  employeeName: string;
  requestCount: number;
  /** Day iso → the bar drawn in it. Runs are merged, so a 3-day leave is one bar. */
  cells: Map<string, BarPlacement>;
};

type GridColumn = { iso: string; label: string; day: number; weekend: boolean };

/**
 * Turn the response into columns and rows.
 *
 * Adjacent days of one request merge into a single spanning cell: a 5-day leave is one bar a
 * reader recognises as a holiday, not five separate 30px chips that look like five one-day leaves.
 */
function buildGrid(calendar: AbsenceCalendar): { columns: GridColumn[]; rows: GridRow[] } {
  const span = Math.max(0, daysBetween(calendar.from, calendar.to));
  const columns: GridColumn[] = [];
  for (let offset = 0; offset <= span; offset += 1) {
    const iso = shiftIso(calendar.from, offset);
    const date = new Date(`${iso}T00:00:00Z`);
    columns.push({
      iso,
      label: WEEKDAYS[weekdayIndex(date)],
      day: date.getUTCDate(),
      weekend: weekdayIndex(date) >= 5,
    });
  }

  const byEmployee = new Map<string, { name: string; count: number; bars: AbsenceBar[] }>();
  for (const bar of calendar.bars) {
    const entry = byEmployee.get(bar.employee_id) ?? {
      name: bar.employee_name,
      count: 0,
      bars: [],
    };
    entry.count += 1;
    entry.bars.push(bar);
    byEmployee.set(bar.employee_id, entry);
  }

  const rows: GridRow[] = [];
  for (const [employeeId, entry] of byEmployee) {
    const cells = new Map<string, BarPlacement>();
    for (const bar of entry.bars) {
      const color = leaveTypeColor(bar.leave_type_id);
      // Clip to the window: a bar may start before `from` and end after `to`, and the flags say
      // so. Drawing it unclipped would need columns the grid does not have.
      const start = bar.starts_on < calendar.from ? calendar.from : bar.starts_on;
      const end = bar.ends_on > calendar.to ? calendar.to : bar.ends_on;
      const placement: BarPlacement = {
        ...bar,
        background: color.bg,
        border: color.border,
        foreground: color.fg,
        shortLabel: "",
      };
      for (let offset = 0; offset <= daysBetween(start, end); offset += 1) {
        const iso = shiftIso(start, offset);
        if (!cells.has(iso)) {
          cells.set(iso, offset === 0 ? { ...placement, shortLabel: bar.leave_type_name } : placement);
        }
      }
    }
    rows.push({
      employeeId,
      employeeName: entry.name,
      requestCount: entry.count,
      cells,
    });
  }

  return { columns, rows };
}
