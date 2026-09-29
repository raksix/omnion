"use client";

/**
 * The retention panel: how much history this organization keeps, and who removes it.
 *
 * It is a third tab rather than a card inside the Feed because it answers a different
 * question — not *what happened* and not *what could happen*, but **what will be forgotten and
 * when**. An operator who cannot see that a March event is still on the bus has no way to know
 * whether the bus is broken or the policy is simply a month wide.
 *
 * Four claims this screen makes, each of which is a way a retention panel lies:
 *
 * 1. **"Due" means the sweeper will really remove it.** An event a receiver is still owed is
 *    history, not due, and a number that ignored the pending deliveries would say "412 due" on
 *    the morning a sweep removes 0. The count comes from the API's own predicate — the same
 *    one the `delete` uses — so the two cannot disagree.
 * 2. **The window is bounded by the server's own rule.** `min_days` and `max_days` arrive in
 *    the read rather than being written here, because a range written in two places is a range
 *    that will disagree, and the input that disagrees with the server is the one that gets a
 *    `400` nobody can act on. The server's refusal is shown as it arrived, not replaced with a
 *    local guess about what went wrong.
 * 3. **A sweep that removes nothing is still a result.** The button answers with the number
 *    it removed, and `0` renders as a sentence rather than a spinner that never resolves —
 *    "the last sweep ran and found nothing" and "no sweep has run" are different states, and a
 *    panel that shows the same empty table for both is hiding one of them.
 * 4. **The run log is on screen.** Five rows, newest first, with what each removed. It is the
 *    same record the background worker writes, so an operator reading it on the morning a
 *    page's audit trail went missing can see when the sweep ran and what it took.
 */
import { useCallback, useEffect, useState } from "react";
import { Eraser, History, LoaderCircle, Save, TriangleAlert } from "lucide-react";

import {
  fetchRetention,
  setRetentionWindow,
  sweepRetention,
  type ApiError,
} from "@/lib/api";
import type { RetentionStatus, SweepResult } from "@/lib/types";

/** Render a count with its thousands separated; `0` is a number, not a blank. */
function count(value: number): string {
  return value.toLocaleString();
}

/** The one-line sentence under the window, naming the default when nothing was ever set. */
function windowSentence(status: RetentionStatus): string {
  const days = status.window_days;
  if (days === 1) return "Events older than a day are swept by the background worker.";
  return `Events older than ${count(days)} days are swept by the background worker.`;
}

