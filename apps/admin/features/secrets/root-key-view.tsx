"use client";

/**
 * `/secrets/root-key` — the key ring, the seal self-check and the rotation ceremony
 * (docs/requests/REQ-125, slice 1).
 *
 * The screen exists to make one irreversible operation safe. A rotation walks every stored
 * version onto a new root key, and if the operator key is wrong the walk cannot read what it
 * has to move — so the wizard makes the operator **confirm the seal first**, states plainly what
 * losing the key costs, and only then opens the job. The progress bar is a real counter read
 * back from the server, never a local animation.
 *
 * Nothing here renders a secret: the ring shows fingerprints, coverage and a key id, which is
 * what a version already records. The `r` key opens the wizard from anywhere on the screen, `Esc`
 * closes it, and the walk's pause/resume are right next to the counter they move.
 */
import { useCallback, useEffect, useRef, useState } from "react";

import { AlertTriangle, CheckCircle2, KeyRound, Pause, Play, RefreshCw, ShieldCheck } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchRootKeyState,
  pauseRewrapJob,
  resumeRewrapJob,
  rotateRootKey,
  type RootKey,
  type RootKeyState,
  type RewrapJob,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** Which step of the wizard the operator is on. */
type Step = "closed" | "confirm" | "running" | "verify";

/** How often a running job's counter is re-read, in milliseconds. */
const POLL_MS = 1_500;

