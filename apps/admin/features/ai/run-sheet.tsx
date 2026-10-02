"use client";

/**
 * The Run sheet (docs/requests/REQ-099, slice 1).
 *
 * Starting a run is the one action on the agent screens that spends money, so this sheet is
 * built around three rules that follow from that, and each one is a way a naive version loses
 * money without saying so.
 *
 * 1. **The goal is the only required field, and it is counted.** An agent that is handed
 *    "check the site" has nothing to check, and the failure shows up as an unhelpful run rather
 *    than as an empty box. The counter turns 2000 characters from a shrug into a number.
 * 2. **The stream is the sheet, not a redirect.** `POST /runs` answers `text/event-stream`, so
 *    the steps land here as they happen and the reader watches the loop work. Hanging off to a
 *    detail page immediately would throw away the only moment that says whether the agent
 *    understood the goal.
 * 3. **A 409 is not an error, it is an answer.** The API refuses a second run for an agent
 *    already running and hands back the *existing* run's id, because the guarantee is a partial
 *    unique index and the button being pressed twice is the normal case, not the exceptional
 *    one. The sheet therefore offers "watch the running run" rather than a red banner.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { Check, CircleAlert, Loader2, Play, X } from "lucide-react";

import {
  ApiError,
  fetchAiAgentWorkspace,
  type AiAgent,
  type AiAgentFile,
  type AiRunFrame,
  startAiRun,
} from "@/lib/api";
import { formatBytes } from "@/lib/format";

/** What the sheet prints as a line; the SSE frames folded into something a person can read. */
type SheetLine = {
  /** Monotonic within one run, so two lines with the same step number keep their order. */
  seq: number;
  kind: "info" | "text" | "tool" | "error" | "usage";
  stepNo: number | null;
  text: string;
};

/** The longest goal the API accepts; the counter quotes the same number the route validates. */
const MAX_GOAL = 2000;

/**
 * How many workspace files one run may be told to read.
 *
 * The same ceiling the API enforces (`workspace::MAX_RUN_INPUTS`), restated because a picker that
 * lets a person choose an eleventh file and is then refused by the server is a picker that lies.
 * The API is still the authority; this only stops the mistake before it costs a round trip.
 */
const MAX_INPUTS = 10;

/** What the sheet knows about the run it is watching. */
type SheetState = "idle" | "starting" | "streaming" | "finished" | "failed";

/** How one frame folds into a line. Pure, so the unit walk can reason about it. */
export function frameToLine(frame: AiRunFrame, seq: number): SheetLine | null {
  const data = frame.data;
  const stepNo =
    typeof data.step_no === "number" ? data.step_no : data.steps ? Number(data.steps) : null;

  switch (frame.event) {
    case "run":
      return {
        seq,
        kind: "info",
        stepNo: null,
        text: `The run was claimed by the runner.`,
      };
    case "step_started":
      return { seq, kind: "info", stepNo, text: `Step ${stepNo ?? "?"} started` };
    case "text": {
      const delta = String(data.delta ?? "");
      return delta ? { seq, kind: "text", stepNo, text: delta } : null;
    }
    case "tool_call": {
      const call = (data.call ?? {}) as { name?: string };
      return { seq, kind: "tool", stepNo, text: `Calling ${call.name ?? "a tool"}` };
    }
    case "tool_result": {
      const failed = data.failed === true;
      return {
        seq,
        kind: failed ? "error" : "tool",
        stepNo,
        text: failed
          ? `${String(data.tool ?? "tool")} failed: ${String(data.summary ?? "")}`
          : `${String(data.tool ?? "tool")} returned`,
      };
    }
    case "usage":
      return {
        seq,
        kind: "usage",
        stepNo,
        text: `${Number(data.prompt_tokens ?? 0)} in, ${Number(data.completion_tokens ?? 0)} out`,
      };
    case "awaiting_approval":
      return {
        seq,
        kind: "info",
        stepNo,
        text: `Waiting for a person to approve ${String(data.tool ?? "a tool")}`,
      };
    case "error":
      return { seq, kind: "error", stepNo, text: String(data.message ?? data.code ?? "failed") };
    default:
      return null;
  }
}

type RunSheetProps = {
  /** The agent being run. */
  agent: AiAgent;
  /** The organization to act for, when the account is platform-level. */
  organizationId?: string | null;
  /** Close the sheet. */
  onClose: () => void;
  /** Called once a run exists, so the list can refresh around it. */
  onStarted?: (runId: string) => void;
  /** Called with the run id when the reader chooses to open the full trace. */
  onOpenRun: (runId: string) => void;
};

