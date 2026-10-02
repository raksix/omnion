"use client";

/**
 * `/ai/evals` — the eval suites (REQ-107, slice 1).
 *
 * The screen answers one question first: **is any of this measuring anything?** A suite list that
 * showed only names would render a green tick beside a suite with no cases, and the request's own
 * risk note says exactly why that is dangerous — "a suite of ten easy cases passes everything".
 * So every row leads with a readiness badge, and the badge is four states rather than a boolean:
 * `Ready`, `No cases`, `Needs a judge`, `Off`. Each says a different thing and only one of them
 * is good news.
 *
 * Four decisions shape it:
 *
 * 1. **A refusal lands on the field it is about.** The API sends `details.field`; the form marks
 *    that input and the sentence goes above it. A screen that could only print the sentence would
 *    make a case editor with ten properties useless.
 *
 * 2. **The delete asks for the key.** A suite is what a schedule and a promotion gate are named
 *    after, so the confirm is the key itself rather than a yes/no dialog.
 *
 * 3. **Runs are not offered yet, and the screen says so.** `Run now` is slice 2's runner. Rather
 *    than ship a button that posts to a route that 404s, the row states that a run is not wired
 *    up and points at what *is* real — the cases you can author today.
 *
 * 4. **Coverage is on the screen, not only in the API.** The tag counts are the answer to "is my
 *    suite measuring one narrow thing?", which is the failure mode a pass rate cannot reveal.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { useRouter } from "next/navigation";
import Link from "next/link";

import {
  AlertTriangle,
  ClipboardList,
  Filter,
  Loader2,
  PlayCircle,
  Plus,
  RefreshCw,
  Search,
  ShieldCheck,
  Trash2,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError } from "@/lib/api";
import { startEvalRun } from "@/lib/eval-run-api";
import {
  createEvalSuite,
  deleteEvalSuite,
  fetchEvalSuites,
  type EvalSuiteList,
  type EvalSuiteSummary,
} from "@/lib/eval-api";

/**
 * How a readiness reads.
 *
 * A word plus a tone, because the request asks for badges readable without colour alone: `Ready`,
 * `No cases`, `Needs a judge` and `Off` are four different words and the colour is the second
 * signal, never the first.
 */
function readinessBadge(readiness: EvalSuiteSummary["readiness"]): { label: string; tone: string } {
  switch (readiness) {
    case "ready":
      return { label: "Ready", tone: "bg-positive-soft text-positive" };
    case "needs_judge":
      return { label: "Needs a judge", tone: "bg-caution-soft text-caution" };
    case "empty":
      return { label: "No cases", tone: "bg-quiet-soft text-muted" };
    default:
      return { label: "Off", tone: "bg-quiet-soft text-muted" };
  }
}

const TARGETS = ["agent", "copilot", "task", "model"] as const;

