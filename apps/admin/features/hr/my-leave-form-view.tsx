"use client";

/**
 * `/hr/me/leave/new` — the employee's own request form (REQ-055, slice 2c).
 *
 * A second form, not a reuse of `/hr/leave/new`, and the difference is one line in each file:
 * the HR form calls `createRequest`, which accepts an `employee_id` in the body; this one calls
 * `createMyLeaveRequest`, whose body type **has no such field**. Reusing the HR form would have
 * been less code and would have handed every employee a control that names somebody else — the
 * control the server would refuse, which is a worse experience than not having it.
 *
 * The day counter is the same arithmetic the store charges, reached through the keyless
 * `/hr/me/leave/preview` rather than recomputed here. The acceptance criterion the leave slice
 * earned — "the number shown before submit equals the stored value" — is worth exactly nothing if
 * the self-service form answers it with a second implementation in TypeScript.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { ArrowLeft } from "lucide-react";

import { ApiError } from "@/lib/api";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";

import {
  createMyLeaveRequest,
  fetchMyLeaveTypes,
  previewMyLeaveDays,
  type LeaveType,
} from "@/lib/hr";

/**
 * The preview failure as a sentence.
 *
 * `ScreenErrorValue` is `string | Error | ApiError | null`, so the three arms are not the same
 * type and a cast would hide the case that matters. An `ApiError` carries the server's own
 * message — the one that says "that range contains no working day" — and that is the text a
 * person needs, so it wins over the fallback. A bare `string` is already a message, and an
 * `Error` carries one; `null` cannot arrive here because the caller only renders this when the
 * value is truthy, but it is answered rather than asserted, so a future caller that forgets the
 * guard gets a sentence instead of a blank line.
 */
function describePreviewError(error: Exclude<ScreenErrorValue, null>): string {
  if (error instanceof ApiError) return error.message;
  if (error instanceof Error) return error.message;
  return error;
}

