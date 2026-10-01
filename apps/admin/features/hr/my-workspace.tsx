"use client";

/**
 * `/hr/me` and `/hr/me/leave` — the caller's own HR record (REQ-055, slice 2c).
 *
 * The one surface in this module with **no permission requirement**, and the screen is built
 * around what that means for the person using it. Every other HR screen assumes a person who
 * administers leave for others; this one is opened by an employee who wants to know three things:
 * am I still employed here, how much holiday is left, and where is my request.
 *
 * Three decisions the screen could get wrong:
 *
 * - **The unlinked account is a state, not an error.** An account with no employee row gets a
 *   `404` from the server, and this screen renders that as "you are not in the directory yet" with
 *   a retry — not as a crash, and not as an empty profile card. An empty card would put an
 *   "edit" button on a row that does not exist, which is the dead-button pattern the module's own
 *   rules forbid.
 * - **The balance and the list arrive together.** `/hr/me/leave` is one endpoint precisely so the
 *   card saying "8 days remaining" and the list of requests that produced it cannot be fetched a
 *   minute apart and disagree. This screen does not re-fetch one without the other.
 * - **The year is a control, not a hidden default.** Entitlement is yearly, so a person checking
 *   last year's card needs the switch to exist. It reads the URL so the year survives a reload
 *   and a shared link.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { CalendarDays, FileText, Palmtree, Plus, User } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  cancelMyLeaveRequest,
  fetchMyDocuments,
  fetchMyLeave,
  fetchMyProfile,
  type MyDocument,
  type MyLeave,
  type MyProfile,
} from "@/lib/hr";

import { BalanceCardView, LeaveStatusBadge } from "./hr-parts";

/** The self-service shelf. Separate from the HR module nav, because it is a different audience. */
function MyWorkspaceNav() {
  return (
    <nav
      aria-label="My workspace"
      data-qa-hr-me-nav
      className="flex flex-wrap items-center gap-1 border-b border-border pb-2"
    >
      <Link
        href="/hr/me"
        data-qa-hr-me-link="profile"
        className="inline-flex h-8 items-center gap-1.5 rounded-md bg-muted px-2.5 text-sm font-medium text-foreground"
      >
        <User className="h-4 w-4" aria-hidden />
        My profile
      </Link>
      <Link
        href="/hr/me/leave"
        data-qa-hr-me-link="leave"
        className="inline-flex h-8 items-center gap-1.5 rounded-md px-2.5 text-sm text-muted-foreground hover:text-foreground"
      >
        <Palmtree className="h-4 w-4" aria-hidden />
        My leave
      </Link>
      <Link
        href="/hr/me/documents"
        data-qa-hr-me-link="documents"
        className="inline-flex h-8 items-center gap-1.5 rounded-md px-2.5 text-sm text-muted-foreground hover:text-foreground"
      >
        <FileText className="h-4 w-4" aria-hidden />
        My documents
      </Link>
    </nav>
  );
}

/** "Active", "On leave", "Terminated" — with text, never colour alone. */
function EmploymentBadge({ status }: { status: string }) {
  const label =
    status === "active" ? "Active" : status === "on_leave" ? "On leave" : "Terminated";
  const tone =
    status === "active"
      ? "bg-emerald-50 text-emerald-700 border-emerald-200"
      : status === "on_leave"
        ? "bg-amber-50 text-amber-700 border-amber-200"
        : "bg-muted text-muted-foreground border-border";
  return (
    <span
      data-qa-hr-me-status
      className={`inline-flex items-center rounded-full border px-2 py-0.5 text-xs font-medium ${tone}`}
    >
      {label}
    </span>
  );
}

