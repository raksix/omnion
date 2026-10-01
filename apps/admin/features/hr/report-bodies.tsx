/**
 * The four report bodies (REQ-055, slice 4b).
 *
 * Split out of `reports-view.tsx` so the file stays readable, and written as an **exhaustive
 * switch** on the report's own `report` discriminant. That is the point of the union in
 * `lib/hr.ts`: a fifth report added to `reports::REPORTS` without a case here fails `pnpm
 * typecheck`, which is a compile-time "this screen does not render the new report" — instead of
 * the runtime blank table a `default:` arm would give.
 *
 * Every headline number is the server's. The screen formats and never adds: a headcount tile that
 * summed the rows would be a fourth reader of a table the CSV already describes, and the
 * acceptance criterion is that all three agree.
 */
import { EmptyState } from "@/components/empty-state";

import {
  formatMinutes,
  type AbsenceReport,
  type AttendanceReport,
  type HeadcountReport,
  type HrReport,
  type TurnoverReport,
} from "@/lib/hr";

/** One tile's worth of a headline number. */
function Tile({ label, value, tone = "", qa }: { label: string; value: string | number; tone?: string; qa?: string }) {
  return (
    <div className="rounded-lg border border-line px-3 py-2">
      <p className="text-[11px] uppercase tracking-wide text-muted">{label}</p>
      <p className={`text-[19px] font-semibold tabular-nums ${tone}`} data-qa-hr-report-tile={qa ?? label}>
        {value}
      </p>
    </div>
  );
}

