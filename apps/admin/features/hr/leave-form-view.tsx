"use client";

/**
 * `/hr/leave/new` — the request form (REQ-055, slice 2b).
 *
 * The acceptance criterion is "the number shown before submit equals the stored value", and the
 * only way to keep that true forever is to have **one** implementation of the working-day rule. So
 * the day counter here is not arithmetic: it is `GET /hr/leave/requests/preview`, debounced, and the
 * value the form will submit is the value the module will store.
 *
 * Two details that are easy to get wrong and expensive when they are:
 *
 * - **The half-day toggle is only offered when `from = to`.** Half of a range is half of *what*?
 *   The module charges `0.5` only for a single day; offering it on a five-day range would show a
 *   day count the server will not accept, which is a form that can be filled in and cannot be sent.
 * - **The reason is optional but the comment on a decision is not the same field.** A request has
 *   no comment; the comment belongs to whoever decides it, and the detail screen owns it.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import Link from "next/link";
import { ArrowLeft, Loader2, Save } from "lucide-react";

import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { ErrorStrip } from "@/components/error-state";

import {
  createRequest,
  fetchLeaveTypes,
  previewDays,
  type DaysPreview,
  type LeaveType,
  type NewLeaveRequest,
} from "@/lib/hr";

/** How long the screen waits after the last keystroke before asking the server for a count. */
const PREVIEW_DEBOUNCE_MS = 350;

