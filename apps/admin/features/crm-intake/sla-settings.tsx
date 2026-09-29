/**
 * `/crm/settings/sla` — the first-response targets, the business-hours window and the
 * escalation path (REQ-117, slice 2).
 *
 * A policy is a promise made to somebody who wrote in: *this is how long before somebody
 * answers you*. The screen's job is to make that promise legible, and three decisions follow
 * from taking it seriously:
 *
 * 1. **A deadline is a stored instant, never a browser calculation.** The screen shows the
 *    policy's own target and window and nothing else. If it recomputed a deadline from the
 *    viewer's clock it would disagree with the escalation the worker has already sent, and the
 *    reader would be the one asked to explain the difference.
 * 2. **The window is stated in the organization's own zone and the screen says what it
 *    means.** A weekly window with a `timezone` label is what v1 stores; the note under the
 *    form says holidays are out of scope, because a calendar that silently treats a public
 *    holiday as a working day is worse than a documented gap.
 * 3. **A reminder at the same minute as the breach is refused in the form**, with the reason
 *    in the sentence rather than in a red border. It is the mistake an operator makes once
 *    and then trusts the screen about.
 */

"use client";

import { useCallback, useEffect, useState } from "react";

import { ApiError, request } from "@/lib/api";

interface SlaPolicy {
  id: string;
  organization_id: string;
  name: string;
  first_response_minutes: number;
  business_hours_only: boolean;
  reminder_minutes: number | null;
  escalate_to_user_id: string | null;
  business_hours: BusinessWindow | Record<string, never>;
  active: boolean;
  window_configured: boolean;
  window: BusinessWindow | Record<string, never>;
}

interface BusinessWindow {
  days?: number[];
  start?: string;
  end?: string;
  timezone?: string;
}

interface PoliciesAnswer {
  policies: SlaPolicy[];
  default_policy_name: string;
  default_rule_name: string;
  holidays: string;
}

/** ISO weekdays, in the order the checkbox row renders. */
const DAYS: { value: number; label: string }[] = [
  { value: 1, label: "Mon" },
  { value: 2, label: "Tue" },
  { value: 3, label: "Wed" },
  { value: 4, label: "Thu" },
  { value: 5, label: "Fri" },
  { value: 6, label: "Sat" },
  { value: 7, label: "Sun" },
];

const MINUTE_CHOICES = [15, 30, 60, 120, 240, 480, 1440];

/** `240` reads as "4 hours" and `90` as "1 h 30 m"; a raw number is a support ticket. */
export function humanMinutes(minutes: number): string {
  if (minutes < 60) return `${minutes} min`;
  if (minutes % 60 === 0) return `${minutes / 60} h`;
  const hours = Math.floor(minutes / 60);
  return `${hours} h ${minutes % 60} m`;
}