export function MyLeaveFormView() {
  const router = useRouter();
  const [types, setTypes] = useState<LeaveType[]>([]);
  const [loadingTypes, setLoadingTypes] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [submitting, setSubmitting] = useState(false);

  const [leaveTypeId, setLeaveTypeId] = useState("");
  const [startsOn, setStartsOn] = useState("");
  const [endsOn, setEndsOn] = useState("");
  const [halfDay, setHalfDay] = useState(false);
  const [reason, setReason] = useState("");
  const [preview, setPreview] = useState<string | null>(null);
  const [previewError, setPreviewError] = useState<Exclude<ScreenErrorValue, null> | null>(null);

  useEffect(() => {
    let live = true;
    void (async () => {
      setLoadingTypes(true);
      try {
        const answer = await fetchMyLeaveTypes();
        if (!live) return;
        setTypes(answer.items);
        // The first *active* type is preselected so the form is submittable without a pointless
        // first click. An inactive type is still offered — an organization may have retired it
        // after somebody was granted it — but it is never the default.
        const preferred = answer.items.find((type) => type.active) ?? answer.items[0];
        if (preferred) setLeaveTypeId(preferred.id);
      } catch (failure) {
        if (live) setError(toScreenError(failure, "The leave types could not be loaded."));
      } finally {
        if (live) setLoadingTypes(false);
      }
    })();
    return () => {
      live = false;
    };
  }, []);

  // The counter follows the dates, not the submit button: a person checking what next Thursday
  // costs should not have to try the form to find out.
  useEffect(() => {
    if (!startsOn || !endsOn) {
      setPreview(null);
      setPreviewError(null);
      return;
    }
    let live = true;
    const timer = setTimeout(() => {
      void (async () => {
        try {
          const answer = await previewMyLeaveDays(startsOn, endsOn, halfDay);
          if (live) {
            setPreview(answer.days);
            setPreviewError(null);
          }
        } catch (failure) {
          if (!live) return;
          setPreview(null);
          // The **whole** screen-error value, not `.message`: the state is typed to carry the
          // union so the render site can pick the right arm per type, and unwrapping it here
          // would throw away the `ApiError` whose message is the one the server wrote.
          setPreviewError(toScreenError(failure, "Those dates could not be counted."));
        }
      })();
    }, 250);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [startsOn, endsOn, halfDay]);

  const submit = useCallback(
    async (event: React.FormEvent) => {
      event.preventDefault();
      setSubmitting(true);
      setError(null);
      try {
        await createMyLeaveRequest({
          leave_type_id: leaveTypeId,
          starts_on: startsOn,
          ends_on: endsOn,
          half_day: halfDay,
          reason: reason.trim() || undefined,
        });
        router.push("/hr/me/leave");
      } catch (failure) {
        setError(toScreenError(failure, "The request could not be sent."));
        setSubmitting(false);
      }
    },
    [leaveTypeId, startsOn, endsOn, halfDay, reason, router],
  );

  return (
    <form onSubmit={submit} className="max-w-xl space-y-6" data-qa-hr-me-leave-form>
      <header className="flex items-center gap-2">
        <Link
          href="/hr/me/leave"
          className="inline-flex h-8 items-center gap-1.5 rounded-md border border-border px-2.5 text-sm"
        >
          <ArrowLeft className="h-3.5 w-3.5" aria-hidden />
          My leave
        </Link>
        <h1 className="text-xl font-semibold tracking-tight">Request leave</h1>
      </header>

      {error ? <ErrorState error={error} onRetry={() => setError(null)} qa="hr-me-form-error" /> : null}

      {loadingTypes ? (
        <p className="text-sm text-muted" aria-busy="true">
          Loading leave types…
        </p>
      ) : (
        <div className="space-y-2">
          <label htmlFor="hr-me-leave-type" className="block text-[13px] font-medium">
            Type
          </label>
          <select
            id="hr-me-leave-type"
            value={leaveTypeId}
            onChange={(event) => setLeaveTypeId(event.target.value)}
            data-qa-hr-me-leave-type
            className="h-9 w-full rounded-md border border-border bg-transparent px-2.5 text-sm"
          >
            {types.map((type) => (
              <option key={type.id} value={type.id}>
                {type.name} · {type.annual_days} days/year
                {type.active ? "" : " (inactive)"}
              </option>
            ))}
          </select>
          {types.length === 0 ? (
            <p className="text-[12.5px] text-muted" data-qa-hr-me-leave-no-types>
              This organization has no leave type yet, so there is nothing to request. An
              administrator can add one under Leave types.
            </p>
          ) : null}
        </div>
      )}

      <div className="grid grid-cols-2 gap-3">
        <div className="space-y-2">
          <label htmlFor="hr-me-leave-from" className="block text-[13px] font-medium">
            From
          </label>
          <input
            id="hr-me-leave-from"
            type="date"
            value={startsOn}
            onChange={(event) => setStartsOn(event.target.value)}
            data-qa-hr-me-leave-from
            className="h-9 w-full rounded-md border border-border bg-transparent px-2.5 text-sm"
          />
        </div>
        <div className="space-y-2">
          <label htmlFor="hr-me-leave-to" className="block text-[13px] font-medium">
            To
          </label>
          <input
            id="hr-me-leave-to"
            type="date"
            value={endsOn}
            onChange={(event) => setEndsOn(event.target.value)}
            data-qa-hr-me-leave-to
            className="h-9 w-full rounded-md border border-border bg-transparent px-2.5 text-sm"
          />
        </div>
      </div>

      <label className="flex items-center gap-2 text-[13px]" data-qa-hr-me-leave-half-day>
        <input type="checkbox" checked={halfDay} onChange={(event) => setHalfDay(event.target.checked)} />
        Half day
      </label>

      <p className="text-[12.5px] text-muted">
        {/*
          The preview's own error is a **screen error**, not a line of text. `toScreenError` returns
          `string | Error | ApiError | null`, so a `previewError.message` here would not compile —
          and the tempting fix, `String(previewError)`, would print `[object Object]` for exactly
          the case a person most needs to read: the server refusing a weekend-only range. The
          inline slot below renders the *description* instead, which is what `ErrorState` shows
          and is a string in every arm.
        */}
        {previewError ? (
          <span className="text-destructive" data-qa-hr-me-leave-preview-error>
            {describePreviewError(previewError)}
          </span>
        ) : preview ? (
          <span data-qa-hr-me-leave-preview>
            <strong data-qa-hr-me-leave-preview-days>{preview}</strong> charged day
            {preview === "1" ? "" : "s"} — weekends are not counted.
          </span>
        ) : (
          <span data-qa-hr-me-leave-preview-idle>Pick your dates to see how many days are charged.</span>
        )}
      </p>

      <div className="space-y-2">
        <label htmlFor="hr-me-leave-reason" className="block text-[13px] font-medium">
          Reason <span className="font-normal text-muted">(optional)</span>
        </label>
        <textarea
          id="hr-me-leave-reason"
          value={reason}
          onChange={(event) => setReason(event.target.value)}
          rows={3}
          data-qa-hr-me-leave-reason
          className="w-full rounded-md border border-border bg-transparent px-2.5 py-2 text-sm"
        />
      </div>

      <button
        type="submit"
        disabled={submitting || types.length === 0}
        data-qa-hr-me-leave-submit
        className="h-9 rounded-md bg-foreground px-3.5 text-sm font-medium text-background disabled:opacity-50"
      >
        {submitting ? "Sending…" : "Send request"}
      </button>
    </form>
  );
}