export function LeaveRequestForm() {
  const router = useRouter();
  const [types, setTypes] = useState<LeaveType[]>([]);
  const [loadingTypes, setLoadingTypes] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);

  const [leaveTypeId, setLeaveTypeId] = useState("");
  const [startsOn, setStartsOn] = useState("");
  const [endsOn, setEndsOn] = useState("");
  const [halfDay, setHalfDay] = useState(false);
  const [reason, setReason] = useState("");

  const [preview, setPreview] = useState<DaysPreview | null>(null);
  const [previewing, setPreviewing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<ScreenErrorValue>(null);
  // The request id the last successful submit answered with, so a double-click cannot raise two.
  const submitted = useRef(false);

  useEffect(() => {
    let live = true;
    fetchLeaveTypes()
      .then((page) => {
        if (!live) {
          return;
        }
        setTypes(page.items);
        // Default to the first *active* type: preselecting a deactivated type would let somebody
        // file leave against a rule the organization has switched off.
        const first = page.items.find((type) => type.active);
        setLeaveTypeId(first?.id ?? page.items[0]?.id ?? "");
      })
      .catch((failure: unknown) => {
        if (live) {
          setError(toScreenError(failure, "The leave types could not be loaded."));
        }
      })
      .finally(() => {
        if (live) {
          setLoadingTypes(false);
        }
      });
    return () => {
      live = false;
    };
  }, []);

  // The counter, from the module. `endsOn` falling behind `startsOn` clears it rather than asking,
  // because a preview of an impossible range is a preview of a refusal.
  const refreshPreview = useCallback(async () => {
    if (!startsOn || !endsOn || endsOn < startsOn) {
      setPreview(null);
      return;
    }
    setPreviewing(true);
    try {
      const result = await previewDays(startsOn, endsOn, halfDay);
      setPreview(result);
    } catch (failure) {
      // A refused preview is information, not a crash: "weekend only" and "over balance" both land
      // here and both belong next to the dates, not in the page-level error state.
      setPreview(null);
      setPreviewing(false);
      return;
    }
    setPreviewing(false);
  }, [startsOn, endsOn, halfDay]);

  useEffect(() => {
    const timer = setTimeout(() => {
      void refreshPreview();
    }, PREVIEW_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [refreshPreview]);

  const sameDay = Boolean(startsOn) && startsOn === endsOn;
  const canSubmit = Boolean(leaveTypeId && startsOn && endsOn && preview) && !saving && !submitted.current;

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!canSubmit) {
      return;
    }
    setSaving(true);
    setSaveError(null);
    const body: NewLeaveRequest = {
      leave_type_id: leaveTypeId,
      starts_on: startsOn,
      ends_on: endsOn,
      half_day: sameDay ? halfDay : false,
      reason: reason.trim() || undefined,
    };
    try {
      const created = await createRequest(body);
      submitted.current = true;
      router.push(`/hr/leave/${created.id}`);
    } catch (failure) {
      setSaveError(toScreenError(failure, "The leave request could not be sent."));
      setSaving(false);
    }
  };

  if (error) {
    return (
      <ErrorState
        error={error}
        onRetry={() => window.location.reload()}
        qa="hr-leave-form-load-error"
      />
    );
  }
  return (
    <form onSubmit={submit} className="max-w-xl space-y-6" data-qa-hr-leave-form>
      <header className="flex items-center gap-2">
        <Link
          href="/hr/leave"
          className="inline-flex h-8 items-center gap-1.5 rounded-md border border-border px-2.5 text-sm"
        >
          <ArrowLeft className="h-3.5 w-3.5" aria-hidden />
          Leave
        </Link>
        <h1 className="text-xl font-semibold tracking-tight">Request leave</h1>
      </header>

      {loadingTypes ? (
        <p className="text-sm text-muted" aria-busy="true">
          Loading leave types…
        </p>
      ) : (
        <div className="space-y-2">
          <label htmlFor="hr-leave-type" className="block text-[13px] font-medium">
            Type
          </label>
          <select
            id="hr-leave-type"
            value={leaveTypeId}
            onChange={(event) => setLeaveTypeId(event.target.value)}
            data-qa-hr-leave-type
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
            <p className="text-[12.5px] text-muted" data-qa-hr-leave-no-types>
              This organization has no leave type yet. An administrator can add one under Leave types.
            </p>
          ) : null}
        </div>
      )}

      <div className="grid grid-cols-2 gap-3">
        <div className="space-y-2">
          <label htmlFor="hr-leave-from" className="block text-[13px] font-medium">
            From
          </label>
          <input
            id="hr-leave-from"
            type="date"
            required
            value={startsOn}
            onChange={(event) => setStartsOn(event.target.value)}
            data-qa-hr-leave-from
            className="h-9 w-full rounded-md border border-border bg-transparent px-2.5 text-sm"
          />
        </div>
        <div className="space-y-2">
          <label htmlFor="hr-leave-to" className="block text-[13px] font-medium">
            To
          </label>
          <input
            id="hr-leave-to"
            type="date"
            required
            min={startsOn || undefined}
            value={endsOn}
            onChange={(event) => setEndsOn(event.target.value)}
            data-qa-hr-leave-to
            className="h-9 w-full rounded-md border border-border bg-transparent px-2.5 text-sm"
          />
        </div>
      </div>

      {sameDay ? (
        <label className="flex items-center gap-2 text-[13px]" data-qa-hr-leave-half-day>
          <input
            type="checkbox"
            checked={halfDay}
            onChange={(event) => setHalfDay(event.target.checked)}
            className="h-4 w-4"
          />
          Half day
        </label>
      ) : (
        <p className="text-[12.5px] text-muted" data-qa-hr-leave-half-day-hidden>
          A half day is charged only when the first and last day are the same day.
        </p>
      )}

      <div
        className="rounded-md border border-border px-3 py-2 text-[13px]"
        data-qa-hr-leave-preview
        aria-live="polite"
      >
        {previewing ? (
          <span className="inline-flex items-center gap-2 text-muted">
            <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
            Counting working days…
          </span>
        ) : preview ? (
          <span>
            <strong data-qa-hr-leave-preview-days>{preview.days}</strong> charged day
            {preview.days === "1" ? "" : "s"}
            {preview.working_days > 0 ? ` over ${preview.working_days} working days` : null}
          </span>
        ) : startsOn && endsOn && endsOn < startsOn ? (
          <span className="text-destructive">The last day is before the first day.</span>
        ) : (
          <span className="text-muted">Pick the first and last day to see the charged days.</span>
        )}
      </div>

      <div className="space-y-2">
        <label htmlFor="hr-leave-reason" className="block text-[13px] font-medium">
          Reason <span className="font-normal text-muted">(optional)</span>
        </label>
        <textarea
          id="hr-leave-reason"
          value={reason}
          maxLength={500}
          rows={3}
          onChange={(event) => setReason(event.target.value)}
          data-qa-hr-leave-reason
          className="w-full rounded-md border border-border bg-transparent px-2.5 py-2 text-sm"
        />
        <p className="text-[11.5px] text-muted">{500 - reason.length} characters left</p>
      </div>

      {saveError ? (
        <ErrorStrip error={saveError} onRetry={() => setSaveError(null)} qa="hr-leave-form-error" />
      ) : null}

      <button
        type="submit"
        disabled={!canSubmit}
        data-qa-hr-leave-submit
        className="inline-flex h-9 items-center gap-2 rounded-md bg-primary px-3 text-sm text-primary-foreground disabled:opacity-60"
      >
        {saving ? <Loader2 className="h-4 w-4 animate-spin" aria-hidden /> : <Save className="h-4 w-4" aria-hidden />}
        Send the request
      </button>
    </form>
  );
}
