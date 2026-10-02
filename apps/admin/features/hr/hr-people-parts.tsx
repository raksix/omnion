"use client";

/**
 * The people core's shared pieces (REQ-055, slice 1).
 *
 * These exist because the directory, the department tree and the org chart all print the *same*
 * person, and three copies of "Ada Lovelace" spelled three ways is three ways to be wrong about
 * whose row you are looking at.
 *
 * - **`EmployeeAvatar`** takes initials derived from the name on purpose rather than a stored
 *   image: the request's risk note forbids personal contact details in list views and screenshots,
 *   and a photograph is exactly that. A deterministic colour hashed from the employee id keeps a
 *   person the same colour in the list, the tree and the chart — an array-index colour would
 *   repaint itself whenever the sort order changed.
 * - **`EmployeeStatusBadge`** adds the two HR statuses the platform badge has no tone for
 *   (`on_leave`, `terminated`) rather than letting them fall through to a neutral grey, because
 *   "on leave" is a state an operator triages on and grey is the tone for "no longer relevant".
 * - **`DepartmentChip`** prints the department with its member count, which is the number the
 *   department list prints; deriving it in one place is what makes the two agree.
 */
import { Users } from "lucide-react";

import { StatusBadge } from "@/components/status-badge";

import type { Department, Employee } from "@/lib/hr";

/** The HR statuses the shared badge has no tone for. */
const EXTRA_TONES: Record<string, string> = {
  on_leave: "bg-caution-soft text-caution",
  terminated: "bg-quiet-soft text-muted",
};

/** The employment type as the directory and the form both print it. */
const TYPE_LABELS: Record<string, string> = {
  full_time: "Full time",
  part_time: "Part time",
  contract: "Contract",
  intern: "Intern",
};

/** A person's name as every HR surface prints it: given name first. */
export function employeeName(employee: {
  first_name: string;
  last_name: string;
}): string {
  return `${employee.first_name} ${employee.last_name}`.trim();
}

/** Up to two initials, taken from the given name and the family name. */
export function initialsOf(first: string, last: string): string {
  const firstInitial = first.trim().charAt(0);
  const lastInitial = last.trim().charAt(0);
  const value = `${firstInitial}${lastInitial}`.toUpperCase();
  return value || "?";
}

/**
 * A stable colour from a uuid.
 *
 * The hash is over the **id**, so the same person is the same colour in every screen and after
 * every reload; the alternative — an index into a palette — repaints whenever a filter or a sort
 * changes, and a directory whose people change colour on every sort reads as broken.
 */
function colorFor(id: string): { fg: string; bg: string } {
  let seed = 0;
  for (let index = 0; index < id.length; index += 1) {
    seed = (seed * 31 + id.charCodeAt(index)) >>> 0;
  }
  const hue = seed % 360;
  return { fg: `hsl(${hue} 55% 32%)`, bg: `hsl(${hue} 62% 93%)` };
}

/** The size ladder the avatar is printed at across the three screens. */
const SIZES = {
  sm: "h-7 w-7 text-[10.5px]",
  md: "h-9 w-9 text-[12px]",
  lg: "h-11 w-11 text-[14px]",
} as const;

/**
 * A person's initials in a coloured disc.
 *
 * `aria-hidden`, because the name is always printed next to it — announcing the initials as well
 * makes a screen reader say "A L, Ada Lovelace", which is worse than not announcing them.
 */
export function EmployeeAvatar({
  employee,
  size = "md",
}: {
  employee: { id: string; first_name: string; last_name: string };
  size?: keyof typeof SIZES;
}) {
  const { fg, bg } = colorFor(employee.id);
  return (
    <span
      aria-hidden
      data-qa-hr-avatar
      style={{ color: fg, backgroundColor: bg }}
      className={`inline-flex shrink-0 items-center justify-center rounded-full font-semibold ${SIZES[size]}`}
    >
      {initialsOf(employee.first_name, employee.last_name)}
    </span>
  );
}

/** The lifecycle badge, with the two HR-only tones filled in. */
export function EmployeeStatusBadge({ status }: { status: string }) {
  if (EXTRA_TONES[status]) {
    return (
      <span
        data-qa-hr-status={status}
        className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${EXTRA_TONES[status]}`}
      >
        {status === "on_leave" ? "On leave" : "Terminated"}
      </span>
    );
  }
  return (
    <span data-qa-hr-status={status}>
      <StatusBadge status={status} />
    </span>
  );
}

/** The employment type as a quiet pill, reading the module's own vocabulary. */
export function EmploymentTypeBadge({ value }: { value: string }) {
  return (
    <span className="inline-flex items-center rounded-md bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
      {TYPE_LABELS[value] ?? value}
    </span>
  );
}

/**
 * A department chip carrying its member count.
 *
 * The count is the same field the department screen prints (`member_count`), so the directory and
 * the tree cannot disagree — a chip that re-counted the rows on screen would say 4 beside a tree
 * that says 5, and the reader would have no way to tell which one is the truth.
 */
export function DepartmentChip({ department }: { department: Department }) {
  return (
    <span
      data-qa-hr-department-chip={department.name}
      className="inline-flex items-center gap-1 rounded-md bg-quiet-soft px-1.5 py-0.5 text-[11.5px] text-ink"
    >
      <Users className="h-3 w-3 text-muted" aria-hidden />
      {department.name}
      <span className="text-muted">({department.member_count})</span>
    </span>
  );
}

/**
 * The sensitive-field block, or the honest reason it is absent.
 *
 * A caller without `hr.employees.sensitive.read` gets the four personal fields **dropped from the
 * response entirely**. This prints "not available for your role" rather than an em dash, because
 * an em dash reads as "this person has no phone number" — and it is precisely the person most
 * likely to need one recorded that a blank row misinforms somebody about.
 */
export function SensitiveField({ label, value }: { label: string; value: string | undefined }) {
  return (
    <div className="flex flex-col gap-0.5">
      <dt className="text-[11.5px] uppercase tracking-wide text-muted">{label}</dt>
      <dd data-qa-hr-sensitive={label} className="text-[13px]">
        {value ? value : <span className="text-muted">Not available for your role</span>}
      </dd>
    </div>
  );
}

/** One employee as the directory and the chart link to them. */
export function EmployeeRef({ employee }: { employee: Pick<Employee, "id" | "first_name" | "last_name" | "position"> }) {
  return (
    <span className="flex min-w-0 flex-col">
      <span className="truncate text-[13px] font-medium text-ink">{employeeName(employee)}</span>
      <span className="truncate text-[11.5px] text-muted">{employee.position}</span>
    </span>
  );
}