export function SlaSettings() {
  const [answer, setAnswer] = useState<PoliciesAnswer | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [editing, setEditing] = useState<SlaPolicy | "new" | null>(null);

  const load = useCallback(async () => {
    try {
      setAnswer(await request<PoliciesAnswer>("/api/v1/crm/sla/policies"));
      setError(null);
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : String(caught));
      setAnswer({ policies: [], default_policy_name: "", default_rule_name: "", holidays: "" });
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const save = async (draft: PolicyDraft) => {
    setBusy(true);
    try {
      const body = JSON.stringify(draft);
      await (draft.id
        ? request<SlaPolicy>(`/api/v1/crm/sla/policies/${draft.id}`, { method: "PATCH", body })
        : request<SlaPolicy>("/api/v1/crm/sla/policies", { method: "POST", body }));
      await load();
      setEditing(null);
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (policy: SlaPolicy) => {
    setBusy(true);
    try {
      await request<null>(`/api/v1/crm/sla/policies/${policy.id}`, { method: "DELETE" });
      await load();
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-6">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-[15px] font-medium text-ink">First-response targets</h2>
          <p className="mt-0.5 max-w-prose text-[12.5px] text-muted">
            How long a lead may wait before somebody answers it. A lead that goes past its
            target without a response is escalated to the policy&apos;s person, once.
          </p>
        </div>
        <button
          type="button"
          disabled={busy}
          onClick={() => setEditing("new")}
          data-testid="sla-add"
          className="rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-paper disabled:opacity-50"
        >
          Add policy
        </button>
      </header>

      {error ? (
        <p
          role="alert"
          data-testid="sla-error"
          className="rounded-md border border-danger/40 bg-danger/5 px-3 py-2 text-[12.5px] text-danger"
        >
          {error}
        </p>
      ) : null}

      {answer === null ? (
        <div className="space-y-2" aria-busy="true" data-testid="sla-skeleton">
          {[0, 1].map((n) => (
            <div key={n} className="h-[62px] animate-pulse rounded-md border border-line bg-line/30" />
          ))}
        </div>
      ) : answer.policies.length === 0 ? (
        <p className="rounded-md border border-line px-3 py-6 text-center text-[12.5px] text-muted">
          No policies yet. A lead with no target is not overdue, so the inbox will show
          nothing until one exists.
        </p>
      ) : (
        <ul className="space-y-2" data-testid="sla-policies">
          {answer.policies.map((policy) => (
            <li
              key={policy.id}
              data-testid="sla-policy"
              data-policy-id={policy.id}
              className="flex flex-wrap items-center gap-3 rounded-md border border-line px-3 py-2.5"
            >
              <div className="min-w-0 flex-1">
                <p className="flex items-center gap-2 text-[13px] text-ink">
                  <span className="truncate font-medium">{policy.name}</span>
                  {policy.name === answer.default_policy_name ? (
                    <span
                      data-testid="sla-default"
                      className="rounded border border-line px-1.5 py-0.5 text-[10.5px] text-muted"
                    >
                      default
                    </span>
                  ) : null}
                  {policy.active ? null : (
                    <span
                      data-testid="sla-inactive"
                      className="rounded border border-line px-1.5 py-0.5 text-[10.5px] text-muted"
                    >
                      inactive
                    </span>
                  )}
                </p>
                <p className="mt-0.5 text-[11.5px] text-muted">
                  first response within {humanMinutes(policy.first_response_minutes)}
                  {policy.business_hours_only
                    ? policy.window_configured
                      ? ` · ${describeWindow(policy.window)}`
                      : " · business hours only, but no window set — the clock runs around the clock"
                    : " · around the clock"}
                  {policy.reminder_minutes
                    ? ` · reminder ${humanMinutes(policy.reminder_minutes)} before`
                    : " · no reminder"}
                  {policy.escalate_to_user_id
                    ? ` · escalates to ${policy.escalate_to_user_id.slice(0, 8)}…`
                    : " · no escalation target"}
                </p>
              </div>
              <div className="flex shrink-0 items-center gap-1">
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => setEditing(policy)}
                  data-testid={`sla-edit-${policy.id.slice(0, 8)}`}
                  className="rounded border border-line px-2 py-1 text-[11.5px] text-ink disabled:opacity-50"
                >
                  Edit
                </button>
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => void remove(policy)}
                  data-testid={`sla-delete-${policy.id.slice(0, 8)}`}
                  className="rounded border border-danger/40 px-2 py-1 text-[11.5px] text-danger disabled:opacity-50"
                >
                  Delete
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}

      {answer ? (
        <p className="text-[11.5px] text-muted" data-testid="sla-scope-note">
          {answer.holidays}. A lead that is already waiting keeps the deadline it arrived
          with — editing a policy does not move a clock that has already started.
        </p>
      ) : null}

      {editing ? (
        <PolicyEditor
          policy={editing === "new" ? null : editing}
          onCancel={() => setEditing(null)}
          onSave={(draft) => void save(draft)}
        />
      ) : null}
    </div>
  );
}

function describeWindow(window: BusinessWindow | Record<string, never>): string {
  const days = Array.isArray(window.days) ? window.days : [];
  const names = days.map((d) => DAYS.find((day) => day.value === d)?.label ?? String(d));
  const zone = typeof window.timezone === "string" ? window.timezone : "UTC";
  return `${names.length ? names.join(" ") : "every day"} ${window.start ?? "?"}–${window.end ?? "?"} ${zone}`;
}

// -------------------------------------------------------------------------------------------
// The editor
// -------------------------------------------------------------------------------------------

interface PolicyDraft {
  id: string | null;
  name: string;
  first_response_minutes: number;
  business_hours_only: boolean;
  reminder_minutes: number | null;
  escalate_to_user_id: string | null;
  business_hours: BusinessWindow;
  active: boolean;
}

function PolicyEditor({
  policy,
  onCancel,
  onSave,
}: {
  policy: SlaPolicy | null;
  onCancel: () => void;
  onSave: (draft: PolicyDraft) => void;
}) {
  const [form, setForm] = useState<PolicyDraft>(() => {
    if (!policy) {
      return {
        id: null,
        name: "",
        first_response_minutes: 240,
        business_hours_only: false,
        reminder_minutes: null,
        escalate_to_user_id: null,
        business_hours: { days: [1, 2, 3, 4, 5], start: "09:00", end: "17:00", timezone: "UTC" },
        active: true,
      };
    }
    const window = policy.window as BusinessWindow;
    return {
      id: policy.id,
      name: policy.name,
      first_response_minutes: policy.first_response_minutes,
      business_hours_only: policy.business_hours_only,
      reminder_minutes: policy.reminder_minutes,
      escalate_to_user_id: policy.escalate_to_user_id,
      business_hours: {
        days: Array.isArray(window.days) ? window.days : [1, 2, 3, 4, 5],
        start: window.start ?? "09:00",
        end: window.end ?? "17:00",
        timezone: window.timezone ?? "UTC",
      },
      active: policy.active,
    };
  });

  // The refusal, stated in a sentence. A reminder that fires at the same instant as the
  // breach is a reminder nobody reads, and the server refuses it; saying so here means the
  // operator never spends a round trip finding out.
  const reminderCollides = form.reminder_minutes === form.first_response_minutes;
  const canSave = form.name.trim().length > 0 && !reminderCollides;

  const toggleDay = (day: number) => {
    const days = form.business_hours.days ?? [];
    const next = days.includes(day) ? days.filter((d) => d !== day) : [...days, day].sort();
    setForm({ ...form, business_hours: { ...form.business_hours, days: next } });
  };

  return (
    <section
      aria-label={form.id ? `Edit ${form.name}` : "Add an SLA policy"}
      data-testid="sla-editor"
      className="space-y-3 rounded-md border border-line bg-line/10 p-4"
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name" htmlFor="sla-name">
          <input
            id="sla-name"
            value={form.name}
            onChange={(event) => setForm({ ...form, name: event.target.value })}
            data-testid="sla-name"
            className="w-full rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
          />
        </Field>
        <Field label="First response within" htmlFor="sla-minutes">
          <select
            id="sla-minutes"
            value={form.first_response_minutes}
            onChange={(event) =>
              setForm({ ...form, first_response_minutes: Number(event.target.value) })
            }
            data-testid="sla-minutes"
            className="w-full rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
          >
            {MINUTE_CHOICES.map((minutes) => (
              <option key={minutes} value={minutes}>
                {humanMinutes(minutes)}
              </option>
            ))}
          </select>
        </Field>
      </div>

      <label className="flex items-center gap-2 text-[12.5px] text-ink">
        <input
          type="checkbox"
          checked={form.business_hours_only}
          onChange={(event) => setForm({ ...form, business_hours_only: event.target.checked })}
          data-testid="sla-business-hours"
          className="h-3.5 w-3.5"
        />
        Only count working hours
      </label>

      {form.business_hours_only ? (
        <fieldset
          data-testid="sla-window"
          className="space-y-2 rounded-md border border-line p-3"
        >
          <legend className="px-1 text-[12px] text-ink">Working hours</legend>
          <div className="flex flex-wrap gap-3">
            {DAYS.map((day) => (
              <label key={day.value} className="flex items-center gap-1 text-[12px] text-ink">
                <input
                  type="checkbox"
                  checked={(form.business_hours.days ?? []).includes(day.value)}
                  onChange={() => toggleDay(day.value)}
                  data-testid={`sla-day-${day.value}`}
                  className="h-3.5 w-3.5"
                />
                {day.label}
              </label>
            ))}
          </div>
          <div className="grid gap-3 sm:grid-cols-3">
            <Field label="Opens" htmlFor="sla-open">
              <input
                id="sla-open"
                type="time"
                value={form.business_hours.start ?? "09:00"}
                onChange={(event) =>
                  setForm({
                    ...form,
                    business_hours: { ...form.business_hours, start: event.target.value },
                  })
                }
                data-testid="sla-open"
                className="w-full rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
              />
            </Field>
            <Field label="Closes" htmlFor="sla-close">
              <input
                id="sla-close"
                type="time"
                value={form.business_hours.end ?? "17:00"}
                onChange={(event) =>
                  setForm({
                    ...form,
                    business_hours: { ...form.business_hours, end: event.target.value },
                  })
                }
                data-testid="sla-close"
                className="w-full rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
              />
            </Field>
            <Field label="Timezone" htmlFor="sla-zone">
              <input
                id="sla-zone"
                value={form.business_hours.timezone ?? "UTC"}
                onChange={(event) =>
                  setForm({
                    ...form,
                    business_hours: { ...form.business_hours, timezone: event.target.value },
                  })
                }
                data-testid="sla-zone"
                className="w-full rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
              />
            </Field>
          </div>
          <p className="text-[11px] text-muted">
            A Friday-evening lead is then due on Monday. Holidays are out of scope in this
            version, so a public holiday is treated as an ordinary working day — stated here
            rather than left to be discovered.
          </p>
        </fieldset>
      ) : null}

      <div className="grid gap-3 sm:grid-cols-2">
        <Field
          label="Reminder"
          htmlFor="sla-reminder"
          hint="How long before the deadline to nudge the owner. Leave empty for none."
        >
          <select
            id="sla-reminder"
            value={form.reminder_minutes ?? ""}
            onChange={(event) =>
              setForm({
                ...form,
                reminder_minutes: event.target.value ? Number(event.target.value) : null,
              })
            }
            data-testid="sla-reminder"
            className="w-full rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
          >
            <option value="">No reminder</option>
            {MINUTE_CHOICES.map((minutes) => (
              <option key={minutes} value={minutes}>
                {humanMinutes(minutes)} before
              </option>
            ))}
          </select>
        </Field>
        <Field
          label="Escalate to"
          htmlFor="sla-escalate"
          hint="Who hears about a breach. Empty means the breach is recorded and nobody is told."
        >
          <input
            id="sla-escalate"
            value={form.escalate_to_user_id ?? ""}
            onChange={(event) =>
              setForm({ ...form, escalate_to_user_id: event.target.value.trim() || null })
            }
            data-testid="sla-escalate"
            className="w-full rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
          />
        </Field>
      </div>

      {reminderCollides ? (
        <p role="alert" data-testid="sla-reminder-collision" className="text-[11.5px] text-danger">
          The reminder is at the same minute as the breach, so it would arrive with the
          escalation rather than before it. Pick an earlier one.
        </p>
      ) : null}

      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={() => onSave(form)}
          disabled={!canSave}
          data-testid="sla-save"
          className="rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-paper disabled:opacity-40"
        >
          {form.id ? "Save policy" : "Add policy"}
        </button>
        <button
          type="button"
          onClick={onCancel}
          data-testid="sla-cancel"
          className="rounded-md border border-line px-3 py-1.5 text-[12.5px] text-ink"
        >
          Cancel
        </button>
      </div>
    </section>
  );
}

function Field({
  label,
  htmlFor,
  hint,
  children,
}: {
  label: string;
  htmlFor: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <div>
      <label htmlFor={htmlFor} className="block text-[12.5px] text-ink">
        {label}
      </label>
      {hint ? <p className="mt-0.5 text-[11px] text-muted">{hint}</p> : null}
      <div className="mt-1">{children}</div>
    </div>
  );
}
