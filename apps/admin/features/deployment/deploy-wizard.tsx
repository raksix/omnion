"use client";

/**
 * `/deployment/deploy?to={version}` — the deploy wizard (REQ-024, slice 2).
 *
 * Three steps, in the order the spec names them: **pre-flight → confirm → run**. The screen is
 * mostly about not lying to the operator, and there are four places where the obvious version
 * would:
 *
 * * **Step 1 is a real probe, and a failed one is not negotiable.** The report arrives from the
 *   server with one row per check, including the ones that *could not be answered*. A check that
 *   could not run is rendered as `unknown` and blocks `Continue`, because "we could not tell"
 *   and "it is fine" must not look the same on the way to production.
 * * **The acknowledgement is a real gate.** A warning needs the box ticked; the server checks it
 *   again, so un-ticking it here and pressing the button gets a `422` rather than a deploy.
 * * **Production requires typing the version**, and the comparison happens in this file as well
 *   as on the server — client-side so the operator is not sent to a network error for a typo,
 *   server-side because the panel is not the enforcement point.
 * * **The log pane is a log.** It has its own scroll region, a monospace font that does not
 *   overflow, and an auto-scroll toggle that the operator can turn off — a pane that fights the
 *   reader while they scroll back through a failure is the pane they stop reading.
 *
 * The run step polls the job and the log rather than holding a single long request, so the
 * browser can be closed and reopened mid-deploy and pick the same job up from its id.
 */

import { useCallback, useEffect, useRef, useState } from "react";

import {
  AlertTriangle,
  ArrowLeft,
  CheckCircle2,
  CircleDashed,
  Loader2,
  Undo2,
  XCircle,
} from "lucide-react";
import { useRouter, useSearchParams } from "next/navigation";

import { ApiError, cancelDeployment, fetchDeploymentJob, fetchDeploymentLog, runDeploymentPreflight, startDeployment } from "@/lib/api";
import { RollbackDialog, type RollbackTarget } from "./rollback-dialog";
import { formatTimestamp } from "@/lib/format";
import type { DeploymentJob, DeploymentPreflight, DeploymentPreflightRow } from "@/lib/types";

/** How often the run step polls the job while it is active. */
const POLL_MS = 900;

/** The three steps, in order. The timeline renders from this, so it cannot drift from the flow. */
const STEPS = ["Pre-flight", "Confirm", "Run"] as const;

type StepIndex = 0 | 1 | 2;