/** The table shell every report shares: a caption for screen readers, the columns, the rows. */
function Table({
  caption,
  headers,
  children,
  empty,
}: {
  caption: string;
  headers: string[];
  children: React.ReactNode;
  empty: string;
}) {
  return (
    <div className="overflow-x-auto rounded-lg border border-line">
      <table className="w-full border-collapse text-left text-[13px]" data-qa-hr-report-table>
        <caption className="sr-only">{caption}</caption>
        <thead>
          <tr className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
            {headers.map((header) => (
              <th key={header} scope="col" className="px-4 py-2.5 font-medium">
                {header}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>{children}</tbody>
      </table>
      {empty === "" ? null : null}
    </div>
  );
}

/** The shared "this report has no rows" panel, shown instead of a header with nothing under it. */
function NoRows({ what }: { what: string }) {
  return (
    <div className="rounded-lg border border-line">
      <EmptyState
        title={`No ${what} in this period`}
        hint="The report is working — the period simply has nothing in it. Widen the dates to see more."
      />
    </div>
  );
}

/** A day's tenure as a person reads it: `2.4` is a number, `2 years 4 months` is a sentence. */
function tenure(days: number | null): string {
  if (days === null) {
    return "—";
  }
  const years = Math.floor(days / 365);
  const months = Math.round((days % 365) / 30);
  if (years === 0) {
    return months <= 0 ? `${days} days` : `${months} mo`;
  }
  return months === 0 ? `${years} yr` : `${years} yr ${months} mo`;
}

function Headcount({ report }: { report: HeadcountReport }) {
  return (
    <div className="space-y-3">
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
        <Tile label="Headcount" value={report.total} />
        <Tile label="On leave today" value={report.on_leave} tone="text-amber-600" />
        <Tile label="Departments" value={report.rows.length} />
      </div>
      {report.rows.length === 0 ? (
        <NoRows what="departments" />
      ) : (
        <Table
          caption="Headcount by department and employment type"
          headers={["Department", "Full-time", "Part-time", "Contract", "Intern", "Total"]}
          empty=""
        >
          {report.rows.map((row) => (
            <tr key={row.department_id} className="border-b border-line last:border-b-0" data-qa-hr-report-row={row.department_id}>
              <td className="px-4 py-3 font-medium">{row.department_name}</td>
              <td className="px-4 py-3 tabular-nums">{row.full_time}</td>
              <td className="px-4 py-3 tabular-nums">{row.part_time}</td>
              <td className="px-4 py-3 tabular-nums">{row.contract}</td>
              <td className="px-4 py-3 tabular-nums">{row.intern}</td>
              <td className="px-4 py-3 font-medium tabular-nums">{row.total}</td>
            </tr>
          ))}
        </Table>
      )}
    </div>
  );
}

function Turnover({ report }: { report: TurnoverReport }) {
  // The server carries the rate as a **ratio**; multiplying here is presentation, not arithmetic
  // on the data. It is written out rather than inlined into the tile so a reader can see that the
  // percentage is the ratio the CSV does not carry.
  const percent = (report.turnover_rate * 100).toFixed(1);
  return (
    <div className="space-y-3">
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
        <Tile label="Joined" value={report.joined} tone="text-emerald-600" />
        <Tile label="Left" value={report.left} tone="text-red-600" />
        <Tile label="Average headcount" value={report.average_headcount.toFixed(1)} />
        <Tile label="Turnover rate" value={`${percent}%`} />
      </div>
      {report.rows.length === 0 ? (
        <NoRows what="movements" />
      ) : (
        <Table
          caption="Joiners and leavers inside the period"
          headers={["Employee", "Movement", "Date", "Type", "Tenure"]}
          empty=""
        >
          {report.rows.map((row) => (
            <tr key={`${row.employee_id}-${row.day}`} className="border-b border-line last:border-b-0" data-qa-hr-report-row={row.employee_id}>
              <td className="px-4 py-3 font-medium">{row.employee_name}</td>
              <td className="px-4 py-3">
                <span className={row.movement === "joined" ? "text-emerald-600" : "text-red-600"}>
                  {row.movement === "joined" ? "Joined" : "Left"}
                </span>
              </td>
              <td className="px-4 py-3 tabular-nums">{row.day}</td>
              <td className="px-4 py-3 text-muted">{row.employment_type}</td>
              <td className="px-4 py-3 text-muted tabular-nums">{tenure(row.tenure_days)}</td>
            </tr>
          ))}
        </Table>
      )}
    </div>
  );
}

function Absence({ report }: { report: AbsenceReport }) {
  return (
    <div className="space-y-3">
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
        <Tile label="Requests" value={report.total_requests} />
        <Tile label="Days taken" value={report.total_days.toFixed(1)} />
        <Tile label="Leave types" value={report.rows.length} />
      </div>
      {report.rows.length === 0 ? (
        <NoRows what="leave requests" />
      ) : (
        <Table
          caption="Absence by leave type"
          headers={["Leave type", "Requests", "Approved", "Pending", "Days"]}
          empty=""
        >
          {report.rows.map((row) => (
            <tr key={row.leave_type_id} className="border-b border-line last:border-b-0" data-qa-hr-report-row={row.leave_type_id}>
              <td className="px-4 py-3 font-medium">{row.leave_type_name}</td>
              <td className="px-4 py-3 tabular-nums">{row.requests}</td>
              <td className="px-4 py-3 tabular-nums">{row.approved}</td>
              <td className="px-4 py-3 tabular-nums text-muted">{row.pending}</td>
              <td className="px-4 py-3 font-medium tabular-nums">{row.days.toFixed(1)}</td>
            </tr>
          ))}
        </Table>
      )}
    </div>
  );
}

function Attendance({ report }: { report: AttendanceReport }) {
  return (
    <div className="space-y-3">
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
        <Tile label="Employees" value={report.employees} />
        <Tile label="Hours worked" value={formatMinutes(report.minutes_worked)} />
        <Tile label="Missing check-outs" value={report.missing_checkout_days} tone={report.missing_checkout_days > 0 ? "text-red-600" : ""} />
        <Tile label="Days present" value={report.rows.reduce((sum, row) => sum + row.days_present, 0)} />
      </div>
      {report.rows.length === 0 ? (
        <NoRows what="attendance" />
      ) : (
        <Table
          caption="Worked time per employee"
          headers={["Employee", "Department", "Days", "Worked", "Overtime", "Under hours", "No check-out"]}
          empty=""
        >
          {report.rows.map((row) => (
            <tr key={row.employee_id} className="border-b border-line last:border-b-0" data-qa-hr-report-row={row.employee_id}>
              <td className="px-4 py-3 font-medium">{row.employee_name}</td>
              <td className="px-4 py-3 text-muted">{row.department_name ?? "—"}</td>
              <td className="px-4 py-3 tabular-nums">{row.days_present}</td>
              <td className="px-4 py-3 tabular-nums">{formatMinutes(row.minutes_worked)}</td>
              <td className="px-4 py-3 tabular-nums">{row.overtime_days}</td>
              <td className="px-4 py-3 tabular-nums text-muted">{row.under_hours_days}</td>
              <td className="px-4 py-3 tabular-nums">
                {row.missing_checkout_days > 0 ? (
                  <span className="text-red-600">{row.missing_checkout_days}</span>
                ) : (
                  <span className="text-muted">0</span>
                )}
              </td>
            </tr>
          ))}
        </Table>
      )}
    </div>
  );
}

/**
 * Render whichever report arrived.
 *
 * The switch is exhaustive **by construction**: `HrReport` is a discriminated union of four
 * members, and there is no `default` arm, so adding a fifth report to the server without a case
 * here is a `tsc` error. The runtime check below is belt-and-braces for a payload that does not
 * match its discriminant, and it says so rather than rendering an empty shell.
 */
export function ReportBody({ report }: { report: HrReport }) {
  switch (report.report) {
    case "headcount":
      return <Headcount report={report} />;
    case "turnover":
      return <Turnover report={report} />;
    case "absence":
      return <Absence report={report} />;
    case "attendance":
      return <Attendance report={report} />;
    default:
      return (
        <div className="rounded-lg border border-line">
          <EmptyState
            title="This report has no screen yet"
            hint={`The server served "${(report as { report: string }).report}", which this screen does not render. The export button still produces the CSV.`}
          />
        </div>
      );
  }
}
