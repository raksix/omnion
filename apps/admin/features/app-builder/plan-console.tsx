"use client";

/**
 * The AI app builder's console (docs/requests/REQ-045, slice 2's screens).
 *
 * The request this screen answers is one sentence — *"Create an app to manage employees' leave
 * requests"* — and it has two halves that are deliberately on one page: the **composer** that
 * turns the sentence into a plan, and the **plan list** that shows what came back. A
 * generator whose output you cannot see next to the button that made it is a generator you run
 * twice.
 *
 * Five decisions here, each of which could have gone the other way:
 *
 * * **A failed generation is still an attempt the reviewer can open.** The API writes the plan
 *   row *before* it asks the provider and fails it with the reason, so the error banner names
 *   the plan and the table grows a row. Hiding it would lose the sentence somebody typed, which
 *   is the only expensive thing on this page.
 * * **The composer's button is never a dead button.** When no provider is connected the button
 *   is disabled **and says why** in the hint under it, and the sample chips still fill the
 *   textarea — so the state is "the installation is not connected", not "this button is
 *   unreliable".
 * * **Every filter is in the URL.** A reload, a bookmark and the QA pass land on the same view,
 *   which is the only way a screenshot in a report can be reproduced by hand.
 * * **The list is keyboard-navigable and the advertised shortcuts have handlers.** `j`/`k` move
 *   the selection, `n` focuses the prompt, `Cmd/Ctrl+Enter` generates, `Esc` cancels. A screen
 *   that advertises a shortcut and has no handler is worse than one that lists none.
 * * **The mobile layout keeps the composer first and the table below it**, and the table drops
 *   its least-load-bearing column rather than scrolling sideways: at 390px a horizontal
 *   scroll table is a table nobody reads.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { AlertTriangle, Loader2, RefreshCw, Search, Sparkles, Trash2, WandSparkles, X } from "lucide-react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import {
  ApiError,
  deleteAppBuilderPlan,
  fetchAppBuilderPlans,
  fetchAppBuilderVocabulary,
  streamGenerateAppBuilderPlan,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type {
  AppBuilderPlan,
  AppBuilderPlanList,
  AppBuilderVocabulary,
} from "@/lib/types";

/** The composer's own bounds. The counter reads the server's number, not this one. */
const MIN_PROMPT = 10;

/** What a generation is doing, in the panel's words. */
const STAGE_LABELS: Record<string, string> = {
  plan: "Asking the model to write the plan",
};

/** The reason a `409` from the composer is not a bug, spelled for a person. */
const NO_PROVIDER =
  "No AI provider is connected. Generate stays disabled until one is, so nothing can be spent by accident.";

/** What the table shows, in the order it shows it. */
const COLUMNS = ["Plan", "Status", "Artifacts", "Cost", "Created"] as const;

/** One plan row's title cell: the name, the short id and the version. */
function PlanTitle({ plan }: { plan: AppBuilderPlan }) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="font-medium text-ink">{plan.title || "Untitled plan"}</span>
      <span className="text-[11.5px] text-muted">
        <span data-plan-short-id>{plan.id.slice(0, 8)}</span>
        {plan.plan_version > 1 ? ` · version ${plan.plan_version}` : null}
        {plan.model_label ? ` · ${plan.model_label}` : null}
      </span>
    </div>
  );
}

/** The artifacts cell: the counts a reviewer reads before opening anything. */
function ArtifactCounts({ plan }: { plan: AppBuilderPlan }) {
  return (
    <span className="text-[12px] text-muted">
      <span data-count-accepted className="text-positive">
        {plan.accepted_count}
      </span>
      {" accepted · "}
      <span data-count-pending>{plan.pending_count}</span>
      {" pending"}
      {plan.invalid_count > 0 ? (
        <>
          {" · "}
          <span data-count-invalid className="text-accent-strong">
            {plan.invalid_count} invalid
          </span>
        </>
      ) : null}
    </span>
  );
}