export function RetentionPanel() {
  const [status, setStatus] = useState<RetentionStatus | null>(null);
  const [days, setDays] = useState("");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [sweeping, setSweeping] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const answer = await fetchRetention();
      setStatus(answer);
      // The input is seeded from the server's value rather than kept across a refresh: an
      // unsaved edit that survives a reload looks saved, and the panel's number would be the
      // only truth about what the window is.
      setDays(String(answer.window_days));
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const save = async () => {
    const parsed = Number.parseInt(days, 10);
    if (!Number.isFinite(parsed)) {
      setError("Enter the window in whole days.");
      return;
    }
    setSaving(true);
    setError(null);
    setNotice(null);
    try {
      const answer = await setRetentionWindow(parsed);
      setStatus(answer);
      setDays(String(answer.window_days));
      setNotice(`History is now kept for ${count(answer.window_days)} days.`);
    } catch (caught) {
      // The server's own message, not a local guess: the refusal names the field and the
      // range, and replacing that with "invalid value" would throw away the only sentence
      // that says which bound was crossed.
      setError((caught as ApiError).message);
    } finally {
      setSaving(false);
    }
  };

  const sweep = async () => {
    setSweeping(true);
    setError(null);
    setNotice(null);
    try {
      const result: SweepResult = await sweepRetention();
      setNotice(
        result.events_deleted === 0
          ? "The sweep ran and found nothing past the window."
          : `Removed ${count(result.events_deleted)} events and ${count(
              result.deliveries_deleted,
            )} deliveries.`,
      );
      // The counts changed underneath, so the status is read back rather than patched: a
      // locally decremented number is a second source of truth next to the server's.
      await load();
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setSweeping(false);
    }
  };

  if (loading && !status) {
    return (
      <div
        role="status"
        data-retention-loading
        className="flex items-center gap-2 rounded-xl border border-line bg-surface px-4 py-6 text-[13px] text-muted"
      >
        <LoaderCircle className="size-4 animate-spin" aria-hidden />
        Reading the retention policy…
      </div>
    );
  }

  if (!status) {
    return (
      <div
        role="alert"
        data-retention-error
        className="flex flex-col gap-3 rounded-xl border border-danger/40 bg-danger-soft px-4 py-4 text-[13px]"
      >
        <p className="flex items-center gap-2 font-medium text-danger">
          <TriangleAlert className="size-4" aria-hidden />
          The retention policy could not be read.
        </p>
        <p className="text-muted">{error ?? "The API did not answer."}</p>
        <button
          type="button"
          onClick={() => void load()}
          data-retention-retry
          className="self-start rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-ink transition hover:bg-quiet-soft"
        >
          Try again
        </button>
      </div>
    );
  }

  const min = status.min_days;
  const max = status.max_days;
  const outOfRange = Number.parseInt(days, 10) < min || Number.parseInt(days, 10) > max;
  const dirty = days !== String(status.window_days);

  return (
    <div className="flex flex-col gap-4" data-retention-panel>
      <section
        aria-label="Retention policy"
        data-retention-policy
        className="flex flex-col gap-4 rounded-xl border border-line bg-surface p-4"
      >
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div className="flex flex-col gap-1">
            <h2 className="flex items-center gap-2 text-[13.5px] font-medium text-ink">
              <History className="size-4" aria-hidden />
              How long history is kept
            </h2>
            <p className="max-w-prose text-[12.5px] text-muted">{windowSentence(status)}</p>
            <p className="max-w-prose text-[12px] text-muted">
              An event a receiver has not had yet is never swept, however old it is — a
              delivery the platform still owes does not go away with a week-old row.
            </p>
          </div>

          <div className="flex items-end gap-2">
            <label className="flex flex-col gap-1 text-[11.5px] text-muted">
              <span className="font-medium">Window (days)</span>
              <input
                id="retention-window"
                data-retention-window
                type="number"
                inputMode="numeric"
                min={min}
                max={max}
                step={1}
                value={days}
                onChange={(event) => setDays(event.target.value)}
                aria-describedby="retention-window-help"
                className="w-28 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
              />
            </label>
            <button
              type="button"
              onClick={() => void save()}
              disabled={saving || outOfRange || !dirty}
              data-retention-save
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-ink transition hover:bg-quiet-soft disabled:cursor-not-allowed disabled:opacity-50"
            >
              {saving ? (
                <LoaderCircle className="size-3.5 animate-spin" aria-hidden />
              ) : (
                <Save className="size-3.5" aria-hidden />
              )}
              Save
            </button>
          </div>
        </div>

        <p id="retention-window-help" className="text-[11.5px] text-muted">
          Between {count(min)} and {count(max)} days. A window of zero is refused rather than
          clamped: the platform will not answer with a number you did not choose.
        </p>

        <dl className="grid grid-cols-2 gap-3 sm:grid-cols-3" data-retention-counts>
          <div className="rounded-lg border border-line bg-canvas px-3 py-2">
            <dt className="text-[11.5px] text-muted">Events on the bus</dt>
            <dd className="text-[15px] font-medium tabular-nums text-ink" data-retention-events>
              {count(status.events)}
            </dd>
          </div>
          <div className="rounded-lg border border-line bg-canvas px-3 py-2">
            <dt className="text-[11.5px] text-muted">Past the window</dt>
            <dd className="text-[15px] font-medium tabular-nums text-ink" data-retention-due>
              {count(status.due)}
            </dd>
          </div>
          <div className="rounded-lg border border-line bg-canvas px-3 py-2 sm:col-span-1">
            <dt className="text-[11.5px] text-muted">Last sweep</dt>
            <dd className="text-[13px] text-ink" data-retention-last-run>
              {status.last_run
                ? new Date(status.last_run.started_at).toLocaleString()
                : "No sweep has run yet"}
            </dd>
          </div>
        </dl>

        <div className="flex flex-wrap items-center gap-3">
          <button
            type="button"
            onClick={() => void sweep()}
            disabled={sweeping}
            data-retention-sweep
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-ink transition hover:bg-quiet-soft disabled:cursor-not-allowed disabled:opacity-50"
          >
            {sweeping ? (
              <LoaderCircle className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <Eraser className="size-3.5" aria-hidden />
            )}
            Sweep now
          </button>
          <p className="text-[11.5px] text-muted">
            The worker sweeps on its own schedule; this runs the same sweep immediately and
            writes the same run log.
          </p>
        </div>

        {error ? (
          <p role="alert" data-retention-write-error className="text-[12.5px] text-danger">
            {error}
          </p>
        ) : null}
        {notice && !error ? (
          <p role="status" data-retention-notice className="text-[12.5px] text-muted">
            {notice}
          </p>
        ) : null}
      </section>

      <section
        aria-label="Sweep history"
        data-retention-runs
        className="rounded-xl border border-line bg-surface"
      >
        <h2 className="border-b border-line px-4 py-3 text-[13.5px] font-medium text-ink">
          Recent sweeps
        </h2>

        {status.recent_runs.length === 0 ? (
          <p
            data-retention-runs-empty
            className="px-4 py-6 text-center text-[12.5px] text-muted"
          >
            No sweep has run for this organization yet. The first one is scheduled, and you can
            run it now with the button above.
          </p>
        ) : (
          <table className="w-full text-left text-[12.5px]">
            <thead className="text-[11.5px] uppercase tracking-wide text-muted">
              <tr>
                <th scope="col" className="px-4 py-2 font-medium">
                  Started
                </th>
                <th scope="col" className="px-4 py-2 font-medium">
                  Window
                </th>
                <th scope="col" className="px-4 py-2 font-medium">
                  Events removed
                </th>
                <th scope="col" className="px-4 py-2 font-medium">
                  Deliveries removed
                </th>
              </tr>
            </thead>
            <tbody>
              {status.recent_runs.map((run) => (
                <tr
                  key={run.id}
                  data-retention-run={run.id}
                  className="border-t border-line/60"
                >
                  <td className="px-4 py-2 text-muted tabular-nums">
                    {new Date(run.started_at).toLocaleString()}
                  </td>
                  <td className="px-4 py-2 text-muted tabular-nums">{count(run.window_days)}d</td>
                  <td className="px-4 py-2 tabular-nums text-ink">{count(run.events_deleted)}</td>
                  <td className="px-4 py-2 tabular-nums text-ink">
                    {count(run.deliveries_deleted)}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </div>
  );
}
