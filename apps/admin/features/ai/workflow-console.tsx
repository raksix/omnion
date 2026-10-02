"use client";

/**
 * The AI workflow builder's draft console (docs/requests/REQ-046, slice 3).
 *
 * The request is one sentence — *"if an invoice is 7 days overdue, email the customer; if 14
 * days, create a task for the sales owner"* — and this screen is where it becomes a rule. It
 * has two halves and they are deliberately on one page: a **generate form** (a sentence, a
 * model, a trigger hint) and the **draft list** it produces, because a generator whose output
 * you cannot see next to the button that made it is a generator you run twice.
 *
 * Four things here are decisions that could have gone the other way, and each of them is the
 * reason the screen does what it does rather than what is simplest:
 *
 * * **The progress panel shows real stages and nothing else.** `plan`, `validate` and — only
 *   when it was actually spent — `repair` arrive as frames from the generation. A panel that
 *   animated a fake progress bar would look the same in every failure, which is exactly when
 *   an operator needs it to be different.
 * * **The no-provider state replaces the form, not the screen.** The form is useless without a
 *   model, and a form that is visible-but-broken teaches the operator that the button is
 *   unreliable rather than that the installation is not connected.
 * * **Every filter lives in the URL.** A reload, a bookmark and the QA walkthrough all land on
 *   the same view, which is the only way a screenshot in a report can be reproduced by hand.
 * * **The list is keyboard-navigable and the shortcuts are real.** `j`/`k` move the selection,
 *   `n` focuses the prompt, `Cmd/Ctrl+Enter` generates, `Esc` cancels. A screen that advertises
 *   a shortcut and has no handler is worse than one that lists none.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  Ban,
  Check,
  ChevronRight,
  Lightbulb,
  Loader2,
  Plus,
  RefreshCw,
  Search,
  Sparkles,
  Trash2,
  X,
} from "lucide-react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import {
  ApiError,
  type AiModel,
  type AiWorkflowDraft,
  type AiWorkflowDraftList,
  type AiWorkflowExample,
  type AiWorkflowStage,
  type AiWorkflowVocabulary,
  fetchAiModels,
  fetchAiWorkflowAuthors,
  fetchAiWorkflowDrafts,
  fetchAiWorkflowVocabulary,
  removeAiWorkflowDraft,
  streamGenerateWorkflowDraft,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** Shortest a prompt may be, the server's own bound — the counter reads the same number. */
const MIN_PROMPT = 10;

/** Longest a prompt may be, the server's own bound (the column's check is 4000 characters). */
const MAX_PROMPT = 4000;

/** What a generation is doing, in the panel's words. */
const STAGE_LABELS: Record<AiWorkflowStage, string> = {
  plan: "Asking the model to write the rule",
  validate: "Checking it against the workflow engine",
  repair: "The first answer was refused — one repair attempt",
};

/** The three stages, in order, for the panel's rail. */
const STAGE_ORDER: AiWorkflowStage[] = ["plan", "validate", "repair"];

/** The trigger hints the form offers; the model proposes the schedule string itself. */
const TRIGGER_HINTS = [
  { value: "manual", label: "Manual — a person starts it", prompt: "" },
  {
    value: "schedule",
    label: "On a schedule — the model proposes the cron",
    prompt: "Run this on a schedule (say, every weekday morning): ",
  },
  {
    value: "event",
    label: "When something happens — the model names the event",
    prompt: "When something happens in the platform: ",
  },
] as const;

/** The model's own name for the generation's draft, when it sent none. */
function fallbackTitle(prompt: string): string {
  const first = prompt.split("\n")[0]?.trim() ?? "";
  return first.length > 60 ? `${first.slice(0, 60).trimEnd()}…` : first || "New workflow";
}

/** One status pill that toggles, for the multi-select. */
function StatusChip({
  status,
  on,
  onToggle,
}: {
  status: string;
  on: boolean;
  onToggle: (status: string) => void;
}) {
  return (
    <button
      type="button"
      onClick={() => onToggle(status)}
      aria-pressed={on}
      data-status-chip={status}
      className={`rounded-full px-2.5 py-1 text-[11.5px] font-medium transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent ${
        on ? "bg-accent-soft text-accent-strong" : "bg-quiet-soft text-muted hover:bg-quiet"
      }`}
    >
      {status}
    </button>
  );
}