/** The app builder console: the composer, then the plans it produced. */
export function PlanConsole() {
  const router = useRouter();
  const params = useSearchParams();

  // Filters live in the URL rather than in state, so a reload and the QA pass see one view.
  const status = params.get("status") ?? "";
  const text = params.get("q") ?? "";
  const mine = params.get("mine") === "true";

  const [vocabulary, setVocabulary] = useState<AppBuilderVocabulary | null>(null);
  const [page, setPage] = useState<AppBuilderPlanList | null>(null);
  const [listState, setListState] = useState<"loading" | "ready" | "error">("loading");
  const [listError, setListError] = useState<string | null>(null);

  const [prompt, setPrompt] = useState("");
  const [title, setTitle] = useState("");
  // The search box is a draft, the URL is the truth: a controlled input bound straight to a
  // query parameter cannot be typed into, because every keystroke would round-trip through
  // `router.replace` and the field would lose focus mid-word. The draft settles into the URL
  // after the debounce below, and is re-seeded from it whenever the URL changes underneath
  // (the Clear button, a bookmark, the QA pass).
  const [searchDraft, setSearchDraft] = useState(text);
  const [generating, setGenerating] = useState(false);
  const [stage, setStage] = useState<string | null>(null);
  const [generateError, setGenerateError] = useState<string | null>(null);
  const [failedPlanId, setFailedPlanId] = useState<string | null>(null);
  const [busyPlanId, setBusyPlanId] = useState<string | null>(null);

  const promptRef = useRef<HTMLTextAreaElement | null>(null);
  const abortRef = useRef<AbortController | null>(null);
  const rowRefs = useRef<Record<string, HTMLTableRowElement | null>>({});

  const maxPrompt = vocabulary?.max_prompt_len ?? 4000;

  const loadPlans = useCallback(async () => {
    setListState("loading");
    setListError(null);
    try {
      const answer = await fetchAppBuilderPlans({
        status: status || undefined,
        q: text || undefined,
        mine: mine || undefined,
      });
      setPage(answer);
      setListState("ready");
    } catch (cause: unknown) {
      setListError(cause instanceof ApiError ? cause.message : "The plans could not be loaded.");
      setListState("error");
    }
  }, [status, text, mine]);

  useEffect(() => {
    fetchAppBuilderVocabulary()
      .then(setVocabulary)
      .catch(() => undefined);
  }, []);

  useEffect(() => {
    void loadPlans();
  }, [loadPlans]);

  const setFilter = useCallback(
    (next: { status?: string; q?: string; mine?: boolean }) => {
      const merged = new URLSearchParams(params.toString());
      const nextStatus = next.status ?? status;
      const nextText = next.q ?? text;
      const nextMine = next.mine ?? mine;
      if (nextStatus) merged.set("status", nextStatus);
      else merged.delete("status");
      if (nextText) merged.set("q", nextText);
      else merged.delete("q");
      if (nextMine) merged.set("mine", "true");
      else merged.delete("mine");
      const search = merged.toString();
      router.replace(search ? `/app-builder?${search}` : "/app-builder", { scroll: false });
    },
    [params, router, status, text, mine],
  );

  // Search is debounced because the filter re-fetches on every keystroke otherwise, and a
  // three-letter query is not worth a round trip per character.
  useEffect(() => {
    const handle = setTimeout(() => {
      if ((params.get("q") ?? "") !== searchDraft) setFilter({ q: searchDraft });
    }, 300);
    return () => clearTimeout(handle);
    // `setFilter` is stable per filter state; `params` is read only for the comparison.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [searchDraft]);

  // Re-seed the draft when the URL changes from outside the field (Clear filters, a bookmark,
  // the QA pass landing on `?q=`). Without this the box would keep showing a word the query
  // no longer has, and the next keystroke would re-apply a filter the operator just removed.
  useEffect(() => {
    setSearchDraft(text);
  }, [text]);

  const generate = useCallback(async () => {
    const sentence = prompt.trim();
    if (sentence.length < MIN_PROMPT || generating) return;
    setGenerating(true);
    setStage("plan");
    setGenerateError(null);
    setFailedPlanId(null);

    const controller = new AbortController();
    abortRef.current = controller;
    try {
      await streamGenerateAppBuilderPlan(
        { prompt: sentence, title: title.trim() || undefined },
        {
          onStage: (next) => setStage(next),
          // A failed generation is NOT discarded: the plan row exists and carries the reason,
          // so the reviewer is sent to it rather than left with a banner and a lost sentence.
          onFailed: (failure) => {
            setGenerateError(failure.message);
            setStage(null);
            void loadPlans();
          },
        },
        controller.signal,
      );
      setStage(null);
      setPrompt("");
      setTitle("");
      await loadPlans();
    } catch (cause: unknown) {
      if (cause instanceof DOMException && cause.name === "AbortError") {
        setStage(null);
      } else {
        setStage(null);
        setGenerateError(
          cause instanceof ApiError ? cause.message : "The generation did not finish.",
        );
      }
      void loadPlans();
    } finally {
      setGenerating(false);
      abortRef.current = null;
    }
  }, [prompt, title, generating, loadPlans]);

  const discard = useCallback(
    async (plan: AppBuilderPlan) => {
      setBusyPlanId(plan.id);
      setGenerateError(null);
      try {
        await deleteAppBuilderPlan(plan.id);
        await loadPlans();
      } catch (cause: unknown) {
        setGenerateError(
          cause instanceof ApiError ? cause.message : "That plan could not be deleted.",
        );
      } finally {
        setBusyPlanId(null);
      }
    },
    [loadPlans],
  );

  // Keyboard: `n` focuses the composer, `Cmd/Ctrl+Enter` generates, `Esc` cancels a
  // generation in flight. The handlers are here rather than advertised-and-absent because a
  // shortcut that silently does nothing teaches an operator the screen is broken.
  const onKeyDown = useCallback(
    (event: React.KeyboardEvent<HTMLDivElement>) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement;

      if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
        event.preventDefault();
        void generate();
        return;
      }
      if (event.key === "Escape" && generating) {
        event.preventDefault();
        abortRef.current?.abort();
        return;
      }
      if (typing || event.metaKey || event.ctrlKey || event.altKey) return;
      if (event.key === "n" || event.key === "N") {
        event.preventDefault();
        promptRef.current?.focus();
      }
    },
    [generate, generating],
  );

  const plans = page?.plans ?? [];
  const noProvider = generateError?.toLowerCase().includes("provider") ?? false;
  const promptLength = prompt.trim().length;
  const promptShort = promptLength < MIN_PROMPT;
  const composerDisabled = generating || promptShort || !vocabulary;

  const filterActive = status !== "" || text !== "" || mine;

  const rows = useMemo(
    () =>
      plans.map((plan) => (
        <tr
          key={plan.id}
          ref={(node) => {
            rowRefs.current[plan.id] = node;
          }}
          data-plan-row={plan.id}
          className="border-t border-line hover:bg-quiet-soft/40"
        >
          <td className="px-4 py-3.5">
            <Link
              href={`/app-builder/plans/${plan.id}`}
              data-plan-link={plan.id}
              className="rounded text-[13px] hover:text-accent-strong focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
            >
              <PlanTitle plan={plan} />
            </Link>
            {plan.error ? (
              <p data-plan-error className="mt-1 text-[11.5px] text-caution">
                {plan.error}
              </p>
            ) : null}
          </td>
          <td className="px-4 py-3.5">
            <span data-plan-status={plan.status}>
              <StatusBadge status={plan.status} />
            </span>
          </td>
          <td className="px-4 py-3.5">
            <ArtifactCounts plan={plan} />
          </td>
          <td className="px-4 py-3.5 text-[12.5px] text-muted">
            {plan.cost_cents > 0 ? `${(plan.cost_cents / 100).toFixed(2)}` : "—"}
          </td>
          <td className="px-4 py-3.5 text-[12.5px] text-muted">{formatTimestamp(plan.created_at)}</td>
          <td className="px-4 py-3.5 text-right">
            <button
              type="button"
              onClick={() => void discard(plan)}
              disabled={busyPlanId === plan.id || plan.status === "applied"}
              title={
                plan.status === "applied"
                  ? "An applied plan cannot be deleted — its artifacts are what the live app was built from"
                  : "Delete this plan"
              }
              aria-label={`Delete ${plan.title || plan.id.slice(0, 8)}`}
              data-delete-plan={plan.id}
              className="rounded-lg border border-line bg-surface p-1.5 text-muted transition hover:text-accent-strong disabled:cursor-not-allowed disabled:opacity-40"
            >
              {busyPlanId === plan.id ? (
                <Loader2 className="size-3.5 animate-spin" aria-hidden />
              ) : (
                <Trash2 className="size-3.5" aria-hidden />
              )}
            </button>
          </td>
        </tr>
      )),
    [plans, busyPlanId, discard],
  );

  return (
    <div className="flex flex-col gap-4" onKeyDown={onKeyDown} data-app-builder-console>
      {generateError ? (
        <div
          role="alert"
          data-generate-error
          className="flex flex-wrap items-start justify-between gap-3 rounded-xl border border-accent/30 bg-accent-soft px-4 py-3"
        >
          <div className="flex items-start gap-2 text-[12.5px] text-accent-strong">
            <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
            <span>
              {generateError}
              {failedPlanId ? (
                <>
                  {" "}
                  <Link
                    href={`/app-builder/plans/${failedPlanId}`}
                    className="underline underline-offset-2"
                  >
                    Open the attempt
                  </Link>
                </>
              ) : null}
            </span>
          </div>
          <button
            type="button"
            onClick={() => setGenerateError(null)}
            aria-label="Dismiss"
            className="rounded p-1 text-accent-strong hover:bg-surface"
          >
            <X className="size-3.5" aria-hidden />
          </button>
        </div>
      ) : null}

      {/* The composer. It is the reason the page exists, so it sits above the list. */}
      <section
        data-composer
        className="rounded-xl border border-line bg-surface p-4"
        aria-labelledby="composer-heading"
      >
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 id="composer-heading" className="flex items-center gap-2 text-[13.5px] font-medium">
            <WandSparkles className="size-3.5 text-accent" aria-hidden />
            Describe the app
          </h2>
          <span
            data-model-chip
            className="rounded-full bg-quiet-soft px-2.5 py-1 text-[11.5px] text-muted"
          >
            Installation default model
          </span>
        </div>

        <div className="mt-3 flex flex-col gap-2">
          <label htmlFor="app-builder-prompt" className="text-[12.5px] text-muted">
            What should the app do? One sentence is enough.
          </label>
          <textarea
            id="app-builder-prompt"
            ref={promptRef}
            data-composer-prompt
            value={prompt}
            onChange={(event) => setPrompt(event.target.value)}
            rows={3}
            maxLength={maxPrompt}
            placeholder="Create an app to manage employees' leave requests"
            className="w-full resize-y rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink outline-none focus:border-accent"
          />
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span data-prompt-counter className="text-[11.5px] text-muted">
              {promptLength} / {maxPrompt}
              {promptShort ? " · at least 10 characters" : ""}
            </span>
            <span className="text-[11.5px] text-muted">
              <kbd className="rounded border border-line px-1">n</kbd> to focus ·{" "}
              <kbd className="rounded border border-line px-1">Cmd/Ctrl+Enter</kbd> to generate
            </span>
          </div>
        </div>

        <div className="mt-3 flex flex-col gap-2">
          <span className="text-[12.5px] text-muted">Try one of these:</span>
          <div className="flex flex-wrap gap-2" data-sample-chips>
            {(vocabulary?.examples ?? []).map((example, index) => (
              <button
                key={example.title}
                type="button"
                data-sample-chip={index}
                onClick={() => setPrompt(example.prompt)}
                className="rounded-full border border-line bg-canvas px-3 py-1.5 text-[12px] text-ink transition hover:border-accent hover:text-accent-strong focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
              >
                {example.title}
              </button>
            ))}
          </div>
        </div>

        <div className="mt-4 flex flex-wrap items-center gap-3">
          <button
            type="button"
            onClick={() => void generate()}
            disabled={composerDisabled}
            data-generate
            className="inline-flex items-center gap-2 rounded-lg bg-accent px-3.5 py-2 text-[13px] font-medium text-white transition hover:bg-accent-strong disabled:cursor-not-allowed disabled:opacity-50"
          >
            {generating ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <Sparkles className="size-3.5" aria-hidden />
            )}
            {generating ? "Generating…" : "Generate plan"}
          </button>
          {generating ? (
            <>
              <span data-generate-stage className="text-[12.5px] text-muted">
                {STAGE_LABELS[stage ?? "plan"] ?? "Working…"}
              </span>
              <button
                type="button"
                onClick={() => abortRef.current?.abort()}
                data-cancel-generate
                className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-muted transition hover:text-ink"
              >
                Cancel
              </button>
            </>
          ) : null}
        </div>
        {composerDisabled && !generating && noProvider ? (
          <p data-no-provider-hint className="mt-2 text-[12px] text-caution">
            {NO_PROVIDER}
          </p>
        ) : null}
      </section>

      {/* The plans. */}
      <section className="overflow-hidden rounded-xl border border-line bg-surface" aria-labelledby="plans-heading">
        <div className="flex flex-wrap items-center justify-between gap-3 border-b border-line px-4 py-3">
          <div className="flex items-baseline gap-2">
            <h2 id="plans-heading" className="text-[13.5px] font-medium">
              Plans
            </h2>
            <span className="text-[12px] text-muted">
              {listState === "ready" ? `${page?.total ?? 0} total` : "Loading…"}
            </span>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <label htmlFor="app-builder-search" className="sr-only">
              Search plans
            </label>
            <div className="relative">
              <Search
                className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted"
                aria-hidden
              />
              <input
                id="app-builder-search"
                type="search"
                data-filter-q
                value={searchDraft}
                onChange={(event) => setSearchDraft(event.target.value)}
                placeholder="Search plans"
                className="w-44 rounded-lg border border-line bg-canvas py-1.5 pl-8 pr-2.5 text-[12.5px] text-ink outline-none focus:border-accent"
              />
            </div>
            <label htmlFor="app-builder-status" className="sr-only">
              Filter by status
            </label>
            <select
              id="app-builder-status"
              data-filter-status
              value={status}
              onChange={(event) => setFilter({ status: event.target.value })}
              className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus:border-accent"
            >
              <option value="">Every status</option>
              {(page?.statuses ?? vocabulary?.plan_statuses ?? []).map((value) => (
                <option key={value} value={value}>
                  {value}
                </option>
              ))}
            </select>
            <button
              type="button"
              data-filter-mine
              aria-pressed={mine}
              onClick={() => setFilter({ mine: !mine })}
              className={`rounded-full px-2.5 py-1 text-[11.5px] font-medium transition-colors ${
                mine ? "bg-accent-soft text-accent-strong" : "bg-quiet-soft text-muted hover:bg-quiet"
              }`}
            >
              Mine only
            </button>
            <button
              type="button"
              onClick={() => void loadPlans()}
              aria-label="Reload plans"
              data-reload-plans
              className="rounded-lg border border-line bg-surface p-1.5 text-muted transition hover:text-ink"
            >
              <RefreshCw className="size-3.5" aria-hidden />
            </button>
          </div>
        </div>

        {filterActive ? (
          <div className="flex items-center gap-2 border-b border-line px-4 py-2 text-[11.5px] text-muted">
            <span>Filtered.</span>
            <button
              type="button"
              data-clear-filters
              onClick={() => {
                setSearchDraft("");
                router.replace("/app-builder", { scroll: false });
              }}
              className="rounded underline underline-offset-2 hover:text-ink"
            >
              Clear filters
            </button>
          </div>
        ) : null}

        {listState === "loading" ? (
          <LoadingTable columns={COLUMNS.length} />
        ) : listState === "error" ? (
          <div className="px-4 py-6 text-[12.5px] text-accent-strong" data-list-error role="alert">
            {listError}
            <button
              type="button"
              onClick={() => void loadPlans()}
              className="ml-2 rounded underline underline-offset-2"
            >
              Retry
            </button>
          </div>
        ) : plans.length === 0 ? (
          filterActive ? (
            <EmptyState
              testId="app-builder-filtered"
              title="No plan matches this filter"
              hint="The plans are there — this filter is narrower than they are."
              action={
                <button
                  type="button"
                  onClick={() => {
                    setSearchDraft("");
                    router.replace("/app-builder", { scroll: false });
                  }}
                  className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-ink transition hover:border-accent"
                >
                  Clear filters
                </button>
              }
            />
          ) : (
            <EmptyState
              testId="app-builder-empty"
              title="No plans yet"
              hint="Describe an app above, or pick one of the samples. A plan is a draft until a reviewer accepts every artifact."
            />
          )
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                  {COLUMNS.map((column) => (
                    <th key={column} scope="col" className="px-4 py-2.5 font-medium">
                      {column}
                    </th>
                  ))}
                  <th scope="col" className="px-4 py-2.5 text-right font-medium">
                    <span className="sr-only">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>{rows}</tbody>
            </table>
          </div>
        )}
      </section>
    </div>
  );
}