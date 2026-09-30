"use client";

/**
 * The AI workflow builder's review screen (docs/requests/REQ-046, slice 3).
 *
 * One draft, and everything a person needs to decide about it: the prompt it answers, the
 * model's own rationale, the definition as JSON, and a **read-only step list** derived from
 * that same definition. The step list is not a second copy — it is projected from the stored
 * JSON, because the request's core promise is that *a generated definition is an ordinary
 * definition*, and a step list the engine could disagree with would be the one part of the
 * review screen that is not the thing being approved.
 *
 * What this slice deliberately does **not** do: approve, reject, revise, test-run. Those are
 * slice 4's routes. The approval bar therefore renders with its buttons **absent** and says
 * why, rather than rendering buttons that would `404`. A screen that shows a control it cannot
 * honour teaches the operator that the platform is broken, which is a worse failure than a
 * screen that is honestly incomplete — and the request's own rule is that there are no dead
 * buttons and no "coming soon" placeholders *in the slice that owns them*.
 */
import { useCallback, useEffect, useState } from "react";

import {
  ArrowLeft,
  Check,
  CircleAlert,
  FileJson,
  Info,
  Loader2,
  Sparkles,
} from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";

import { StatusBadge } from "@/components/status-badge";
import { ApiError, type AiWorkflowDraftDetail, fetchAiWorkflowDraft } from "@/lib/api";
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

/** The screen. */
export function AiWorkflowDraftReview({ draftId }: { draftId: string }) {
  const router = useRouter();
  const [draft, setDraft] = useState<AiWorkflowDraftDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

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

  const retry = useCallback(() => setReloadToken((token) => token + 1), []);

  if (error) {
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

  const definition = draft.definition
    ? JSON.stringify(draft.definition, null, 2)
    : null;
  const isAiStep = (action: string | null) => action === "ai.prompt";

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
            className="underline underline-offset-2 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
          >
            open it in the builder
          </Link>
          .
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

        <section
          aria-labelledby="definition-heading"
          className="rounded-xl border border-line bg-surface"
        >
          <h3
            id="definition-heading"
            className="flex items-center gap-1.5 px-3 pt-3 text-[13px] font-semibold"
          >
            <FileJson aria-hidden className="size-3.5" />
            Definition
          </h3>
          <p className="px-3 pt-0.5 text-[11.5px] text-muted">
            The exact JSON the workflow API accepts. Editing it lands in slice 4.
          </p>
          <pre
            data-definition-editor
            className="mx-3 mb-3 mt-2 max-h-96 overflow-auto rounded-lg bg-quiet-soft/60 p-2.5 font-mono text-[11.5px] leading-relaxed"
          >
            {definition ?? "// no definition yet"}
          </pre>
        </section>
      </div>

      {/* ---- What this screen does not do yet ---------------------------------------------- */}
      <p
        data-review-pending
        className="flex items-start gap-2 rounded-lg border border-line bg-quiet-soft/40 px-3 py-2.5 text-[12px] text-muted"
      >
        <Info aria-hidden className="mt-0.5 size-3.5 shrink-0" />
        <span>
          Approve, reject, ask for changes and test-run arrive with the next slice. Until then
          the draft stays <strong className="font-medium text-ink">draft</strong> and nothing
          runs.
        </span>
      </p>

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
