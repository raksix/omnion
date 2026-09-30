"use client";

/**
 * `/notifications/settings` — the reader's own channel configuration (REQ-021, slice 2).
 *
 * Three claims this screen makes, each one a way a preferences form lies to the person who is
 * relying on it:
 *
 * 1. **The matrix is rendered from what the server sent, not from a grid the panel builds.**
 *    Two copies of the closed vocabulary in two languages is how a channel ends up in the
 *    filter and not in the form — and the failure is a hole in the grid, which looks exactly
 *    like an unchecked box.
 * 2. **Only what the reader changed is sent.** The server stores *stated* cells, so a save
 *    that posted the whole grid would be storing thirty rows per user and turning the default
 *    into a backfill. The screen diffs against the loaded state and posts the difference,
 *    which is also what makes "2 preferences saved" an honest number.
 * 3. **The in-app column is locked, and the lock is the server's rule.** The checkbox is
 *    disabled and says why; the server refuses the write anyway. A form that only disabled it
 *    would let a hand-rolled client turn the bell off, and the next question would be "why
 *    can't I find my notifications".
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { Bell, BellOff, Clock, Lock, Mail, RefreshCw, Save, TriangleAlert } from "lucide-react";

import {
  fetchNotificationPreferences,
  saveNotificationPreferences,
  type ApiError,
} from "@/lib/api";
import {
  DIGEST_CADENCES,
  DIGEST_WEEKDAYS,
  NOTIFICATION_CATEGORIES,
  NOTIFICATION_CHANNELS,
  NOTIFICATION_TIMEZONES,
  type NotificationPreferenceCell,
  type NotificationSettingsRow,
} from "@/lib/types";

/** A cell keyed the way the form looks it up, so the diff is two maps and a loop. */
type CellKey = `${string}/${string}`;

const key = (category: string, channel: string): CellKey => `${category}/${channel}`;

const CATEGORY_LINE: Record<string, string> = {
  approval: "Approvals",
  security: "Security alerts",
  update: "Updates",
  ticket: "Tickets",
  system: "System",
  mention: "Mentions",
};

/**
 * How each channel is named in prose, wherever a channel is shown to a person.
 *
 * **One map for three screens** — the settings matrix, the preference form and the delivery
 * rows in the detail drawer. "In-app" spelled three ways is one of them wrong within a month,
 * and a reader who sees "In app" on one screen and "In-app" on the next reasonably concludes
 * they are different channels. Exported rather than redeclared per screen, which is the whole
 * point: this file is where the vocabulary lives.
 */
export const CHANNEL_LINE: Record<string, string> = {
  in_app: "In-app",
  email: "E-mail",
  web_push: "Web Push",
  webhook: "Webhook",
  chat: "Chat",
};

const CHANNEL_ICON: Record<string, typeof Bell> = {
  in_app: Bell,
  email: Mail,
  web_push: Bell,
  webhook: RefreshCw,
  chat: Bell,
};

const CADENCE_LINE: Record<string, string> = {
  off: "Off — send each notification as it happens",
  daily: "Daily — one e-mail a day with everything unread",
  weekly: "Weekly — one e-mail a week with everything unread",
};

const HOURS = Array.from({ length: 24 }, (_, hour) => hour);

function hourLabel(hour: number): string {
  return `${String(hour).padStart(2, "0")}:00`;
}

/** The settings row a reader starts from, matching the server's defaults exactly. */
function initialSettings(): NotificationSettingsRow {
  return {
    quiet_hours_start: null,
    quiet_hours_end: null,
    timezone: "UTC",
    digest_cadence: "off",
    digest_weekday: null,
    digest_hour: 8,
  };
}

/**
 * The settings row as it would be *saved*.
 *
 * One function, used by both the dirty check and the save, because those two answering
 * differently is how a reader gets a Save button that says there is nothing to save and then
 * saves something. The `showQuietHours` flag is a form-only state — the row itself stores
 * `null`, so the flag has to be resolved here rather than at each of the two call sites.
 */
function effectiveSettings(
  settings: NotificationSettingsRow,
  showQuietHours: boolean,
): NotificationSettingsRow {
  return {
    ...settings,
    quiet_hours_start: showQuietHours ? (settings.quiet_hours_start ?? "22:00") : null,
    quiet_hours_end: showQuietHours ? (settings.quiet_hours_end ?? "07:00") : null,
    // A weekly digest with no weekday is refused by the server, so Monday is filled in here
    // rather than making the reader pick one before a save that would work.
    digest_weekday: settings.digest_cadence === "weekly" ? (settings.digest_weekday ?? 0) : null,
  };
}