/** The generation's progress panel. */
function ProgressPanel({
  stages,
  onCancel,
}: {
  stages: AiWorkflowStage[];
  onCancel: () => void;
}) {
  const reached = stages.length;
  return (
    <div
      role="status"
      aria-live="polite"
      data-generating="true"
      className="rounded-lg border border-line bg-quiet-soft/40 px-3.5 py-3"
    >
      <ol className="flex flex-col gap-1.5">
        {STAGE_ORDER.map((stage, index) => {
          const done = stages.includes(stage);
          const active = stages[stages.length - 1] === stage;
          return (
            <li
              key={stage}
              data-stage={stage}
              data-stage-state={done ? (active ? "active" : "done") : "pending"}
              className="flex items-center gap-2 text-[12.5px]"
            >
              {done ? (
                active ? (
                  <Loader2 aria-hidden className="size-3.5 animate-spin text-accent" />
                ) : (
                  <Check aria-hidden className="size-3.5 text-positive" />
                )
              ) : (
                <span
                  aria-hidden
                  className="size-3.5 rounded-full border border-line"
                />
              )}
              <span className={done ? "text-ink" : "text-muted"}>
                {STAGE_LABELS[stage]}
              </span>
            </li>
          );
        })}
      </ol>
      <div className="mt-2.5 flex items-center justify-between">
        <span className="text-[11.5px] text-muted">
          {reached} of {STAGE_ORDER.length - 1} steps
        </span>
        <button
          type="button"
          onClick={onCancel}
          data-cancel-generation
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
        >
          <X aria-hidden className="size-3.5" />
          Cancel
        </button>
      </div>
    </div>
  );
}

