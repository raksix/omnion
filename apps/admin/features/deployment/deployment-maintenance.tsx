"use client";

/**
 * `/deployment/maintenance` — the maintenance window screen (REQ-024, slice 3).
 *
 * The screen is one form per environment, and the thing worth reading twice is the difference
 * between **enabled** and **active**. They are separate because a window scheduled for 22:00 is
 * enabled and not open, and the operator who opened it is the person who relies on the promise.
 * A screen that shows only a toggle tells them the site is frozen when it is not — or, worse,
 * the other way round: the operator leaves believing a window closes at 22:00 and opens a
 * deploy at 21:59.
 *
 * So the panel renders three states the API computes, never re-derived here:
 *
 * * `active` — the window is open **right now**, and the row says so in words.
 * * `enabled` — configured, with a schedule that has not arrived.
 * * neither — never configured, or turned off. Those are different facts and `updated_at` is
 *   what tells them apart, so "never set up" and "set up and switched off" do not share a line.
 *
 * The save is a `PUT` that answers `422` with the *server's* message (too long, end before
 * start, enabled with no message), and those three are shown verbatim rather than re-worded
 * here: the operator is filling in a form the server already rejected, and a second wording is
 * a second thing to keep in sync.
 */

import { useCallback, useEffect, useState } from "react";

import { CalendarClock, CircleAlert, Loader2, Save, TriangleAlert } from "lucide-react";

import { ApiError, fetchDeploymentMaintenance, saveDeploymentMaintenance } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { DeploymentMaintenanceResponse, DeploymentMaintenanceWindow } from "@/lib/types";

/** The three environments the table knows. Rendered from one list so a fourth can never appear
 *  in the API and be missing here. */
const ENVIRONMENTS = ["production", "staging", "sandbox"] as const;

type EnvironmentName = (typeof ENVIRONMENTS)[number];

/** One form's state. Separate from the row so typing never re-renders the other two forms. */
type Draft = {
  enabled: boolean;
  message: string;
  scope: string;
  startsAt: string;
  endsAt: string;
};

function draftFrom(row: DeploymentMaintenanceWindow | undefined): Draft {
  return {
    // An environment with no row is *not* pre-enabled. The unset shape is what the server
    // returns for "never configured", and a checkbox that starts ticked for a window nobody
    // opened is the one default this screen must not have.
    enabled: row?.enabled ?? false,
    message: row?.message ?? "",
    scope: row?.scope ?? "admin",
    startsAt: row?.starts_at ? toLocalInput(row.starts_at) : "",
    endsAt: row?.ends_at ? toLocalInput(row.ends_at) : "",
  };
}

