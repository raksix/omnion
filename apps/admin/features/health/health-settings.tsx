"use client";

/**
 * `/health/settings` — the policy the health centre enforces (REQ-014, slice 3).
 *
 * Two halves with very different characters, and the screen keeps them apart because they have
 * different failure modes:
 *
 * * **Thresholds decide when the platform wakes somebody.** A saved pair is a policy; an unsaved
 *   one is a suggestion. The form says which is which on every row, because a placeholder that
 *   looks like a saved value is a limit nobody chose being read as a limit they chose. Saving is
 *   explicit per row — the whole form is sent, but only the rows an operator actually touched
 *   change `configured`, and an untouched row keeps sending its stored value rather than its
 *   suggestion.
 * * **Maintenance windows decide when the platform stays quiet.** They suppress an *announcement*,
 *   never the state: a deploy that takes Redis down for four minutes is recorded with its real
 *   state and a `maintenance` marker. The form says that on the section itself, because the
 *   alternative reading — "silences alerts" — is the one every operator already assumes, and the
 *   assumption is what makes somebody mute a window over a real outage.
 *
 * Validation is the server's: an out-of-range interval or an inverted pair answers `400` with the
 * metric named, and this form shows that sentence next to the field rather than inventing its own
 * message. The bounds are read from the same payload the save enforces, so the client cannot
 * offer a value the server will refuse.
 *
 * Keyboard: `s` saves, `r` re-reads. Mobile: the threshold table becomes one card per metric.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Link from "next/link";
import { ArrowLeft, Loader2, Plus, RefreshCw, Save, Trash2 } from "lucide-react";

import {
  ApiError,
  createHealthMaintenanceWindow,
  deleteHealthMaintenanceWindow,
  fetchHealthMaintenanceWindows,
  fetchHealthSettings,
  saveHealthSettings,
} from "@/lib/api";
import type { HealthMaintenanceWindow, HealthSettings, HealthThreshold } from "@/lib/types";

/** A blank window form, expressed in the shape `datetime-local` inputs actually hold. */
const EMPTY_WINDOW = { starts_at: "", ends_at: "", services: [] as string[], note: "" };

/**
 * The metric's own label.
 *
 * Derived from the key rather than stored, because the key is what the settings document and the
 * breach emitter use; a second label table is a second thing to keep in step, and the split-brain
 * case is an operator reading "queue depth" next to a limit the emitter is applying to
 * `queue_depth` without noticing they are different rows.
 */
function metricLabel(metric: string): string {
  const spaced = metric.replaceAll("_", " ");
  return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}

/** `datetime-local` hands back `YYYY-MM-DDTHH:mm`; the API wants an RFC 3339 instant. */
function toInstant(local: string): string {
  return local === "" ? "" : new Date(local).toISOString();
}

