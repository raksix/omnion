"use client";

/**
 * The AI workflow builder's review screen (docs/requests/REQ-046, slice 3, decision bar in
 * slice 4).
 *
 * One draft, and everything a person needs to decide about it: the prompt it answers, the
 * model's own rationale, the definition as JSON, and a **read-only step list** derived from
 * that same definition. The step list is not a second copy — it is projected from the stored
 * JSON, because the request's core promise is that *a generated definition is an ordinary
 * definition*, and a step list the engine could disagree with would be the one part of the
 * review screen that is not the thing being approved.
 *
 * Slice 3 rendered the approval bar with its buttons **absent** and said why. That was the
 * honest half: a screen that shows a control it cannot honour teaches the operator the
 * platform is broken. This slice puts the bar in, and the two properties that make it more
 * than four buttons are worth naming:
 *
 * * **The bar is drawn from the draft's status, not from a flag.** `approvable` is a
 *   derivation (`draft` or `failed`, with a definition, and no workflow yet) rather than
 *   something the component tracks itself, so a re-fetch after a decision cannot leave the
 *   screen offering to decide a draft that is already a rule.
 * * **Every action reports what happened, in place.** Approve answers a workflow id, reject
 *   answers the reason that is now on the row, revise answers a stream with the same stage
 *   panel the console uses, and test-run answers a plan. A decision bar whose success is
 *   only visible after a page reload is a bar that gets pressed twice.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  ArrowLeft,
  Check,
  CircleAlert,
  FileJson,
  FlaskConical,
  Info,
  Loader2,
  MessageSquare,
  Sparkles,
  ThumbsDown,
  ThumbsUp,
  X,
} from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";

import { StatusBadge } from "@/components/status-badge";
import {
  ApiError,
  type AiWorkflowDraftDetail,
  type AiWorkflowStage,
  type AiWorkflowTestRun,
  approveAiWorkflowDraft,
  fetchAiWorkflowDraft,
  rejectAiWorkflowDraft,
  saveAiWorkflowDefinition,
  streamReviseWorkflowDraft,
  testRunAiWorkflowDraft,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** One row of the read-only step list. */
function StepRow({
  step,
  highlighted,
}: {
  step: AiWorkflowDraftDetail["steps"][number];
  highlighted: boolean;
}) {
  return (
    <li
      data-step={step.position}
      className="flex flex-col gap-1 border-b border-line px-3 py-2.5 last:border-b-0"
    >
      <div className="flex flex-wrap items-center gap-2">
        <span className="w-5 shrink-0 text-[11.5px] tabular-nums text-muted">
          {step.position}.
        </span>
        <span className="text-[13px] font-medium text-ink">{step.name || "(unnamed step)"}</span>
        {step.action ? (
          <code className="rounded bg-quiet-soft px-1.5 py-0.5 font-mono text-[11.5px]">
            {step.action}
          </code>
        ) : (
          <code className="rounded bg-quiet-soft px-1.5 py-0.5 font-mono text-[11.5px]">
            {step.kind}
          </code>
        )}
        {highlighted ? (
          <span
            data-ai-step
            className="inline-flex items-center gap-1 rounded-full bg-accent-soft px-1.5 py-0.5 text-[10.5px] font-medium text-accent-strong"
          >
            <Sparkles aria-hidden className="size-3" />
            asks a model
          </span>
        ) : null}
      </div>
      {step.params && typeof step.params === "object" ? (
        <pre
          data-step-params={step.position}
          className="ml-7 max-h-40 overflow-auto rounded bg-quiet-soft/60 px-2 py-1.5 font-mono text-[11.5px] text-muted"
        >
          {JSON.stringify(step.params, null, 2)}
        </pre>
      ) : null}
    </li>
  );
}

/** What a decision action is currently doing, and what it answered. */
type Outcome =
  | { kind: "approve"; message: string; workflowId: string | null }
  | { kind: "reject"; message: string }
  | { kind: "revise"; message: string }
  | { kind: "save"; message: string }
  | { kind: "test-run"; message: string; run: AiWorkflowTestRun };

