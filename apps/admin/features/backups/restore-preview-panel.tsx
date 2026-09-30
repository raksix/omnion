"use client";

/**
 * The restore preview (REQ-013, slice 2).
 *
 * This panel is the sentence an operator reads before the platform overwrites itself, so
 * every choice on it is about not letting the reader skip the part that matters:
 *
 * - **The price is the headline, not a footnote.** `total_live_dropped` is what the operator
 *   loses by choosing this restore point, and it is a number the manifest cannot produce —
 *   the archive's own counts are all about the archive. When it is zero the panel says so
 *   plainly, because "you lose nothing" is the sentence that makes pressing the button a
 *   considered act rather than a reflex.
 * - **A warning is drawn at its own severity.** `danger` is a refusal-coloured block, not a
 *   yellow one, and a `data_loss` warning is the single most important line on the screen.
 *   Mapping all three severities to one colour is how a data-loss warning becomes a badge.
 * - **The phrase is shown before it is required.** An operator who has to be told the phrase
 *   at the moment of submitting has already committed; the whole point is that the cost of
 *   this restore is legible while they are still deciding.
 * - **The parts are ticked, not implied.** Every available part carries a checkbox that
 *   starts ticked, because "restore the whole archive" is the common case and a panel that
 *   made the operator tick five boxes to undo one mistake is a panel they will not use. But
 *   the array that is POSTed is the ticked set, never "everything available" — the API
 *   refuses an empty one by name, and a control that silently widened the selection is the
 *   dead control this product does not ship.
 * - **The button is disabled until the phrase matches, and the reason is on the line under
 *   it.** A destructive control that is merely *red* invites the click; a disabled control
 *   with a sentence saying what is missing is a control that teaches.
 * - **A refusal is rendered as the API's own sentence.** `ApiError.message` already names
 *   the rule that refused and, where it can, the part that was asked for. Rewording it in
 *   the panel is how "the confirmation phrase does not match, the phrase for this run is
 *   RESTORE ae11f7e8" becomes "Invalid confirmation", which is the version that gets a
 *   support ticket.
 */
import { useCallback, useEffect, useState } from "react";

import {
  AlertTriangle,
  CircleAlert,
  CircleCheck,
  Clock3,
  Info,
  Loader2,
  OctagonX,
  ShieldAlert,
  TriangleAlert,
} from "lucide-react";

import {
  ApiError,
  cancelRestoreJob,
  listRestoreJobs,
  previewRestore,
  queueRestore,
  restoreBackup,
} from "@/lib/api";
import { formatBytes, formatTimestamp } from "@/lib/format";
import type {
  RestorablePart,
  RestoreJob,
  RestoreOutcome,
  RestorePreview,
  RestoreWarning,
} from "@/lib/types";

/** Each severity's own visual, so a data-loss warning cannot be mistaken for a note. */
const WARNING_TONE = {
  danger: {
    box: "border-danger bg-danger-soft",
    text: "text-danger",
    Icon: AlertTriangle,
  },
  caution: {
    box: "border-caution bg-caution-soft",
    text: "text-caution",
    Icon: TriangleAlert,
  },
  notice: {
    box: "border-line bg-panel",
    text: "text-muted",
    Icon: Info,
  },
} as const;

/** The tone each restore mode is drawn in: a replacement is not a merge. */
const MODE_TONE: Record<string, string> = {
  replace: "bg-danger-soft text-danger",
  merge: "bg-info-soft text-info",
  advisory: "bg-quiet-soft text-muted",
};

/** The severities, loudest first, so a screen reader hits the worst warning first. */
const SEVERITY_ORDER = ["danger", "caution", "notice"] as const;

/**
 * Each restore job's own tone, and the sentence that goes with it.
 *
 * **An abort is a success, and it is drawn as one.** This is the half a queue makes easy to
 * get wrong: the panel is full of danger colours, so an operator who stopped a restore — who
 * *got what they wanted* — is the person most likely to read a red row as a failure. `aborted`
 * is therefore drawn in the ordinary ink, and it says in words that nothing was written. A
 * loss and a designed answer must not look alike, in either direction.
 */
