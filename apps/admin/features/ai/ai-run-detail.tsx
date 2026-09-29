"use client";

/**
 * One run, in full (docs/requests/REQ-099, slice 1).
 *
 * The acceptance box this screen exists to satisfy is "the same run reloaded after completion
 * shows an identical step list (replay matches SSE)". That makes the trace the product here,
 * and three rules follow from it.
 *
 * 1. **Arguments are collapsed and redacted.** The store already redacted them before the row
 *    existed; a transcript is read by people who are not the agent's author, and an accordion
 *    that opens by default turns a five-step trace into a wall of JSON. The redaction is the
 *    store's, not a second client-side rule that could disagree with it.
 * 2. **A live run tails.** `GET /runs/{id}/events` replays the step rows and keeps polling, so
 *    the trace grows while the run is going. The frames are the *rows*, not a second source —
 *    which is exactly why "replay matches SSE" is measurable rather than asserted.
 * 3. **Resume is offered only where it can work.** A run whose steps all completed, one that is
 *    still going, and one with a step left `running` by a crash each get a different refusal
 *    from the API, and the button is shown for the states the API accepts rather than letting
 *    the reader find out by pressing it.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  Ban,
  Check,
  ChevronDown,
  Copy,
  FileCheck,
  FileWarning,
  Loader2,
  Play,
  RotateCw,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  type AiRunDetail,
  type AiRunStep,
  attachAiRun,
  cancelAiRun,
  fetchAiAgent,
  fetchAiRun,
  resumeAiRun,
  type AiAgent,
} from "@/lib/api";
import { formatBytes } from "@/lib/format";
import { useSession } from "@/lib/session";

/** How a step kind reads, because `tool_result` is not a sentence. */
const KIND_LABEL: Record<string, string> = {
  message: "Message",
  tool_call: "Tool call",
  tool_result: "Tool result",
  approval: "Approval",
  note: "Note",
  error: "Error",
};

const STATUS_TONE: Record<string, string> = {
  running: "bg-accent-soft text-accent-strong",
  queued: "bg-quiet-soft text-muted",
  awaiting_approval: "bg-caution-soft text-caution",
  completed: "bg-positive-soft text-positive",
  failed: "bg-danger/10 text-danger",
  cancelled: "bg-quiet-soft text-muted",
};

const REASON_LABEL: Record<string, string> = {
  final_answer: "Final answer",
  max_steps: "Max steps",
  deadline: "Deadline",
  token_budget: "Token budget",
  cancelled: "Cancelled",
  loop_detected: "Loop detected",
  error: "Error",
};

function cost(micros: number): string {
  if (!micros) return "—";
  const dollars = micros / 1_000_000;
  return dollars < 0.01 ? `$${dollars.toFixed(4)}` : `$${dollars.toFixed(2)}`;
}

function tokens(count: number): string {
  if (count < 1000) return String(count);
  if (count < 1_000_000) return `${Math.round(count / 1000)}k`;
  return `${(count / 1_000_000).toFixed(1)}M`;
}

/** A run's JSON payload as readable text, with the ceiling a transcript row deserves. */
function pretty(value: unknown): string {
  if (value === null || value === undefined) return "";
  const text = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  return text.length > 4000 ? `${text.slice(0, 4000)}\n…` : text;
}