/** `/secrets/root-key`. */
export function RootKeyView() {
  const [state, setState] = useState<RootKeyState | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [step, setStep] = useState<Step>("closed");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [accepted, setAccepted] = useState(false);
  const heading = useRef<HTMLHeadingElement | null>(null);

  const load = useCallback(async () => {
    try {
      const next = await fetchRootKeyState();
      setState(next);
      setStatus("ready");
      setLoadError(null);
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The key ring could not be read." },
      );
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const job = state?.job ?? null;

  // A running walk is followed by re-reading the job, so the counter is the server's own and
  // not a local guess. A paused or finished job stops the poll.
  useEffect(() => {
    if (step !== "running" || !job || job.status !== "running") {
      return;
    }
    const timer = window.setInterval(() => {
      void load();
    }, POLL_MS);
    return () => window.clearInterval(timer);
  }, [step, job, load]);

  // `r` opens the wizard from anywhere on the screen; `Esc` closes it. The wizard is a dialog,
  // so the shortcut only fires while the focus is inside the page and never while a field is
  // being typed into.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" || target?.tagName === "TEXTAREA" || target?.isContentEditable;
      if (event.key === "Escape" && step !== "closed") {
        setStep("closed");
        setAccepted(false);
        return;
      }
      if (typing || event.metaKey || event.ctrlKey || event.altKey) {
        return;
      }
      if (event.key === "r" && step === "closed" && state?.seal.healthy) {
        event.preventDefault();
        setStep("confirm");
        window.setTimeout(() => heading.current?.focus(), 0);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [step, state?.seal.healthy]);

  const rotate = async () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const job = await rotateRootKey();
      setNotice(
        `The new key is active. ${job.total_count} version(s) are being moved onto it.`,
      );
      setAccepted(false);
      setStep("running");
      await load();
    } catch (cause) {
      setError(
        cause instanceof ApiError ? cause.message : "The rotation could not be started.",
      );
    } finally {
      setBusy(false);
    }
  };

  const pause = async () => {
    if (!job) return;
    setBusy(true);
    setError(null);
    try {
      const paused = await pauseRewrapJob(job.id);
      setNotice(
        paused.resume_note
          ? `The walk is paused. ${paused.resume_note}`
          : "The walk is paused.",
      );
      await load();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The walk could not be paused.");
    } finally {
      setBusy(false);
    }
  };

  const resume = async () => {
    if (!job) return;
    setBusy(true);
    setError(null);
    try {
      await resumeRewrapJob(job.id);
      setNotice("The walk resumed from its cursor.");
      setStep("running");
      await load();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The walk could not be resumed.");
    } finally {
      setBusy(false);
    }
  };

  if (status === "loading") {
    return <LoadingTable columns={4} />;
  }

  if (status === "error" && loadError) {
    return (
      <div className="flex flex-col gap-3">
        <p
          role="alert"
          data-root-key-error
          className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
        >
          {loadError.message}
        </p>
        <p className="text-[11.5px] text-muted">
          Code <code className="font-mono">{loadError.code}</code>
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="flex h-8 w-fit items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  const seal = state?.seal;
  const keys = state?.keys ?? [];
  const versionsToRewrap = state?.versions_to_rewrap ?? 0;

  return (
    <div className="flex flex-col gap-4">
      {/* The self-check, first, because a rotation is refused without it. */}
      <section
        data-root-key-seal
        className={`rounded-xl border p-4 ${
          seal?.healthy ? "border-line bg-surface" : "border-danger/40 bg-danger-soft"
        }`}
      >
        <div className="flex flex-wrap items-start gap-3">
          {seal?.healthy ? (
            <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-positive" aria-hidden />
          ) : (
            <AlertTriangle className="mt-0.5 size-4 shrink-0 text-caution" aria-hidden />
          )}
          <div className="min-w-0 flex-1">
            <h2 className="text-[13.5px] font-medium">
              {seal?.healthy
                ? "The operator key opens every key in the ring"
                : "The operator key cannot open the whole ring"}
            </h2>
            <p className="mt-0.5 text-[12.5px] text-muted">{seal?.guidance}</p>
            {seal && !seal.healthy && seal.unsealed.length > 0 ? (
              <ul data-root-key-unsealed className="mt-2 flex flex-col gap-1">
                {seal.unsealed.map(([keyId, code]) => (
                  <li key={keyId} className="font-mono text-[11.5px] text-muted">
                    {keyId.slice(0, 12)}… — {code}
                  </li>
                ))}
              </ul>
            ) : null}
            <p className="mt-2 text-[11.5px] text-muted">
              {seal?.source
                ? `Read from ${seal.source}`
                : "Read from the OMNION_KEY_ENCRYPTION_KEY environment variable"}
              {" · "}
              {seal?.sealed ?? 0} key(s) verified
            </p>
          </div>
          <button
            type="button"
            onClick={() => void load()}
            className="flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-[12.5px] transition hover:bg-panel"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Re-check
          </button>
        </div>
      </section>

      {notice ? (
        <p
          role="status"
          data-root-key-notice
          className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]"
        >
          {notice}
        </p>
      ) : null}
      {error ? (
        <p
          role="alert"
          data-root-key-action-error
          className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
        >
          {error}
        </p>
      ) : null}

      {/* The live walk, with a real counter. */}
      {job ? (
        <section
          data-root-key-job
          className="rounded-xl border border-line bg-surface p-4"
        >
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="text-[13.5px] font-medium">Re-wrapping stored versions</h2>
            <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] font-medium text-muted">
              {job.status}
            </span>
            {job.pause_reason ? (
              <span className="text-[11.5px] text-muted">{job.pause_reason}</span>
            ) : null}
          </div>
          <p
            data-root-key-counter
            className="mt-2 text-[13px] tabular-nums"
            aria-live="polite"
          >
            {job.rewrapped_count} of {job.total_count} versions moved
          </p>
          <div
            role="progressbar"
            aria-valuenow={job.rewrapped_count}
            aria-valuemin={0}
            aria-valuemax={job.total_count}
            aria-label="Versions moved onto the new key"
            className="mt-2 h-2 w-full overflow-hidden rounded-full bg-quiet-soft"
          >
            <div
              className="h-full rounded-full bg-accent transition-[width] duration-300"
              style={{ width: `${Math.round(job.progress * 100)}%` }}
            />
          </div>
          {job.resume_note ? (
            <p data-root-key-resume-note className="mt-2 text-[12px] text-muted">
              {job.resume_note}
            </p>
          ) : null}
          {job.last_error ? (
            <p data-root-key-job-error className="mt-2 text-[12px] text-caution">
              {job.last_error}
            </p>
          ) : null}
          <div className="mt-3 flex flex-wrap gap-2">
            {job.status === "running" ? (
              <button
                type="button"
                data-root-key-pause
                onClick={() => void pause()}
                disabled={busy}
                className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel disabled:opacity-60"
              >
                <Pause className="size-3.5" aria-hidden />
                Pause
              </button>
            ) : (
              <button
                type="button"
                data-root-key-resume
                onClick={() => void resume()}
                disabled={busy}
                className="flex h-8 items-center gap-1.5 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-60"
              >
                <Play className="size-3.5" aria-hidden />
                Resume
              </button>
            )}
          </div>
        </section>
      ) : null}

      <section className="rounded-xl border border-line bg-surface">
        <div className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <h2 className="text-[13.5px] font-medium">Key ring</h2>
          <span className="text-[11.5px] text-muted">
            {versionsToRewrap > 0
              ? `${versionsToRewrap} version(s) still on a retired key`
              : "Every stored version is on the active key"}
          </span>
          <button
            type="button"
            data-root-key-rotate
            onClick={() => {
              setAccepted(false);
              setStep("confirm");
              window.setTimeout(() => heading.current?.focus(), 0);
            }}
            disabled={!seal?.healthy || job?.status === "running"}
            title={
              !seal?.healthy
                ? "The operator key must open the whole ring before a rotation"
                : undefined
            }
            className="ml-auto flex h-8 items-center gap-1.5 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:cursor-not-allowed disabled:bg-accent-soft disabled:text-accent-strong"
          >
            <KeyRound className="size-3.5" aria-hidden />
            Rotate the root key
          </button>
        </div>

        {keys.length === 0 ? (
          <EmptyState
            title="This installation has no root key yet"
            hint="The first key is created the first time a secret is sealed. Once it exists, rotating it is an online ceremony — every stored version is moved onto the new key while the platform keeps serving."
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[640px] text-left text-[12.5px]">
              <thead className="border-b border-line text-[11.5px] text-muted">
                <tr>
                  <th className="px-4 py-2 font-medium">Status</th>
                  <th className="px-4 py-2 font-medium">Fingerprint</th>
                  <th className="px-4 py-2 font-medium">Versions</th>
                  <th className="px-4 py-2 font-medium">Created</th>
                  <th className="px-4 py-2 font-medium">Retired</th>
                </tr>
              </thead>
              <tbody>
                {keys.map((key) => (
                  <RingRow key={key.key_id} row={key} />
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>

      {state && state.recent_jobs.length > 0 ? (
        <section className="rounded-xl border border-line bg-surface">
          <h2 className="border-b border-line px-4 py-3 text-[13.5px] font-medium">
            Recent rotations
          </h2>
          <div className="overflow-x-auto">
            <table className="w-full min-w-[560px] text-left text-[12.5px]">
              <thead className="border-b border-line text-[11.5px] text-muted">
                <tr>
                  <th className="px-4 py-2 font-medium">Outcome</th>
                  <th className="px-4 py-2 font-medium">Versions</th>
                  <th className="px-4 py-2 font-medium">Started</th>
                  <th className="px-4 py-2 font-medium">Finished</th>
                </tr>
              </thead>
              <tbody>
                {state.recent_jobs.map((past) => (
                  <tr key={past.id} className="border-b border-line last:border-0">
                    <td className="px-4 py-2">
                      <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] font-medium text-muted">
                        {past.status}
                      </span>
                    </td>
                    <td className="px-4 py-2 tabular-nums">
                      {past.rewrapped_count} of {past.total_count}
                    </td>
                    <td className="px-4 py-2 text-muted">{formatTimestamp(past.started_at)}</td>
                    <td className="px-4 py-2 text-muted">
                      {past.completed_at ? formatTimestamp(past.completed_at) : "—"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </section>
      ) : null}

      {/* The wizard. Three steps, and step one cannot be skipped. */}
      {step !== "closed" ? (
        <div
          className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-ink/40 p-4 pt-[8vh]"
          role="dialog"
          aria-modal="true"
          aria-labelledby="root-key-wizard-title"
          data-root-key-wizard
          onClick={(event) => {
            if (event.target === event.currentTarget) setStep("closed");
          }}
        >
          <div className="w-full max-w-lg rounded-xl border border-line bg-surface p-5 shadow-xl">
            <h3
              id="root-key-wizard-title"
              ref={heading}
              tabIndex={-1}
              className="text-[15px] font-medium outline-none"
            >
              {step === "confirm"
                ? "Step 1 of 3 — confirm the operator key"
                : step === "running"
                  ? "Step 2 of 3 — moving the stored versions"
                  : "Step 3 of 3 — the walk is finished"}
            </h3>

            {step === "confirm" ? (
              <div className="mt-3 flex flex-col gap-3">
                <p className="text-[12.5px] text-muted">
                  A rotation generates a new root key, wraps it with your operator key, makes it
                  active and then moves every stored version onto it. Consumers keep resolving
                  throughout: a version the walk has not reached still names the old key, and the
                  old key stays in the ring until the walk finishes.
                </p>
                <p className="flex gap-2 rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution">
                  <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
                  <span>
                    The root key is never stored by the platform — only wrapped with the operator
                    key. <strong>Losing that key makes every locally stored secret
                    unrecoverable.</strong> The check above must be green before this continues.
                  </span>
                </p>
                <label className="flex items-start gap-2 text-[12.5px]">
                  <input
                    type="checkbox"
                    checked={accepted}
                    data-root-key-accept
                    onChange={(event) => setAccepted(event.target.checked)}
                    className="mt-0.5 size-4 rounded border-line"
                  />
                  <span>
                    The operator key is available and I understand the re-wrap moves every stored
                    version onto the new key.
                  </span>
                </label>
                <div className="flex justify-end gap-2">
                  <button
                    type="button"
                    onClick={() => setStep("closed")}
                    className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-quiet-soft"
                  >
                    Cancel
                  </button>
                  <button
                    type="button"
                    data-root-key-rotate-confirm
                    onClick={() => void rotate()}
                    disabled={!accepted || busy || !seal?.healthy}
                    className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:cursor-not-allowed disabled:bg-accent-soft disabled:text-accent-strong"
                  >
                    {busy ? "Starting…" : "Start the rotation"}
                  </button>
                </div>
              </div>
            ) : null}

            {step === "running" && job ? (
              <div className="mt-3 flex flex-col gap-3">
                <p data-root-key-wizard-counter className="text-[13px] tabular-nums">
                  {job.rewrapped_count} of {job.total_count} versions moved
                </p>
                <div
                  role="progressbar"
                  aria-valuenow={job.rewrapped_count}
                  aria-valuemin={0}
                  aria-valuemax={job.total_count}
                  aria-label="Versions moved onto the new key"
                  className="h-2 w-full overflow-hidden rounded-full bg-quiet-soft"
                >
                  <div
                    className="h-full rounded-full bg-accent transition-[width] duration-300"
                    style={{ width: `${Math.round(job.progress * 100)}%` }}
                  />
                </div>
                <p className="text-[12.5px] text-muted">
                  {job.status === "paused"
                    ? "The walk is paused. You can resume it from this screen; the counter and the cursor are kept, so it picks up where it stopped."
                    : "The walk runs in the background. Secrets keep resolving while it does."}
                </p>
                <div className="flex justify-end">
                  <button
                    type="button"
                    onClick={() => {
                      setStep("closed");
                      void load();
                    }}
                    className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-quiet-soft"
                  >
                    Close
                  </button>
                </div>
              </div>
            ) : null}

            {step === "verify" ? (
              <div className="mt-3 flex flex-col gap-3">
                <p className="flex items-center gap-2 text-[12.5px]">
                  <ShieldCheck className="size-4 text-positive" aria-hidden />
                  Every stored version is on the active key. The previous key is kept retired for
                  the audit trail.
                </p>
                <div className="flex justify-end">
                  <button
                    type="button"
                    onClick={() => setStep("closed")}
                    className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
                  >
                    Done
                  </button>
                </div>
              </div>
            ) : null}
          </div>
        </div>
      ) : null}
    </div>
  );
}

/** One ring row. Mobile turns the table into cards through the shared `sm:` rules. */
function RingRow({ row }: { row: RootKey }) {
  return (
    <tr data-root-key-row className="border-b border-line last:border-0">
      <td className="px-4 py-2">
        <span
          className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${
            row.status === "active"
              ? "bg-positive-soft text-positive"
              : "bg-quiet-soft text-muted"
          }`}
        >
          {row.status === "active" ? "active" : row.status === "retiring" ? "rotating" : "retired"}
        </span>
      </td>
      <td className="px-4 py-2 font-mono text-[11.5px]">{row.fingerprint}</td>
      <td className="px-4 py-2 tabular-nums">{row.version_count}</td>
      <td className="px-4 py-2 text-muted">{formatTimestamp(row.created_at)}</td>
      <td className="px-4 py-2 text-muted">
        {row.retired_at ? formatTimestamp(row.retired_at) : "—"}
        {row.retired_reason ? ` · ${row.retired_reason}` : ""}
      </td>
    </tr>
  );
}

/** The job type is re-exported for the walkthrough's assertions. */
export type { RewrapJob };