/** The console. */
export function AiWorkflowConsole() {
  const router = useRouter();
  const searchParams = useSearchParams();

  // Every filter is read from the URL and written back to it, so the view IS the link.
  const urlStatus = useMemo(
    () => (searchParams.get("status") ?? "").split(",").filter(Boolean),
    [searchParams],
  );
  const urlQuery = searchParams.get("q") ?? "";
  const urlBy = searchParams.get("by") ?? "";
  const urlOffset = Number(searchParams.get("offset") ?? "0") || 0;

  const [list, setList] = useState<AiWorkflowDraftList | null>(null);
  const [authors, setAuthors] = useState<{ id: string; drafts: number }[]>([]);
  const [vocabulary, setVocabulary] = useState<AiWorkflowVocabulary | null>(null);
  const [models, setModels] = useState<AiModel[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  // The generate form.
  const [prompt, setPrompt] = useState("");
  const [model, setModel] = useState("");
  const [triggerHint, setTriggerHint] = useState<string>("manual");
  const [promptError, setPromptError] = useState<string | null>(null);
  const [stages, setStages] = useState<AiWorkflowStage[]>([]);
  const [generating, setGenerating] = useState(false);
  const abortRef = useRef<AbortController | null>(null);
  const promptRef = useRef<HTMLTextAreaElement | null>(null);
  const searchRef = useRef<HTMLInputElement | null>(null);
  const [selected, setSelected] = useState(0);

  const generating_ = generating;
  const promptLength = prompt.trim().length;

  const writeUrl = useCallback(
    (next: { status?: string[]; q?: string; by?: string; offset?: number }) => {
      const params = new URLSearchParams(searchParams.toString());
      const put = (key: string, value: string | undefined) => {
        if (value && value.length > 0) params.set(key, value);
        else params.delete(key);
      };
      if (next.status !== undefined) put("status", next.status.join(","));
      if (next.q !== undefined) put("q", next.q);
      if (next.by !== undefined) put("by", next.by);
      put("offset", next.offset && next.offset > 0 ? String(next.offset) : undefined);
      const query = params.toString();
      router.replace(query ? `/ai/workflows?${query}` : "/ai/workflows", { scroll: false });
    },
    [router, searchParams],
  );

  // The list, the author roster and the vocabulary. The vocabulary is what tells the console
  // whether a provider is connected at all: the action list is served by a route that needs a
  // model behind it, and a `403`/`409` there is the no-provider state.
  useEffect(() => {
    let cancelled = false;
    setError(null);

    Promise.all([
      fetchAiWorkflowDrafts({
        status: urlStatus,
        q: urlQuery,
        by: urlBy,
        offset: urlOffset,
      }),
      fetchAiWorkflowVocabulary(),
      fetchAiWorkflowAuthors().catch(() => [] as { id: string; drafts: number }[]),
      fetchAiModels().catch(() => [] as AiModel[]),
    ])
      .then(([drafts, words, roster, registry]) => {
        if (cancelled) return;
        setList(drafts);
        setVocabulary(words);
        setAuthors(roster);
        setModels(registry);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setList(null);
        setError(
          cause instanceof ApiError ? cause.message : "The draft console could not be loaded.",
        );
      });

    return () => {
      cancelled = true;
    };
  }, [urlStatus, urlQuery, urlBy, urlOffset, reloadToken]);

  const drafts = list?.drafts ?? [];
  const enabledModels = (models ?? []).filter((entry) => entry.enabled);
  const noProvider = (models !== null && enabledModels.length === 0) || vocabulary === null;

  const toggleStatus = useCallback(
    (status: string) => {
      const next = urlStatus.includes(status)
        ? urlStatus.filter((value) => value !== status)
        : [...urlStatus, status];
      writeUrl({ status: next, offset: 0 });
    },
    [urlStatus, writeUrl],
  );

  /** Generate, and open the draft when the row is stored. */
  const generate = useCallback(async () => {
    const trimmed = prompt.trim();
    if (trimmed.length < MIN_PROMPT) {
      setPromptError(`Describe the workflow in at least ${MIN_PROMPT} characters.`);
      promptRef.current?.focus();
      return;
    }
    if (trimmed.length > MAX_PROMPT) {
      setPromptError(`A prompt is at most ${MAX_PROMPT} characters.`);
      promptRef.current?.focus();
      return;
    }

    setPromptError(null);
    setError(null);
    setNotice(null);
    setGenerating(true);
    setStages([]);
    const controller = new AbortController();
    abortRef.current = controller;

    try {
      await streamGenerateWorkflowDraft(
        { prompt: trimmed, model: model || undefined },
        {
          onStage: (stage) => setStages((current) => [...current, stage]),
          onDone: async (done) => {
            setNotice(
              done.repaired
                ? `Draft ready — the first answer was refused and repaired (${done.attempts} calls, ${done.tokens} tokens).`
                : `Draft ready — ${done.attempts} call, ${done.tokens} tokens.`,
            );
            // The review screen is the point of the generation; opening it is the success
            // state. A notice that says "ready" on the list screen is a second click.
            router.push(`/ai/workflows/${done.draft_id}`);
          },
        },
        controller.signal,
      );
    } catch (cause: unknown) {
      if (cause instanceof DOMException && cause.name === "AbortError") {
        setNotice("Generation cancelled. Nothing was kept.");
      } else {
        const message =
          cause instanceof ApiError ? cause.message : "The generation did not go through.";
        setError(message);
        // A 409 here is the console's no-provider state, and the message has to survive the
        // form being replaced by a link to /ai — so it is shown above the form, not inside it.
      }
    } finally {
      setGenerating(false);
      abortRef.current = null;
      setReloadToken((token) => token + 1);
    }
  }, [prompt, model, router]);

  /** Remove a draft, and say so. */
  const remove = useCallback(
    async (draft: AiWorkflowDraft) => {
      setError(null);
      setNotice(null);
      try {
        await removeAiWorkflowDraft(draft.id);
        setNotice(`"${draft.title}" deleted.`);
        setReloadToken((token) => token + 1);
      } catch (cause: unknown) {
        setError(
          cause instanceof ApiError ? cause.message : "The draft could not be deleted.",
        );
      }
    },
    [],
  );

  /** The keyboard path the screen advertises. */
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target &&
        (target.tagName === "INPUT" ||
          target.tagName === "TEXTAREA" ||
          target.tagName === "SELECT");

      if (event.key === "Escape" && generating_) {
        event.preventDefault();
        abortRef.current?.abort();
        return;
      }
      if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
        event.preventDefault();
        if (generating_) return;
        void generate();
        return;
      }
      if (typing || event.metaKey || event.ctrlKey || event.altKey) return;

      if (event.key === "n") {
        event.preventDefault();
        promptRef.current?.focus();
      } else if (event.key === "/") {
        event.preventDefault();
        searchRef.current?.focus();
      } else if (event.key === "j" || event.key === "k") {
        if (drafts.length === 0) return;
        event.preventDefault();
        setSelected((current) => {
          const next = event.key === "j" ? current + 1 : current - 1;
          return Math.min(Math.max(next, 0), drafts.length - 1);
        });
      } else if (event.key === "Enter" && drafts[selected]) {
        router.push(`/ai/workflows/${drafts[selected].id}`);
      }
    };

    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [drafts, generate, generating_, router, selected]);

  const examples: AiWorkflowExample[] = vocabulary?.examples ?? [];

  return (
    <div className="flex flex-col gap-6">
      {error ? (
        <p
          role="alert"
          data-console-error
          className="rounded-lg border border-caution/40 bg-caution-soft px-3 py-2 text-[12.5px]"
        >
          {error}
        </p>
      ) : null}
      {notice ? (
        <p
          role="status"
          data-console-notice
          className="rounded-lg border border-positive/30 bg-positive-soft px-3 py-2 text-[12.5px]"
        >
          {notice}
        </p>
      ) : null}

      {/* ---- Generate -------------------------------------------------------------------- */}
      <section
        aria-labelledby="generate-heading"
        className="rounded-xl border border-line bg-surface p-4"
      >
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2
            id="generate-heading"
            className="flex items-center gap-2 text-[14px] font-semibold"
          >
            <Sparkles aria-hidden className="size-4 text-accent" />
            Describe the workflow you want
          </h2>
          <span className="text-[11.5px] text-muted">
            <kbd className="rounded border border-line px-1">n</kbd> to focus ·{" "}
            <kbd className="rounded border border-line px-1">⌘/Ctrl</kbd>+
            <kbd className="rounded border border-line px-1">↵</kbd> to generate
          </span>
        </div>

        {noProvider ? (
          <div data-no-provider className="mt-3">
            <EmptyState
              testId="ai-workflow-no-provider"
              title="No AI model is connected"
              hint="The console needs a model to ask. Connect a provider, register a model and make one the default — the generated workflow then lands here as a draft waiting for review."
              action={
                <Link
                  href="/ai"
                  className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white hover:opacity-90 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
                >
                  Open the AI Hub
                  <ChevronRight aria-hidden className="size-3.5" />
                </Link>
              }
            />
          </div>
        ) : (
          <form
            className="mt-3 flex flex-col gap-3"
            onSubmit={(event) => {
              event.preventDefault();
              void generate();
            }}
          >
            <div className="flex flex-col gap-1.5">
              <label
                htmlFor="ai-workflow-prompt"
                className="text-[12.5px] font-medium text-ink"
              >
                Prompt
              </label>
              <textarea
                id="ai-workflow-prompt"
                ref={promptRef}
                data-ai-workflow-prompt
                value={prompt}
                onChange={(event) => {
                  setPrompt(event.target.value);
                  if (promptError) setPromptError(null);
                }}
                rows={4}
                maxLength={MAX_PROMPT + 100}
                placeholder="If an invoice is 7 days overdue, email the customer; if 14 days overdue, create a task for the sales owner."
                aria-describedby="ai-workflow-prompt-count"
                aria-invalid={promptError ? true : undefined}
                className="w-full resize-y rounded-lg border border-line bg-background px-3 py-2 text-[13px] outline-none focus-visible:border-accent"
              />
              <div className="flex items-center justify-between gap-2">
                {promptError ? (
                  <p role="alert" data-prompt-error className="text-[12px] text-caution">
                    {promptError}
                  </p>
                ) : (
                  <p className="text-[12px] text-muted">
                    Plain language. The model proposes the trigger and the steps; you review
                    them before anything runs.
                  </p>
                )}
                <span
                  id="ai-workflow-prompt-count"
                  data-prompt-counter
                  className="shrink-0 text-[11.5px] text-muted tabular-nums"
                >
                  {promptLength} / {MAX_PROMPT}
                </span>
              </div>
            </div>

            <div className="flex flex-wrap items-end gap-3">
              <div className="flex min-w-48 flex-col gap-1.5">
                <label
                  htmlFor="ai-workflow-model"
                  className="text-[12.5px] font-medium text-ink"
                >
                  Model
                </label>
                <select
                  id="ai-workflow-model"
                  data-ai-workflow-model
                  value={model}
                  onChange={(event) => setModel(event.target.value)}
                  className="rounded-lg border border-line bg-background px-2.5 py-1.5 text-[12.5px] outline-none focus-visible:border-accent"
                >
                  <option value="">Installation default</option>
                  {enabledModels.map((entry) => (
                    <option key={entry.id} value={entry.model_id}>
                      {entry.provider_name} · {entry.display_name || entry.model_key}
                      {entry.is_default ? " (default)" : ""}
                    </option>
                  ))}
                </select>
              </div>

              <div className="flex min-w-56 flex-col gap-1.5">
                <label
                  htmlFor="ai-workflow-trigger"
                  className="text-[12.5px] font-medium text-ink"
                >
                  Trigger hint
                </label>
                <select
                  id="ai-workflow-trigger"
                  data-ai-workflow-trigger
                  value={triggerHint}
                  onChange={(event) => {
                    const next = event.target.value;
                    const hint = TRIGGER_HINTS.find((entry) => entry.value === next);
                    setTriggerHint(next);
                    // The hint NARROWS the prompt; it does not replace it. Prefilling the
                    // opening words is what the spec asks for, and it is only prefill — a
                    // sentence already typed is never overwritten by choosing a trigger.
                    if (hint?.prompt && prompt.trim() === "") setPrompt(hint.prompt);
                  }}
                  className="rounded-lg border border-line bg-background px-2.5 py-1.5 text-[12.5px] outline-none focus-visible:border-accent"
                >
                  {TRIGGER_HINTS.map((hint) => (
                    <option key={hint.value} value={hint.value}>
                      {hint.label}
                    </option>
                  ))}
                </select>
              </div>

              <button
                type="submit"
                data-generate-draft
                disabled={generating_}
                className="ml-auto inline-flex items-center gap-1.5 rounded-md bg-accent px-3.5 py-2 text-[12.5px] font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-60 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
              >
                {generating_ ? (
                  <Loader2 aria-hidden className="size-3.5 animate-spin" />
                ) : (
                  <Sparkles aria-hidden className="size-3.5" />
                )}
                {generating_ ? "Generating…" : "Generate draft"}
              </button>
            </div>

            {generating_ ? (
              <ProgressPanel
                stages={stages}
                onCancel={() => abortRef.current?.abort()}
              />
            ) : null}
          </form>
        )}

        {/* The empty state's three click-to-fill examples, offered whenever there is no
            prompt yet — an operator who has written nothing is exactly who needs them. */}
        {!noProvider && prompt.trim() === "" && examples.length > 0 ? (
          <div data-examples className="mt-4 flex flex-col gap-2">
            <p className="flex items-center gap-1.5 text-[12px] font-medium text-muted">
              <Lightbulb aria-hidden className="size-3.5" />
              Try one of these
            </p>
            <div className="grid gap-2 sm:grid-cols-3">
              {examples.map((example) => (
                <button
                  key={example.title}
                  type="button"
                  data-example={example.title}
                  onClick={() => {
                    setPrompt(example.prompt);
                    promptRef.current?.focus();
                  }}
                  className="rounded-lg border border-line px-3 py-2.5 text-left transition-colors hover:border-accent/50 hover:bg-quiet-soft/40 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
                >
                  <span className="flex items-center gap-1.5 text-[12.5px] font-medium">
                    <Plus aria-hidden className="size-3.5 text-accent" />
                    {example.title}
                  </span>
                  <span className="mt-1 block text-[11.5px] text-muted">{example.note}</span>
                </button>
              ))}
            </div>
          </div>
        ) : null}

        {/* The closed action registry, read from the engine. It is shown rather than hidden
            because the risk the request names is model drift, and the honest answer to that
            is a vocabulary an operator can read before they trust an answer. */}
        {vocabulary && vocabulary.actions.length > 0 ? (
          <details data-action-vocabulary className="mt-3">
            <summary className="cursor-pointer text-[12px] text-muted hover:text-ink focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent">
              What a generated step may name — {vocabulary.actions.length} actions
            </summary>
            <ul className="mt-2 grid gap-1.5 sm:grid-cols-2">
              {vocabulary.actions.map((action) => (
                <li key={action.action} className="flex items-start gap-2 text-[11.5px]">
                  <code className="rounded bg-quiet-soft px-1 py-0.5 font-mono">
                    {action.action}
                  </code>
                  <span className="text-muted">
                    {action.summary}
                    {action.host ? " (host)" : ""}
                  </span>
                </li>
              ))}
            </ul>
          </details>
        ) : null}
      </section>

      {/* ---- List ------------------------------------------------------------------------ */}
      <section aria-labelledby="drafts-heading" className="flex flex-col gap-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 id="drafts-heading" className="text-[14px] font-semibold">
            Drafts
            {list ? (
              <span className="ml-1.5 text-[12.5px] font-normal text-muted">
                {list.total} total
              </span>
            ) : null}
          </h2>
          <button
            type="button"
            onClick={() => setReloadToken((token) => token + 1)}
            data-reload-drafts
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
          >
            <RefreshCw aria-hidden className="size-3.5" />
            Refresh
          </button>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <div className="relative min-w-56 flex-1">
            <Search
              aria-hidden
              className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted"
            />
            <input
              ref={searchRef}
              type="search"
              data-ai-workflow-search
              value={urlQuery}
              onChange={(event) => writeUrl({ q: event.target.value, offset: 0 })}
              placeholder="Search titles and prompts…"
              aria-label="Search drafts"
              className="w-full rounded-md border border-line bg-background py-1.5 pl-8 pr-2.5 text-[12.5px] outline-none focus-visible:border-accent"
            />
          </div>

          <div className="flex flex-wrap items-center gap-1.5" role="group" aria-label="Filter by status">
            {(list?.statuses ?? []).map((status) => (
              <StatusChip
                key={status}
                status={status}
                on={urlStatus.includes(status)}
                onToggle={toggleStatus}
              />
            ))}
          </div>

          {authors.length > 0 ? (
            <select
              data-ai-workflow-author
              value={urlBy}
              onChange={(event) => writeUrl({ by: event.target.value, offset: 0 })}
              aria-label="Filter by author"
              className="rounded-md border border-line bg-background px-2 py-1.5 text-[12.5px] outline-none focus-visible:border-accent"
            >
              <option value="">Anyone</option>
              {authors.map((author) => (
                <option key={author.id} value={author.id}>
                  {author.id.slice(0, 8)}… · {author.drafts}
                </option>
              ))}
            </select>
          ) : null}
        </div>

        {list === null && !error ? (
          <LoadingTable columns={5} />
        ) : drafts.length === 0 ? (
          <EmptyState
            testId="ai-workflow-drafts"
            title={
              urlStatus.length > 0 || urlQuery
                ? "No drafts match these filters"
                : "Describe the workflow you want"
            }
            hint={
              urlStatus.length > 0 || urlQuery
                ? "Clear the search or the status chips to see the drafts you have."
                : "Write one sentence — what should happen, and when. The model proposes the rule and you review it before it runs."
            }
            action={
              urlStatus.length > 0 || urlQuery ? (
                <button
                  type="button"
                  data-clear-filters
                  onClick={() => writeUrl({ status: [], q: "", by: "", offset: 0 })}
                  className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px] hover:bg-quiet-soft focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
                >
                  Clear the filters
                </button>
              ) : null
            }
          />
        ) : (
          <>
            {/* The table at ≥ md, cards below it: a five-column table at 390 px is a
                horizontal scroll, which reads as a broken screen rather than a narrow one. */}
            <div className="hidden overflow-x-auto rounded-lg border border-line md:block">
              <table className="w-full border-collapse text-left text-[13px]">
                <thead>
                  <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                    <th scope="col" className="px-3 py-2 font-medium">Status</th>
                    <th scope="col" className="px-3 py-2 font-medium">Title</th>
                    <th scope="col" className="px-3 py-2 font-medium">Model</th>
                    <th scope="col" className="px-3 py-2 font-medium">Updated</th>
                    <th scope="col" className="px-3 py-2 font-medium">Cost</th>
                    <th scope="col" className="px-3 py-2 text-right font-medium">Actions</th>
                  </tr>
                </thead>
                <tbody>
                  {drafts.map((draft, index) => (
                    <tr
                      key={draft.id}
                      data-draft-row={draft.id}
                      data-selected={index === selected ? "true" : undefined}
                      className={`border-b border-line last:border-b-0 ${
                        index === selected ? "bg-quiet-soft/50" : ""
                      }`}
                    >
                      <td className="px-3 py-2.5">
                        <StatusBadge status={draft.status} />
                      </td>
                      <td className="px-3 py-2.5">
                        <Link
                          href={`/ai/workflows/${draft.id}`}
                          data-draft-open
                          className="font-medium text-ink underline-offset-2 hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
                        >
                          {draft.title || fallbackTitle("")}
                        </Link>
                        {draft.error ? (
                          <p data-draft-error className="mt-0.5 text-[11.5px] text-caution">
                            {draft.error.length > 90
                              ? `${draft.error.slice(0, 90)}…`
                              : draft.error}
                          </p>
                        ) : null}
                      </td>
                      <td className="px-3 py-2.5 text-muted">{draft.model_key ?? "—"}</td>
                      <td className="px-3 py-2.5 text-muted">
                        {formatTimestamp(draft.updated_at)}
                      </td>
                      <td className="px-3 py-2.5 text-muted tabular-nums">
                        {draft.tokens > 0 ? `${draft.tokens} tokens` : "—"}
                      </td>
                      <td className="px-3 py-2.5">
                        <div className="flex items-center justify-end gap-1">
                          <Link
                            href={`/ai/workflows/${draft.id}`}
                            data-draft-review
                            className="rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
                          >
                            Open
                          </Link>
                          <button
                            type="button"
                            data-draft-delete={draft.id}
                            onClick={() => void remove(draft)}
                            aria-label={`Delete ${draft.title}`}
                            className="rounded-md border border-line px-1.5 py-1 text-muted hover:bg-quiet-soft hover:text-danger focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
                          >
                            <Trash2 aria-hidden className="size-3.5" />
                          </button>
                        </div>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>

            <ul className="flex flex-col gap-2 md:hidden">
              {drafts.map((draft, index) => (
                <li
                  key={draft.id}
                  data-draft-card={draft.id}
                  data-selected={index === selected ? "true" : undefined}
                  className="rounded-lg border border-line p-3"
                >
                  <div className="flex items-center justify-between gap-2">
                    <StatusBadge status={draft.status} />
                    <span className="text-[11.5px] text-muted">
                      {formatTimestamp(draft.updated_at)}
                    </span>
                  </div>
                  <Link
                    href={`/ai/workflows/${draft.id}`}
                    className="mt-1.5 block text-[13.5px] font-medium underline-offset-2 hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
                  >
                    {draft.title || "Untitled draft"}
                  </Link>
                  <p className="mt-0.5 text-[11.5px] text-muted">
                    {draft.model_key ?? "—"}
                    {draft.tokens > 0 ? ` · ${draft.tokens} tokens` : ""}
                  </p>
                  {draft.error ? (
                    <p className="mt-1 text-[11.5px] text-caution">{draft.error}</p>
                  ) : null}
                </li>
              ))}
            </ul>

            {list && list.total > (list.page_size ?? 20) ? (
              <nav
                aria-label="Draft pages"
                className="flex items-center justify-end gap-2 text-[12.5px]"
              >
                <button
                  type="button"
                  data-page-prev
                  disabled={urlOffset === 0}
                  onClick={() => writeUrl({ offset: Math.max(urlOffset - (list.page_size ?? 20), 0) })}
                  className="rounded-md border border-line px-2 py-1 disabled:opacity-40 hover:bg-quiet-soft focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
                >
                  Previous
                </button>
                <span className="text-muted tabular-nums">
                  {urlOffset + 1}–{urlOffset + drafts.length} of {list.total}
                </span>
                <button
                  type="button"
                  data-page-next
                  disabled={urlOffset + drafts.length >= list.total}
                  onClick={() => writeUrl({ offset: urlOffset + (list.page_size ?? 20) })}
                  className="rounded-md border border-line px-2 py-1 disabled:opacity-40 hover:bg-quiet-soft focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
                >
                  Next
                </button>
              </nav>
            ) : null}
          </>
        )}
      </section>
    </div>
  );
}