/** One step of the accordion. */
function StepRow({ step, open, onToggle }: { step: AiRunStep; open: boolean; onToggle: () => void }) {
  const label = KIND_LABEL[step.kind] ?? step.kind;
  return (
    <li data-run-step={step.step_no} className="border-t border-line first:border-t-0">
      <button
        type="button"
        data-run-step-toggle={step.step_no}
        aria-expanded={open}
        onClick={onToggle}
        className="flex w-full items-center gap-2 px-3 py-2 text-left hover:bg-quiet-soft/50"
      >
        <span className="w-8 shrink-0 font-mono text-[12px] text-muted">{step.step_no}</span>
        <span className="min-w-0 flex-1">
          <span className="block text-[12.5px] font-medium">
            {label}
            {step.tool ? <span className="ml-1.5 font-mono text-[12px] text-muted">{step.tool}</span> : null}
          </span>
          <span className="mt-0.5 block text-[11.5px] text-muted">
            {step.status}
            {step.duration_ms !== null ? ` · ${step.duration_ms}ms` : ""}
            {step.prompt_tokens + step.completion_tokens > 0
              ? ` · ${tokens(step.prompt_tokens + step.completion_tokens)} tokens`
              : ""}
            {step.cost_micros ? ` · ${cost(step.cost_micros)}` : ""}
          </span>
        </span>
        {step.error ? <span className="shrink-0 text-[11.5px] text-danger">failed</span> : null}
        <ChevronDown
          className={`size-3.5 shrink-0 text-muted transition-transform ${open ? "rotate-180" : ""}`}
          aria-hidden
        />
      </button>
      {open ? (
        <div data-run-step-body={step.step_no} className="space-y-2 px-3 pb-3 pl-11">
          {step.arguments && Object.keys(step.arguments).length > 0 ? (
            <div>
              <p className="text-[11.5px] font-medium text-muted">Arguments (redacted)</p>
              <pre className="mt-1 overflow-x-auto rounded-lg border border-line bg-canvas p-2 font-mono text-[11.5px]">
                {pretty(step.arguments)}
              </pre>
            </div>
          ) : null}
          {step.result !== null && step.result !== undefined ? (
            <div>
              <p className="text-[11.5px] font-medium text-muted">Result</p>
              <pre className="mt-1 max-h-64 overflow-auto rounded-lg border border-line bg-canvas p-2 font-mono text-[11.5px]">
                {pretty(step.result)}
              </pre>
            </div>
          ) : null}
          {step.error ? (
            <p className="text-[12px] text-danger">{step.error}</p>
          ) : null}
        </div>
      ) : null}
    </li>
  );
}

