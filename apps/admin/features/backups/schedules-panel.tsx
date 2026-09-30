"use client";

/**
 * The backup schedules table and its editor (REQ-013, slice 3).
 *
 * This screen exists because `backup_schedules` shipped in slice 1 with a `next_run_at`
 * column and a cadence sentence in the API — and no writer for that column and no worker to
 * read it. A schedule could be created here, listed, and rendered beside an empty next-run
 * cell for ever. The column is written now, and the table is the place an operator sees
 * whether it took: **a schedule with no next run is rendered as a defect, not as a blank.**
 *
 * Four things on this screen are decisions rather than decoration:
 *
 * - **The next run is shown in the schedule's own timezone, with the zone named.** A backup
 *   that fires at "02:00" and a column that says `23:00Z` are the same instant, and an
 *   operator who cannot tell which one the worker will use cannot tell whether the schedule
 *   is right. Showing the local reading *and* the zone is the only presentation that answers
 *   "when does this actually run".
 * - **A disabled schedule shows "paused", never a next run.** The API clears the column when
 *   a schedule is switched off, and the table has to agree — a next run beside a disabled row
 *   is a promise the worker will not keep.
 * - **"Run now" is separated from the editor on purpose.** It produces a backup and changes
 *   nothing else: it does not advance the schedule, so an operator testing a schedule at
 *   09:00 has not silently consumed the 02:00 slot.
 * - **An unknown timezone is refused by the server and the message is shown verbatim.** The
 *   form offers a real list, but a schedule can be created by API too, and a row that says
 *   `Europe/Istanbool` must be fixable from here rather than only from a terminal.
 */
import { useCallback, useEffect, useState } from "react";

