"use client";

/**
 * The employee directory (REQ-055, slice 1): `/hr/employees`.
 *
 * The screen the request's slice 1 names first, and the one the module could not be worked
 * without: leave, attendance and onboarding are all rows that point at a person, and a person
 * who does not exist in the panel does not exist for the module.
 *
 * Four things it is careful about:
 *
 * * **No personal contact fields in the list.** The request's risk note names them, and the
 *   server drops them from the response for anyone without `hr.employees.sensitive.read`. This
 *   list therefore prints work contact only — the work address is a company directory field, and
 *   the four personal ones belong on the detail screen, behind the same permission. A directory
 *   that rendered them grey would still have put them in the DOM and in every screenshot.
 * * **The counts are the server's.** `total_estimate` is the query's own count, not
 *   `rows.length`, so "showing 100 of 312" is what the database said and not what the page
 *   happened to load.
 * * **Filters live in the URL.** A filtered directory that forgets its filter on reload is a
 *   directory nobody can share with the person they are talking to.
 * * **A terminated employee stays in the list**, badged, rather than vanishing: "who left and
 *   when" is a question the screen exists to answer, and a row that disappears on termination
 *   cannot answer it.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { Loader2, Pencil, UserPlus, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  EMPLOYEE_STATUSES,
  EMPLOYMENT_TYPES,
  createEmployee,
  fetchDepartments,
  fetchEmployees,
  suggestEmployeeNumber,
  type Department,
  type Employee,
  type EmployeeFilters,
} from "@/lib/hr";

import {
  DepartmentChip,
  EmployeeAvatar,
  EmployeeStatusBadge,
  EmploymentTypeBadge,
  employeeName,
} from "./hr-people-parts";

const COLUMNS = 7;
const PAGE_SIZE = 50;

/** The status chips, reading the module's own vocabulary rather than raw values. */
const STATUS_FILTERS = EMPLOYEE_STATUSES.map((status) => ({ value: status.value, label: status.label }));