export function AiEvalsView() {
  const router = useRouter();
  const [data, setData] = useState<EvalSuiteList | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Filters. `tag` is the one that cannot be answered by the suite row — tags live on the cases —
  // so it is the one filter that costs a query per suite, and it is only sent when chosen.
  const [search, setSearch] = useState("");
  const [target, setTarget] = useState<string>("all");
  const [blockingOnly, setBlockingOnly] = useState(false);
  const [tag, setTag] = useState<string>("");

  const [formOpen, setFormOpen] = useState(false);
  const [draft, setDraft] = useState({
    key: "",
    name: "",
    description: "",
    target: "model" as string,
    model_id: "",
    threshold_percent: "90",
    blocking: false,
  });
  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [formField, setFormField] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [confirmText, setConfirmText] = useState("");
  const [rowMessage, setRowMessage] = useState<Record<string, string>>({});
  const [runningKey, setRunningKey] = useState<string | null>(null);

  const load = useCallback(() => {
    setBusy(true);
    setError(null);
    fetchEvalSuites({
      q: search.trim() || null,
      target: target === "all" ? null : target,
      blocking: blockingOnly ? true : null,
      tag: tag || null,
    })
      .then(setData)
      .catch((cause: unknown) => {
        setData(null);
        setError(cause instanceof ApiError ? cause.message : "The suites could not be loaded.");
      })
      .finally(() => setBusy(false));
  }, [search, target, blockingOnly, tag]);

  useEffect(load, [load]);

  const suites = useMemo(() => data?.suites ?? [], [data]);
  // The list is filtered server-side, so "no rows" has two causes and they need different
  // screens: `is_empty` means the organization owns no suite at all, while zero rows with
  // suites present means the filters excluded everything. The API sends `total` for exactly
  // this — without it, an operator who filtered everything out would be told to go and make a
  // suite they already have. Read off `data` rather than `suites` so this stays a pure
  // expression the loader's null guard above can prove.
  const filteredOut = suites.length === 0 && data?.is_empty === false;

  /** Where a refusal belongs, so the form marks one input rather than shouting. */
  const fieldOf = (cause: unknown): string | null =>
    cause instanceof ApiError && cause.details && typeof cause.details.field === "string"
      ? cause.details.field
      : null;

  const submit = useCallback(async () => {
    setSaving(true);
    setFormError(null);
    setFormField(null);
    const body: Parameters<typeof createEvalSuite>[0] = {
      key: draft.key.trim(),
      name: draft.name.trim(),
      description: draft.description.trim(),
      target: draft.target,
      // Only the reference the chosen target needs. Sending the others would make the store
      // refuse a perfectly good suite with "target needs only `model_id`, but `agent_id` is
      // also set" — a sentence about a field the form never showed.
      model_id: draft.target === "model" && draft.model_id ? draft.model_id : null,
      threshold_percent: Number.parseInt(draft.threshold_percent, 10) || 90,
      blocking: draft.blocking,
    };
    try {
      await createEvalSuite(body);
      setFormOpen(false);
      setDraft({
        key: "",
        name: "",
        description: "",
        target: "model",
        model_id: "",
        threshold_percent: "90",
        blocking: false,
      });
      load();
      router.refresh();
    } catch (cause: unknown) {
      setFormError(cause instanceof ApiError ? cause.message : "The suite could not be created.");
      setFormField(fieldOf(cause));
    } finally {
      setSaving(false);
    }
  }, [draft, load, router]);

  /**
   * Start a run.
   *
   * The route **enqueues**: it resolves the model, writes a `queued` row and returns. It does
   * not score — a route that scored inline would hold the request open for the length of a
   * suite, and forty judge calls is minutes. So the button says "queued" rather than "done" and
   * links to the history, because a panel that waited for a finished result here would hang for
   * exactly as long as the work takes.
   */
  const run = useCallback(
    async (key: string) => {
      setRunningKey(key);
      setRowMessage((state) => ({ ...state, [key]: "" }));
      try {
        await startEvalRun(key);
        setRowMessage((state) => ({
          ...state,
          [key]: "Queued. A runner claims it and scores the suite; the history shows progress.",
        }));
        load();
      } catch (cause: unknown) {
        setRowMessage((state) => ({
          ...state,
          [key]: cause instanceof ApiError ? cause.message : "The run could not be started.",
        }));
      } finally {
        setRunningKey(null);
      }
    },
    [load],
  );

  const remove = useCallback(
    async (key: string) => {
      try {
        await deleteEvalSuite(key);
        setConfirming(null);
        setConfirmText("");
        setRowMessage((state) => ({ ...state, [key]: "" }));
        load();
        router.refresh();
      } catch (cause: unknown) {
        setRowMessage((state) => ({
          ...state,
          [key]: cause instanceof ApiError ? cause.message : "The suite could not be removed.",
        }));
      }
    },
    [load, router],
  );

  if (error) {
    return (
      <div data-eval-error className="flex flex-col gap-3">
        <p className="rounded-lg bg-danger-soft px-3 py-2 text-[13px] text-danger">{error}</p>
        <button
          type="button"
          onClick={load}
          className="self-start rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
        >
          Retry
        </button>
      </div>
    );
  }

  if (!data) return <LoadingTable columns={6} rows={3} />;

  return (
    <div data-eval-suites className="flex flex-col gap-5">
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Suites</p>
          <p className="mt-1 text-2xl font-semibold text-ink">{data.total}</p>
        </div>
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Blocking gates</p>
          <p className="mt-1 text-2xl font-semibold text-ink">{data.blocking_count}</p>
        </div>
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Scheduled</p>
          <p className="mt-1 text-2xl font-semibold text-ink">{data.scheduled_count}</p>
        </div>
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Cases</p>
          <p className="mt-1 text-2xl font-semibold text-ink">{data.case_count}</p>
        </div>
      </div>

      <div className="flex flex-wrap items-center justify-between gap-3">
        <label className="flex min-w-[220px] flex-1 items-center gap-2 rounded-lg border border-line bg-canvas px-3 py-1.5">
          <Search aria-hidden size={14} className="text-muted" />
          <input
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Search key, name or description"
            aria-label="Search suites"
            className="w-full bg-transparent text-[13px] text-ink outline-none"
          />
        </label>
        <div className="flex flex-wrap items-center gap-2">
          <label className="flex items-center gap-1.5 text-[12px] text-muted">
            <Filter aria-hidden size={14} />
            <span className="sr-only">Target</span>
            <select
              value={target}
              onChange={(event) => setTarget(event.target.value)}
              aria-label="Filter by target"
              className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12px] text-ink"
            >
              <option value="all">Every target</option>
              {TARGETS.map((value) => (
                <option key={value} value={value}>
                  {value}
                </option>
              ))}
            </select>
          </label>
          <label className="flex items-center gap-1.5 text-[12px] text-muted">
            <input
              type="checkbox"
              checked={blockingOnly}
              onChange={(event) => setBlockingOnly(event.target.checked)}
              className="accent-accent"
            />
            Blocking only
          </label>
          <button
            type="button"
            onClick={load}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
          >
            {busy ? (
              <Loader2 aria-hidden size={14} className="animate-spin" />
            ) : (
              <RefreshCw aria-hidden size={14} />
            )}
            Refresh
          </button>
          <button
            type="button"
            onClick={() => setFormOpen((open) => !open)}
            aria-expanded={formOpen}
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white transition hover:bg-accent-strong"
          >
            {formOpen ? <X aria-hidden size={14} /> : <Plus aria-hidden size={14} />}
            New suite
          </button>
        </div>
      </div>

      {data.coverage.length > 0 ? (
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-[12px] text-muted">Coverage</span>
          {data.coverage.map((row) => (
            <button
              key={row.tag}
              type="button"
              onClick={() => setTag(tag === row.tag ? "" : row.tag)}
              data-eval-tag
              aria-pressed={tag === row.tag}
              className={`rounded-full border px-2.5 py-0.5 text-[11px] transition ${
                tag === row.tag
                  ? "border-accent bg-accent-soft text-accent"
                  : "border-line text-muted hover:bg-canvas"
              }`}
            >
              {row.tag} · {row.cases}
            </button>
          ))}
        </div>
      ) : null}

      {formOpen ? (
        <form
          data-eval-suite-form
          onSubmit={(event) => {
            event.preventDefault();
            void submit();
          }}
          className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
        >
          <div className="grid gap-3 sm:grid-cols-2">
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Key
              <input
                value={draft.key}
                onChange={(event) => setDraft({ ...draft, key: event.target.value })}
                required
                placeholder="seo-content-gate"
                aria-label="Suite key"
                className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                  formField === "key" ? "border-danger" : "border-line"
                }`}
              />
            </label>
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Name
              <input
                value={draft.name}
                onChange={(event) => setDraft({ ...draft, name: event.target.value })}
                required
                placeholder="SEO content gate"
                aria-label="Suite name"
                className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                  formField === "name" ? "border-danger" : "border-line"
                }`}
              />
            </label>
          </div>
          <label className="flex flex-col gap-1 text-[12px] text-muted">
            Description
            <input
              value={draft.description}
              onChange={(event) => setDraft({ ...draft, description: event.target.value })}
              placeholder="What this suite proves about the platform"
              aria-label="Suite description"
              className="rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink"
            />
          </label>
          <div className="grid gap-3 sm:grid-cols-3">
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Target
              <select
                value={draft.target}
                onChange={(event) => setDraft({ ...draft, target: event.target.value })}
                aria-label="Target kind"
                className="rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink"
              >
                {TARGETS.map((value) => (
                  <option key={value} value={value}>
                    {value}
                  </option>
                ))}
              </select>
            </label>
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Model under test
              <input
                value={draft.model_id}
                onChange={(event) => setDraft({ ...draft, model_id: event.target.value })}
                disabled={draft.target !== "model"}
                placeholder={draft.target === "model" ? "Model id" : "Set on the agent"}
                aria-label="Model under test"
                className="rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink disabled:opacity-50"
              />
            </label>
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Pass threshold (%)
              <input
                type="number"
                min={1}
                max={100}
                value={draft.threshold_percent}
                onChange={(event) =>
                  setDraft({ ...draft, threshold_percent: event.target.value })
                }
                aria-label="Pass threshold"
                className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                  formField === "threshold_percent" ? "border-danger" : "border-line"
                }`}
              />
            </label>
          </div>
          <label className="flex items-center gap-2 text-[12px] text-muted">
            <input
              type="checkbox"
              checked={draft.blocking}
              onChange={(event) => setDraft({ ...draft, blocking: event.target.checked })}
              className="accent-accent"
            />
            Blocking — this suite is the promotion gate
          </label>
          {formError ? (
            <p
              data-eval-suite-form-error
              className="rounded-lg bg-danger-soft px-3 py-2 text-[12px] text-danger"
            >
              {formError}
            </p>
          ) : null}
          <div className="flex items-center gap-2">
            <button
              type="submit"
              disabled={saving || !draft.key.trim() || !draft.name.trim()}
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white disabled:opacity-50"
            >
              {saving ? (
                <Loader2 aria-hidden size={14} className="animate-spin" />
              ) : (
                <ShieldCheck aria-hidden size={14} />
              )}
              Create suite
            </button>
            <button
              type="button"
              onClick={() => {
                setFormOpen(false);
                setFormError(null);
                setFormField(null);
              }}
              className="rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
            >
              Cancel
            </button>
          </div>
        </form>
      ) : null}

      {data.is_empty ? (
        <EmptyState
          title="No eval suites yet"
          hint="A suite is a set of cases and what their output must satisfy. Start with the behaviour you most want to keep from regressing — a real failure you have seen is worth ten easy cases."
          action={
            <button
              type="button"
              onClick={() => setFormOpen(true)}
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white"
            >
              <Plus aria-hidden size={14} />
              New suite
            </button>
          }
        />
      ) : filteredOut ? (
        <EmptyState
          title="No suite matches these filters"
          hint="Clear the search box or widen the target filter. The counts at the top always describe every suite the organization owns, not the filtered page."
        />
      ) : (
        <ul className="flex flex-col gap-2">
          {suites.map((suite) => {
            const badge = readinessBadge(suite.readiness);
            return (
              <li
                key={suite.id}
                data-eval-suite-row
                className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
              >
                <div className="flex flex-wrap items-center gap-2">
                  <ClipboardList aria-hidden size={15} className="text-muted" />
                  <Link
                    href={`/ai/evals/${encodeURIComponent(suite.key)}`}
                    className="text-[14px] font-medium text-ink underline-offset-2 hover:underline"
                  >
                    {suite.name}
                  </Link>
                  <code className="rounded bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
                    {suite.key}
                  </code>
                  <span
                    data-eval-readiness={suite.readiness}
                    title={suite.readiness_note}
                    className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${badge.tone}`}
                  >
                    {badge.label}
                  </span>
                  {suite.blocking ? (
                    <span className="rounded-full bg-accent-soft px-2 py-0.5 text-[11px] text-accent">
                      Gate
                    </span>
                  ) : null}
                  {suite.schedule ? (
                    <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
                      {suite.schedule}
                    </span>
                  ) : null}
                  <span className="ml-auto text-[12px] text-muted">
                    {suite.enabled_case_count}/{suite.case_count} cases
                  </span>
                </div>

                {suite.description ? (
                  <p className="text-[12px] text-muted">{suite.description}</p>
                ) : null}

                {/*
                  The readiness note is shown, not merely used as a tooltip: "No cases" and
                  "Needs a judge" both read as a green-ish row to a colour-blind operator
                  scanning a table, and the sentence is the part that tells them what to do.
                */}
                <p
                  data-eval-readiness-note
                  className={`text-[12px] ${
                    suite.readiness === "ready" ? "text-muted" : "text-caution"
                  }`}
                >
                  {suite.readiness_note}
                </p>

                <div className="flex flex-wrap items-center gap-2">
                  <Link
                    href={`/ai/evals/${encodeURIComponent(suite.key)}`}
                    className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
                  >
                    Open
                  </Link>
                  {/*
                    Two independent reasons this can be disabled, and both are shown.

                    **A missing key** comes from the server's own `viewer_missing`, so the title
                    names the exact permission rather than guessing from the role — a control
                    that refuses with a reason is one the operator can act on (by asking for the
                    key), while a greyed-out button with no text is a dead button, which the
                    definition of done forbids outright.

                    **A suite that cannot run** is the other half and it is the one that used to
                    be missed: a suite with no enabled case, or with a rubric case and no judge
                    model, would post successfully and then sit queued forever. The readiness
                    badge already says so on the row; the button repeats it in words.
                  */}
                  {(() => {
                    const missingRun = data.viewer_missing.includes("ai.evals.run");
                    const blocked = suite.readiness !== "ready";
                    const reason = missingRun
                      ? `Missing ${data.viewer_missing.filter((key) => key.startsWith("ai.evals")).join(", ")} — you may read these suites but not spend tokens running them.`
                      : blocked
                        ? `This suite is not runnable: ${suite.readiness_note}`
                        : null;
                    return (
                      <button
                        type="button"
                        onClick={() => void run(suite.key)}
                        disabled={reason !== null || runningKey === suite.key}
                        data-eval-run={suite.key}
                        data-eval-run-disabled={reason !== null ? "true" : undefined}
                        title={reason ?? "Queue a run of this suite and open the history"}
                        className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-60"
                      >
                        {runningKey === suite.key ? (
                          <Loader2 aria-hidden size={14} className="animate-spin" />
                        ) : (
                          <PlayCircle aria-hidden size={14} />
                        )}
                        Run now
                      </button>
                    );
                  })()}
                  <Link
                    href="/ai/evals/runs"
                    className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
                  >
                    Runs
                  </Link>
                  <button
                    type="button"
                    onClick={() => {
                      setConfirming(suite.key);
                      setConfirmText("");
                    }}
                    className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
                  >
                    <Trash2 aria-hidden size={14} />
                    Delete
                  </button>
                  {rowMessage[suite.key] ? (
                    <span className="text-[12px] text-danger">{rowMessage[suite.key]}</span>
                  ) : null}
                </div>

                {confirming === suite.key ? (
                  <div
                    data-eval-delete-confirm
                    className="flex flex-col gap-2 rounded-lg border border-danger bg-danger-soft p-3"
                  >
                    <p className="flex items-center gap-1.5 text-[12px] text-danger">
                      <AlertTriangle aria-hidden size={14} />
                      Type <code className="font-mono">{suite.key}</code> to remove this suite and
                      its {suite.case_count} case(s).
                    </p>
                    <div className="flex flex-wrap items-center gap-2">
                      <input
                        value={confirmText}
                        onChange={(event) => setConfirmText(event.target.value)}
                        aria-label={`Type the key ${suite.key} to confirm`}
                        className="rounded-lg border border-line bg-canvas px-3 py-1.5 text-[13px] text-ink"
                      />
                      <button
                        type="button"
                        disabled={confirmText !== suite.key}
                        onClick={() => void remove(suite.key)}
                        className="rounded-lg bg-danger px-3 py-1.5 text-[12px] font-medium text-white disabled:opacity-50"
                      >
                        Delete
                      </button>
                      <button
                        type="button"
                        onClick={() => {
                          setConfirming(null);
                          setConfirmText("");
                        }}
                        className="rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
                      >
                        Cancel
                      </button>
                    </div>
                  </div>
                ) : null}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