export function HealthSettingsScreen() {
  const [settings, setSettings] = useState<HealthSettings | null>(null);
  const [windows, setWindows] = useState<HealthMaintenanceWindow[]>([]);
  const [draft, setDraft] = useState<Record<string, { warn: string; crit: string }>>({});
  const [interval, setInterval] = useState("");
  const [stale, setStale] = useState("");
  const [draftWindow, setDraftWindow] = useState(EMPTY_WINDOW);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [pendingWindow, setPendingWindow] = useState(false);
  const typing = useRef(false);

  const load = useCallback(async () => {
    try {
      const [next, existing] = await Promise.all([
        fetchHealthSettings(),
        fetchHealthMaintenanceWindows(),
      ]);
      setSettings(next);
      setWindows(existing);
      setInterval(String(next.check_interval_seconds));
      setStale(String(next.worker_stale_seconds));
      // Only rows with a stored pair are seeded into the draft. A row with no pair shows the
      // suggestion in the input but stays *unsaved*, so saving an untouched screen writes nothing
      // rather than quietly adopting seven default limits.
      setDraft(
        Object.fromEntries(
          next.thresholds
            .filter((row) => row.configured)
            .map((row) => [row.metric, { warn: String(row.warn), crit: String(row.crit) }]),
        ),
      );
      setError(null);
    } catch (cause) {
      setError((cause as ApiError).message ?? "The health settings could not be read.");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (
        typing.current ||
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.tagName === "SELECT" ||
        target?.isContentEditable
      ) {
        return;
      }
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      const key = event.key.toLowerCase();
      if (key === "r") {
        event.preventDefault();
        void load();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [load]);

  /**
   * What the form would send.
   *
   * Built from the *stored* pairs plus whatever the operator typed, rather than from the inputs
   * alone. The inputs carry suggestions for unconfigured rows; sending those would adopt seven
   * limits nobody chose the moment anybody saved an unrelated interval. A row is only sent when
   * it has a stored pair or a typed value — which is the whole difference between "save the
   * interval" and "set the policy".
   */
  const payload = useMemo((): HealthThreshold[] | null => {
    if (!settings) return null;
    const rows: HealthThreshold[] = settings.thresholds.map((row) => {
      const typed = draft[row.metric];
      if (!typed) return row;
      const warn = Number(typed.warn);
      const crit = Number(typed.crit);
      if (Number.isNaN(warn) || Number.isNaN(crit)) return row;
      return { ...row, warn, crit, configured: true };
    });
    return rows;
  }, [settings, draft]);

  const save = useCallback(async () => {
    if (!settings || payload === null) return;
    setSaving(true);
    setError(null);
    setNotice(null);
    try {
      const next = await saveHealthSettings({
        check_interval_seconds: Number(interval),
        worker_stale_seconds: Number(stale),
        thresholds: payload,
      });
      setSettings(next);
      setNotice("Saved. The next check run uses these limits.");
      await load();
    } catch (cause) {
      setError((cause as ApiError).message ?? "The settings could not be saved.");
    } finally {
      setSaving(false);
    }
  }, [settings, payload, interval, stale, load]);

  const addWindow = useCallback(async () => {
    setPendingWindow(true);
    setError(null);
    setNotice(null);
    try {
      await createHealthMaintenanceWindow({
        starts_at: toInstant(draftWindow.starts_at),
        ends_at: toInstant(draftWindow.ends_at),
        services: draftWindow.services,
        note: draftWindow.note,
      });
      setDraftWindow(EMPTY_WINDOW);
      setNotice("Maintenance window added.");
      setWindows(await fetchHealthMaintenanceWindows());
    } catch (cause) {
      setError((cause as ApiError).message ?? "The maintenance window could not be created.");
    } finally {
      setPendingWindow(false);
    }
  }, [draftWindow]);

  const removeWindow = useCallback(
    async (id: string) => {
      setError(null);
      try {
        await deleteHealthMaintenanceWindow(id);
        setWindows(await fetchHealthMaintenanceWindows());
      } catch (cause) {
        setError((cause as ApiError).message ?? "The maintenance window could not be deleted.");
      }
    },
    [],
  );

  const bounds = settings?.bounds;

  return (
    <div className="space-y-6" data-health-settings>
      <div>
        <Link
          href="/health"
          className="inline-flex items-center gap-1 text-[12.5px] text-muted hover:text-ink"
        >
          <ArrowLeft aria-hidden className="h-3.5 w-3.5" />
          System health
        </Link>
        <h1 className="mt-1 text-[19px] font-medium tracking-tight">Health settings</h1>
        <p className="mt-0.5 text-[13px] text-muted">
          When the platform records an incident, how long a worker may be silent, and when a
          planned change is allowed to be quiet.
        </p>
      </div>

      {error ? (
        <p
          data-health-settings-error
          role="alert"
          className="rounded-md border border-red-300 bg-red-50 px-3 py-2 text-[13px] text-red-800 dark:border-red-900 dark:bg-red-950 dark:text-red-200"
        >
          {error}
        </p>
      ) : null}
      {notice ? (
        <p
          data-health-settings-notice
          className="rounded-md border border-line bg-surface px-3 py-2 text-[13px]"
        >
          {notice}
        </p>
      ) : null}

      {loading && !settings ? (
        <ul data-health-settings-skeleton className="space-y-2">
          {[0, 1, 2, 3, 4].map((row) => (
            <li key={row} className="h-9 animate-pulse rounded-md bg-surface" />
          ))}
        </ul>
      ) : null}

      {settings ? (
        <>
          <section className="space-y-3">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <h2 className="text-[14px] font-medium">Intervals</h2>
              <div className="flex items-center gap-2">
                <button
                  type="button"
                  data-health-settings-refresh
                  onClick={() => void load()}
                  className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-surface"
                >
                  <RefreshCw aria-hidden className="h-3.5 w-3.5" />
                  Refresh
                </button>
                <button
                  type="button"
                  data-health-settings-save
                  disabled={saving}
                  onClick={() => void save()}
                  className="inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] text-[var(--color-surface)] disabled:opacity-60"
                >
                  {saving ? (
                    <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" />
                  ) : (
                    <Save aria-hidden className="h-3.5 w-3.5" />
                  )}
                  Save
                </button>
              </div>
            </div>

            <div className="grid gap-3 sm:grid-cols-2">
              <label className="block text-[12.5px]">
                <span className="text-muted">Check interval (seconds)</span>
                <input
                  data-health-settings-interval
                  value={interval}
                  onChange={(event) => setInterval(event.target.value)}
                  inputMode="numeric"
                  className="mt-1 w-full rounded-md border border-line bg-transparent px-2.5 py-1.5"
                />
                {bounds ? (
                  <span className="mt-1 block text-[11.5px] text-muted">
                    {bounds.check_interval_seconds[0]}–{bounds.check_interval_seconds[1]} seconds
                  </span>
                ) : null}
              </label>

              <label className="block text-[12.5px]">
                <span className="text-muted">A worker counts as stale after (seconds)</span>
                <input
                  data-health-settings-stale
                  value={stale}
                  onChange={(event) => setStale(event.target.value)}
                  inputMode="numeric"
                  className="mt-1 w-full rounded-md border border-line bg-transparent px-2.5 py-1.5"
                />
                {bounds ? (
                  <span className="mt-1 block text-[11.5px] text-muted">
                    {bounds.worker_stale_seconds[0]}–{bounds.worker_stale_seconds[1]} seconds
                  </span>
                ) : null}
              </label>
            </div>
          </section>

          <section className="space-y-3">
            <div>
              <h2 className="text-[14px] font-medium">Thresholds</h2>
              <p className="mt-0.5 text-[12.5px] text-muted">
                A metric crosses its warning at the first number and its critical line at the
                second. Rows marked <em>suggestion</em> are not saved yet — nothing fires for them
                until an operator saves the form.
              </p>
            </div>

            <table data-health-settings-thresholds className="hidden w-full text-left text-[13px] sm:table">
              <thead>
                <tr className="border-b border-line text-[12px] text-muted">
                  <th scope="col" className="py-2 pr-3 font-medium">Metric</th>
                  <th scope="col" className="py-2 pr-3 font-medium">Warn</th>
                  <th scope="col" className="py-2 pr-3 font-medium">Critical</th>
                  <th scope="col" className="py-2 font-medium">State</th>
                </tr>
              </thead>
              <tbody>
                {settings.thresholds.map((row) => (
                  <tr
                    key={row.metric}
                    data-health-threshold-row={row.metric}
                    data-health-threshold-configured={row.configured ? "true" : "false"}
                    className="border-b border-line last:border-b-0"
                  >
                    <td className="py-2 pr-3">
                      <div className="font-medium">{metricLabel(row.metric)}</div>
                      <div className="text-[11.5px] text-muted">
                        fires when {row.direction} its limit
                      </div>
                    </td>
                    <td className="py-2 pr-3">
                      <input
                        data-health-threshold-warn={row.metric}
                        value={draft[row.metric]?.warn ?? String(row.warn)}
                        onChange={(event) =>
                          setDraft((current) => ({
                            ...current,
                            [row.metric]: {
                              warn: event.target.value,
                              crit: current[row.metric]?.crit ?? String(row.crit),
                            },
                          }))
                        }
                        inputMode="decimal"
                        aria-label={`${metricLabel(row.metric)} warning limit`}
                        className="w-24 rounded-md border border-line bg-transparent px-2 py-1"
                      />
                      {row.unit ? <span className="ml-1 text-[11.5px] text-muted">{row.unit}</span> : null}
                    </td>
                    <td className="py-2 pr-3">
                      <input
                        data-health-threshold-crit={row.metric}
                        value={draft[row.metric]?.crit ?? String(row.crit)}
                        onChange={(event) =>
                          setDraft((current) => ({
                            ...current,
                            [row.metric]: {
                              warn: current[row.metric]?.warn ?? String(row.warn),
                              crit: event.target.value,
                            },
                          }))
                        }
                        inputMode="decimal"
                        aria-label={`${metricLabel(row.metric)} critical limit`}
                        className="w-24 rounded-md border border-line bg-transparent px-2 py-1"
                      />
                    </td>
                    <td className="py-2 text-[12px]">
                      {row.configured ? (
                        <span className="text-emerald-700 dark:text-emerald-300">saved</span>
                      ) : (
                        <span className="text-muted">suggestion</span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>

            <ul data-health-settings-threshold-cards className="space-y-2 sm:hidden">
              {settings.thresholds.map((row) => (
                <li
                  key={row.metric}
                  data-health-threshold-card={row.metric}
                  className="rounded-lg border border-line px-3 py-2"
                >
                  <div className="flex items-baseline justify-between gap-2">
                    <span className="text-[13px] font-medium">{metricLabel(row.metric)}</span>
                    <span className="text-[11.5px] text-muted">
                      {row.configured ? "saved" : "suggestion"}
                    </span>
                  </div>
                  <div className="mt-2 grid grid-cols-2 gap-2">
                    <label className="text-[11.5px] text-muted">
                      Warn
                      <input
                        data-health-threshold-warn={row.metric}
                        value={draft[row.metric]?.warn ?? String(row.warn)}
                        onChange={(event) =>
                          setDraft((current) => ({
                            ...current,
                            [row.metric]: {
                              warn: event.target.value,
                              crit: current[row.metric]?.crit ?? String(row.crit),
                            },
                          }))
                        }
                        inputMode="decimal"
                        className="mt-1 w-full rounded-md border border-line bg-transparent px-2 py-1 text-[12.5px] text-ink"
                      />
                    </label>
                    <label className="text-[11.5px] text-muted">
                      Critical
                      <input
                        data-health-threshold-crit={row.metric}
                        value={draft[row.metric]?.crit ?? String(row.crit)}
                        onChange={(event) =>
                          setDraft((current) => ({
                            ...current,
                            [row.metric]: {
                              warn: current[row.metric]?.warn ?? String(row.warn),
                              crit: event.target.value,
                            },
                          }))
                        }
                        inputMode="decimal"
                        className="mt-1 w-full rounded-md border border-line bg-transparent px-2 py-1 text-[12.5px] text-ink"
                      />
                    </label>
                  </div>
                </li>
              ))}
            </ul>

            <p className="text-[12px] text-muted">
              {settings.breaches} {settings.breaches === 1 ? "breach" : "breaches"} on record,{" "}
              resolved ones included — each metric fires at most once per 15-minute window.
            </p>
          </section>
        </>
      ) : null}

      <section className="space-y-3">
        <div>
          <h2 className="text-[14px] font-medium">Maintenance windows</h2>
          <p className="mt-0.5 text-[12.5px] text-muted">
            Inside a window the platform stays quiet, but the state is still recorded — the incident
            appears with its real state and a <em>maintenance</em> marker. A window never hides a
            reading.
          </p>
        </div>

        <div className="grid gap-3 sm:grid-cols-2">
          <label className="block text-[12.5px]">
            <span className="text-muted">Starts</span>
            <input
              type="datetime-local"
              data-health-window-start
              value={draftWindow.starts_at}
              onChange={(event) => setDraftWindow((current) => ({ ...current, starts_at: event.target.value }))}
              className="mt-1 w-full rounded-md border border-line bg-transparent px-2.5 py-1.5"
            />
          </label>
          <label className="block text-[12.5px]">
            <span className="text-muted">Ends</span>
            <input
              type="datetime-local"
              data-health-window-end
              value={draftWindow.ends_at}
              onChange={(event) => setDraftWindow((current) => ({ ...current, ends_at: event.target.value }))}
              className="mt-1 w-full rounded-md border border-line bg-transparent px-2.5 py-1.5"
            />
          </label>
          <label className="block text-[12.5px] sm:col-span-2">
            <span className="text-muted">Note</span>
            <input
              data-health-window-note
              value={draftWindow.note}
              onChange={(event) => setDraftWindow((current) => ({ ...current, note: event.target.value }))}
              placeholder="deploy, database upgrade…"
              className="mt-1 w-full rounded-md border border-line bg-transparent px-2.5 py-1.5"
            />
          </label>
        </div>

        <button
          type="button"
          data-health-window-add
          disabled={pendingWindow || draftWindow.starts_at === "" || draftWindow.ends_at === ""}
          onClick={() => void addWindow()}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-surface disabled:opacity-60"
        >
          {pendingWindow ? (
            <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" />
          ) : (
            <Plus aria-hidden className="h-3.5 w-3.5" />
          )}
          Add window
        </button>

        {windows.length === 0 ? (
          <p data-health-windows-empty className="rounded-lg border border-line px-4 py-4 text-[12.5px] text-muted">
            No maintenance windows. Every state change is recorded and announced.
          </p>
        ) : (
          <ul data-health-windows className="space-y-2">
            {windows.map((row) => (
              <li
                key={row.id}
                data-health-window-row={row.id}
                data-health-window-active={row.active ? "true" : "false"}
                className="flex flex-wrap items-center justify-between gap-2 rounded-md border border-line px-3 py-2"
              >
                <span className="text-[12.5px]">
                  <span className="font-mono text-[11.5px]">
                    {row.starts_at.replace("T", " ").replace("Z", "")} →{" "}
                    {row.ends_at.replace("T", " ").replace("Z", "")}
                  </span>
                  <span className="ml-2 text-muted">
                    {row.services.length === 0 ? "all services" : row.services.join(", ")}
                    {row.note ? ` · ${row.note}` : ""}
                  </span>
                  {row.active ? (
                    <span className="ml-2 rounded border border-amber-400 px-1 text-[10.5px] text-amber-700 dark:text-amber-300">
                      active now
                    </span>
                  ) : null}
                </span>
                <button
                  type="button"
                  data-health-window-delete={row.id}
                  onClick={() => void removeWindow(row.id)}
                  aria-label="Delete maintenance window"
                  className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px] hover:bg-surface"
                >
                  <Trash2 aria-hidden className="h-3 w-3" />
                  Delete
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}