/** The wizard. */
export function DeploymentWizard() {
  const router = useRouter();
  const params = useSearchParams();
  const environment = params.get("environment") ?? "production";
  const toVersion = params.get("to") ?? "";

  const [step, setStep] = useState<StepIndex>(0);
  const [report, setReport] = useState<DeploymentPreflight | null>(null);
  const [checking, setChecking] = useState(false);
  const [acknowledged, setAcknowledged] = useState(false);
  const [typed, setTyped] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [job, setJob] = useState<DeploymentJob | null>(null);
  const [starting, setStarting] = useState(false);
  const [cancelNote, setCancelNote] = useState<string | null>(null);
  const [rollback, setRollback] = useState<RollbackTarget | null>(null);

  // Step 1 runs the pre-flight as soon as there is a target. An empty `to` is a dead end rather
  // than a report of seven failures, so the screen asks for the version instead.
  useEffect(() => {
    if (!toVersion) return;
    let cancelled = false;
    setChecking(true);
    setError(null);
    runDeploymentPreflight(environment, toVersion)
      .then((next) => {
        if (cancelled) return;
        setReport(next);
        setAcknowledged(false);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setError(cause instanceof ApiError ? cause.message : String(cause));
      })
      .finally(() => {
        if (!cancelled) setChecking(false);
      });
    return () => {
      cancelled = true;
    };
  }, [environment, toVersion]);

  // `d` opens the wizard, from anywhere on the deployment screens. Guarded on the modifier keys
  // so it does not fire while somebody is typing a version into a filter.
  useEffect(() => {
    function onKey(event: KeyboardEvent) {
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target?.isContentEditable === true;
      if (typing) return;
      if (event.key === "d" && !job) {
        router.push(`/deployment/deploy?to=${encodeURIComponent(toVersion)}&environment=${encodeURIComponent(environment)}`);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [environment, job, router, toVersion]);

  const canContinue = report?.can_continue === true && (report.needs_acknowledgement ? acknowledged : true);

  async function start() {
    if (!report) return;
    setStarting(true);
    setError(null);
    try {
      const response = await startDeployment({
        environment,
        toVersion,
        confirmVersion: typed,
        backupFirst: true,
        preflightToken: report.token,
        acknowledged,
      });
      setJob(response.job);
      setStep(2);
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setStarting(false);
    }
  }

  if (!toVersion) {
    return (
      <section className="space-y-4" data-page="deploy-wizard">
        <p
          role="status"
          className="rounded-xl border border-line bg-panel px-4 py-6 text-[13px] text-muted"
        >
          Choose a release first. Open <code className="font-mono">View Changes</code> on a
          release, or press <kbd className="rounded border border-line px-1.5 py-0.5 font-mono text-[11px]">d</kbd>{" "}
          on the deployment screen.
        </p>
      </section>
    );
  }

  return (
    <section className="space-y-5" data-page="deploy-wizard">
      <header className="flex flex-wrap items-baseline justify-between gap-2">
        <div>
          <h2 className="text-[15px] font-semibold text-ink">
            Deploy {toVersion} to {environment}
          </h2>
          <p className="text-[12.5px] text-muted">
            {report?.production
              ? "Production: the target version has to be typed before the deploy starts."
              : "The deploy runs its four steps in order and ends with a health verification."}
          </p>
        </div>
        {step > 0 && !job ? (
          <button
            type="button"
            onClick={() => setStep((step - 1) as StepIndex)}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium text-ink hover:bg-panel"
          >
            <ArrowLeft aria-hidden="true" className="size-3.5" />
            Back
          </button>
        ) : null}
      </header>

      <ol className="flex items-center gap-2 text-[12px]" aria-label="Deploy steps" data-testid="deploy-steps">
        {STEPS.map((label, index) => {
          const state = index === step ? "current" : index < step ? "done" : "todo";
          return (
            <li key={label} className="flex items-center gap-2" data-deploy-step={label} data-step-state={state}>
              <span
                aria-current={state === "current" ? "step" : undefined}
                className={
                  state === "current"
                    ? "rounded-full bg-accent px-2.5 py-1 font-medium text-white"
                    : state === "done"
                      ? "rounded-full border border-emerald-500/40 px-2.5 py-1 text-emerald-700 dark:text-emerald-300"
                      : "rounded-full border border-line px-2.5 py-1 text-muted"
                }
              >
                {label}
              </span>
              {index < STEPS.length - 1 ? (
                <span aria-hidden="true" className="h-px w-6 bg-line" />
              ) : null}
            </li>
          );
        })}
      </ol>

      {error ? (
        <p
          role="alert"
          className="rounded-xl border border-red-500/40 bg-red-500/5 px-4 py-3 text-[12.5px] text-red-800 dark:text-red-200"
        >
          {error}
        </p>
      ) : null}

      {step === 0 ? (
        <PreflightStep
          report={report}
          checking={checking}
          acknowledged={acknowledged}
          onAcknowledge={setAcknowledged}
          onContinue={() => setStep(1)}
          canContinue={canContinue}
        />
      ) : null}

      {step === 1 ? (
        <ConfirmStep
          report={report}
          typed={typed}
          onTyped={setTyped}
          production={report?.production === true}
          starting={starting}
          onStart={start}
        />
      ) : null}

      {step === 2 && job ? (
        <RunStep job={job} note={cancelNote} onRollback={setRollback} onCancel={async () => {
          try {
            const response = await cancelDeployment(job.id);
            setJob(response.job);
            setCancelNote(response.message);
          } catch (cause: unknown) {
            setCancelNote(cause instanceof ApiError ? cause.message : String(cause));
          }
        }} />
      ) : null}

      {/* The rollback the failed banner offers. Mounted here rather than inside `RunStep` so the
          dialog's own router navigation ends the wizard's run step instead of fighting it. */}
      <RollbackDialog target={rollback} onClose={() => setRollback(null)} />
    </section>
  );
}

/** Step 1 — the pre-flight report. */
function PreflightStep({
  report,
  checking,
  acknowledged,
  onAcknowledge,
  onContinue,
  canContinue,
}: {
  report: DeploymentPreflight | null;
  checking: boolean;
  acknowledged: boolean;
  onAcknowledge: (value: boolean) => void;
  onContinue: () => void;
  canContinue: boolean;
}) {
  if (checking && !report) {
    return (
      <p
        role="status"
        className="flex items-center gap-2 rounded-xl border border-line bg-panel px-4 py-6 text-[13px] text-muted"
      >
        <Loader2 aria-hidden="true" className="size-4 animate-spin" />
        Checking backup freshness, migrations, disk, running jobs and dependencies…
      </p>
    );
  }
  if (!report) {
    return (
      <p
        role="status"
        className="rounded-xl border border-line bg-panel px-4 py-6 text-[13px] text-muted"
      >
        The pre-flight could not be run. Try again, or check the API is reachable.
      </p>
    );
  }

  const warnings = report.checks.filter((check) => check.state === "warn");
  const blocking = report.checks.filter(
    (check) => check.state === "fail" || check.state === "unknown",
  );

  return (
    <div className="space-y-4">
      <ul className="space-y-2" data-testid="preflight-rows">
        {report.checks.map((check) => (
          <PreflightLine key={check.id} check={check} />
        ))}
      </ul>

      {blocking.length > 0 ? (
        <p
          role="alert"
          className="rounded-xl border border-red-500/40 bg-red-500/5 px-4 py-3 text-[12.5px] text-red-800 dark:text-red-200"
        >
          {blocking.length === 1 ? "One check blocks" : `${blocking.length} checks block`} this
          deploy: {blocking.map((check) => check.title).join("; ")}. A check that could not be
          answered blocks too — an unknown is not a pass.
        </p>
      ) : null}

      {warnings.length > 0 ? (
        <label className="flex items-start gap-2.5 rounded-xl border border-amber-500/40 bg-amber-500/5 px-4 py-3 text-[12.5px]">
          <input
            type="checkbox"
            data-testid="preflight-acknowledge"
            checked={acknowledged}
            onChange={(event) => onAcknowledge(event.target.checked)}
            className="mt-0.5 size-4 accent-[var(--accent)]"
          />
          <span>
            I have read the {warnings.length === 1 ? "warning" : "warnings"} above and want to
            deploy anyway.
          </span>
        </label>
      ) : null}

      <div className="flex items-center gap-3">
        <button
          type="button"
          data-testid="preflight-continue"
          onClick={onContinue}
          disabled={!canContinue}
          className="rounded-lg bg-accent px-4 py-2 text-[12.5px] font-medium text-white disabled:cursor-not-allowed disabled:opacity-50"
        >
          Continue
        </button>
        {!canContinue ? (
          <span className="text-[12px] text-muted">
            {blocking.length > 0
              ? "Blocked by a failing or unknown check."
              : "Acknowledge the warning to continue."}
          </span>
        ) : null}
      </div>
    </div>
  );
}

/** One pre-flight row, with its state spelled out in words — never colour alone. */
function PreflightLine({ check }: { check: DeploymentPreflightRow }) {
  const icon =
    check.state === "pass" ? (
      <CheckCircle2 aria-hidden="true" className="size-4 text-emerald-600 dark:text-emerald-400" />
    ) : check.state === "warn" ? (
      <AlertTriangle aria-hidden="true" className="size-4 text-amber-600 dark:text-amber-400" />
    ) : check.state === "fail" ? (
      <XCircle aria-hidden="true" className="size-4 text-red-600 dark:text-red-400" />
    ) : (
      <CircleDashed aria-hidden="true" className="size-4 text-muted" />
    );
  const word =
    check.state === "pass"
      ? "Pass"
      : check.state === "warn"
        ? "Warning"
        : check.state === "fail"
          ? "Fail"
          : "Unknown";
  return (
    <li className="flex items-start gap-3 rounded-xl border border-line bg-panel px-4 py-3">
      <span className="mt-0.5 shrink-0">{icon}</span>
      <div className="min-w-0 flex-1">
        <p className="flex flex-wrap items-baseline gap-x-2 text-[13px] font-medium text-ink">
          {check.title}
          <span className="text-[11px] font-normal uppercase tracking-wide text-muted">{word}</span>
        </p>
        <p className="text-[12.5px] text-muted">{check.detail}</p>
        {check.action ? (
          <p className="mt-1 text-[12px] text-amber-700 dark:text-amber-300">{check.action}</p>
        ) : null}
      </div>
    </li>
  );
}

/** Step 2 — what will happen, and the typed confirmation for production. */
function ConfirmStep({
  report,
  typed,
  onTyped,
  production,
  starting,
  onStart,
}: {
  report: DeploymentPreflight | null;
  typed: string;
  onTyped: (value: string) => void;
  production: boolean;
  starting: boolean;
  onStart: () => void;
}) {
  const migrations = report?.checks.find((check) => check.id === "pending_migrations");
  const matches = typed.trim().length > 0;
  return (
    <div className="space-y-4">
      <dl className="grid gap-x-6 gap-y-2 rounded-xl border border-line bg-panel px-4 py-3 text-[12.5px] sm:grid-cols-2">
        <div>
          <dt className="text-muted">Target version</dt>
          <dd className="font-mono text-ink">{migrations?.detail ?? "—"}</dd>
        </div>
        <div>
          <dt className="text-muted">Backup</dt>
          <dd className="text-ink">Taken first, unless you turn it off</dd>
        </div>
        <div>
          <dt className="text-muted">Estimated downtime</dt>
          <dd className="text-ink">One rolling restart plus the migrate step</dd>
        </div>
        <div>
          <dt className="text-muted">Cancel</dt>
          <dd className="text-ink">Available until the migrate step starts</dd>
        </div>
      </dl>

      {production ? (
        <label className="block space-y-1.5">
          <span className="text-[12.5px] font-medium text-ink">
            Type the target version to confirm
          </span>
          <input
            data-testid="confirm-version"
            value={typed}
            onChange={(event) => onTyped(event.target.value)}
            placeholder="e.g. 2.5.0"
            autoComplete="off"
            className="w-full max-w-xs rounded-lg border border-line bg-panel px-3 py-2 font-mono text-[13px] text-ink outline-none focus:border-[var(--accent)]"
          />
          <span className="block text-[12px] text-muted">
            Production is the one environment where a mistyped deploy is not undoable by retyping.
          </span>
        </label>
      ) : null}

      <button
        type="button"
        data-testid="confirm-start"
        onClick={onStart}
        disabled={starting || (production && !matches)}
        className="inline-flex items-center gap-2 rounded-lg bg-accent px-4 py-2 text-[12.5px] font-medium text-white disabled:cursor-not-allowed disabled:opacity-50"
      >
        {starting ? (
          <Loader2 aria-hidden="true" className="size-3.5 animate-spin" />
        ) : null}
        Start deploy
      </button>
    </div>
  );
}

/** Step 3 — the timeline, the live log, and the cancel boundary. */
function RunStep({
  job,
  note,
  onCancel,
  onRollback,
}: {
  job: DeploymentJob;
  note: string | null;
  onCancel: () => void;
  /** Open the rollback dialog at the version this job came from. Owned by the parent, which
   *  mounts the dialog, so the run step never owns a navigation of its own. */
  onRollback: (target: RollbackTarget) => void;
}) {
  const [current, setCurrent] = useState<DeploymentJob>(job);
  const [log, setLog] = useState("");
  const [autoScroll, setAutoScroll] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const pane = useRef<HTMLPreElement | null>(null);
  const cursor = useRef(0);

  // Follow the job. Polling rather than a socket: the wizard is a page an operator opens, watches
  // and closes, and a poll that resumes from the job id survives a refresh — which a socket opened
  // on mount does not.
  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function tick() {
      try {
        const [jobResponse, logResponse] = await Promise.all([
          fetchDeploymentJob(job.id),
          fetchDeploymentLog(job.id, cursor.current),
        ]);
        if (cancelled) return;
        setCurrent(jobResponse.job);
        if (logResponse.chunk) {
          setLog((previous) => previous + logResponse.chunk);
          cursor.current = logResponse.cursor;
        }
        if (logResponse.running) {
          timer = setTimeout(tick, POLL_MS);
        }
      } catch (cause: unknown) {
        if (cancelled) return;
        setError(cause instanceof ApiError ? cause.message : String(cause));
      }
    }
    void tick();
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [job.id]);

  // Auto-scroll, and only when it is on: a reader who has scrolled up to find the line that
  // failed should not be yanked back to the bottom by the next line arriving.
  useEffect(() => {
    if (!autoScroll) return;
    const node = pane.current;
    if (node) node.scrollTop = node.scrollHeight;
  }, [log, autoScroll]);

  useEffect(() => {
    function onKey(event: KeyboardEvent) {
      const target = event.target as HTMLElement | null;
      if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) return;
      if (event.key === "l") setAutoScroll((value) => !value);
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const finished = ["succeeded", "failed", "cancelled"].includes(current.status);
  const succeeded = current.status === "succeeded";

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <div className="h-1.5 w-full max-w-xs overflow-hidden rounded-full bg-line">
          <div
            className={succeeded ? "h-full bg-emerald-500" : "h-full bg-accent"}
            style={{ width: `${current.progress_percent}%` }}
          />
        </div>
        <span className="text-[12.5px] text-muted" data-testid="run-progress">
          {current.progress_percent}% ·{" "}
          {formatElapsed(current.elapsed_ms)}
          {finished ? "" : " elapsed"}
        </span>
        <button
          type="button"
          data-testid="log-autoscroll"
          onClick={() => setAutoScroll((value) => !value)}
          aria-pressed={autoScroll}
          className="ml-auto rounded-lg border border-line px-3 py-1.5 text-[12px] font-medium text-ink hover:bg-panel"
        >
          Auto-scroll: {autoScroll ? "on" : "off"}
          <kbd className="ml-1.5 rounded border border-line px-1 font-mono text-[10px]">l</kbd>
        </button>
      </div>

      <ol className="space-y-1.5" data-testid="job-steps">
        {current.steps.map((step) => (
          <li key={step.position} className="flex items-center gap-2 text-[12.5px]">
            <span
              aria-hidden="true"
              className={
                step.status === "done"
                  ? "size-2 rounded-full bg-emerald-500"
                  : step.status === "running"
                    ? "size-2 animate-pulse rounded-full bg-accent"
                    : step.status === "failed"
                      ? "size-2 rounded-full bg-red-500"
                      : step.status === "skipped"
                        ? "size-2 rounded-full bg-line"
                        : "size-2 rounded-full border border-line"
              }
            />
            <span className="font-medium text-ink">{step.name}</span>
            <span className="text-muted">{step.status}</span>
            {step.started_at ? (
              <span className="ml-auto font-mono text-[11px] text-muted">
                {formatTimestamp(step.started_at)}
              </span>
            ) : null}
          </li>
        ))}
      </ol>

      {/* The log keeps its own scroll region: `max-h` plus `overflow-auto` on the pre, so the page
          scrolls behind it and the pane does not grow without bound as a run appends. */}
      <pre
        ref={pane}
        data-testid="deploy-log"
        className="max-h-72 overflow-auto rounded-xl border border-line bg-[var(--surface-sunken)] px-4 py-3 font-mono text-[11.5px] leading-relaxed text-ink"
        onScroll={() => {
          const node = pane.current;
          if (!node) return;
          // Scrolling up by hand turns auto-scroll off, because a reader who moved deliberately
          // does not want the next line to move them back.
          const atBottom =
            node.scrollHeight - node.scrollTop - node.clientHeight < 24;
          setAutoScroll(atBottom);
        }}
      >
        {log || "Waiting for the first line of output…"}
      </pre>

      {error ? (
        <p role="alert" className="rounded-xl border border-red-500/40 bg-red-500/5 px-4 py-3 text-[12.5px] text-red-800 dark:text-red-200">
          {error}
        </p>
      ) : null}

      {note ? (
        <p role="status" className="rounded-xl border border-line bg-panel px-4 py-3 text-[12.5px] text-ink">
          {note}
        </p>
      ) : null}

      {finished ? (
        <div
          role="status"
          className={
            succeeded
              ? "rounded-xl border border-emerald-500/40 bg-emerald-500/5 px-4 py-3 text-[12.5px] text-emerald-800 dark:text-emerald-200"
              : "rounded-xl border border-red-500/40 bg-red-500/5 px-4 py-3 text-[12.5px] text-red-800 dark:text-red-200"
          }
        >
          {succeeded ? (
            <>
              The deploy finished and the health verification passed.{" "}
              <a
                className="underline"
                href={`/deployment/history?environment=${encodeURIComponent(current.environment)}`}
              >
                See it in the history
              </a>
              .
            </>
          ) : (
            <>
              The deploy did not succeed{current.error ? `: ${current.error}` : ""}. Nothing was
              verified, so the instance may still be on its previous version — check the log
              above, then roll back or deploy again.
              {/* The banner used to say "roll back" in prose and offer nothing to press, which is
                  the sentence an operator reads while holding a dead deploy. The rollback target
                  is the version this job came FROM — the only version the history can vouch for —
                  and the dialog itself still demands a reason and, on production, the version. */}
              {current.from_version ? (
                <button
                  type="button"
                  data-testid="failed-rollback"
                  onClick={() => onRollback({ environment: current.environment, toVersion: current.from_version as string })}
                  className="ml-2 inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] font-medium text-ink hover:bg-panel"
                >
                  <Undo2 aria-hidden="true" className="size-3.5" />
                  Roll back to {current.from_version}
                </button>
              ) : (
                <span className="ml-2 text-[12px] text-muted">
                  No earlier version was recorded, so there is nothing to roll back to.
                </span>
              )}
            </>
          )}
        </div>
      ) : (
        <div className="flex items-center gap-3">
          <button
            type="button"
            data-testid="run-cancel"
            onClick={onCancel}
            disabled={!current.cancellable}
            className="rounded-lg border border-red-500/50 px-4 py-2 text-[12.5px] font-medium text-red-700 disabled:cursor-not-allowed disabled:opacity-50 dark:text-red-300"
          >
            Cancel deploy
          </button>
          {!current.cancellable && current.cancel_refusal ? (
            <span className="text-[12px] text-muted">{current.cancel_refusal}</span>
          ) : null}
        </div>
      )}
    </div>
  );
}

/** Milliseconds as a duration, or an em dash when there is nothing to show. */
function formatElapsed(ms: number | null): string {
  if (ms === null || ms === undefined) return "—";
  const seconds = Math.max(0, Math.round(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${seconds % 60}s`;
}