/** A labelled pair. A definition list rather than a grid of divs, so it reads as data. */
function Field({ label, value }: { label: string; value: string | null | undefined }) {
  return (
    <div className="min-w-0">
      <dt className="text-xs text-muted">{label}</dt>
      <dd className="truncate text-sm text-foreground" title={value ?? undefined}>
        {value && value.trim().length > 0 ? value : <span className="text-muted">Not set</span>}
      </dd>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// The profile
// ---------------------------------------------------------------------------------------------

/** The caller's own record. */
export function MyProfileView() {
  const [profile, setProfile] = useState<MyProfile | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setProfile(await fetchMyProfile());
    } catch (failure) {
      setError(toScreenError(failure, "Your employee record could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  if (loading) {
    return (
      <div data-qa-hr-me-profile>
        <LoadingTable rows={4} columns={3} />
      </div>
    );
  }
  if (error) {
    return <ErrorState error={error} onRetry={load} qa="hr-me-profile-error" />;
  }
  if (!profile) {
    return (
      <EmptyState
        title="No employee record"
        hint="Your account is not linked to an employee record yet. An HR administrator can link it from the employees screen."
      />
    );
  }

  return (
    <div data-qa-hr-me-profile className="space-y-4">
      <header className="flex flex-wrap items-center gap-3">
        <h2 className="text-lg font-semibold">
          {profile.first_name} {profile.last_name}
        </h2>
        <EmploymentBadge status={profile.employee_status} />
        <span className="text-sm text-muted">{profile.position}</span>
      </header>

      <dl className="grid grid-cols-1 gap-x-6 gap-y-3 rounded-lg border border-border p-4 sm:grid-cols-2 lg:grid-cols-3">
        <Field label="Employee number" value={profile.employee_no} />
        <Field label="Work e-mail" value={profile.work_email} />
        <Field label="Phone" value={profile.phone} />
        <Field label="Department" value={profile.department} />
        <Field label="Reports to" value={profile.manager_name} />
        <Field label="Employment type" value={profile.employment_type.replace(/_/g, " ")} />
        <Field label="Start date" value={profile.start_date} />
        <Field label="End date" value={profile.end_date} />
        <Field label="Location" value={profile.location} />
      </dl>

      {/*
        The personal block. The request's risk note keeps these out of the *directory*, and the
        directory is exactly where this screen is not — it is the person's own record, and an
        employee who cannot read their own home number cannot correct it.
      */}
      <section className="rounded-lg border border-border p-4">
        <h3 className="mb-3 text-sm font-medium">Personal details</h3>
        <dl className="grid grid-cols-1 gap-x-6 gap-y-3 sm:grid-cols-2">
          <Field label="Personal e-mail" value={profile.personal_email} />
          <Field label="Personal phone" value={profile.personal_phone} />
          <Field label="Address" value={profile.address} />
          <Field label="Emergency contact" value={profile.emergency_contact} />
        </dl>
      </section>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// The leave
// ---------------------------------------------------------------------------------------------

/** The caller's own balances and requests, with the request form's entry point. */
export function MyLeaveView() {
  const router = useRouter();
  const params = useSearchParams();
  const yearParam = Number(params.get("year"));
  const year = Number.isInteger(yearParam) && yearParam > 2000 && yearParam < 2200
    ? yearParam
    : new Date().getFullYear();

  const [leave, setLeave] = useState<MyLeave | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [cancelling, setCancelling] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setLeave(await fetchMyLeave(year));
    } catch (failure) {
      setError(toScreenError(failure, "Your leave could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [year]);

  useEffect(() => {
    void load();
  }, [load]);

  // Withdrawing is a POST, so it needs the same care as any other mutation on this panel: the row
  // is disabled while it runs, and a failure leaves the list exactly as it was rather than
  // optimistically removing a request the server may have refused.
  const withdraw = useCallback(
    async (id: string) => {
      setCancelling(id);
      try {
        await cancelMyLeaveRequest(id);
        await load();
      } catch (failure) {
        setError(toScreenError(failure, "That request could not be withdrawn."));
      } finally {
        setCancelling(null);
      }
    },
    [load],
  );

  const shiftYear = (delta: number) => {
    router.push(`/hr/me/leave?year=${year + delta}`);
  };

  if (loading) {
    return (
      <div data-qa-hr-me-leave>
        <LoadingTable rows={3} columns={2} />
      </div>
    );
  }
  if (error) {
    return <ErrorState error={error} onRetry={load} qa="hr-me-leave-error" />;
  }
  if (!leave) {
    return <EmptyState title="No leave record" hint="Your account has no employee record yet." />;
  }

  return (
    <div data-qa-hr-me-leave className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <CalendarDays className="h-4 w-4 text-muted" aria-hidden />
          <span className="text-sm font-medium" data-qa-hr-me-leave-year>
            {leave.year}
          </span>
        </div>
        <div className="flex items-center gap-1">
          <button
            type="button"
            onClick={() => shiftYear(-1)}
            data-qa-hr-me-year-prev
            className="h-8 rounded-md border border-border px-2.5 text-sm hover:bg-muted"
          >
            {year - 1}
          </button>
          <button
            type="button"
            onClick={() => shiftYear(1)}
            data-qa-hr-me-year-next
            className="h-8 rounded-md border border-border px-2.5 text-sm hover:bg-muted"
          >
            {year + 1}
          </button>
        </div>
      </div>

      <section aria-labelledby="my-balances" className="space-y-2">
        <h3 id="my-balances" className="text-sm font-medium">
          Balances
        </h3>
        {leave.balances.length === 0 ? (
          <EmptyState
            title="No leave types yet"
            hint="Your organization has not published a leave type catalogue yet."
          />
        ) : (
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {leave.balances.map((card) => (
              // `card.id`, not `card.leave_type.id`: the server flattens the type into the card
              // (`#[serde(flatten)]`), so the wire carries `id` and `name` at the top level. A key
              // built from the nested name would be `undefined` on every card — which still
              // *matches* React's key check as a string, so the list would render and the warning
              // would be the only symptom. Reading the flattened shape is also what
              // `BalanceCardView` already does with `card.name`.
              <BalanceCardView key={card.id} card={card} />
            ))}
          </div>
        )}
      </section>

      <section aria-labelledby="my-requests" className="space-y-2">
        <div className="flex items-center justify-between gap-2">
          <h3 id="my-requests" className="text-sm font-medium">
            My requests
          </h3>
          <Link
            href="/hr/me/leave/new"
            data-qa-hr-me-leave-new
            className="inline-flex h-8 items-center gap-1.5 rounded-md border border-border px-2.5 text-sm hover:bg-muted"
          >
            <Plus className="h-4 w-4" aria-hidden />
            Request leave
          </Link>
        </div>

        {leave.requests.length === 0 ? (
          <EmptyState
            title="No requests this year"
            hint="When you ask for leave it appears here with its status and the approver's comment."
          />
        ) : (
          <div className="overflow-x-auto rounded-lg border border-border">
            <table className="w-full text-sm">
              <caption className="sr-only">Your leave requests for {leave.year}</caption>
              <thead>
                <tr className="border-b border-border text-left text-xs text-muted">
                  <th scope="col" className="px-3 py-2 font-medium">Type</th>
                  <th scope="col" className="px-3 py-2 font-medium">Dates</th>
                  <th scope="col" className="px-3 py-2 font-medium">Days</th>
                  <th scope="col" className="px-3 py-2 font-medium">Status</th>
                  <th scope="col" className="px-3 py-2 font-medium">
                    <span className="sr-only">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {leave.requests.map((request) => (
                  <tr key={request.id} className="border-b border-border last:border-0">
                    <td className="px-3 py-2">{request.leave_type_name}</td>
                    <td className="px-3 py-2 whitespace-nowrap">
                      {request.starts_on} → {request.ends_on}
                    </td>
                    <td className="px-3 py-2">{request.days}</td>
                    <td className="px-3 py-2">
                      <LeaveStatusBadge status={request.leave_status} />
                    </td>
                    <td className="px-3 py-2 text-right">
                      {/*
                        Only a **pending** request can be withdrawn, and the condition is read from
                        the status rather than from a "can I?" flag the server would have to
                        maintain. An approved request is history: pretending it was never agreed is
                        not something the employee gets to do, and the server refuses it anyway.
                      */}
                      {request.leave_status === "pending" ? (
                        <button
                          type="button"
                          onClick={() => withdraw(request.id)}
                          disabled={cancelling === request.id}
                          data-qa-hr-me-withdraw={request.id}
                          className="h-7 rounded-md border border-border px-2 text-xs hover:bg-muted disabled:opacity-50"
                        >
                          {cancelling === request.id ? "Withdrawing…" : "Withdraw"}
                        </button>
                      ) : null}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {leave.total > leave.requests.length ? (
              <p className="px-3 py-2 text-xs text-muted" data-qa-hr-me-leave-total>
                Showing {leave.requests.length} of {leave.total}.
              </p>
            ) : null}
          </div>
        )}
      </section>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// The documents
// ---------------------------------------------------------------------------------------------

/** The caller's own documents, with the expiry the request cares about. */
export function MyDocumentsView() {
  const [documents, setDocuments] = useState<MyDocument[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const answer = await fetchMyDocuments();
      setDocuments(answer.items);
    } catch (failure) {
      setError(toScreenError(failure, "Your documents could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  if (loading) {
    return (
      <div data-qa-hr-me-documents>
        <LoadingTable rows={3} columns={2} />
      </div>
    );
  }
  if (error) {
    return <ErrorState error={error} onRetry={load} qa="hr-me-documents-error" />;
  }
  if (documents.length === 0) {
    return (
      <EmptyState
        title="No documents yet"
        hint="Contracts and certificates uploaded by HR appear here, with any expiry date."
      />
    );
  }

  return (
    <div data-qa-hr-me-documents className="overflow-x-auto rounded-lg border border-border">
      <table className="w-full text-sm">
        <caption className="sr-only">Your documents</caption>
        <thead>
          <tr className="border-b border-border text-left text-xs text-muted">
            <th scope="col" className="px-3 py-2 font-medium">Title</th>
            <th scope="col" className="px-3 py-2 font-medium">Kind</th>
            <th scope="col" className="px-3 py-2 font-medium">Expires</th>
          </tr>
        </thead>
        <tbody>
          {documents.map((document) => (
            <tr key={document.id} className="border-b border-border last:border-0">
              <td className="px-3 py-2">{document.title}</td>
              <td className="px-3 py-2">{document.kind.replace(/_/g, " ")}</td>
              <td className="px-3 py-2 whitespace-nowrap">
                {/*
                  The expiry badge carries the word, not only a colour: "in 12 days" is the fact an
                  employee needs, and a coloured dot that only means "soon" to somebody who already
                  knows the convention is not information.
                */}
                {document.expires_on ? (
                  <span className={document.expiring_soon ? "font-medium text-amber-700" : ""}>
                    {document.expires_on}
                    {document.expiring_soon ? " · expiring soon" : ""}
                  </span>
                ) : (
                  <span className="text-muted">No expiry</span>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export { MyWorkspaceNav };
