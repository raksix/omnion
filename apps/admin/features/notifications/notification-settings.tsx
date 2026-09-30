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
import {
  Bell,
  BellOff,
  CircleCheck,
  Clock,
  Lock,
  Mail,
  RefreshCw,
  Save,
  Send,
  TriangleAlert,
} from "lucide-react";

import {
  fetchNotificationChannels,
  fetchNotificationPreferences,
  saveNotificationPreferences,
  sendTestNotificationDelivery,
  type ApiError,
} from "@/lib/api";
import {
  DIGEST_CADENCES,
  DIGEST_WEEKDAYS,
  NOTIFICATION_CATEGORIES,
  NOTIFICATION_CHANNELS,
  NOTIFICATION_TIMEZONES,
  type NotificationChannelReadiness,
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

/**
 * The channels a test can actually go over, in the order the screen lists them.
 *
 * **Derived from the channel vocabulary rather than retyped.** `in_app` is filtered out
 * because it cannot be tested (the row *is* the notification), and everything else in the
 * closed list is offered — including the ones this installation cannot send over, because a
 * button that returns the server's reason teaches more than a button that is simply missing.
 * A hard-coded list of two would go stale the day a connector ships.
 */
const TESTABLE_CHANNELS = NOTIFICATION_CHANNELS.filter((channel) => channel !== "in_app");

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
  // Channel readiness and the in-flight test, kept here rather than in a child because the
  // test *is* about the matrix above it: the button that sends is per channel, and the reason
  // a channel cannot be tested belongs next to the row that proves the other channels can.
  const [readiness, setReadiness] = useState<NotificationChannelReadiness[]>([]);
  const [testing, setTesting] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<{
    channel: string;
    delivered: boolean;
    detail: string;
  } | null>(null);

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
   * Channel readiness, fetched on its own rather than folded into `load`.
   *
   * **A screen that cannot render must still be able to answer "is e-mail working?"** — so
   * this read is deliberately not part of the critical path: it fails into an empty list
   * rather than into the form's error state, which would replace a working preferences matrix
   * with a red banner because a *different* endpoint said no. The buttons fall back to "test
   * it and see" when readiness is unknown, which is the honest thing to offer.
   */
  useEffect(() => {
    void (async () => {
      try {
        setReadiness(await fetchNotificationChannels());
      } catch {
        setReadiness([]);
      }
    })();
  }, []);

  /**
   * Send one real notification through one channel and show what the transport said.
   *
   * The button is never disabled for an unavailable channel: pressing it produces the reason
   * in the server's own words, which is more useful than a greyed button whose tooltip is the
   * only place the answer exists. What *is* disabled is the channel already in flight, so a
   * double click cannot queue two sends whose results race each other in the same line.
   */
  const runTest = async (channel: string) => {
    setTesting(channel);
    setTestResult(null);
    try {
      const answer = await sendTestNotificationDelivery({ channel });
      setTestResult({
        channel: answer.channel,
        delivered: answer.delivered,
        detail: answer.detail,
      });
    } catch (caught) {
      setTestResult({
        channel,
        delivered: false,
        detail: (caught as ApiError).message,
      });
    } finally {
      setTesting(null);
    }
  };

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

      {/* ------------------------------------------------------------- test delivery */}
      {/*
        **The section the request named and the platform could not draw.** REQ-021's own API
        table lists `POST /notifications/preferences/test` and its screen spec says "test
        delivery", and until this tick neither existed — so the one control a reader reaches
        for when asking "did my e-mail actually go out?" was missing, and no acceptance box
        could be ticked about it.

        **One row per channel, and the in-app row explains itself instead of offering a
        button.** In-app cannot be tested because the test notification *is* an in-app
        notification: a green "delivered" there would be the platform proving it can write to
        a table it owns, which is true of every installation and tells nobody anything.
      */}
      <section aria-labelledby="test-heading" className="space-y-3" data-test-delivery>
        <div className="flex flex-wrap items-baseline justify-between gap-2">
          <h2 id="test-heading" className="flex items-center gap-2 text-[15px] font-semibold">
            <Send className="h-4 w-4" aria-hidden />
            Test delivery
          </h2>
          <p className="text-[12px] text-muted">
            Sends a real notification over one channel and reports what the transport answered.
          </p>
        </div>

        <ul className="divide-y divide-line rounded-lg border border-line">
          {TESTABLE_CHANNELS.map((channel) => {
            const Icon = CHANNEL_ICON[channel] ?? Bell;
            const state = readiness.find((entry) => entry.channel === channel);
            const busy = testing === channel;
            return (
              <li
                key={channel}
                className="flex flex-wrap items-center gap-3 px-3 py-2.5"
                data-test-channel={channel}
              >
                <Icon className="h-4 w-4 shrink-0 text-muted" aria-hidden />
                <span className="min-w-32 text-[13px] font-medium">
                  {CHANNEL_LINE[channel] ?? channel}
                </span>
                {state ? (
                  state.available ? (
                    <span
                      className="inline-flex items-center gap-1 text-[12px] text-emerald-700 dark:text-emerald-400"
                      data-test-ready={channel}
                    >
                      <CircleCheck className="h-3.5 w-3.5" aria-hidden />
                      Ready
                    </span>
                  ) : (
                    <span
                      className="text-[12px] text-muted"
                      title={state.detail}
                      data-test-unavailable={channel}
                    >
                      {state.detail}
                    </span>
                  )
                ) : (
                  <span className="text-[12px] text-muted">Not checked</span>
                )}
                <button
                  type="button"
                  onClick={() => void runTest(channel)}
                  disabled={busy}
                  data-test-button={channel}
                  className="ml-auto inline-flex min-h-9 items-center gap-2 rounded-md border border-line px-3 py-1.5 text-[13px] disabled:opacity-50"
                >
                  {busy ? (
                    <RefreshCw className="h-3.5 w-3.5 animate-spin" aria-hidden />
                  ) : (
                    <Send className="h-3.5 w-3.5" aria-hidden />
                  )}
                  {busy ? "Sending…" : "Send a test"}
                </button>
              </li>
            );
          })}
        </ul>

        <p className="text-[12px] text-muted">
          In-app is not listed: it is always on, and a test over it would only prove the
          platform can write to its own database.
        </p>

        {testResult ? (
          <p
            role="status"
            data-test-result={testResult.channel}
            data-delivered={testResult.delivered ? "yes" : "no"}
            className={
              testResult.delivered
                ? "flex items-start gap-2 rounded-md border border-emerald-200 bg-emerald-50 px-3 py-2 text-[13px] text-emerald-900 dark:border-emerald-900 dark:bg-emerald-950/40 dark:text-emerald-200"
                : "flex items-start gap-2 rounded-md border border-red-200 bg-red-50 px-3 py-2 text-[13px] text-red-900 dark:border-red-900 dark:bg-red-950/40 dark:text-red-200"
            }
          >
            {testResult.delivered ? (
              <CircleCheck className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
            ) : (
              <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
            )}
            <span>
              <strong className="font-medium">
                {CHANNEL_LINE[testResult.channel] ?? testResult.channel}:{" "}
                {testResult.delivered ? "delivered" : "not delivered"}
              </strong>{" "}
              — {testResult.detail}
            </span>
          </p>
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