export function AiRunDetailView({ runId }: { runId: string }) {
  const { user } = useSession();
  const organizationId = user?.organization_id ?? undefined;

  const [run, setRun] = useState<AiRunDetail | null>(null);
  const [agent, setAgent] = useState<AiAgent | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [open, setOpen] = useState<Set<number>>(new Set());
  const [live, setLive] = useState(false);
  const [copied, setCopied] = useState(false);
  const attachAbort = useRef(0);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const detail = await fetchAiRun(runId, organizationId);
      setRun(detail);
      if (detail.agent_id) {
        setAgent(await fetchAiAgent(detail.agent_id, organizationId).catch(() => null));
      }
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The run could not be loaded.");
    } finally {
      setLoading(false);
    }
  }, [runId, organizationId]);

  useEffect(() => {
    void load();
  }, [load]);

  // The live tail. A replay, not a subscription: the API answers from the step rows and keeps
  // polling, so the trace on a reloaded page is the same trace the stream produced.
  useEffect(() => {
    if (!run || (run.status !== "running" && run.status !== "queued")) return;
    const generation = attachAbort.current + 1;
    attachAbort.current = generation;
    setLive(true);
    void (async () => {
      try {
        await attachAiRun(runId, { onFrame: () => void load() }, organizationId);
      } catch {
        // A dropped stream is not a failure: the run row is the authority, so re-read it once
        // and let the reader decide whether the trace still says the run is going.
      } finally {
        if (attachAbort.current === generation) setLive(false);
      }
    })();
    return () => {
      if (attachAbort.current === generation) attachAbort.current += 1;
    };
  }, [run, runId, organizationId, load]);

  const steps = run?.steps ?? [];
  // The run's named workspace references, in the order the sheet wrote them. A run created before
  // this column existed (or by a trigger that named nothing) carries an empty list, not `undefined`
  // — the route always sends the key, so the screen never has to guard for a missing field.
  const inputs = run?.inputs ?? [];
  const missing = inputs.filter((input) => !input.resolved);
  const running = run?.status === "running" || run?.status === "queued";
  // Resume is offered for the states the API actually accepts. A run whose steps all completed
  // is finished, one that is still going does not need it, and one with a step left `running`
  // by a crash is refused as ambiguous — offering the button anyway would teach the reader that
  // pressing it sometimes does nothing.
  const canResume =
    Boolean(run) &&
    !running &&
    run?.status !== "completed" &&
    steps.some((step) => step.status !== "completed");

  const act = async (action: "cancel" | "resume") => {
    setBusy(true);
    setError(null);
    try {
      if (action === "cancel") await cancelAiRun(runId, organizationId);
      else await resumeAiRun(runId, organizationId);
      setNotice(action === "cancel" ? "Cancellation requested." : "The run was requeued.");
      await load();
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The run could not be changed.");
    } finally {
      setBusy(false);
    }
  };

  const copyTranscript = async () => {
    const text = steps
      .map(
        (step) =>
          `#${step.step_no} ${KIND_LABEL[step.kind] ?? step.kind}${step.tool ? ` ${step.tool}` : ""} [${step.status}]` +
          (step.arguments ? `\n${pretty(step.arguments)}` : "") +
          (step.result !== null && step.result !== undefined ? `\n${pretty(step.result)}` : "") +
          (step.error ? `\nerror: ${step.error}` : ""),
      )
      .join("\n\n");
    const header = `# Run ${run?.id ?? runId}\n# goal: ${run?.goal ?? ""}\n# stop reason: ${run?.stop_reason ?? "—"} (${run?.status ?? "—"})\n\n`;
    try {
      await navigator.clipboard.writeText(header + text);
      setCopied(true);
    } catch {
      // Clipboard permission is not guaranteed (an http origin, a denied prompt). Say so
      // rather than pretending the copy worked.
      setError("The browser would not let the panel write to the clipboard.");
    }
  };

  const summary = useMemo(
    () =>
      steps
        .filter((step) => step.kind === "message" && step.result)
        .map((step) => pretty(step.result))
        .join("\n\n"),
    [steps],
  );

  if (loading && !run) {
    return (
      <p className="text-[13px] text-muted">Loading the run…</p>
    );
  }

  if (!run) {
    return (
      <div data-run-detail-error className="space-y-3">
        <p className="text-[13px] text-danger">{error ?? "The run could not be loaded."}</p>
        <button
          type="button"
          onClick={() => void load()}
          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px]"
        >
          Retry
        </button>
        <Link href="/ai/runs" className="block text-[12.5px] text-accent-strong underline underline-offset-2">
          Back to the run history
        </Link>
      </div>
    );
  }

  return (
    <div data-run-detail className="space-y-4">
      <header className="flex flex-wrap items-start gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="text-[15px] font-semibold" data-run-detail-goal>
              {run.goal}
            </h2>
            <span
              data-run-detail-status={run.status}
              className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${
                STATUS_TONE[run.status] ?? "bg-quiet-soft text-muted"
              }`}
            >
              {run.status.replace(/_/g, " ")}
            </span>
            {live ? (
              <span data-run-detail-live className="inline-flex items-center gap-1 text-[11.5px] text-accent-strong">
                <Loader2 className="size-3 animate-spin" aria-hidden />
                live
              </span>
            ) : null}
          </div>
          <p className="mt-1 text-[12.5px] text-muted">
            {agent ? (
              <Link href={`/ai/agents/${agent.id}`} className="underline underline-offset-2">
                {agent.name}
              </Link>
            ) : (
              <span>{run.trigger}</span>
            )}
            {" · "}
            {run.current_step} steps · {tokens(run.prompt_tokens + run.completion_tokens)} tokens ·{" "}
            {cost(run.cost_micros)}
            {run.stop_reason ? ` · ${REASON_LABEL[run.stop_reason] ?? run.stop_reason}` : ""}
            {run.resume_count > 0 ? ` · resumed ${run.resume_count}×` : ""}
          </p>
          {run.error ? <p className="mt-1 text-[12.5px] text-danger">{run.error}</p> : null}
        </div>

        <div className="flex flex-wrap items-center gap-1.5">
          {running ? (
            <button
              type="button"
              data-run-detail-cancel
              disabled={busy}
              onClick={() => void act("cancel")}
              className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12.5px] disabled:opacity-40"
            >
              <Ban className="size-3" aria-hidden />
              Cancel
            </button>
          ) : null}
          {canResume ? (
            <button
              type="button"
              data-run-detail-resume
              disabled={busy}
              onClick={() => void act("resume")}
              className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12.5px] disabled:opacity-40"
            >
              <Play className="size-3" aria-hidden />
              Resume
            </button>
          ) : null}
          <button
            type="button"
            data-run-detail-copy
            onClick={() => void copyTranscript()}
            className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12.5px]"
          >
            {copied ? <Check className="size-3" aria-hidden /> : <Copy className="size-3" aria-hidden />}
            {copied ? "Copied" : "Copy transcript"}
          </button>
          <button
            type="button"
            data-run-detail-refresh
            onClick={() => void load()}
            className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12.5px] text-muted"
          >
            <RotateCw className="size-3" aria-hidden />
          </button>
        </div>
      </header>

      {error ? (
        <p data-run-detail-error className="rounded-xl border border-danger/40 bg-danger/5 px-3 py-2 text-[12.5px] text-danger">
          {error}
        </p>
      ) : null}
      {notice ? <p data-run-detail-notice className="text-[12.5px] text-positive">{notice}</p> : null}

      {summary ? (
        <section data-run-detail-answer className="rounded-xl border border-line bg-surface p-3.5">
          <h3 className="text-[12px] font-medium text-muted">The answer</h3>
          <p className="mt-1 whitespace-pre-wrap text-[13px]">{summary}</p>
        </section>
      ) : null}

      {inputs.length > 0 ? (
        <section data-run-detail-inputs className="rounded-xl border border-line bg-surface p-3.5">
          <h3 className="text-[12px] font-medium text-muted">Named workspace inputs</h3>
          <ul className="mt-2 space-y-1">
            {inputs.map((input) => (
              <li
                key={input.id}
                data-run-input={input.path}
                data-run-input-resolved={input.resolved}
                className="flex flex-wrap items-center gap-2 text-[12.5px]"
              >
                {input.resolved ? (
                  <FileCheck className="size-3.5 shrink-0 text-positive" aria-hidden />
                ) : (
                  <FileWarning className="size-3.5 shrink-0 text-danger" aria-hidden />
                )}
                <span className="font-mono">{input.path}</span>
                <span className="text-muted">
                  {input.resolved ? formatBytes(input.size_bytes) : "missing"}
                </span>
              </li>
            ))}
          </ul>
          {missing.length > 0 ? (
            // Named, not counted: "one input is missing" sends a person to the workspace tab to
            // look at every file, and the file that is gone is the one they cannot see is gone.
            <p data-run-detail-inputs-missing className="mt-2 text-[12px] text-danger">
              {missing.length} of {inputs.length} named input(s) cannot be resolved:{" "}
              {missing.map((input) => input.path).join(", ")}
            </p>
          ) : null}
        </section>
      ) : null}

      {steps.length === 0 ? (
        <EmptyState
          title="No steps yet"
          hint="A run writes its first step before its first provider call. If the runner is switched off, no step ever appears and the start endpoint would have said so."
        />
      ) : (
        <ul data-run-trace className="overflow-hidden rounded-xl border border-line bg-surface">
          {steps.map((step) => (
            <StepRow
              key={step.step_no}
              step={step}
              open={open.has(step.step_no)}
              onToggle={() =>
                setOpen((previous) => {
                  const next = new Set(previous);
                  if (next.has(step.step_no)) next.delete(step.step_no);
                  else next.add(step.step_no);
                  return next;
                })
              }
            />
          ))}
        </ul>
      )}

      <Link href="/ai/runs" className="block text-[12.5px] text-accent-strong underline underline-offset-2">
        Back to the run history
      </Link>
    </div>
  );
}