/** An ISO instant as the value a `datetime-local` input wants, in the browser's own zone. */
function toLocalInput(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}`;
}

/** A `datetime-local` value as the instant the API takes. `null` for an empty field. */
function fromLocalInput(value: string): string | null {
  if (!value) return null;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? null : date.toISOString();
}

/** The word for a window's state. The three cases are mutually exclusive on purpose. */
function windowState(row: DeploymentMaintenanceWindow | undefined): {
  label: string;
  tone: string;
  detail: string;
} {
  if (!row || row.updated_at === null) {
    return {
      label: "Never configured",
      tone: "text-muted-foreground",
      detail: "No window has been set up for this environment.",
    };
  }
  if (row.active) {
    return {
      label: "Open now",
      tone: "text-amber-600 dark:text-amber-400",
      detail: "Write routes are answering 503 with this message. Reads and health probes are not affected.",
    };
  }
  if (row.enabled) {
    return {
      label: "Scheduled",
      tone: "text-sky-600 dark:text-sky-400",
      detail: "Configured, but the window has not opened yet.",
    };
  }
  return {
    label: "Off",
    tone: "text-muted-foreground",
    detail: "Configured and switched off. Writes are not affected.",
  };
}

/** The screen. */
export function DeploymentMaintenance() {
  const [data, setData] = useState<DeploymentMaintenanceResponse | null>(null);
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState<string | null>(null);
  const [refusals, setRefusals] = useState<Record<string, string>>({});

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const response = await fetchDeploymentMaintenance();
      setData(response);
      setDrafts(
        Object.fromEntries(
          ENVIRONMENTS.map((environment) => [
            environment,
            draftFrom(response.windows.find((row) => row.environment === environment)),
          ]),
        ),
      );
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The maintenance windows could not be read.",
      );
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const setDraft = useCallback((environment: string, patch: Partial<Draft>) => {
    setDrafts((current) => ({ ...current, [environment]: { ...current[environment], ...patch } }));
    // Editing clears the previous refusal: leaving a stale "the message is too long" under a
    // message that is now 100 characters is a message about the past.
    setRefusals((current) => {
      if (!(environment in current)) return current;
      const next = { ...current };
      delete next[environment];
      return next;
    });
  }, []);

  const save = useCallback(
    async (environment: EnvironmentName) => {
      const draft = drafts[environment];
      if (!draft) return;
      setSaving(environment);
      setRefusals((current) => ({ ...current, [environment]: "" }));
      try {
        await saveDeploymentMaintenance({
          environment,
          enabled: draft.enabled,
          message: draft.message,
          scope: draft.scope,
          startsAt: fromLocalInput(draft.startsAt),
          endsAt: fromLocalInput(draft.endsAt),
        });
        await load();
      } catch (caught) {
        // The server's own sentence, not a rewording: the form was rejected by a rule with a
        // specific reason and the operator needs that reason, not "could not save".
        setRefusals((current) => ({
          ...current,
          [environment]:
            caught instanceof ApiError
              ? caught.message
              : "The window could not be saved.",
        }));
      } finally {
        setSaving(null);
      }
    },
    [drafts, load],
  );

  if (loading && !data) {
    return (
      <div className="flex items-center gap-2 p-6 text-[13px] text-muted-foreground">
        <Loader2 className="size-4 animate-spin" aria-hidden="true" />
        Reading the maintenance windows…
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <p className="max-w-3xl text-[13px] text-muted-foreground">
        A window is a promise to every API client: while it is open, write routes answer{" "}
        <code className="rounded bg-muted px-1 py-0.5 text-[12px]">503</code> with the message
        below. Reads, health probes and this screen itself are not affected. A window scoped to{" "}
        <strong>admin</strong> refuses panel writes only — the public site keeps serving, which is
        usually what an operator wants when the work is a settings change.
      </p>

      {error ? (
        <div
          role="alert"
          className="flex items-start gap-2 rounded-lg border border-destructive/40 bg-destructive/5 p-3 text-[13px]"
        >
          <CircleAlert className="mt-0.5 size-4 shrink-0 text-destructive" aria-hidden="true" />
          <div>
            <p className="font-medium">{error}</p>
            <button
              type="button"
              onClick={() => void load()}
              className="mt-1 text-[12.5px] underline underline-offset-2"
            >
              Try again
            </button>
          </div>
        </div>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-3">
        {ENVIRONMENTS.map((environment) => {
          const row = data?.windows.find((entry) => entry.environment === environment);
          const draft = drafts[environment] ?? draftFrom(row);
          const state = windowState(row);
          const refusal = refusals[environment];
          const max = data?.max_message_length ?? 280;

          return (
            <section
              key={environment}
              data-maintenance-env={environment}
              aria-label={`${environment} maintenance window`}
              className={`flex flex-col gap-3 rounded-xl border p-4 ${
                row?.active
                  ? "border-amber-500/50 bg-amber-500/5"
                  : "border-border bg-card"
              }`}
            >
              <header className="flex items-start justify-between gap-2">
                <div>
                  <h2 className="text-[14px] font-semibold capitalize">{environment}</h2>
                  <p data-maintenance-state={environment} className={`text-[12.5px] ${state.tone}`}>
                  {state.label}
                </p>
                </div>
                {row?.active ? (
                  <TriangleAlert
                    className="size-4 shrink-0 text-amber-600 dark:text-amber-400"
                    aria-label="A window is open"
                  />
                ) : null}
              </header>

              <p className="text-[12px] text-muted-foreground">{state.detail}</p>

              <label className="flex items-center gap-2 text-[13px]">
                <input
                  type="checkbox"
                  data-maintenance-enabled={environment}
                  checked={draft.enabled}
                  onChange={(event) =>
                    setDraft(environment, { enabled: event.target.checked })
                  }
                  className="size-4 accent-amber-600"
                />
                <span className="font-medium">Window enabled</span>
              </label>

              <div className="flex flex-col gap-1">
                <label
                  htmlFor={`${environment}-message`}
                  className="text-[12.5px] font-medium"
                >
                  Banner message
                </label>
                <textarea
                  id={`${environment}-message`}
                  data-maintenance-message={environment}
                  value={draft.message}
                  maxLength={max}
                  rows={2}
                  onChange={(event) => setDraft(environment, { message: event.target.value })}
                  placeholder="Core upgrade until 14:00 — writes are refused."
                  aria-describedby={`${environment}-message-hint`}
                  className="w-full rounded-md border border-border bg-background px-2 py-1.5 text-[13px]"
                />
                <p id={`${environment}-message-hint`} className="text-[11.5px] text-muted-foreground">
                  {draft.message.trim().length}/{max} characters. Required while the window is
                  enabled — a window that refuses every write and says nothing is not a window.
                </p>
              </div>

              <div className="flex flex-col gap-1">
                <label htmlFor={`${environment}-scope`} className="text-[12.5px] font-medium">
                  Scope
                </label>
                <select
                  id={`${environment}-scope`}
                  data-maintenance-scope={environment}
                  value={draft.scope}
                  onChange={(event) => setDraft(environment, { scope: event.target.value })}
                  className="w-full rounded-md border border-border bg-background px-2 py-1.5 text-[13px]"
                >
                  <option value="admin">Admin only — panel writes refused</option>
                  <option value="all">Everything — all writes refused</option>
                </select>
              </div>

              <div className="grid gap-2 sm:grid-cols-2">
                <div className="flex flex-col gap-1">
                  <label
                    htmlFor={`${environment}-starts`}
                    className="text-[12.5px] font-medium"
                  >
                    <CalendarClock className="mr-1 inline size-3.5" aria-hidden="true" />
                    Starts
                  </label>
                  <input
                    id={`${environment}-starts`}
                    type="datetime-local"
                    value={draft.startsAt}
                    onChange={(event) => setDraft(environment, { startsAt: event.target.value })}
                    className="w-full rounded-md border border-border bg-background px-2 py-1.5 text-[12.5px]"
                  />
                </div>
                <div className="flex flex-col gap-1">
                  <label htmlFor={`${environment}-ends`} className="text-[12.5px] font-medium">
                    Ends
                  </label>
                  <input
                    id={`${environment}-ends`}
                    type="datetime-local"
                    value={draft.endsAt}
                    onChange={(event) => setDraft(environment, { endsAt: event.target.value })}
                    className="w-full rounded-md border border-border bg-background px-2 py-1.5 text-[12.5px]"
                  />
                </div>
              </div>
              <p className="text-[11.5px] text-muted-foreground">
                Leave both empty for an open-ended window that starts as soon as it is enabled.
              </p>

              {refusal ? (
                <p
                  role="alert"
                  className="rounded-md border border-destructive/40 bg-destructive/5 px-2 py-1.5 text-[12.5px] text-destructive"
                >
                  {refusal}
                </p>
              ) : null}

              <button
                type="button"
                data-maintenance-save={environment}
                onClick={() => void save(environment)}
                disabled={saving === environment}
                className="mt-auto inline-flex items-center justify-center gap-1.5 rounded-md border border-border bg-background px-3 py-1.5 text-[13px] font-medium hover:bg-accent disabled:opacity-60"
              >
                {saving === environment ? (
                  <Loader2 className="size-3.5 animate-spin" aria-hidden="true" />
                ) : (
                  <Save className="size-3.5" aria-hidden="true" />
                )}
                Save window
              </button>

              <p className="text-[11.5px] text-muted-foreground">
                {row?.updated_at
                  ? `Last changed ${formatTimestamp(row.updated_at)}.`
                  : "This environment has never had a window."}
              </p>
            </section>
          );
        })}
      </div>
    </div>
  );
}