export function EmployeeDirectoryView() {
  const router = useRouter();
  const params = useSearchParams();

  const [search, setSearch] = useState(params.get("search") ?? "");
  const [departmentId, setDepartmentId] = useState(params.get("department") ?? "");
  const [status, setStatus] = useState(params.get("status") ?? "");
  const [employmentType, setEmploymentType] = useState(params.get("type") ?? "");
  const [rows, setRows] = useState<Employee[]>([]);
  const [total, setTotal] = useState(0);
  const [departments, setDepartments] = useState<Department[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [formError, setFormError] = useState<ScreenErrorValue>(null);
  const [creating, setCreating] = useState(false);
  const [saving, setSaving] = useState(false);

  // The form lives in the drawer rather than on its own route for one reason the request's QA plan
  // makes concrete: it names `/hr/employees/new` as a screen, and a route that renders is a
  // screen the harness can visit on its own. The drawer is the *editing* surface; the route is
  // what a bookmark points at.
  const [draft, setDraft] = useState<{
    employee_no: string;
    first_name: string;
    last_name: string;
    work_email: string;
    position: string;
    department_id: string;
    manager_id: string;
    employment_type: string;
    start_date: string;
    location: string;
  } | null>(null);

  const filters = useMemo<EmployeeFilters>(
    () => ({
      search: search.trim() || undefined,
      department_id: departmentId || undefined,
      include_subdepartments: departmentId ? true : undefined,
      status: status || undefined,
      employment_type: employmentType || undefined,
      limit: PAGE_SIZE,
    }),
    [search, departmentId, status, employmentType],
  );

  // The filter bar writes the URL rather than only its own state: a shared link to "active people
  // in Engineering" is the difference between a directory and a search result.
  useEffect(() => {
    const next = new URLSearchParams();
    if (search.trim()) next.set("search", search.trim());
    if (departmentId) next.set("department", departmentId);
    if (status) next.set("status", status);
    if (employmentType) next.set("type", employmentType);
    const suffix = next.toString();
    router.replace(suffix ? `/hr/employees?${suffix}` : "/hr/employees", { scroll: false });
  }, [search, departmentId, status, employmentType, router]);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [page, tree] = await Promise.all([fetchEmployees(filters), fetchDepartments()]);
      setRows(page.items);
      setTotal(page.total_estimate);
      setDepartments(tree.items);
    } catch (caught) {
      setError(toScreenError(caught, "The employee directory could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [filters]);

  useEffect(() => {
    void load();
  }, [load]);

  const startCreate = useCallback(async () => {
    setFormError(null);
    // The number arrives from the server so the form never guesses one and collides: the
    // suggestion is the same rule the store enforces on write.
    let employeeNo = "";
    try {
      const suggested = await suggestEmployeeNumber();
      employeeNo = suggested.employee_no;
    } catch {
      // A form with an empty number still works — the server suggests one on create. Failing to
      // load a *suggestion* is not a reason to refuse to open a form.
    }
    setDraft({
      employee_no: employeeNo,
      first_name: "",
      last_name: "",
      work_email: "",
      position: "",
      department_id: departmentId || departments[0]?.id || "",
      manager_id: "",
      employment_type: "full_time",
      start_date: new Date().toISOString().slice(0, 10),
      location: "",
    });
  }, [departmentId, departments]);

  const field = useCallback(<K extends keyof NonNullable<typeof draft>>(key: K, value: string) => {
    setDraft((current) => (current ? { ...current, [key]: value } : current));
  }, []);

  async function submit() {
    if (!draft) {
      return;
    }
    setSaving(true);
    setFormError(null);
    try {
      const created = await createEmployee({
        employee_no: draft.employee_no.trim() || undefined,
        first_name: draft.first_name.trim(),
        last_name: draft.last_name.trim(),
        work_email: draft.work_email.trim(),
        position: draft.position.trim(),
        department_id: draft.department_id,
        manager_id: draft.manager_id || undefined,
        employment_type: draft.employment_type,
        start_date: draft.start_date || undefined,
        location: draft.location.trim() || undefined,
      });
      setNotice(`${employeeName(created)} was added as ${created.employee_no}.`);
      setDraft(null);
      void load();
    } catch (caught) {
      // Field-level refusals carry `details.field`, and the panel's shared error reader turns
      // that into a message the form can attach to the input it belongs to.
      setFormError(toScreenError(caught, "That employee could not be saved."));
    } finally {
      setSaving(false);
    }
  }

  const filtered = rows.length > 0 && total > rows.length;

  return (
    <div className="flex flex-col gap-4">
      <header className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h2 className="text-[15px] font-semibold">Employees</h2>
          <p className="text-[12.5px] text-muted">
            Everyone in the directory, with the department and the person they report to.
          </p>
        </div>
        <button
          type="button"
          data-qa-hr-employees-new
          onClick={() => void startCreate()}
          className="inline-flex items-center gap-1.5 rounded border px-3 py-1.5 text-[13px]"
        >
          <UserPlus aria-hidden className="h-3.5 w-3.5" />
          Add employee
        </button>
      </header>

      <div className="flex flex-wrap items-end gap-2" data-qa-hr-employees-filters>
        <label className="text-[12px] font-medium">
          Search
          <input
            data-qa-hr-employees-search
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Name, number or work e-mail"
            className="mt-1 w-56 rounded border px-2 py-1.5 text-[13px]"
          />
        </label>
        <label className="text-[12px] font-medium">
          Department
          <select
            data-qa-hr-employees-department
            value={departmentId}
            onChange={(event) => setDepartmentId(event.target.value)}
            className="mt-1 rounded border px-2 py-1.5 text-[13px]"
          >
            <option value="">All departments</option>
            {departments.map((department) => (
              <option key={department.id} value={department.id}>
                {department.name} ({department.member_count})
              </option>
            ))}
          </select>
        </label>
        <label className="text-[12px] font-medium">
          Status
          <select
            data-qa-hr-employees-status
            value={status}
            onChange={(event) => setStatus(event.target.value)}
            className="mt-1 rounded border px-2 py-1.5 text-[13px]"
          >
            <option value="">Any status</option>
            {STATUS_FILTERS.map((entry) => (
              <option key={entry.value} value={entry.value}>
                {entry.label}
              </option>
            ))}
          </select>
        </label>
        <label className="text-[12px] font-medium">
          Type
          <select
            data-qa-hr-employees-type
            value={employmentType}
            onChange={(event) => setEmploymentType(event.target.value)}
            className="mt-1 rounded border px-2 py-1.5 text-[13px]"
          >
            <option value="">Any type</option>
            {EMPLOYMENT_TYPES.map((entry) => (
              <option key={entry.value} value={entry.value}>
                {entry.label}
              </option>
            ))}
          </select>
        </label>
        {search || departmentId || status || employmentType ? (
          <button
            type="button"
            data-qa-hr-employees-clear
            onClick={() => {
              setSearch("");
              setDepartmentId("");
              setStatus("");
              setEmploymentType("");
            }}
            className="inline-flex items-center gap-1 rounded px-2 py-1.5 text-[12.5px] text-muted hover:text-ink"
          >
            <X aria-hidden className="h-3 w-3" />
            Clear
          </button>
        ) : null}
      </div>

      {notice ? (
        <p role="status" data-qa-hr-employees-notice className="text-[12.5px] text-positive">
          {notice}
        </p>
      ) : null}

      {error ? (
        <ErrorState error={error} onRetry={() => void load()} />
      ) : loading ? (
        <LoadingTable columns={COLUMNS} />
      ) : rows.length === 0 ? (
        <EmptyState
          title="No employees yet"
          hint={
            search || departmentId || status || employmentType
              ? "No one matches these filters. Clear them to see the whole directory."
              : "Add the first person and the department tree, the leave requests and the attendance clock all have something to point at."
          }
          action={
            search || departmentId || status || employmentType ? null : (
              <button
                type="button"
                onClick={() => void startCreate()}
                className="inline-flex items-center gap-1.5 rounded border px-3 py-1.5 text-[13px]"
              >
                <UserPlus aria-hidden className="h-3.5 w-3.5" />
                Add employee
              </button>
            )
          }
        />
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-[13px]" data-qa-hr-employees-table>
            <thead>
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                <th scope="col" className="px-4 py-2.5">Employee</th>
                <th scope="col" className="px-4 py-2.5">Number</th>
                <th scope="col" className="px-4 py-2.5">Department</th>
                <th scope="col" className="px-4 py-2.5">Manager</th>
                <th scope="col" className="px-4 py-2.5">Type</th>
                <th scope="col" className="px-4 py-2.5">Started</th>
                <th scope="col" className="px-4 py-2.5">Status</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((employee) => (
                <tr key={employee.id} className="border-t border-line">
                  <td className="px-4 py-3">
                    <Link
                      href={`/hr/employees/${employee.id}`}
                      data-qa-hr-employee-row={employee.employee_no}
                      className="flex min-w-0 items-center gap-2.5 hover:underline"
                    >
                      <EmployeeAvatar employee={employee} />
                      <span className="flex min-w-0 flex-col">
                        <span className="truncate font-medium text-ink">{employeeName(employee)}</span>
                        <span className="truncate text-[11.5px] text-muted">{employee.work_email}</span>
                      </span>
                    </Link>
                  </td>
                  <td className="px-4 py-3 text-muted">{employee.employee_no}</td>
                  <td className="px-4 py-3">
                    <span className="inline-flex items-center rounded-md bg-quiet-soft px-1.5 py-0.5 text-[11.5px] text-ink">
                      {employee.department_name}
                    </span>
                  </td>
                  <td className="px-4 py-3 text-muted">{employee.manager_name ?? "—"}</td>
                  <td className="px-4 py-3">
                    <EmploymentTypeBadge value={employee.employment_type} />
                  </td>
                  <td className="px-4 py-3 text-muted">{employee.start_date}</td>
                  <td className="px-4 py-3">
                    <EmployeeStatusBadge status={employee.employee_status} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          <p data-qa-hr-employees-count className="px-4 py-2.5 text-[12px] text-muted">
            {filtered
              ? `Showing ${rows.length} of ${total}. Narrow the filters to see the rest.`
              : `${total} ${total === 1 ? "employee" : "employees"}.`}
          </p>
        </div>
      )}

      {draft ? (
        <div
          role="dialog"
          aria-modal="true"
          aria-label="Add employee"
          data-qa-hr-employee-form
          className="fixed inset-0 z-50 flex justify-end bg-black/30"
        >
          <div className="flex h-full w-full max-w-md flex-col gap-3 overflow-y-auto bg-panel p-5 shadow-xl">
            <div className="flex items-center justify-between">
              <h3 className="text-[15px] font-semibold">Add employee</h3>
              <button
                type="button"
                onClick={() => setDraft(null)}
                aria-label="Close"
                className="rounded p-1 text-muted hover:text-ink"
              >
                <X aria-hidden className="h-4 w-4" />
              </button>
            </div>

            {formError ? (
              <ErrorState error={formError} onRetry={() => void submit()} qa="hr-employee-form-error" />
            ) : null}

            <div className="grid grid-cols-2 gap-3">
              <label className="text-[12px] font-medium">
                First name
                <input
                  data-qa-hr-employee-first-name
                  value={draft.first_name}
                  onChange={(event) => field("first_name", event.target.value)}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                />
              </label>
              <label className="text-[12px] font-medium">
                Last name
                <input
                  data-qa-hr-employee-last-name
                  value={draft.last_name}
                  onChange={(event) => field("last_name", event.target.value)}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                />
              </label>
            </div>

            <label className="text-[12px] font-medium">
              Work e-mail
              <input
                data-qa-hr-employee-work-email
                type="email"
                value={draft.work_email}
                onChange={(event) => field("work_email", event.target.value)}
                className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
              />
            </label>

            <div className="grid grid-cols-2 gap-3">
              <label className="text-[12px] font-medium">
                Position
                <input
                  data-qa-hr-employee-position
                  value={draft.position}
                  onChange={(event) => field("position", event.target.value)}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                />
              </label>
              <label className="text-[12px] font-medium">
                Employee number
                <input
                  data-qa-hr-employee-number
                  value={draft.employee_no}
                  onChange={(event) => field("employee_no", event.target.value)}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                />
              </label>
            </div>

            <label className="text-[12px] font-medium">
              Department
              <select
                data-qa-hr-employee-department
                value={draft.department_id}
                onChange={(event) => field("department_id", event.target.value)}
                className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
              >
                {departments.map((department) => (
                  <option key={department.id} value={department.id}>
                    {department.name}
                  </option>
                ))}
              </select>
            </label>

            <div className="grid grid-cols-2 gap-3">
              <label className="text-[12px] font-medium">
                Reports to
                <select
                  data-qa-hr-employee-manager
                  value={draft.manager_id}
                  onChange={(event) => field("manager_id", event.target.value)}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                >
                  <option value="">Nobody</option>
                  {rows.map((employee) => (
                    <option key={employee.id} value={employee.id}>
                      {employeeName(employee)}
                    </option>
                  ))}
                </select>
              </label>
              <label className="text-[12px] font-medium">
                Employment type
                <select
                  data-qa-hr-employee-type
                  value={draft.employment_type}
                  onChange={(event) => field("employment_type", event.target.value)}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                >
                  {EMPLOYMENT_TYPES.map((entry) => (
                    <option key={entry.value} value={entry.value}>
                      {entry.label}
                    </option>
                  ))}
                </select>
              </label>
            </div>

            <div className="grid grid-cols-2 gap-3">
              <label className="text-[12px] font-medium">
                Start date
                <input
                  data-qa-hr-employee-start
                  type="date"
                  value={draft.start_date}
                  onChange={(event) => field("start_date", event.target.value)}
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                />
              </label>
              <label className="text-[12px] font-medium">
                Location
                <input
                  data-qa-hr-employee-location
                  value={draft.location}
                  onChange={(event) => field("location", event.target.value)}
                  placeholder="Office or city"
                  className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
                />
              </label>
            </div>

            <div className="mt-auto flex justify-end gap-2 pt-2">
              <button
                type="button"
                onClick={() => setDraft(null)}
                className="rounded border px-3 py-1.5 text-[13px]"
              >
                Cancel
              </button>
              <button
                type="button"
                data-qa-hr-employee-submit
                onClick={() => void submit()}
                disabled={saving}
                className="inline-flex items-center gap-1.5 rounded border border-transparent bg-ink px-3 py-1.5 text-[13px] text-panel disabled:opacity-50"
              >
                {saving ? <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" /> : null}
                <Pencil aria-hidden className="h-3.5 w-3.5" />
                Add employee
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}