import {
  CalendarClock,
  Loader2,
  Pencil,
  Play,
  Plus,
  Power,
  PowerOff,
  Trash2,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import type { BackupScheduleInput } from "@/lib/api";
import {
  ApiError,
  createBackupSchedule,
  deleteBackupSchedule,
  fetchBackupSchedules,
  runBackupScheduleNow,
  updateBackupSchedule,
} from "@/lib/api";
import type { BackupSchedule } from "@/lib/types";

/** The four frequencies, as the options render them. */
const FREQUENCIES: Frequency[] = ["hourly", "daily", "weekly", "monthly"];

/** The five parts, in the order the manifest stores them. */
const PARTS = ["database", "media", "configuration", "themes", "plugins"] as const;

/** Weekdays, indexed by the `day_of_week` column. */
const WEEKDAYS = [
  "Sunday",
  "Monday",
  "Tuesday",
  "Wednesday",
  "Thursday",
  "Friday",
  "Saturday",
];

/**
 * The zones the form offers.
 *
 * A short list, not the whole IANA database: a picker with 600 entries is a picker nobody
 * reads, and every deployment on this platform is in one of these. The server accepts any
 * name the build knows, so a zone outside this list is still creatable — and the error it
 * produces is shown verbatim rather than swallowed.
 */
const ZONES = [
  "UTC",
  "Europe/Istanbul",
  "Europe/London",
  "Europe/Berlin",
  "Europe/Paris",
  "Europe/Moscow",
  "America/New_York",
  "America/Chicago",
  "America/Los_Angeles",
  "America/Sao_Paulo",
  "Asia/Dubai",
  "Asia/Kolkata",
  "Asia/Shanghai",
  "Asia/Tokyo",
  "Australia/Sydney",
] as const;

/**
 * The four frequencies the schema allows.
 *
 * A literal union rather than `string`, because the conditional fields below branch on it
 * and a `string` would make every branch's narrowing unchecked: `draft.frequency === "weekly"`
 * would be a comparison against a value that could be anything, and the day-of-week field
 * would appear and disappear on a typo rather than on a real choice.
 */
type Frequency = "hourly" | "daily" | "weekly" | "monthly";

/** What the editor holds, which is not what the API takes: ids and switches are added here. */
interface Draft {
  name: string;
  frequency: Frequency;
  at_time: string;
  day_of_week: number;
  day_of_month: number;
  timezone: string;
  scopes: string[];
  retention_count: number;
  enabled: boolean;
}

function emptyDraft(): Draft {
  return {
    name: "",
    frequency: "daily",
    // 02:00 rather than 12:00, because a backup that competes with the working day is a
    // backup that gets turned off the first week.
    at_time: "02:00",
    day_of_week: 1,
    day_of_month: 1,
    timezone: "Europe/Istanbul",
    scopes: [...PARTS],
    retention_count: 7,
    enabled: true,
  };
}

function draftOf(schedule: BackupSchedule): Draft {
  return {
    name: schedule.name,
    // Narrowed rather than cast: a row whose frequency is not one of the four is a defect
    // in the data, and a cast would let the editor save it straight back. Falling back to
    // `daily` means the form opens on something valid instead of a select with no option
    // selected, which renders as an empty control the operator has to guess about.
    frequency: (FREQUENCIES as string[]).includes(schedule.frequency)
      ? (schedule.frequency as Frequency)
      : "daily",
    at_time: schedule.at_time ? schedule.at_time.slice(0, 5) : "02:00",
    day_of_week: schedule.day_of_week ?? 1,
    day_of_month: schedule.day_of_month ?? 1,
    timezone: schedule.timezone,
    scopes: schedule.scopes.length > 0 ? schedule.scopes : [...PARTS],
    retention_count: schedule.retention_count,
    enabled: schedule.enabled,
  };
}

/** The instant a next run happens, in the schedule's own zone. */
function localRun(at: string | null, zone: string): string {
  if (!at) return "—";
  const parsed = new Date(at);
  if (Number.isNaN(parsed.getTime())) return "—";
  // `Intl` is the platform's own timezone database rather than a second copy of the
  // schedule's rules, so the table and the worker cannot disagree about what "02:00 in
  // Europe/Istanbul" means. An unknown zone throws, and the raw value is shown instead —
  // which is the honest answer for a zone this build does not have.
  try {
    return new Intl.DateTimeFormat("en-GB", {
      timeZone: zone,
      dateStyle: "medium",
      timeStyle: "short",
    }).format(parsed);
  } catch {
    return `${parsed.toISOString().replace("T", " ").slice(0, 16)} (raw — ${zone} is unknown here)`;
  }
}

export function BackupSchedulesPanel({ onRan }: { onRan?: () => void }) {
  const [rows, setRows] = useState<BackupSchedule[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [editing, setEditing] = useState<{ id: string | null; draft: Draft } | null>(null);

  const reload = useCallback(() => {
    setLoading(true);
    fetchBackupSchedules()
      .then(setRows)
      .catch((cause: unknown) =>
        setError(cause instanceof ApiError ? cause.message : "The schedules could not be read."),
      )
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    reload();
  }, [reload]);

  async function save() {
    if (!editing) return;
    setBusy("save");
    setError(null);
    setNotice(null);
    const { id, draft } = editing;
    // The editor's `at_time` is `HH:MM` and the row's is `HH:MM:SS`; the API accepts both
    // and the panel renders the row's, so nothing is normalised here.
    const body: BackupScheduleInput = {
      name: draft.name.trim(),
      frequency: draft.frequency,
      // Hourly has no time of day of its own, and the API refuses one that is sent anyway
      // rather than storing a field the worker would have to ignore.
      at_time: draft.frequency === "hourly" ? null : draft.at_time,
      day_of_week: draft.frequency === "weekly" ? draft.day_of_week : null,
      day_of_month: draft.frequency === "monthly" ? draft.day_of_month : null,
      timezone: draft.timezone,
      scopes: draft.scopes,
      retention_count: draft.retention_count,
      enabled: draft.enabled,
    };
    try {
      const saved = id
        ? await updateBackupSchedule(id, body)
        : await createBackupSchedule(body);
      setNotice(
        id
          ? `"${saved.name}" saved. Next run ${localRun(saved.next_run_at, saved.timezone)} ${saved.timezone}.`
          : `"${saved.name}" created. Next run ${localRun(saved.next_run_at, saved.timezone)} ${saved.timezone}.`,
      );
      setEditing(null);
      reload();
    } catch (cause) {
      // The server's message verbatim. A refused timezone or a duplicate name comes back
      // with the constraint's own wording, and rewriting it here would be a second
      // definition of the same rule in a place that cannot enforce it.
      setError(cause instanceof ApiError ? cause.message : "The schedule could not be saved.");
    } finally {
      setBusy(null);
    }
  }

  async function runNow(row: BackupSchedule) {
    setBusy(row.id);
    setError(null);
    setNotice(null);
    try {
      const result = await runBackupScheduleNow(row.id);
      const failed = result.parts.filter((part) => part.status === "failed");
      setNotice(
        failed.length === 0
          ? `"${row.name}" ran: all ${result.parts.length} parts were written. Its next scheduled run is unchanged.`
          : `"${row.name}" ran: ${result.parts.length - failed.length} of ${result.parts.length} parts were written. ` +
              failed.map((part) => `${part.part}: ${part.error ?? "failed"}`).join(" "),
      );
      onRan?.();
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "That schedule could not be run.");
    } finally {
      setBusy(null);
    }
  }

  async function setEnabled(row: BackupSchedule, enabled: boolean) {
    setBusy(row.id);
    setError(null);
    setNotice(null);
    try {
      const saved = await updateBackupSchedule(row.id, { ...draftOf(row), enabled });
      setNotice(
        enabled
          ? `"${saved.name}" resumed. Next run ${localRun(saved.next_run_at, saved.timezone)} ${saved.timezone}.`
          : `"${saved.name}" paused. Its runs are kept; it will not produce another until you resume it.`,
      );
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "That schedule could not be changed.");
    } finally {
      setBusy(null);
    }
  }

  async function remove(row: BackupSchedule) {
    setBusy(row.id);
    setError(null);
    setNotice(null);
    try {
      await deleteBackupSchedule(row.id);
      // The sentence names what survives, because "deleted" and "your restore points are
      // gone" are different outcomes and only the first is true.
      setNotice(
        `"${row.name}" was removed. The backups it produced are still here — deleting a schedule stops future runs, it does not delete history.`,
      );
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "That schedule could not be removed.");
    } finally {
      setBusy(null);
    }
  }

  const draft = editing?.draft;
  const canSave =
    draft !== undefined &&
    draft.name.trim().length > 0 &&
    draft.name.trim().length <= 64 &&
    draft.scopes.length > 0 &&
    draft.retention_count >= 1 &&
    draft.retention_count <= 365;

  return (
    <section className="mt-6" data-testid="backup-schedules">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="flex items-center gap-2 text-[15px] font-medium">
            <CalendarClock className="h-4 w-4" aria-hidden />
            Schedules
          </h2>
          <p className="mt-0.5 text-[12.5px] text-muted">
            A schedule takes a backup on its own. The worker checks every minute, and a paused
            schedule keeps every run it has already produced.
          </p>
        </div>
        <button
          type="button"
          onClick={() => setEditing({ id: null, draft: emptyDraft() })}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium hover:bg-panel"
          data-testid="backup-schedule-new"
        >
          <Plus className="h-3.5 w-3.5" aria-hidden />
          New schedule
        </button>
      </div>

      {error ? (
        <p
          role="alert"
          className="mt-3 rounded-lg border border-bad/30 bg-bad/5 px-3 py-2 text-[12.5px] text-bad"
        >
          {error}
        </p>
      ) : null}
      {notice ? (
        <p
          role="status"
          className="mt-3 rounded-lg border border-ok/30 bg-ok/5 px-3 py-2 text-[12.5px] text-ok"
        >
          {notice}
        </p>
      ) : null}

      {editing && draft ? (
        <div
          className="mt-3 rounded-xl border border-line bg-panel p-4"
          data-testid="backup-schedule-editor"
        >
          <div className="flex items-center justify-between">
            <p className="text-[13px] font-medium">
              {editing.id ? "Edit schedule" : "New schedule"}
            </p>
            <button
              type="button"
              onClick={() => setEditing(null)}
              className="rounded p-1 text-muted hover:bg-surface"
              aria-label="Close the schedule editor"
            >
              <X className="h-4 w-4" aria-hidden />
            </button>
          </div>

          <div className="mt-3 grid grid-cols-1 gap-3 sm:grid-cols-2">
            <label className="block text-[12.5px]">
              <span className="font-medium">Name</span>
              <input
                value={draft.name}
                onChange={(event) =>
                  setEditing({ ...editing, draft: { ...draft, name: event.target.value } })
                }
                maxLength={64}
                placeholder="Nightly"
                className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5"
                data-testid="backup-schedule-name"
              />
            </label>

            <label className="block text-[12.5px]">
              <span className="font-medium">Frequency</span>
              <select
                value={draft.frequency}
                onChange={(event) =>
                  setEditing({
                    ...editing,
                    draft: { ...draft, frequency: event.target.value as Frequency },
                  })
                }
                className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5"
                data-testid="backup-schedule-frequency"
              >
                <option value="hourly">Every hour</option>
                <option value="daily">Every day</option>
                <option value="weekly">Every week</option>
                <option value="monthly">Every month</option>
              </select>
            </label>

            {/* Conditional fields: shown only when the frequency can use them, because a
                hidden-but-submitted day-of-week is a value the server has to decide about. */}
            {draft.frequency !== "hourly" ? (
              <label className="block text-[12.5px]">
                <span className="font-medium">Time of day</span>
                <input
                  type="time"
                  value={draft.at_time}
                  onChange={(event) =>
                    setEditing({ ...editing, draft: { ...draft, at_time: event.target.value } })
                  }
                  className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5"
                  data-testid="backup-schedule-time"
                />
              </label>
            ) : null}

            {draft.frequency === "weekly" ? (
              <label className="block text-[12.5px]">
                <span className="font-medium">Day of the week</span>
                <select
                  value={draft.day_of_week}
                  onChange={(event) =>
                    setEditing({
                      ...editing,
                      draft: { ...draft, day_of_week: Number(event.target.value) },
                    })
                  }
                  className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5"
                  data-testid="backup-schedule-weekday"
                >
                  {WEEKDAYS.map((day, index) => (
                    <option key={day} value={index}>
                      {day}
                    </option>
                  ))}
                </select>
              </label>
            ) : null}

            {draft.frequency === "monthly" ? (
              <label className="block text-[12.5px]">
                <span className="font-medium">Day of the month</span>
                {/* 1–28 because the schema refuses anything higher, and a picker that
                    offers the 29th–31st is a form that can be submitted and then rejected
                    by a rule the operator was never shown. */}
                <select
                  value={draft.day_of_month}
                  onChange={(event) =>
                    setEditing({
                      ...editing,
                      draft: { ...draft, day_of_month: Number(event.target.value) },
                    })
                  }
                  className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5"
                  data-testid="backup-schedule-dayofmonth"
                >
                  {Array.from({ length: 28 }, (_, index) => index + 1).map((day) => (
                    <option key={day} value={day}>
                      {day}
                    </option>
                  ))}
                </select>
                <span className="mt-1 block text-[11.5px] text-muted">
                  Up to the 28th, so every month has that day.
                </span>
              </label>
            ) : null}

            <label className="block text-[12.5px]">
              <span className="font-medium">Timezone</span>
              <select
                value={draft.timezone}
                onChange={(event) =>
                  setEditing({ ...editing, draft: { ...draft, timezone: event.target.value } })
                }
                className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5"
                data-testid="backup-schedule-zone"
              >
                {ZONES.map((zone) => (
                  <option key={zone} value={zone}>
                    {zone}
                  </option>
                ))}
              </select>
            </label>

            <label className="block text-[12.5px]">
              <span className="font-medium">Keep this many runs</span>
              <input
                type="number"
                min={1}
                max={365}
                value={draft.retention_count}
                onChange={(event) =>
                  setEditing({
                    ...editing,
                    draft: { ...draft, retention_count: Number(event.target.value) },
                  })
                }
                className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5"
                data-testid="backup-schedule-retention"
              />
            </label>
          </div>

          <fieldset className="mt-3">
            <legend className="text-[12.5px] font-medium">Parts</legend>
            <div className="mt-1.5 flex flex-wrap gap-2">
              {PARTS.map((part) => {
                const on = draft.scopes.includes(part);
                return (
                  <label
                    key={part}
                    className={`inline-flex cursor-pointer items-center gap-1.5 rounded-lg border px-2.5 py-1 text-[12px] ${
                      on ? "border-accent/40 bg-accent/5" : "border-line"
                    }`}
                  >
                    <input
                      type="checkbox"
                      checked={on}
                      onChange={(event) =>
                        setEditing({
                          ...editing,
                          draft: {
                            ...draft,
                            scopes: event.target.checked
                              ? [...draft.scopes, part]
                              : draft.scopes.filter((item) => item !== part),
                          },
                        })
                      }
                      className="accent-current"
                    />
                    {part}
                  </label>
                );
              })}
            </div>
            {draft.scopes.length === 0 ? (
              <p className="mt-1.5 text-[11.5px] text-bad">
                A schedule needs at least one part — a backup of nothing is not a backup.
              </p>
            ) : null}
          </fieldset>

          <label className="mt-3 flex items-center gap-2 text-[12.5px]">
            <input
              type="checkbox"
              checked={draft.enabled}
              onChange={(event) =>
                setEditing({ ...editing, draft: { ...draft, enabled: event.target.checked } })
              }
              className="accent-current"
              data-testid="backup-schedule-enabled"
            />
            Start enabled
          </label>

          <div className="mt-4 flex flex-wrap items-center gap-2">
            <button
              type="button"
              onClick={save}
              disabled={!canSave || busy === "save"}
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-50"
              data-testid="backup-schedule-save"
            >
              {busy === "save" ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
              Save schedule
            </button>
            {draft.name.trim().length === 0 ? (
              <span className="text-[11.5px] text-muted">A schedule needs a name.</span>
            ) : null}
          </div>
        </div>
      ) : null}

      {loading ? (
        <div className="mt-3">
          <LoadingTable columns={6} rows={2} />
        </div>
      ) : rows.length === 0 ? (
        <div className="mt-3">
          <EmptyState
            title="No schedules"
            hint="Backups only happen when you press the button, or on a schedule. A schedule takes one on its own and keeps the last few."
            action={
              <button
                type="button"
                onClick={() => setEditing({ id: null, draft: emptyDraft() })}
                className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
              >
                <Plus className="h-3.5 w-3.5" aria-hidden />
                New schedule
              </button>
            }
          />
        </div>
      ) : (
        <div className="mt-3 overflow-x-auto rounded-xl border border-line">
          <table className="w-full min-w-[820px] text-left text-[12.5px]">
            <thead className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
              <tr>
                <th className="px-3 py-2 font-medium">Name</th>
                <th className="px-3 py-2 font-medium">Frequency</th>
                <th className="px-3 py-2 font-medium">Next run</th>
                <th className="px-3 py-2 font-medium">Last run</th>
                <th className="px-3 py-2 font-medium">Parts</th>
                <th className="px-3 py-2 text-right font-medium">Actions</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr key={row.id} className="border-b border-line last:border-0" data-schedule={row.id}>
                  <td className="px-3 py-2">
                    <span className="font-medium">{row.name}</span>
                    {!row.enabled ? (
                      <span className="ml-2 text-[11px] text-muted">paused</span>
                    ) : null}
                  </td>
                  <td className="px-3 py-2 text-muted">{row.cadence}</td>
                  <td className="px-3 py-2">
                    {row.enabled ? (
                      localRun(row.next_run_at, row.timezone)
                    ) : (
                      <span className="text-muted">paused</span>
                    )}
                    <span className="ml-1.5 text-[11px] text-muted">{row.timezone}</span>
                    {/*
                      An enabled schedule with no next run is a defect, and it is rendered as
                      one. The column was shipped with the table and nothing wrote it for two
                      slices; a blank cell would have hidden that, and the sentence names it.
                    */}
                    {row.enabled && !row.next_run_at ? (
                      <span className="mt-0.5 block text-[11px] text-bad">
                        no next run computed — this schedule will not fire until it is saved again
                      </span>
                    ) : null}
                  </td>
                  <td className="px-3 py-2 text-muted">
                    {row.last_run_at ? localRun(row.last_run_at, row.timezone) : "never"}
                  </td>
                  <td className="px-3 py-2 text-muted">
                    {row.scopes.length === PARTS.length ? "all five" : row.scopes.join(", ")}
                  </td>
                  <td className="px-3 py-2">
                    <div className="flex items-center justify-end gap-1">
                      <button
                        type="button"
                        onClick={() => runNow(row)}
                        disabled={busy === row.id}
                        className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12px] hover:bg-panel disabled:opacity-50"
                        title="Take a backup now. This does not change the next scheduled run."
                        data-testid={`backup-schedule-run-${row.id}`}
                      >
                        <Play className="h-3.5 w-3.5" aria-hidden />
                        Run now
                      </button>
                      <button
                        type="button"
                        onClick={() => setEnabled(row, !row.enabled)}
                        disabled={busy === row.id}
                        className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12px] hover:bg-panel disabled:opacity-50"
                        title={row.enabled ? "Pause this schedule" : "Resume this schedule"}
                      >
                        {row.enabled ? (
                          <PowerOff className="h-3.5 w-3.5" aria-hidden />
                        ) : (
                          <Power className="h-3.5 w-3.5" aria-hidden />
                        )}
                        {row.enabled ? "Pause" : "Resume"}
                      </button>
                      <button
                        type="button"
                        onClick={() => setEditing({ id: row.id, draft: draftOf(row) })}
                        className="rounded-lg border border-line p-1.5 hover:bg-panel"
                        aria-label={`Edit ${row.name}`}
                      >
                        <Pencil className="h-3.5 w-3.5" aria-hidden />
                      </button>
                      <button
                        type="button"
                        onClick={() => remove(row)}
                        disabled={busy === row.id}
                        className="rounded-lg border border-line p-1.5 text-bad hover:bg-panel disabled:opacity-50"
                        aria-label={`Delete ${row.name}`}
                        data-testid={`backup-schedule-delete-${row.id}`}
                      >
                        <Trash2 className="h-3.5 w-3.5" aria-hidden />
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
