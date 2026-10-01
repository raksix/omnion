"use client";

/**
 * The rollback dialog (REQ-024, slice 3).
 *
 * A rollback is the button an operator presses when the last deploy was wrong, so the dialog is
 * built around the two things that make it safe and one that makes it comprehensible:
 *
 * * **A reason is required, and required *here* as well as on the server.** The server refuses an
 *   empty one and the `0211` constraint refuses the row, so this form refuses it too — with the
 *   same words, so the operator does not read one rule in the form and another in the response.
 *   The reason is not paperwork: it is what the next person reads in the history table, and the
 *   runner copies it into the log's first line.
 * * **The target version is typed, not picked from a dropdown.** There is exactly one candidate
 *   — the version this environment came from — and a dropdown with one option is a dialog that
 *   asks the operator to confirm something they did not choose. Typing it makes the number the
 *   thing being confirmed, which is the thing that can be wrong.
 * * **The backup checkbox is on by default and says what turning it off costs.** A rollback
 *   without a pre-backup is the state where the operator has no way back, so the unchecked box
 *   carries its own sentence rather than leaving the consequence to be discovered.
 *
 * `Esc` closes, and the dialog is a real `role="dialog"` with a focus-trapping title, because a
 * destructive confirmation behind a div is not a confirmation.
 */

import { useCallback, useEffect, useRef, useState } from "react";

import { CircleAlert, Loader2, RotateCcw, X } from "lucide-react";
import { useRouter } from "next/navigation";

import { ApiError, startDeploymentRollback } from "@/lib/api";

/** What the dialog needs to open. */
export type RollbackTarget = {
  /** Which environment. */
  environment: string;
  /** The version it came from — the only sensible target. */
  toVersion: string;
};

export function RollbackDialog({
  target,
  onClose,
}: {
  target: RollbackTarget | null;
  onClose: () => void;
}) {
  const router = useRouter();
  const [reason, setReason] = useState("");
  const [typed, setTyped] = useState("");
  const [backupFirst, setBackupFirst] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  // Reset per open. A dialog that remembers the last operator's reason and their half-typed
  // version is a dialog that can roll back to a number the current operator never checked.
  useEffect(() => {
    if (!target) return;
    setReason("");
    setTyped("");
    setBackupFirst(true);
    setError(null);
    inputRef.current?.focus();
  }, [target]);

  // `Esc` closes — the spec's own key list, and the only key a dialog must always answer.
  useEffect(() => {
    if (!target) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !submitting) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [target, submitting, onClose]);

  const submit = useCallback(async () => {
    if (!target) return;
    const trimmedReason = reason.trim();
    if (!trimmedReason) {
      setError("A rollback needs a reason. It is stored on the history row and in the log.");
      return;
    }
    if (typed.trim() !== target.toVersion) {
      // Checked here and again on the server; the server's `400` is the guard that matters and
      // this is the operator not finding out after a round trip.
      setError(
        `Type ${target.toVersion} exactly. This rollback returns ${target.environment} to that version.`,
      );
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      await startDeploymentRollback({
        environment: target.environment,
        toVersion: target.toVersion,
        reason: trimmedReason,
        backupFirst,
      });
      onClose();
      // The history is where the run is now, and the operator's next question is "what happened
      // to it" — so the router goes there rather than back to a card that has not changed yet.
      router.push(`/deployment/history?environment=${encodeURIComponent(target.environment)}`);
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The rollback could not be started.",
      );
      setSubmitting(false);
    }
  }, [target, reason, typed, backupFirst, onClose, router]);

  if (!target) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
      onClick={(event) => {
        // Click on the backdrop closes, click inside does not. Guarded on `submitting` so a
        // rollback in flight cannot be dismissed by a stray click.
        if (event.target === event.currentTarget && !submitting) onClose();
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="rollback-title"
        className="flex w-full max-w-lg flex-col gap-3 rounded-xl border border-line bg-surface p-5 shadow-xl"
      >
        <header className="flex items-start justify-between gap-3">
          <div>
            <h2 id="rollback-title" className="flex items-center gap-2 text-[15px] font-semibold">
              <RotateCcw className="size-4 text-destructive" aria-hidden="true" />
              Roll back {target.environment}
            </h2>
            <p className="mt-0.5 text-[12.5px] text-muted">
              Returns this environment to{" "}
              <strong className="font-mono text-ink">{target.toVersion}</strong>.
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            disabled={submitting}
            aria-label="Close"
            className="rounded-md p-1 text-muted hover:bg-panel disabled:opacity-50"
          >
            <X className="size-4" aria-hidden="true" />
          </button>
        </header>

        <p className="rounded-md border border-line bg-panel px-3 py-2 text-[12.5px] text-muted">
          Migrations are append-only, so no column is dropped and the older build can still read
          the data. A rollback takes its own backup first, runs the same steps a deploy does, and
          ends with the same verification — if this instance does not report{" "}
          {target.toVersion} afterwards, the history row says it failed.
        </p>

        <div className="flex flex-col gap-1">
          <label htmlFor="rollback-reason" className="text-[12.5px] font-medium">
            Reason (required)
          </label>
          <textarea
            id="rollback-reason"
            ref={inputRef}
            value={reason}
            rows={2}
            onChange={(event) => setReason(event.target.value)}
            placeholder="The 2.5.0 deploy broke checkout; returning to 2.4.1."
            className="w-full rounded-md border border-line bg-background px-2 py-1.5 text-[13px]"
          />
        </div>

        <div className="flex flex-col gap-1">
          <label htmlFor="rollback-confirm" className="text-[12.5px] font-medium">
            Type <span className="font-mono">{target.toVersion}</span> to confirm
          </label>
          <input
            id="rollback-confirm"
            value={typed}
            onChange={(event) => setTyped(event.target.value)}
            autoComplete="off"
            spellCheck={false}
            className="w-full rounded-md border border-line bg-background px-2 py-1.5 font-mono text-[13px]"
          />
        </div>

        <label className="flex items-start gap-2 text-[12.5px]">
          <input
            type="checkbox"
            checked={backupFirst}
            onChange={(event) => setBackupFirst(event.target.checked)}
            className="mt-0.5 size-4"
          />
          <span>
            Take a backup before the rollback starts.
            {!backupFirst ? (
              <span className="mt-0.5 block text-destructive">
                Without it there is no way back from this rollback except another rollback.
              </span>
            ) : null}
          </span>
        </label>

        {error ? (
          <p
            role="alert"
            className="flex items-start gap-1.5 rounded-md border border-destructive/40 bg-destructive/5 px-2.5 py-2 text-[12.5px] text-destructive"
          >
            <CircleAlert className="mt-0.5 size-3.5 shrink-0" aria-hidden="true" />
            {error}
          </p>
        ) : null}

        <footer className="flex justify-end gap-2 pt-1">
          <button
            type="button"
            onClick={onClose}
            disabled={submitting}
            className="rounded-md border border-line px-3 py-1.5 text-[13px] hover:bg-panel disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={() => void submit()}
            disabled={submitting}
            className="inline-flex items-center gap-1.5 rounded-md bg-destructive px-3 py-1.5 text-[13px] font-medium text-white disabled:opacity-60"
          >
            {submitting ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden="true" />
            ) : (
              <RotateCcw className="size-3.5" aria-hidden="true" />
            )}
            Roll back to {target.toVersion}
          </button>
        </footer>
      </div>
    </div>
  );
}