const JOB_TONE: Record<RestoreJob["status"], { box: string; text: string; note: string }> = {
  queued: {
    box: "border-line",
    text: "text-muted",
    note: "Waiting for a worker. You can still stop it — nothing has been written.",
  },
  running: {
    box: "border-info",
    text: "text-info",
    note: "The safety backup has been taken and the objects are being written. This one can no longer be stopped.",
  },
  succeeded: {
    box: "border-ok",
    text: "text-ok",
    note: "Finished. The objects are back in the library.",
  },
  failed: {
    box: "border-danger",
    text: "text-danger",
    note: "It stopped on an error. The reason is below.",
  },
  aborted: {
    box: "border-line",
    text: "text-ink",
    note: "Stopped before the first write. Nothing on the platform was changed.",
  },
};

function sortedWarnings(warnings: RestoreWarning[]): RestoreWarning[] {
  return [...warnings].sort(
    (a, b) => SEVERITY_ORDER.indexOf(a.severity) - SEVERITY_ORDER.indexOf(b.severity),
  );
}

export function RestorePreviewPanel({
  backupId,
  onClose,
}: {
  backupId: string;
  onClose: () => void;
}) {
  const [preview, setPreview] = useState<RestorePreview | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [typed, setTyped] = useState("");
  // Which parts are ticked. Seeded from the preview the moment it arrives, so the default is
  // "everything available" without the panel having to guess what available means.
  const [selected, setSelected] = useState<string[]>([]);
  const [restoring, setRestoring] = useState(false);
  const [outcome, setOutcome] = useState<RestoreOutcome | null>(null);
  const [restoreError, setRestoreError] = useState<string | null>(null);

  // --- The queued restore (REQ-013 slice 2c) ---------------------------------------------
  // The immediate restore above cannot be stopped: it runs inside the `POST` and a `POST` in
  // flight cannot be un-pressed. So there are two buttons, and the difference is stated on
  // both of them rather than left for the operator to infer — a panel with one green "restore"
  // and one red "restore, but stoppable" and no words is a panel where the wrong one gets
  // pressed by somebody in a hurry.
  const [jobs, setJobs] = useState<RestoreJob[]>([]);
  const [queueing, setQueueing] = useState(false);
  const [queueError, setQueueError] = useState<string | null>(null);
  const [cancelling, setCancelling] = useState<string | null>(null);

  async function load() {
    setLoading(true);
    setError(null);
    try {
      const next = await previewRestore(backupId);
      setPreview(next);
      // Re-seed the selection on every load, and clear the outcome: a preview that changed
      // under the operator invalidates both the ticks and the result they were looking at.
      setSelected(next.parts.filter((part) => part.available).map((part) => part.part));
      setOutcome(null);
      setRestoreError(null);
      setTyped("");
    } catch (cause) {
      setPreview(null);
      setSelected([]);
      setError(
        cause instanceof ApiError
          ? cause.message
          : "This restore point could not be read from the destination.",
      );
    } finally {
      setLoading(false);
    }
  }

  function toggle(part: string) {
    setSelected((current) =>
      current.includes(part) ? current.filter((name) => name !== part) : [...current, part],
    );
  }

  async function performRestore() {
    if (!preview) return;
    setRestoring(true);
    setRestoreError(null);
    setOutcome(null);
    try {
      setOutcome(await restoreBackup(backupId, selected, typed));
    } catch (cause) {
      setRestoreError(
        cause instanceof ApiError
          ? cause.message
          : "The restore could not be started. Nothing was changed.",
      );
    } finally {
      setRestoring(false);
    }
  }

  /**
   * Read this run's restores.
   *
   * **A failure here is silent on purpose, and deliberately so.** Two facts are independent:
   * "I cannot see whether anything is queued" and "I may restore". An unrelated `GET`
   * failing must not take away a working restore button — and it must not leave an error
   * banner over the confirmation, where an operator would read it as "this restore is
   * refused". The list simply shows its own empty state, which is a lie by omission, so the
   * empty state names the possibility: it says what an empty list means *and* that the read
   * may have failed.
   */
  const loadJobs = useCallback(async () => {
    try {
      setJobs(await listRestoreJobs(backupId));
    } catch {
      setJobs([]);
    }
  }, [backupId]);

  /** Queue the restore instead of performing it, so it can still be stopped. */
  async function performQueue() {
    if (!preview) return;
    setQueueing(true);
    setQueueError(null);
    setOutcome(null);
    try {
      const job = await queueRestore(backupId, selected, typed);
      setJobs((current) => [job, ...current.filter((other) => other.id !== job.id)]);
      setTyped("");
    } catch (cause) {
      setQueueError(
        cause instanceof ApiError
          ? cause.message
          : "The restore could not be queued. Nothing was changed.",
      );
    } finally {
      setQueueing(false);
    }
  }

  /**
   * Stop a queued restore.
   *
   * The job is replaced by whatever the API says it is now, rather than by a locally
   * invented "stopped". A cancel can lose the race to the worker, and a panel that showed
   * "stopped" in that case would be telling an operator their library is untouched while
   * objects land in it.
   */
  async function performCancel(job: RestoreJob) {
    setCancelling(job.id);
    setQueueError(null);
    try {
      const settled = await cancelRestoreJob(job.id);
      setJobs((current) => current.map((other) => (other.id === settled.id ? settled : other)));
    } catch (cause) {
      setQueueError(
        cause instanceof ApiError
          ? cause.message
          : "This restore could not be stopped. Re-read the list before assuming it is still queued.",
      );
      await loadJobs();
    } finally {
      setCancelling(null);
    }
  }

  // Poll the queue while the panel is open.
  //
  // **Two seconds, and only while something can change.** A job advances because the *worker*
  // advanced it, and nothing the operator does here produces that event — so a list that only
  // refreshes on a click is a spinner for ever, and a cancel that appears not to work is a
  // control an operator learns to distrust. The interval stops the moment nothing is in a
  // live state: a finished restore cannot change again, and a poll that costs one request a
  // second on every backup detail screen is the kind of cost that gets the whole panel
  // switched off.
  const liveJob = jobs.some(
    (job) => job.status === "queued" || job.status === "running",
  );
  useEffect(() => {
    void loadJobs();
  }, [loadJobs]);
  useEffect(() => {
    if (!liveJob) return;
    const timer = setInterval(() => void loadJobs(), 2000);
    return () => clearInterval(timer);
  }, [liveJob, loadJobs]);

  // The phrase is never pre-filled, and the input is never enabled before the preview has
  // arrived: a confirmation field that accepts a guess typed from a previous restore is
  // exactly the mis-click the guard exists to catch.
  const phraseMatches = preview ? typed === preview.confirm_phrase : false;
  // Three independent conditions, and the button says which one is missing rather than
  // simply being grey. A destructive control that is disabled for an unexplained reason is
  // a control people click anyway.
  const nothingSelected = selected.length === 0;
  const blocked =
    restoring ||
    !preview ||
    !preview.restorable ||
    nothingSelected ||
    !phraseMatches ||
    outcome !== null ||
    restoreError !== null;

  return (
    <div
      className="rounded-xl border border-line bg-panel px-4 py-3"
      data-testid="restore-preview"
    >
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div>
          <p className="flex items-center gap-1.5 text-[13.5px] font-medium">
            <ShieldAlert className="h-3.5 w-3.5" />
            Restore preview
          </p>
          <p className="mt-0.5 text-[12px] text-muted">
            Reads this run&apos;s artifacts off the destination and counts what a restore would
            cost. It changes nothing.
          </p>
        </div>
        <div className="flex items-center gap-1.5">
          <button
            type="button"
            onClick={() => void load()}
            disabled={loading}
            data-testid="restore-preview-load"
            className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted disabled:opacity-60"
          >
            {loading ? <Loader2 className="h-3 w-3 animate-spin" /> : <ShieldAlert className="h-3 w-3" />}
            {preview ? "Re-check" : "Check what this would restore"}
          </button>
          <button
            type="button"
            onClick={onClose}
            className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted"
          >
            Close
          </button>
        </div>
      </div>

      {error ? (
        <p className="mt-3 flex items-start gap-1.5 text-[12px] text-danger" data-testid="restore-preview-error">
          <CircleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          {error}
        </p>
      ) : null}

      {!preview && !error ? (
        <p className="mt-3 text-[12px] text-muted" data-testid="restore-preview-idle">
          Nothing has been read yet. A restore replaces live data, so this screen will not
          guess — it re-reads every artifact and tells you what would be lost.
        </p>
      ) : null}

      {preview ? (
        <div className="mt-3 space-y-3">
          {/* The price. The largest number on the screen, on purpose. */}
          <div className="grid gap-2 sm:grid-cols-3">
            <div
              className={`rounded-lg border px-3 py-2 ${
                preview.total_live_dropped > 0 ? "border-danger bg-danger-soft" : "border-line"
              }`}
              data-testid="restore-preview-dropped"
            >
              <p className="text-[11px] uppercase tracking-wide text-muted">Would be lost</p>
              <p
                className={`mt-0.5 text-[19px] font-semibold tabular-nums ${
                  preview.total_live_dropped > 0 ? "text-danger" : "text-ink"
                }`}
              >
                {preview.total_live_dropped}
              </p>
              <p className="mt-0.5 text-[11.5px] text-muted">
                live items not in this archive
              </p>
            </div>
            <div className="rounded-lg border border-line px-3 py-2">
              <p className="text-[11px] uppercase tracking-wide text-muted">Overwritten</p>
              <p className="mt-0.5 text-[19px] font-semibold tabular-nums text-ink">
                {preview.total_live_matches}
              </p>
              <p className="mt-0.5 text-[11.5px] text-muted">live items this archive holds</p>
            </div>
            <div className="rounded-lg border border-line px-3 py-2">
              <p className="text-[11px] uppercase tracking-wide text-muted">Age</p>
              <p className="mt-0.5 text-[19px] font-semibold tabular-nums text-ink">
                {preview.age_days}d
              </p>
              <p className="mt-0.5 text-[11.5px] text-muted">
                {preview.finished_at
                  ? preview.finished_at.replace("T", " ").slice(0, 16)
                  : "this run never finished"}
              </p>
            </div>
          </div>

          {/* The warnings, loudest first. */}
          {preview.warnings.length > 0 ? (
            <ul className="space-y-1.5" data-testid="restore-preview-warnings">
              {sortedWarnings(preview.warnings).map((warning) => {
                const tone = WARNING_TONE[warning.severity];
                const Icon = tone.Icon;
                return (
                  <li
                    key={`${warning.code}-${warning.message}`}
                    className={`flex items-start gap-1.5 rounded-lg border px-2.5 py-1.5 text-[12px] ${tone.box}`}
                    data-testid={`restore-warning-${warning.severity}`}
                  >
                    <Icon className={`mt-0.5 h-3.5 w-3.5 shrink-0 ${tone.text}`} />
                    <span className={tone.text}>{warning.message}</span>
                  </li>
                );
              })}
            </ul>
          ) : (
            <p
              className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-muted"
              data-testid="restore-preview-clean"
            >
              Nothing to warn about: every part was re-read and this restore loses no live
              data.
            </p>
          )}

          {/* The parts, with each one's mode and its own cost. */}
          <table className="w-full text-left text-[12px]">
            <thead className="text-[11px] uppercase tracking-wide text-muted">
              <tr>
                <th className="py-1 font-medium">Part</th>
                <th className="py-1 font-medium">Mode</th>
                <th className="py-1 text-right font-medium">Items</th>
                <th className="py-1 text-right font-medium">Size</th>
                <th className="py-1 text-right font-medium">Overwrites</th>
                <th className="py-1 text-right font-medium">Drops</th>
                <th className="py-1 font-medium">Note</th>
              </tr>
            </thead>
            <tbody>
              {preview.parts.map((part: RestorablePart) => (
                <tr key={part.part} data-testid="restore-part-row" className="border-t border-line">
                  <td className="py-1 font-medium">
                    <label className="flex items-center gap-1.5">
                      <input
                        type="checkbox"
                        checked={selected.includes(part.part)}
                        // A part that is not available cannot be ticked: the control would
                        // be a checkbox that lies, and a lie here becomes a `400` at the
                        // worst possible moment.
                        disabled={!part.available}
                        onChange={() => toggle(part.part)}
                        data-testid={`restore-part-check-${part.part}`}
                        aria-label={`Restore the ${part.part} part`}
                        className="h-3.5 w-3.5 accent-danger"
                      />
                      {part.part}
                    </label>
                  </td>
                  <td className="py-1">
                    <span
                      className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${
                        MODE_TONE[part.mode] ?? "bg-quiet-soft text-muted"
                      }`}
                    >
                      {part.mode}
                    </span>
                  </td>
                  <td className="py-1 text-right tabular-nums">{part.item_count}</td>
                  <td className="py-1 text-right tabular-nums">{formatBytes(part.size_bytes)}</td>
                  <td className="py-1 text-right tabular-nums">{part.live_matches}</td>
                  <td
                    className={`py-1 text-right tabular-nums ${
                      part.live_dropped > 0 ? "text-danger" : "text-muted"
                    }`}
                  >
                    {part.live_dropped}
                  </td>
                  <td className="py-1 text-muted">
                    {part.available
                      ? (part.checksum?.slice(0, 12) ?? "on the destination")
                      : (part.reason ?? "unavailable")}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>

          {/*
            The typed confirmation, and the button. The phrase is SHOWN before it is
            demanded, so the operator can see what the API will ask for while they are still
            deciding rather than after they have committed.
          */}
          {preview.restorable ? (
            <div className="rounded-lg border border-line px-3 py-2">
              <label
                htmlFor="restore-confirm-phrase"
                className="block text-[11.5px] font-medium"
              >
                Type <code className="font-mono">{preview.confirm_phrase}</code> to confirm
              </label>
              <input
                id="restore-confirm-phrase"
                value={typed}
                onChange={(event) => setTyped(event.target.value)}
                autoComplete="off"
                spellCheck={false}
                placeholder={preview.confirm_phrase}
                data-testid="restore-confirm-input"
                aria-describedby="restore-confirm-state"
                className="mt-1 w-full max-w-[280px] rounded-lg border border-line bg-canvas px-2 py-1 font-mono text-[12px]"
              />
              <p
                id="restore-confirm-state"
                className="mt-1 text-[11.5px] text-muted"
                data-testid="restore-confirm-state"
              >
                {/* The one sentence that says what is missing, rather than three grey
                    reasons. A control disabled for an unexplained reason is a control
                    people click anyway. */}
                {phraseMatches
                  ? `Matches. Restoring takes a safety backup first, so nothing here is lost.`
                  : nothingSelected
                    ? "Tick at least one part above."
                    : "The phrase does not match yet."}
              </p>

              <div className="mt-2.5 flex flex-wrap items-center gap-2">
                <button
                  type="button"
                  onClick={() => void performRestore()}
                  disabled={blocked}
                  data-testid="restore-run"
                  className="inline-flex items-center gap-1.5 rounded-lg border border-danger bg-danger-soft px-3 py-1.5 text-[12.5px] font-medium text-danger disabled:cursor-not-allowed disabled:opacity-50"
                >
                  {restoring ? (
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  ) : (
                    <ShieldAlert className="h-3.5 w-3.5" />
                  )}
                  {restoring
                    ? "Restoring…"
                    : `Restore ${selected.length} part${selected.length === 1 ? "" : "s"}`}
                </button>
                {/*
                  The two ways to restore, and the difference is on the labels. The immediate
                  one is the default because a restore of a small library finishes before an
                  operator can change their mind; the queued one is offered beside it, never
                  hidden behind a menu, because the moment somebody *wants* to stop a restore is
                  the moment they are looking for a control and a menu is one click too late.
                */}
                <button
                  type="button"
                  onClick={() => void performQueue()}
                  disabled={blocked || queueing}
                  data-testid="restore-queue"
                  title="Queue this restore so you can stop it before it writes anything"
                  className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium text-ink disabled:cursor-not-allowed disabled:opacity-50"
                >
                  {queueing ? (
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  ) : (
                    <Clock3 className="h-3.5 w-3.5" />
                  )}
                  {queueing ? "Queueing…" : "Queue it (can be stopped)"}
                </button>
                <span className="text-[11.5px] text-muted">
                  {selected.length > 0 ? (
                    <>
                      Selected: <span className="font-mono">{selected.join(", ")}</span>
                    </>
                  ) : (
                    "Nothing selected."
                  )}
                </span>
              </div>
              <p className="mt-1 text-[11.5px] text-muted" data-testid="restore-queue-hint">
                The button above starts the restore now and cannot be stopped.{" "}
                <strong className="font-medium text-ink">Queue it</strong> instead and the
                restore waits for a worker — you get a Stop control until it begins writing,
                and the platform takes a safety backup first either way.
              </p>
            </div>
          ) : (
            <p
              className="rounded-lg border border-danger bg-danger-soft px-3 py-2 text-[12px] text-danger"
              data-testid="restore-preview-not-restorable"
            >
              Nothing on this run can be restored. Fix the destination before considering it
              again — there is no phrase to confirm, because there is nothing to confirm.
            </p>
          )}

          {/* The refusal. The API's own sentence, not a rewording: it names the rule that
              refused and, where it can, the part that was asked for, and paraphrasing that
              in the panel is how "the phrase for this run is RESTORE ae11f7e8" turns into
              "Invalid confirmation" and a support ticket. */}
          {restoreError ? (
            <p
              className="flex items-start gap-1.5 rounded-lg border border-danger bg-danger-soft px-3 py-2 text-[12px] text-danger"
              role="alert"
              data-testid="restore-error"
            >
              <CircleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
              <span>
                {restoreError}
                <span className="mt-0.5 block text-[11.5px]">
                  Nothing was changed — a refused restore takes no safety backup.
                </span>
              </span>
            </p>
          ) : null}

          {/* What happened. The price the operator agreed to is repeated here next to what
              actually landed, because a restore that wrote three of four objects and said
              "done" is the failure this screen exists to make visible. */}
          {outcome ? (
            <div
              className="rounded-lg border border-line bg-panel px-3 py-2"
              data-testid="restore-outcome"
            >
              <p className="flex items-center gap-1.5 text-[12.5px] font-medium">
                <CircleCheck className="h-3.5 w-3.5 text-ok" />
                {outcome.summary}
              </p>
              <dl className="mt-1.5 grid grid-cols-2 gap-x-3 gap-y-0.5 text-[11.5px] sm:grid-cols-4">
                <div>
                  <dt className="text-muted">Objects restored</dt>
                  <dd className="tabular-nums" data-testid="restore-objects-restored">
                    {outcome.media.objects_restored}
                  </dd>
                </div>
                <div>
                  <dt className="text-muted">Objects failed</dt>
                  <dd
                    className={`tabular-nums ${outcome.media.objects_failed > 0 ? "text-danger" : ""}`}
                    data-testid="restore-objects-failed"
                  >
                    {outcome.media.objects_failed}
                  </dd>
                </div>
                <div>
                  <dt className="text-muted">Bytes</dt>
                  <dd className="tabular-nums">{formatBytes(outcome.media.bytes_restored)}</dd>
                </div>
                <div>
                  <dt className="text-muted">Live items dropped</dt>
                  <dd className="tabular-nums">{outcome.live_dropped}</dd>
                </div>
              </dl>
              {outcome.media.objects_failed > 0 ? (
                <ul
                  className="mt-1.5 max-h-40 space-y-0.5 overflow-y-auto text-[11.5px] text-danger"
                  data-testid="restore-failures"
                >
                  {outcome.media.failures.map((failure) => (
                    <li key={`${failure.archive_key}-${failure.reason}`}>
                      <span className="font-mono">{failure.storage_key}</span> — {failure.reason}
                    </li>
                  ))}
                </ul>
              ) : null}
              <p className="mt-1.5 text-[11.5px] text-muted">
                A protected safety backup was taken first:{" "}
                <span className="font-mono" data-testid="restore-safety-id">
                  {outcome.safety_backup_id}
                </span>
                . Nothing was deleted from the library — a restore adds back what the archive
                holds.
              </p>
            </div>
          ) : null}

          {/* The queue. Every state the API can hold, each drawn at its own tone, each with
              the one control that is meaningful for it. */}
          <div className="border-t border-line pt-2" data-testid="restore-jobs">
            <p className="flex items-center gap-1.5 text-[11.5px] font-medium">
              <Clock3 className="h-3.5 w-3.5" />
              Queued and finished restores
            </p>

            {queueError ? (
              <p
                className="mt-1.5 flex items-start gap-1.5 rounded-lg border border-danger bg-danger-soft px-2.5 py-1.5 text-[12px] text-danger"
                role="alert"
                data-testid="restore-queue-error"
              >
                <CircleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                <span>{queueError}</span>
              </p>
            ) : null}

            {jobs.length === 0 ? (
              <p className="mt-1 text-[11.5px] text-muted" data-testid="restore-jobs-empty">
                Nothing is queued. An immediate restore is above; a queued one waits for a
                worker and can be stopped until it starts writing. If you expected to see one
                here, this list could not be read — the restore controls above are unaffected.
              </p>
            ) : (
              <ul className="mt-1.5 space-y-1.5">
                {jobs.map((job) => {
                  const tone = JOB_TONE[job.status];
                  const stopping = cancelling === job.id;
                  return (
                    <li
                      key={job.id}
                      className={`rounded-lg border px-2.5 py-1.5 ${tone.box}`}
                      data-testid={`restore-job-${job.status}`}
                    >
                      <div className="flex flex-wrap items-start justify-between gap-2">
                        <div className="min-w-0">
                          <p className={`text-[12px] font-medium ${tone.text}`}>
                            <span className="font-mono">{job.parts.join(", ")}</span> ·{" "}
                            {job.status}
                          </p>
                          <p className="text-[11.5px] text-muted">{tone.note}</p>
                          <p className="mt-0.5 text-[11px] text-muted">
                            Queued {formatTimestamp(job.created_at)}
                            {job.started_at ? ` · started ${formatTimestamp(job.started_at)}` : ""}
                            {job.finished_at ? ` · ended ${formatTimestamp(job.finished_at)}` : ""}
                            {job.live_dropped > 0
                              ? ` · ${job.live_dropped} live item(s) priced as dropped`
                              : ""}
                          </p>
                          {job.safety_backup_id ? (
                            <p className="mt-0.5 text-[11px] text-muted">
                              Safety backup:{" "}
                              <span className="font-mono">{job.safety_backup_id}</span>
                            </p>
                          ) : null}
                          {job.error ? (
                            <p
                              className="mt-0.5 text-[11.5px] text-danger"
                              data-testid="restore-job-error"
                            >
                              {job.error}
                            </p>
                          ) : null}
                          {job.result ? (
                            <p className="mt-0.5 text-[11.5px] text-muted">
                              {job.result.media.objects_restored} object(s) restored,{" "}
                              {job.result.media.objects_failed} failed.
                            </p>
                          ) : null}
                        </div>

                        {/* Exactly one control, and only where it means something. A Stop
                            button on a running restore would be a control that discards the
                            safety backup the operator was told they had. */}
                        {job.cancellable ? (
                          <button
                            type="button"
                            onClick={() => void performCancel(job)}
                            disabled={stopping}
                            data-testid={`restore-job-stop-${job.id}`}
                            aria-label="Stop this queued restore before it writes anything"
                            className="inline-flex shrink-0 items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] font-medium text-ink disabled:opacity-60"
                          >
                            {stopping ? (
                              <Loader2 className="h-3 w-3 animate-spin" />
                            ) : (
                              <OctagonX className="h-3 w-3" />
                            )}
                            {stopping ? "Stopping…" : "Stop"}
                          </button>
                        ) : null}
                      </div>
                    </li>
                  );
                })}
              </ul>
            )}
          </div>
        </div>
      ) : null}
    </div>
  );
}