/** The screen. */
export function AiWorkflowDraftReview({ draftId }: { draftId: string }) {
  const router = useRouter();
  const [draft, setDraft] = useState<AiWorkflowDraftDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const [busy, setBusy] = useState<"approve" | "reject" | "revise" | "save" | "test-run" | null>(
    null,
  );
  const [rejectionReason, setRejectionReason] = useState("");
  const [revisionNote, setRevisionNote] = useState("");
  const [stage, setStage] = useState<AiWorkflowStage | null>(null);
  const [testRun, setTestRun] = useState<AiWorkflowTestRun | null>(null);
  const abortRef = useRef<AbortController | null>(null);

  const retry = useCallback(() => setReloadToken((token) => token + 1), []);

  // A decision changes the row, so the bar and the step list are re-read from the API rather
  // than patched locally: a decision is the one operation on this screen where a
  // locally-updated copy could disagree with what was stored.
  const refresh = useCallback(async () => {
    const loaded = await fetchAiWorkflowDraft(draftId);
    setDraft(loaded);
    return loaded;
  }, [draftId]);

  useEffect(() => {
    let cancelled = false;
    setError(null);
    fetchAiWorkflowDraft(draftId)
      .then((loaded) => {
        if (!cancelled) setDraft(loaded);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setDraft(null);
        setError(
          cause instanceof ApiError
            ? cause.message
            : "That workflow draft could not be loaded.",
        );
      });
    return () => {
      cancelled = true;
    };
  }, [draftId, reloadToken]);

  const run = useCallback(
    async (action: NonNullable<typeof busy>, work: () => Promise<Outcome>) => {
      setBusy(action);
      setOutcome(null);
      setError(null);
      try {
        setOutcome(await work());
        await refresh();
      } catch (cause) {
        setOutcome(null);
        setError(
          cause instanceof ApiError
            ? cause.message
            : "That draft could not be changed. Nothing was saved.",
        );
      } finally {
        setBusy(null);
      }
    },
    [refresh],
  );

  const approve = useCallback(() => {
    void run("approve", async () => {
      const decided = await approveAiWorkflowDraft(draftId);
      return {
        kind: "approve",
        workflowId: decided.workflow_id ?? null,
        message: decided.workflow_id
          ? `Approved. It is now workflow ${decided.workflow_id}, disabled — enable it there when the schedule is right.`
          : "Approved, but the API reported no workflow. Open the draft again.",
      };
    });
  }, [draftId, run]);

  const reject = useCallback(() => {
    const reason = rejectionReason.trim();
    if (reason.length === 0) {
      setError("A rejection needs a reason the person who asked for it can read.");
      return;
    }
    void run("reject", async () => {
      await rejectAiWorkflowDraft(draftId, reason);
      setRejectionReason("");
      return { kind: "reject", message: "Rejected, and the reason is on the draft." };
    });
  }, [draftId, rejectionReason, run]);

  const revise = useCallback(() => {
    const note = revisionNote.trim();
    if (note.length === 0) {
      setError("Asking for changes needs a note saying what to change.");
      return;
    }
    setBusy("revise");
    setOutcome(null);
    setError(null);
    setStage("plan");
    abortRef.current?.abort();
    const controller = new AbortController();
    abortRef.current = controller;
    void streamReviseWorkflowDraft(
      draftId,
      note,
      undefined,
      {
        onStage: (next) => setStage(next),
        onDone: (done) => {
          setStage(null);
          setBusy(null);
          setRevisionNote("");
          setOutcome({
            kind: "revise",
            message: done.repaired
              ? "Revised. The first answer was refused and repaired once."
              : "Revised. Open the steps to see what changed.",
          });
          void refresh();
        },
      },
      controller.signal,
    ).catch((cause: unknown) => {
      if (controller.signal.aborted) {
        setStage(null);
        setBusy(null);
        return;
      }
      setStage(null);
      setBusy(null);
      setOutcome(null);
      setError(
        cause instanceof ApiError
          ? cause.message
          : "The revision did not finish. The draft keeps the definition it had.",
      );
    });
  }, [draftId, refresh, revisionNote]);

  const cancelRevision = useCallback(() => {
    abortRef.current?.abort();
    abortRef.current = null;
    setStage(null);
    setBusy(null);
  }, []);

  useEffect(() => () => abortRef.current?.abort(), []);

  const definition = draft?.definition ? JSON.stringify(draft.definition, null, 2) : null;
  const isAiStep = (action: string | null) => action === "ai.prompt";

  // Derived from the row rather than tracked here — see the module comment.
  const approvable = useMemo(() => {
    if (!draft) return false;
    return (
      (draft.status === "draft" || draft.status === "failed") &&
      draft.has_definition &&
      draft.workflow_id === null
    );
  }, [draft]);

  if (error && !draft) {
    return (
      <div className="flex flex-col items-start gap-3">
        <p
          role="alert"
          data-review-error
          className="rounded-lg border border-caution/40 bg-caution-soft px-3 py-2 text-[12.5px]"
        >
          {error}
        </p>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={retry}
            data-review-retry
            className="rounded-md border border-line px-3 py-1.5 text-[12.5px] hover:bg-quiet-soft focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
          >
            Retry
          </button>
          <Link
            href="/ai/workflows"
            className="rounded-md border border-line px-3 py-1.5 text-[12.5px] hover:bg-quiet-soft focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
          >
            Back to the console
          </Link>
        </div>
      </div>
    );
  }

  if (draft === null) {
    return (
      <div
        role="status"
        aria-live="polite"
        data-review-loading
        className="flex items-center gap-2 text-[13px] text-muted"
      >
        <Loader2 aria-hidden className="size-4 animate-spin" />
        Loading the draft…
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-5">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <Link
          href="/ai/workflows"
          data-back-to-console
          className="inline-flex items-center gap-1.5 text-[12.5px] text-muted underline-offset-2 hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
        >
          <ArrowLeft aria-hidden className="size-3.5" />
          All drafts
        </Link>
        <StatusBadge status={draft.status} />
      </div>

      <header className="flex flex-col gap-1.5">
        <h2 className="text-[16px] font-semibold" data-draft-title>
          {draft.title || "Untitled draft"}
        </h2>
        <p className="text-[12px] text-muted">
          {draft.model_key ?? "unknown model"} · created{" "}
          {formatTimestamp(draft.created_at)} · updated {formatTimestamp(draft.updated_at)}
          {draft.tokens_input + draft.tokens_output > 0
            ? ` · ${draft.tokens_input + draft.tokens_output} tokens`
            : ""}
        </p>
      </header>

      {/* ---- The error the row carries ---------------------------------------------------- */}
      {draft.error ? (
        <p
          role="alert"
          data-review-generation-error
          className="flex items-start gap-2 rounded-lg border border-caution/40 bg-caution-soft px-3 py-2 text-[12.5px]"
        >
          <CircleAlert aria-hidden className="mt-0.5 size-3.5 shrink-0" />
          <span>
            <strong className="font-medium">Generation failed.</strong> {draft.error}
          </span>
        </p>
      ) : null}

      {/* ---- What the last action answered ------------------------------------------------ */}
      {outcome ? (
        <p
          role="status"
          aria-live="polite"
          data-outcome={outcome.kind}
          className="flex items-start gap-2 rounded-lg border border-positive/30 bg-positive-soft px-3 py-2 text-[12.5px]"
        >
          <Check aria-hidden className="mt-0.5 size-3.5 shrink-0" />
          <span>{outcome.message}</span>
        </p>
      ) : null}

      {/* ---- An action that could not be done --------------------------------------------- */}
      {error && draft ? (
        <p
          role="alert"
          data-review-action-error
          className="flex items-start gap-2 rounded-lg border border-caution/40 bg-caution-soft px-3 py-2 text-[12.5px]"
        >
          <CircleAlert aria-hidden className="mt-0.5 size-3.5 shrink-0" />
          <span>{error}</span>
        </p>
      ) : null}

      {/* ---- The prompt and the rationale ------------------------------------------------ */}
      <section
        aria-labelledby="prompt-heading"
        className="rounded-xl border border-line bg-surface p-4"
      >
        <h3 id="prompt-heading" className="text-[13px] font-semibold">
          The prompt
        </h3>
        <p
          data-draft-prompt
          className="mt-1.5 whitespace-pre-wrap text-[13px] leading-relaxed text-ink"
        >
          {draft.prompt}
        </p>
        {draft.rationale ? (
          <>
            <h3 className="mt-3 text-[13px] font-semibold">Why the model built this</h3>
            <p
              data-draft-rationale
              className="mt-1.5 whitespace-pre-wrap text-[13px] leading-relaxed text-muted"
            >
              {draft.rationale}
            </p>
          </>
        ) : null}
        {draft.revision_count > 0 ? (
          <p className="mt-2 flex items-start gap-1.5 text-[12px] text-muted">
            <Info aria-hidden className="mt-0.5 size-3.5 shrink-0" />
            <span>
              Revised {draft.revision_count}×
              {draft.revision_note ? `: "${draft.revision_note}"` : ""}
            </span>
          </p>
        ) : null}
        {draft.decision_reason ? (
          <p className="mt-2 text-[12px] text-muted">
            Decision: {draft.decision_reason}
            {draft.decided_at ? ` · ${formatTimestamp(draft.decided_at)}` : ""}
          </p>
        ) : null}
      </section>

      {draft.workflow_id ? (
        <p
          data-draft-workflow
          className="flex flex-wrap items-center gap-2 rounded-lg border border-positive/30 bg-positive-soft px-3 py-2 text-[12.5px]"
        >
          <Check aria-hidden className="size-3.5" />
          This draft became workflow{" "}
          <Link
            href={`/workflows/${draft.workflow_id}/builder`}
            data-open-in-builder
            className="underline underline-offset-2 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
          >
            open it in the builder
          </Link>
          . It is disabled until you enable it there.
        </p>
      ) : null}

      {/* ---- The steps and the definition -------------------------------------------------- */}
      <div className="grid gap-4 lg:grid-cols-2">
        <section
          aria-labelledby="steps-heading"
          className="rounded-xl border border-line bg-surface"
        >
          <h3 id="steps-heading" className="px-3 pt-3 text-[13px] font-semibold">
            Steps ({draft.steps.length})
          </h3>
          <p className="px-3 pt-0.5 text-[11.5px] text-muted">
            Read-only. This is the definition the engine runs, projected from the same JSON.
          </p>
          <ul data-step-list className="mt-2 flex flex-col">
            {draft.steps.length === 0 ? (
              <li className="px-3 py-6 text-center text-[12.5px] text-muted">
                No steps yet — the draft is still waiting on the model.
              </li>
            ) : (
              draft.steps.map((step) => (
                <StepRow key={step.position} step={step} highlighted={isAiStep(step.action)} />
              ))
            )}
          </ul>
        </section>

        <DefinitionEditor
          definition={definition}
          editable={draft.status === "draft"}
          busy={busy === "save"}
          onSave={(next) =>
            void run("save", async () => {
              const saved = await saveAiWorkflowDefinition(draftId, next);
              return {
                kind: "save",
                message: `Saved. ${saved.steps.length} step${saved.steps.length === 1 ? "" : "s"} now, revalidated by the engine.`,
              };
            })
          }
        />
      </div>

      {/* ---- The test run's plan ----------------------------------------------------------- */}
      {testRun ? (
        <section
          aria-labelledby="test-run-heading"
          data-test-run
          className="rounded-xl border border-line bg-surface p-4"
        >
          <h3
            id="test-run-heading"
            className="flex items-center gap-1.5 text-[13px] font-semibold"
          >
            <FlaskConical aria-hidden className="size-3.5" />
            Test run · <span className="text-positive">{testRun.verdict}</span>
          </h3>
          <p className="mt-1 text-[12px] text-muted">{testRun.note}</p>
          <ol data-test-run-steps className="mt-2 flex flex-col gap-1">
            {testRun.steps.map((step) => (
              <li
                key={step.position}
                className="flex flex-wrap items-center gap-2 rounded border border-line px-2 py-1.5 text-[12px]"
              >
                <span className="w-5 shrink-0 tabular-nums text-muted">{step.position}.</span>
                <span className="font-medium text-ink">{step.name}</span>
                {step.action ? (
                  <code className="rounded bg-quiet-soft px-1.5 py-0.5 font-mono text-[11px]">
                    {step.action}
                  </code>
                ) : null}
                {step.host ? (
                  <span className="rounded-full bg-accent-soft px-1.5 py-0.5 text-[10.5px] font-medium text-accent-strong">
                    leaves the process{step.permission ? ` · needs ${step.permission}` : ""}
                  </span>
                ) : null}
              </li>
            ))}
          </ol>
        </section>
      ) : null}

      {/* ---- The approval bar -------------------------------------------------------------- */}
      <ApprovalBar
        approvable={approvable}
        busy={busy}
        stage={stage}
        hasWorkflow={draft.workflow_id !== null}
        rejectionReason={rejectionReason}
        onRejectionReason={setRejectionReason}
        revisionNote={revisionNote}
        onRevisionNote={setRevisionNote}
        onApprove={approve}
        onReject={reject}
        onRevise={revise}
        onCancelRevision={cancelRevision}
        onTestRun={() =>
          void run("test-run", async () => {
            const result = await testRunAiWorkflowDraft(draftId);
            setTestRun(result);
            return {
              kind: "test-run",
              message: `Validated. ${result.steps.length} step${result.steps.length === 1 ? "" : "s"} would run, nothing was dispatched.`,
              run: result,
            };
          })
        }
      />

      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={() => router.push("/ai/workflows")}
          data-review-done
          className="rounded-md bg-accent px-3.5 py-2 text-[12.5px] font-medium text-white hover:opacity-90 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
        >
          Back to the console
        </button>
        <button
          type="button"
          onClick={retry}
          data-review-refresh
          className="rounded-md border border-line px-3 py-2 text-[12.5px] hover:bg-quiet-soft focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
        >
          Refresh
        </button>
      </div>
    </div>
  );
}

/** The definition editor: a textarea, a server validation and an explicit save. */
function DefinitionEditor({
  definition,
  editable,
  busy,
  onSave,
}: {
  definition: string | null;
  editable: boolean;
  busy: boolean;
  onSave: (next: unknown) => void;
}) {
  const [text, setText] = useState(definition ?? "");
  const [parseError, setParseError] = useState<string | null>(null);

  // A re-read of the draft (after a save, a revision or a decision) is the authority: an
  // editor that kept the operator's unsaved text would then save over a definition that has
  // changed underneath them.
  useEffect(() => {
    setText(definition ?? "");
    setParseError(null);
  }, [definition]);

  const dirty = text !== (definition ?? "");

  const save = useCallback(() => {
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch (cause) {
      setParseError(
        cause instanceof Error
          ? `That is not JSON: ${cause.message}`
          : "That is not JSON.",
      );
      return;
    }
    setParseError(null);
    onSave(parsed);
  }, [onSave, text]);

  return (
    <section aria-labelledby="definition-heading" className="rounded-xl border border-line bg-surface">
      <h3
        id="definition-heading"
        className="flex flex-wrap items-center gap-1.5 px-3 pt-3 text-[13px] font-semibold"
      >
        <FileJson aria-hidden className="size-3.5" />
        Definition
      </h3>
      <p className="px-3 pt-0.5 text-[11.5px] text-muted">
        {editable
          ? "The exact JSON the workflow API accepts. Saving revalidates it server-side; an invalid save changes nothing."
          : "Read-only: a draft can only be edited while it is `draft`."}
      </p>
      {editable ? (
        <textarea
          data-definition-editor
          value={text}
          onChange={(event) => setText(event.target.value)}
          spellCheck={false}
          rows={16}
          aria-label="Definition JSON"
          className="mx-3 mt-2 w-[calc(100%-1.5rem)] rounded-lg bg-quiet-soft/60 p-2.5 font-mono text-[11.5px] leading-relaxed focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
        />
      ) : (
        <pre
          data-definition-editor
          className="mx-3 mb-3 mt-2 max-h-96 overflow-auto rounded-lg bg-quiet-soft/60 p-2.5 font-mono text-[11.5px] leading-relaxed"
        >
          {definition ?? "// no definition yet"}
        </pre>
      )}
      {parseError ? (
        <p
          role="alert"
          data-definition-parse-error
          className="mx-3 mt-1.5 text-[11.5px] text-caution"
        >
          {parseError}
        </p>
      ) : null}
      {editable ? (
        <div className="flex items-center gap-2 px-3 pb-3 pt-1.5">
          <button
            type="button"
            onClick={save}
            disabled={!dirty || busy}
            data-definition-save
            className="rounded-md border border-line px-2.5 py-1.5 text-[12px] hover:bg-quiet-soft disabled:opacity-45 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
          >
            {busy ? "Saving…" : "Save definition"}
          </button>
          <button
            type="button"
            onClick={() => {
              setText(definition ?? "");
              setParseError(null);
            }}
            disabled={!dirty || busy}
            data-definition-revert
            className="rounded-md border border-line px-2.5 py-1.5 text-[12px] hover:bg-quiet-soft disabled:opacity-45 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
          >
            Revert
          </button>
          {dirty ? (
            <span data-definition-dirty className="text-[11.5px] text-muted">
              unsaved changes
            </span>
          ) : null}
        </div>
      ) : null}
    </section>
  );
}

/** The decision bar: approve, reject, ask for changes, test run. */
function ApprovalBar({
  approvable,
  busy,
  stage,
  hasWorkflow,
  rejectionReason,
  onRejectionReason,
  revisionNote,
  onRevisionNote,
  onApprove,
  onReject,
  onRevise,
  onCancelRevision,
  onTestRun,
}: {
  approvable: boolean;
  busy: "approve" | "reject" | "revise" | "save" | "test-run" | null;
  stage: AiWorkflowStage | null;
  hasWorkflow: boolean;
  rejectionReason: string;
  onRejectionReason: (value: string) => void;
  revisionNote: string;
  onRevisionNote: (value: string) => void;
  onApprove: () => void;
  onReject: () => void;
  onRevise: () => void;
  onCancelRevision: () => void;
  onTestRun: () => void;
}) {
  const revising = busy === "revise";
  const disabled = busy !== null;

  return (
    <section
      aria-labelledby="approval-heading"
      data-approval-bar
      className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
    >
      <h3 id="approval-heading" className="text-[13px] font-semibold">
        Decision
      </h3>

      {hasWorkflow ? (
        <p data-approval-decided className="text-[12px] text-muted">
          This draft is a rule now. The decision bar closes once a draft has become a
          workflow — change the rule itself in the builder.
        </p>
      ) : !approvable ? (
        <p data-approval-unavailable className="text-[12px] text-muted">
          Nothing to decide yet: a draft needs a validated definition before a person can
          approve or reject it.
        </p>
      ) : null}

      {/* The revision panel is a panel, not a `prompt()`: a revision can take as long as a
          generation, and the operator has to be able to see the stages and cancel it. */}
      <div className="flex flex-col gap-2">
        <label
          htmlFor="revision-note"
          className="text-[12px] font-medium text-ink"
        >
          Ask for changes
        </label>
        <textarea
          id="revision-note"
          data-revision-note
          value={revisionNote}
          onChange={(event) => onRevisionNote(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Escape") onCancelRevision();
          }}
          rows={2}
          disabled={!approvable || disabled}
          placeholder="Say what should change — the model answers with a new definition."
          className="rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px] focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent disabled:opacity-45"
        />
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            onClick={onRevise}
            disabled={!approvable || disabled || revisionNote.trim().length === 0}
            data-revise
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px] hover:bg-quiet-soft disabled:opacity-45 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
          >
            {revising ? <Loader2 aria-hidden className="size-3.5 animate-spin" /> : <MessageSquare aria-hidden className="size-3.5" />}
            {revising ? "Revising…" : "Ask for changes"}
          </button>
          {revising ? (
            <button
              type="button"
              onClick={onCancelRevision}
              data-revise-cancel
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12px] hover:bg-quiet-soft focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
            >
              <X aria-hidden className="size-3.5" />
              Cancel
            </button>
          ) : null}
          {stage ? (
            <span
              role="status"
              aria-live="polite"
              data-revision-stage
              className="inline-flex items-center gap-1.5 text-[12px] text-muted"
            >
              <Loader2 aria-hidden className="size-3.5 animate-spin" />
              {stage === "repair" ? "the first answer was refused — repairing" : stage}
            </span>
          ) : null}
        </div>
      </div>

      <div className="flex flex-col gap-2">
        <label htmlFor="rejection-reason" className="text-[12px] font-medium text-ink">
          Rejection reason
        </label>
        <input
          id="rejection-reason"
          data-rejection-reason
          value={rejectionReason}
          onChange={(event) => onRejectionReason(event.target.value)}
          disabled={!approvable || disabled}
          placeholder="Required — the person who asked for it reads this."
          className="rounded-md border border-line bg-surface px-2.5 py-1.5 text-[12.5px] focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent disabled:opacity-45"
        />
      </div>

      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          onClick={onApprove}
          disabled={!approvable || disabled}
          data-approve
          className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3.5 py-2 text-[12.5px] font-medium text-white hover:opacity-90 disabled:opacity-45 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
        >
          {busy === "approve" ? (
            <Loader2 aria-hidden className="size-3.5 animate-spin" />
          ) : (
            <ThumbsUp aria-hidden className="size-3.5" />
          )}
          {busy === "approve" ? "Approving…" : "Approve"}
        </button>
        <button
          type="button"
          onClick={onReject}
          disabled={!approvable || disabled || rejectionReason.trim().length === 0}
          data-reject
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-2 text-[12.5px] hover:bg-quiet-soft disabled:opacity-45 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
        >
          {busy === "reject" ? (
            <Loader2 aria-hidden className="size-3.5 animate-spin" />
          ) : (
            <ThumbsDown aria-hidden className="size-3.5" />
          )}
          {busy === "reject" ? "Rejecting…" : "Reject"}
        </button>
        <button
          type="button"
          onClick={onTestRun}
          disabled={!approvable || disabled}
          data-test-run-button
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-2 text-[12.5px] hover:bg-quiet-soft disabled:opacity-45 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
        >
          {busy === "test-run" ? (
            <Loader2 aria-hidden className="size-3.5 animate-spin" />
          ) : (
            <FlaskConical aria-hidden className="size-3.5" />
          )}
          {busy === "test-run" ? "Checking…" : "Test run"}
        </button>
      </div>

      {approvable ? (
        <p data-approval-note className="flex items-start gap-1.5 text-[11.5px] text-muted">
          <Info aria-hidden className="mt-0.5 size-3.5 shrink-0" />
          <span>
            <strong className="font-medium text-ink">Approve</strong> creates the workflow{" "}
            <strong className="font-medium text-ink">disabled</strong>. Nothing fires until you
            enable it there — and if a step needs a permission you do not hold, the approval is
            refused rather than quietly granted.
          </span>
        </p>
      ) : null}
    </section>
  );
}

/** Exported for the unit test that reads the bar's own source. */
export const APPROVAL_BAR_MARKERS = {
  approve: "data-approve",
  reject: "data-reject",
  revise: "data-revise",
  testRun: "data-test-run-button",
} as const;

// Keep the two imports that only the editor and the revision path use alive under
// `noUnusedLocals`: both are used, and the assertion below fails loudly if that stops
// being true.