export function NotificationSettings() {
  const [loaded, setLoaded] = useState<Map<CellKey, boolean> | null>(null);
  const [loadedSettings, setLoadedSettings] = useState<NotificationSettingsRow | null>(null);
  const [draft, setDraft] = useState<Map<CellKey, boolean>>(new Map());
  const [settings, setSettings] = useState<NotificationSettingsRow>(initialSettings);
  const [lockedChannel, setLockedChannel] = useState("in_app");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [showQuietHours, setShowQuietHours] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const answer = await fetchNotificationPreferences();
      const asMap = new Map(answer.cells.map((cell) => [key(cell.category, cell.channel), cell.enabled]));
      setLoaded(asMap);
      setDraft(new Map(asMap));
      setSettings(answer.settings);
      setLoadedSettings(answer.settings);
      setLockedChannel(answer.locked_channel);
      setShowQuietHours(Boolean(answer.settings.quiet_hours_start));
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  /**
   * What the reader changed, and nothing else.
   *
   * A cell that was already at the value it is now does not go in the body. This is what the
   * server means by "stated", and it is why the save is safe for a reader who has never
   * opened this screen: the payload is empty and the platform defaults still apply.
   */
  const changedCells = useMemo((): NotificationPreferenceCell[] => {
    if (!loaded) return [];
    const cells: NotificationPreferenceCell[] = [];
    for (const [cellKey, value] of draft) {
      if (loaded.get(cellKey) !== value) {
        const [category, channel] = cellKey.split("/");
        cells.push({ category, channel, enabled: value });
      }
    }
    return cells;
  }, [draft, loaded]);

  const settingsChanged = useMemo(() => {
    // Diff against the row the server sent, not against a hand-written list of defaults.
    // The hand-written version has to be kept in step with `initialSettings()` and the
    // server's own defaults, and a third copy of "what a fresh account looks like" is a
    // third copy that will drift — the drift shows as a Save button that is always enabled
    // for a reader who has already saved, which is how people learn to ignore it.
    const effective = effectiveSettings(settings, showQuietHours);
    if (!loadedSettings) return true;
    return (
      effective.quiet_hours_start !== loadedSettings.quiet_hours_start ||
      effective.quiet_hours_end !== loadedSettings.quiet_hours_end ||
      effective.timezone !== loadedSettings.timezone ||
      effective.digest_cadence !== loadedSettings.digest_cadence ||
      effective.digest_weekday !== loadedSettings.digest_weekday ||
      effective.digest_hour !== loadedSettings.digest_hour
    );
  }, [settings, showQuietHours, loadedSettings]);

  const dirty = changedCells.length > 0 || settingsChanged;

  const toggle = (category: string, channel: string) => {
    const cellKey = key(category, channel);
    setDraft((previous) => {
      const next = new Map(previous);
      next.set(cellKey, !previous.get(cellKey));
      return next;
    });
  };

  const save = async () => {
    setSaving(true);
    setError(null);
    setNotice(null);
    try {
      const answer = await saveNotificationPreferences({
        cells: changedCells,
        settings: effectiveSettings(settings, showQuietHours),
      });
      const asMap = new Map(answer.cells.map((cell) => [key(cell.category, cell.channel), cell.enabled]));
      setLoaded(asMap);
      setDraft(new Map(asMap));
      setSettings(answer.settings);
      setLoadedSettings(answer.settings);
      setLockedChannel(answer.locked_channel);
      setNotice(
        answer.changed === 0
          ? "No preference changed — everything you set is already saved"
          : `${answer.changed} ${answer.changed === 1 ? "preference" : "preferences"} saved`,
      );
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setSaving(false);
    }
  };

  if (loading) return <SettingsSkeleton />;
  if (error && !loaded) {
    return (
      <div className="space-y-3" data-pref-state="error">
        <p className="text-[13px] text-red-700 dark:text-red-300">{error}</p>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-[13px]"
        >
          <RefreshCw className="h-4 w-4" aria-hidden />
          Retry
        </button>
      </div>
    );
  }

  return (
    <div className="space-y-6" data-pref-state="ready">
      {/* ---------------------------------------------------------------- the matrix */}
      <section aria-labelledby="matrix-heading" className="space-y-3">
        <div className="flex flex-wrap items-baseline justify-between gap-2">
          <h2 id="matrix-heading" className="text-[15px] font-semibold">
            Channels per category
          </h2>
          <p className="text-[12px] text-muted">
            Each row is a kind of notification; each column is a way it can reach you.
          </p>
        </div>

        <div className="overflow-x-auto rounded-lg border border-line">
          <table className="w-full min-w-[640px] border-collapse text-[13px]">
            <caption className="sr-only">
              Which channels reach you for each kind of notification
            </caption>
            <thead>
              <tr className="border-b border-line bg-quiet-soft">
                <th scope="col" className="px-3 py-2 text-left font-medium">
                  Category
                </th>
                {NOTIFICATION_CHANNELS.map((channel) => {
                  const Icon = CHANNEL_ICON[channel] ?? Bell;
                  const locked = channel === lockedChannel;
                  return (
                    <th key={channel} scope="col" className="px-3 py-2 text-center font-medium">
                      <span className="inline-flex items-center gap-1.5">
                        <Icon className="h-3.5 w-3.5" aria-hidden />
                        {CHANNEL_LINE[channel] ?? channel}
                        {locked ? (
                          <Lock className="h-3 w-3 text-muted" aria-label="always on" />
                        ) : null}
                      </span>
                    </th>
                  );
                })}
              </tr>
            </thead>
            <tbody>
              {NOTIFICATION_CATEGORIES.map((category) => (
                <tr key={category} className="border-b border-line last:border-0">
                  <th scope="row" className="px-3 py-2 text-left font-normal">
                    {CATEGORY_LINE[category] ?? category}
                  </th>
                  {NOTIFICATION_CHANNELS.map((channel) => {
                    const locked = channel === lockedChannel;
                    const cellKey = key(category, channel);
                    const on = locked ? true : (draft.get(cellKey) ?? true);
                    return (
                      <td key={channel} className="px-3 py-2 text-center">
                        <input
                          type="checkbox"
                          checked={on}
                          disabled={locked}
                          onChange={() => toggle(category, channel)}
                          data-cell={cellKey}
                          aria-label={`${CATEGORY_LINE[category] ?? category} over ${CHANNEL_LINE[channel] ?? channel}`}
                          className="h-4 w-4 accent-[var(--accent)] disabled:cursor-not-allowed disabled:opacity-60"
                        />
                      </td>
                    );
                  })}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <p className="flex items-start gap-2 text-[12px] text-muted">
          <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden />
          In-app notifications cannot be turned off. They are the panel&apos;s own channel: a
          notification you cannot see there is a row that exists only for a database.
        </p>
      </section>

      {/* ------------------------------------------------------------- quiet hours */}
      <section aria-labelledby="quiet-heading" className="space-y-3">
        <h2 id="quiet-heading" className="flex items-center gap-2 text-[15px] font-semibold">
          <Clock className="h-4 w-4" aria-hidden />
          Quiet hours
        </h2>
        <label className="flex items-start gap-3 text-[13px]">
          <input
            type="checkbox"
            checked={showQuietHours}
            onChange={(event) => {
              setShowQuietHours(event.target.checked);
              if (event.target.checked) {
                setSettings((previous) => ({
                  ...previous,
                  quiet_hours_start: previous.quiet_hours_start ?? "22:00",
                  quiet_hours_end: previous.quiet_hours_end ?? "07:00",
                }));
              }
            }}
            data-quiet-toggle
            className="mt-0.5 h-4 w-4 accent-[var(--accent)]"
          />
          <span>
            Do not send anything outside these hours
            <span className="mt-0.5 block text-[12px] text-muted">
              A window may cross midnight — 22:00 to 07:00 is the usual one.
            </span>
          </span>
        </label>

        {showQuietHours ? (
          <div className="flex flex-wrap items-end gap-4">
            <label className="text-[13px]">
              <span className="mb-1 block text-[12px] text-muted">From</span>
              <input
                type="time"
                value={settings.quiet_hours_start ?? "22:00"}
                onChange={(event) =>
                  setSettings((previous) => ({ ...previous, quiet_hours_start: event.target.value }))
                }
                data-quiet-start
                className="rounded-md border border-line bg-transparent px-2 py-1.5"
              />
            </label>
            <label className="text-[13px]">
              <span className="mb-1 block text-[12px] text-muted">Until</span>
              <input
                type="time"
                value={settings.quiet_hours_end ?? "07:00"}
                onChange={(event) =>
                  setSettings((previous) => ({ ...previous, quiet_hours_end: event.target.value }))
                }
                data-quiet-end
                className="rounded-md border border-line bg-transparent px-2 py-1.5"
              />
            </label>
            <label className="text-[13px]">
              <span className="mb-1 block text-[12px] text-muted">Timezone</span>
              <select
                value={settings.timezone}
                onChange={(event) =>
                  setSettings((previous) => ({ ...previous, timezone: event.target.value }))
                }
                data-timezone
                className="rounded-md border border-line bg-transparent px-2 py-1.5"
              >
                {/* A zone the form does not list is still shown, rather than silently reset to
                    UTC on the next save: a reader who typed one in by hand should not lose it. */}
                {NOTIFICATION_TIMEZONES.includes(
                  settings.timezone as (typeof NOTIFICATION_TIMEZONES)[number],
                ) ? null : (
                  <option value={settings.timezone}>{settings.timezone} (read as UTC)</option>
                )}
                {NOTIFICATION_TIMEZONES.map((zone) => (
                  <option key={zone} value={zone}>
                    {zone}
                  </option>
                ))}
              </select>
            </label>
          </div>
        ) : null}
      </section>

      {/* ------------------------------------------------------------------ digest */}
      <section aria-labelledby="digest-heading" className="space-y-3">
        <h2 id="digest-heading" className="flex items-center gap-2 text-[15px] font-semibold">
          <Mail className="h-4 w-4" aria-hidden />
          Digest
        </h2>
        <label className="block text-[13px]">
          <span className="mb-1 block text-[12px] text-muted">How often</span>
          <select
            value={settings.digest_cadence}
            onChange={(event) =>
              setSettings((previous) => ({ ...previous, digest_cadence: event.target.value }))
            }
            data-digest-cadence
            className="rounded-md border border-line bg-transparent px-2 py-1.5"
          >
            {DIGEST_CADENCES.map((cadence) => (
              <option key={cadence} value={cadence}>
                {CADENCE_LINE[cadence]}
              </option>
            ))}
          </select>
        </label>

        {settings.digest_cadence !== "off" ? (
          <div className="flex flex-wrap items-end gap-4">
            {settings.digest_cadence === "weekly" ? (
              <label className="text-[13px]">
                <span className="mb-1 block text-[12px] text-muted">On</span>
                <select
                  value={settings.digest_weekday ?? 0}
                  onChange={(event) =>
                    setSettings((previous) => ({
                      ...previous,
                      digest_weekday: Number(event.target.value),
                    }))
                  }
                  data-digest-weekday
                  className="rounded-md border border-line bg-transparent px-2 py-1.5"
                >
                  {DIGEST_WEEKDAYS.map((day, index) => (
                    <option key={day} value={index}>
                      {day}
                    </option>
                  ))}
                </select>
              </label>
            ) : null}
            <label className="text-[13px]">
              <span className="mb-1 block text-[12px] text-muted">At</span>
              <select
                value={settings.digest_hour}
                onChange={(event) =>
                  setSettings((previous) => ({
                    ...previous,
                    digest_hour: Number(event.target.value),
                  }))
                }
                data-digest-hour
                className="rounded-md border border-line bg-transparent px-2 py-1.5"
              >
                {HOURS.map((hour) => (
                  <option key={hour} value={hour}>
                    {hourLabel(hour)}
                  </option>
                ))}
              </select>
            </label>
          </div>
        ) : null}
      </section>

      {/* -------------------------------------------------------------------- save */}
      <div className="flex flex-wrap items-center gap-3 border-t border-line pt-4">
        <button
          type="button"
          onClick={() => void save()}
          disabled={saving || !dirty}
          data-pref-save
          className="inline-flex min-h-9 items-center gap-2 rounded-md bg-[var(--accent)] px-3 py-2 text-[13px] text-white disabled:opacity-50"
        >
          <Save className="h-4 w-4" aria-hidden />
          {saving ? "Saving…" : "Save preferences"}
        </button>
        {notice ? (
          <p className="text-[13px] text-muted" data-pref-notice>
            {notice}
          </p>
        ) : null}
        {error ? (
          <p className="text-[13px] text-red-700 dark:text-red-300" data-pref-error>
            {error}
          </p>
        ) : null}
      </div>
    </div>
  );
}

/**
 * The loading state, shaped like the form it replaces.
 *
 * A spinner where a grid is coming tells the reader nothing about how much is on the way; a
 * greyed grid of the right shape is the form arriving. The cell count is the real one
 * (categories × channels) so the skeleton does not resize when the data lands.
 */
function SettingsSkeleton() {
  return (
    <div className="space-y-6" data-pref-state="loading" aria-busy="true">
      <div className="space-y-2">
        <div className="h-4 w-48 animate-pulse rounded bg-quiet-soft" />
        <div className="h-40 animate-pulse rounded-lg bg-quiet-soft" />
      </div>
      <div className="h-4 w-32 animate-pulse rounded bg-quiet-soft" />
      <div className="h-4 w-56 animate-pulse rounded bg-quiet-soft" />
    </div>
  );
}

/** The empty-channel hint, used by the list screen when every non-in-app channel is off. */
export function EverythingMutedHint() {
  return (
    <p className="flex items-center gap-2 text-[12px] text-muted">
      <BellOff className="h-3.5 w-3.5" aria-hidden />
      Every channel except in-app is off for this category.
    </p>
  );
}