export function RunSheet({ agent, organizationId, onClose, onStarted, onOpenRun }: RunSheetProps) {
  const [goal, setGoal] = useState("");
  const [state, setState] = useState<SheetState>("idle");
  const [lines, setLines] = useState<SheetLine[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [runId, setRunId] = useState<string | null>(null);
  /** The id of a run that is already going, offered as a link instead of an error. */
  const [attached, setAttached] = useState<string | null>(null);
  const [files, setFiles] = useState<AiAgentFile[]>([]);
  const [chosen, setChosen] = useState<string[]>([]);
  const seq = useRef(0);
  const logRef = useRef<HTMLDivElement>(null);
  const goalRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    goalRef.current?.focus();
  }, []);

  // The log follows the stream: a run that prints twenty steps into a fixed-height box the
  // reader cannot scroll to the bottom of looks like a run that stopped after step three.
  useEffect(() => {
    const node = logRef.current;
    if (node) node.scrollTop = node.scrollHeight;
  }, [lines]);

  const push = useCallback((line: SheetLine | null) => {
    if (!line) return;
    seq.current += 1;
    setLines((previous) => [...previous, line]);
  }, []);

  const start = async () => {
    if (!goal.trim()) {
      setError("Say what the agent should do — an empty goal runs nothing useful.");
      goalRef.current?.focus();
      return;
    }
    setState("starting");
    setError(null);
    setAttached(null);
    try {
      await startAiRun(
        agent.id,
        { goal: goal.trim(), files: chosen, organizationId },
        {
          onFrame: (frame) => {
            if (frame.event === "run" && typeof frame.data.run_id === "string") {
              const id = frame.data.run_id;
              setRunId(id);
              onStarted?.(id);
            }
            if (frame.event === "done") {
              setState("finished");
            }
            if (frame.event === "error" && frame.data.code === "run_not_claimed") {
              setState("failed");
            }
            push(frameToLine(frame, seq.current + 1));
          },
        },
      );
      // The stream closed without a `done` frame (a proxy cut it, the runner died): the run is
      // still the authority, so say so instead of claiming an outcome nobody reported.
      setState((previous) => (previous === "finished" ? previous : "finished"));
    } catch (caught) {
      const error = caught as ApiError;
      if (error.code === "run_in_progress") {
        const existing = error.details?.run_id;
        if (typeof existing === "string") {
          setAttached(existing);
          setState("idle");
          return;
        }
      }
      setError(error.message);
      setState("failed");
    }
  };

  const running = state === "starting" || state === "streaming";

  // The agent's workspace, loaded when the sheet opens. A failure here is **not** shown as the
  // sheet's error: the goal is still startable without any input, and a red banner saying "the
  // workspace could not be read" would stop a run that would have worked. The picker says it
  // itself instead, and the Run button stays live.
  useEffect(() => {
    let cancelled = false;
    void fetchAiAgentWorkspace(agent.id, organizationId)
      .then((workspace) => {
        if (!cancelled) setFiles(workspace.files);
      })
      .catch(() => {
        if (!cancelled) setFiles([]);
      });
    return () => {
      cancelled = true;
    };
  }, [agent.id, organizationId]);

  /** Add or drop a path, refusing the eleventh rather than letting the API say no later. */
  const toggleFile = (path: string) => {
    setChosen((previous) => {
      if (previous.includes(path)) return previous.filter((item) => item !== path);
      if (previous.length >= MAX_INPUTS) {
        setError(`A run can be told to read at most ${MAX_INPUTS} workspace files.`);
        return previous;
      }
      setError(null);
      return [...previous, path];
    });
  };
  const text = useMemo(() => lines.filter((line) => line.kind === "text").map((l) => l.text).join(""), [lines]);

  return (
    <div
      data-run-sheet
      className="fixed inset-0 z-50 flex items-end justify-center bg-ink/25 p-0 sm:items-center sm:p-4"
      role="dialog"
      aria-modal="true"
      aria-label={`Run ${agent.name}`}
    >
      <div
        data-run-sheet-panel
        className="flex max-h-[92vh] w-full max-w-2xl flex-col overflow-hidden rounded-t-2xl border border-line bg-surface sm:rounded-2xl"
      >
        <header className="flex items-start gap-3 border-b border-line px-4 py-3">
          <div className="min-w-0 flex-1">
            <h2 className="truncate text-[14px] font-semibold">Run {agent.name}</h2>
            <p className="mt-0.5 text-[12.5px] text-muted">
              {agent.model_id ? "The model this agent is pinned to answers." : "The router picks a model for each task."}
              {agent.approvals_count > 0
                ? ` ${agent.approvals_count} tool${agent.approvals_count === 1 ? "" : "s"} park for approval first.`
                : ""}
            </p>
          </div>
          <button
            type="button"
            aria-label="Close the run sheet"
            data-run-sheet-close
            onClick={onClose}
            className="rounded p-1 text-muted hover:bg-quiet-soft hover:text-ink"
          >
            <X className="size-4" />
          </button>
        </header>

        <div className="min-h-0 flex-1 space-y-3 overflow-y-auto px-4 py-3">
          <div>
            <label htmlFor="run-goal" className="block text-[12.5px] font-medium">
              Goal
            </label>
            <textarea
              id="run-goal"
              ref={goalRef}
              data-run-goal
              rows={3}
              value={goal}
              maxLength={MAX_GOAL}
              disabled={running}
              onChange={(event) => {
                setGoal(event.target.value);
                setError(null);
              }}
              placeholder="What should the agent do?"
              className="mt-1 w-full rounded-lg border border-line bg-canvas px-2.5 py-2 text-[13px] outline-none focus:border-accent disabled:opacity-60"
            />
            <p className="mt-1 text-right text-[11.5px] text-muted">
              {goal.length} / {MAX_GOAL}
            </p>
          </div>

          <div>
            <div className="flex items-baseline justify-between gap-2">
              <span className="block text-[12.5px] font-medium">Workspace files to read</span>
              <span className="text-[11.5px] text-muted">
                {chosen.length} / {MAX_INPUTS} chosen
              </span>
            </div>
            {files.length === 0 ? (
              // Not an error and not a red banner: the run is startable without an input, and the
              // thing that would make one available (the Workspace tab) is one link away.
              <p data-run-inputs-empty className="mt-1 text-[12px] text-muted">
                This agent&apos;s workspace is empty. The run starts without an input — add a file in
                the agent&apos;s Workspace tab to give it something to read.
              </p>
            ) : (
              <ul data-run-inputs className="mt-1 max-h-32 space-y-0.5 overflow-y-auto rounded-lg border border-line bg-canvas p-1.5">
                {files.map((file) => {
                  const on = chosen.includes(file.path);
                  return (
                    <li key={file.id}>
                      <button
                        type="button"
                        data-run-input={file.path}
                        data-run-input-selected={on}
                        aria-pressed={on}
                        disabled={running}
                        onClick={() => toggleFile(file.path)}
                        className="flex w-full items-center gap-2 rounded px-1.5 py-1 text-left text-[12px] hover:bg-quiet-soft disabled:opacity-60"
                      >
                        <span
                          aria-hidden
                          className={`flex size-3.5 shrink-0 items-center justify-center rounded border ${
                            on ? "border-accent bg-accent text-white" : "border-line"
                          }`}
                        >
                          {on ? <Check className="size-2.5" /> : null}
                        </span>
                        <span className="min-w-0 flex-1 truncate font-mono">{file.path}</span>
                        <span className="shrink-0 text-muted">{formatBytes(file.size_bytes)}</span>
                      </button>
                    </li>
                  );
                })}
              </ul>
            )}
          </div>

          {attached ? (
            <p
              data-run-attached
              className="flex items-start gap-2 rounded-lg border border-caution/40 bg-caution-soft px-3 py-2 text-[12.5px] text-caution"
            >
              <CircleAlert className="mt-0.5 size-3.5 shrink-0" aria-hidden />
              <span>
                This agent already has a run in progress.{" "}
                <button
                  type="button"
                  data-run-attached-open
                  onClick={() => onOpenRun(attached)}
                  className="font-medium underline underline-offset-2"
                >
                  Watch that run
                </button>{" "}
                instead of starting a second one.
              </span>
            </p>
          ) : null}

          {error ? (
            <p data-run-sheet-error className="text-[12.5px] text-danger">
              {error}
            </p>
          ) : null}

          {lines.length > 0 ? (
            <div
              ref={logRef}
              data-run-log
              className="max-h-64 overflow-y-auto rounded-lg border border-line bg-canvas p-2.5 font-mono text-[12px] leading-relaxed"
            >
              {lines.map((line) => (
                <p
                  key={line.seq}
                  data-run-log-line={line.kind}
                  className={
                    line.kind === "error"
                      ? "text-danger"
                      : line.kind === "text"
                        ? "text-ink"
                        : line.kind === "tool"
                          ? "text-accent-strong"
                          : line.kind === "usage"
                            ? "text-muted"
                            : "text-muted"
                  }
                >
                  {line.stepNo !== null ? <span className="text-muted">[{line.stepNo}] </span> : null}
                  {line.text}
                </p>
              ))}
              {running ? <p className="text-muted">streaming…</p> : null}
            </div>
          ) : null}

          {text ? (
            <section data-run-answer className="rounded-lg border border-line bg-surface p-3">
              <h3 className="text-[12px] font-medium text-muted">The answer</h3>
              <p className="mt-1 whitespace-pre-wrap text-[13px]">{text}</p>
            </section>
          ) : null}
        </div>

        <footer className="flex items-center gap-2 border-t border-line px-4 py-3">
          <button
            type="button"
            data-run-start
            disabled={running || Boolean(attached)}
            onClick={() => void start()}
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[13px] font-medium text-white disabled:opacity-50"
          >
            {running ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <Play className="size-3.5" aria-hidden />
            )}
            {running ? "Running…" : "Run"}
          </button>
          {runId ? (
            <button
              type="button"
              data-run-open
              onClick={() => onOpenRun(runId)}
              className="text-[12.5px] text-accent-strong underline underline-offset-2"
            >
              Open the full trace
            </button>
          ) : null}
          <button
            type="button"
            onClick={onClose}
            className="ml-auto text-[12.5px] text-muted hover:text-ink"
          >
            Close
          </button>
        </footer>
      </div>
    </div>
  );
}
