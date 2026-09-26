"use client";

/**
 * `/analytics/goals` — conversions and funnels (REQ-007, slice 3).
 *
 * A goal is what the site decided counts: a page someone read, an event it sent, a file it
 * downloaded or a form it submitted — and, when the visitor should do several things in order, a
 * funnel of steps. This screen is the whole life of a goal: the list with its conversions and
 * rate for the current range, an editor for the name and the ordered steps, a switch, a delete,
 * and the funnel of the selected goal drawn from the same range the toolbar shows.
 *
 * Two rules the screen states out loud rather than hiding:
 *
 * * **Order matters.** A step is only reached when every earlier step was, so the funnel counts
 *   only ever go down — and where they go down the screen shows the drop-off.
 * * **A dash is not a zero.** A rate without a denominator (a range that met nobody) reads `—`.
 */

import { useCallback, useEffect, useMemo, useState } from "react";

import {
  ArrowDown,
  ArrowUp,
  Check,
  Plus,
  RefreshCw,
  Target,
  Trash2,
  X,
} from "lucide-react";

import {
  ApiError,
  createAnalyticsGoal,
  deleteAnalyticsGoal,
  fetchAnalyticsGoalFunnel,
  fetchAnalyticsGoals,
  updateAnalyticsGoal,
  type AnalyticsFunnel,
  type AnalyticsGoalMatch,
  type AnalyticsGoalSummary,
  type AnalyticsGoalsResponse,
} from "@/lib/api";

import { useAnalytics } from "./analytics-shell";
import {
  DataTable,
  EmptyPanel,
  ErrorPanel,
  LastSeen,
  LoadingRows,
  Panel,
  formatCount,
  formatRate,
  type Column,
} from "./parts";

/** The kinds a step can match, with the label the editor shows. */
const KINDS: { value: string; label: string }[] = [
  { value: "pageview", label: "Page view" },
  { value: "event", label: "Event" },
  { value: "download", label: "Download" },
  { value: "form_submit", label: "Form submit" },
];

/** One step while it is being edited. */
type StepDraft = {
  kind: string;
  path: string;
  name: string;
  file: string;
};

/** The editor's own state. */
type Draft = {
  name: string;
  enabled: boolean;
  steps: StepDraft[];
};

/** A step the editor cannot save yet, with the reason beside the field. */
function stepProblem(step: StepDraft): string | null {
  const filled = (value: string) => value.trim().length > 0;
  switch (step.kind) {
    case "pageview":
      return filled(step.path) ? null : "A page step needs a path.";
    case "event":
      return filled(step.name) ? null : "An event step needs the event name.";
    case "download":
      return filled(step.file) || filled(step.path)
        ? null
        : "A download step needs a file or a path.";
    default:
      return filled(step.name) || filled(step.path)
        ? null
        : "A form step needs the form name, or the page it was sent from.";
  }
}

/** Build the `match` object of one step, dropping what is blank. */
function stepMatch(step: StepDraft): AnalyticsGoalMatch {
  const pattern = (value: string) => {
    const trimmed = value.trim();
    return trimmed.length > 0 ? trimmed : undefined;
  };

  return {
    path: pattern(step.path),
    name: step.kind === "pageview" || step.kind === "download" ? undefined : pattern(step.name),
    file: step.kind === "download" ? pattern(step.file) : undefined,
  };
}

/** What the editor shows beside a step that has no patterns yet. */
function draftOf(goal?: AnalyticsGoalSummary): Draft {
  if (!goal) {
    return {
      name: "",
      enabled: true,
      steps: [{ kind: "pageview", path: "", name: "", file: "" }],
    };
  }

  return {
    name: goal.name,
    enabled: goal.enabled,
    steps: goal.steps.map((step) => ({
      kind: step.kind,
      path: step.match.path ?? "",
      name: step.match.name ?? "",
      file: step.match.file ?? "",
    })),
  };
}

/** A one-line description of what a step matches. */
function describeMatch(kind: string, match: AnalyticsGoalMatch): string {
  const parts: string[] = [];
  if (match.name) {
    parts.push(kind === "form_submit" ? `form ${match.name}` : match.name);
  }
  if (match.file) {
    parts.push(match.file);
  }
  if (match.path) {
    parts.push(match.path);
  }

  return parts.length > 0 ? parts.join(" · ") : "(nothing yet)";
}

/** The goals screen. */
export function AnalyticsGoalsView() {
  const { siteId, from, to, refreshToken, markLoaded } = useAnalytics();
  const [data, setData] = useState<AnalyticsGoalsResponse | null>(null);
  const [status, setStatus] = useState<"idle" | "loading" | "ready" | "error">("idle");
  const [error, setError] = useState<{ message: string; code: string } | null>(null);
  const [attempt, setAttempt] = useState(0);

  const [editorOpen, setEditorOpen] = useState(false);
  const [editing, setEditing] = useState<AnalyticsGoalSummary | null>(null);
  const [draft, setDraft] = useState<Draft>(draftOf());
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const [selected, setSelected] = useState<string | null>(null);
  const [funnel, setFunnel] = useState<AnalyticsFunnel | null>(null);
  const [funnelError, setFunnelError] = useState<string | null>(null);

  const reload = useCallback(() => setAttempt((value) => value + 1), []);

  useEffect(() => {
    if (!siteId) {
      setData(null);
      setStatus("idle");
      return;
    }

    let live = true;
    setStatus("loading");
    setError(null);

    fetchAnalyticsGoals({ site_id: siteId, from, to })
      .then((answer) => {
        if (!live) {
          return;
        }
        setData(answer);
        setStatus("ready");
        markLoaded();
      })
      .catch((cause: unknown) => {
        if (!live) {
          return;
        }
        setStatus("error");
        setError(
          cause instanceof ApiError
            ? { message: cause.message, code: cause.code }
            : { message: "The goals could not be read.", code: "unknown_error" },
        );
      });

    return () => {
      live = false;
    };
  }, [siteId, from, to, attempt, refreshToken, markLoaded]);

  const goals = useMemo(() => data?.goals ?? [], [data]);

  // The funnel of the selected goal, for the range the toolbar shows.
  useEffect(() => {
    if (!siteId || !selected) {
      setFunnel(null);
      return;
    }

    let live = true;
    setFunnelError(null);
    fetchAnalyticsGoalFunnel(selected, { site_id: siteId, from, to })
      .then((answer) => {
        if (live) {
          setFunnel(answer);
        }
      })
      .catch((cause: unknown) => {
        if (live) {
          setFunnel(null);
          setFunnelError(
            cause instanceof ApiError ? cause.message : "The funnel could not be read.",
          );
        }
      });

    return () => {
      live = false;
    };
  }, [siteId, selected, from, to, attempt, refreshToken]);

  const startCreate = () => {
    setEditing(null);
    setDraft(draftOf());
    setSaveError(null);
    setEditorOpen(true);
  };

  const startEdit = (goal: AnalyticsGoalSummary) => {
    setEditing(goal);
    setDraft(draftOf(goal));
    setSaveError(null);
    setEditorOpen(true);
  };

  const patchStep = (index: number, changes: Partial<StepDraft>) => {
    setDraft((current) => ({
      ...current,
      steps: current.steps.map((step, position) =>
        position === index ? { ...step, ...changes } : step,
      ),
    }));
  };

  const moveStep = (index: number, direction: -1 | 1) => {
    setDraft((current) => {
      const target = index + direction;
      if (target < 0 || target >= current.steps.length) {
        return current;
      }
      const steps = [...current.steps];
      const [moved] = steps.splice(index, 1);
      steps.splice(target, 0, moved);
      return { ...current, steps };
    });
  };

  const save = async () => {
    if (!siteId) {
      return;
    }
    if (draft.name.trim().length === 0) {
      setSaveError("A goal needs a name.");
      return;
    }
    if (draft.steps.length < 1 || draft.steps.length > 5) {
      setSaveError("A funnel is 1–5 steps.");
      return;
    }
    const problem = draft.steps.map(stepProblem).find((entry) => entry !== null);
    if (problem) {
      setSaveError(problem);
      return;
    }

    const steps = draft.steps.map((step) => ({ kind: step.kind, match: stepMatch(step) }));
    const body = {
      name: draft.name.trim(),
      kind: steps[steps.length - 1].kind,
      match: steps[steps.length - 1].match,
      enabled: draft.enabled,
      steps,
    };
    const query = { site_id: siteId, from, to };

    setSaving(true);
    setSaveError(null);
    try {
      const saved = editing
        ? await updateAnalyticsGoal(editing.id, query, body)
        : await createAnalyticsGoal(query, body);
      setEditorOpen(false);
      setEditing(null);
      setSelected((current) => current ?? saved.id);
      reload();
    } catch (cause) {
      setSaveError(
        cause instanceof ApiError
          ? cause.message
          : "The goal could not be saved.",
      );
    } finally {
      setSaving(false);
    }
  };

  const toggle = async (goal: AnalyticsGoalSummary) => {
    if (!siteId) {
      return;
    }
    try {
      await updateAnalyticsGoal(goal.id, { site_id: siteId, from, to }, { enabled: !goal.enabled });
      reload();
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? { message: cause.message, code: cause.code }
          : { message: "The goal could not be switched.", code: "unknown_error" },
      );
    }
  };

  const remove = async (goal: AnalyticsGoalSummary) => {
    if (!siteId) {
      return;
    }
    try {
      await deleteAnalyticsGoal(goal.id, { site_id: siteId, from, to });
      if (selected === goal.id) {
        setSelected(null);
      }
      reload();
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? { message: cause.message, code: cause.code }
          : { message: "The goal could not be deleted.", code: "unknown_error" },
      );
    }
  };

  const columns: Column<AnalyticsGoalSummary>[] = [
    {
      key: "name",
      label: "Goal",
      render: (row) => (
        <button
          type="button"
          data-goal-open={row.id}
          onClick={() => setSelected(row.id)}
          className="flex flex-col items-start text-left"
        >
          <span className="text-ink">{row.name}</span>
          <span className="text-[11.5px] text-muted">
            {row.steps.length === 1
              ? "Single step"
              : `${row.steps.length} steps`}
          </span>
        </button>
      ),
    },
    {
      key: "kind",
      label: "Kind",
      render: (row) => (
        <span className="text-muted">{KINDS.find((entry) => entry.value === row.kind)?.label ?? row.kind}</span>
      ),
    },
    {
      key: "match",
      label: "Match",
      render: (row) => (
        <span className="font-mono text-[11.5px] text-muted">
          {describeMatch(row.kind, row.match)}
        </span>
      ),
    },
    {
      key: "conversions",
      label: "Conversions",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.conversions),
    },
    {
      key: "rate",
      label: "Rate vs visitors",
      align: "right",
      numeric: true,
      render: (row) => formatRate(row.rate),
    },
    {
      key: "last_hit",
      label: "Last hit",
      render: (row) => <LastSeen value={row.last_hit} />,
    },
    {
      key: "enabled",
      label: "Enabled",
      render: (row) => (
        <button
          type="button"
          data-goal-toggle={row.id}
          aria-pressed={row.enabled}
          onClick={() => void toggle(row)}
          className={`flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-[11.5px] transition ${
            row.enabled
              ? "border-accent bg-accent-soft text-accent-strong"
              : "border-line text-muted hover:bg-quiet-soft"
          }`}
        >
          {row.enabled ? <Check className="size-3" aria-hidden /> : <X className="size-3" aria-hidden />}
          {row.enabled ? "On" : "Off"}
        </button>
      ),
    },
    {
      key: "actions",
      label: "",
      align: "right",
      render: (row) => (
        <span className="flex items-center justify-end gap-1.5">
          <button
            type="button"
            data-goal-edit={row.id}
            onClick={() => startEdit(row)}
            className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-ink transition hover:bg-quiet-soft"
          >
            Edit
          </button>
          <button
            type="button"
            data-goal-delete={row.id}
            onClick={() => void remove(row)}
            className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-accent-strong transition hover:bg-quiet-soft"
            aria-label={`Delete ${row.name}`}
          >
            <Trash2 className="size-3.5" aria-hidden />
          </button>
        </span>
      ),
    },
  ];

  if (status === "error" && error) {
    return <ErrorPanel message={error.message} code={error.code} onRetry={reload} />;
  }

  if (status === "loading" && !data) {
    return (
      <Panel title="Goals" bodyClassName="p-0">
        <LoadingRows rows={5} label="Loading the goals" />
      </Panel>
    );
  }

  return (
    <div className="flex flex-col gap-4" data-analytics-goals>
      {editorOpen ? (
        <Panel
          title={editing ? `Edit ${editing.name}` : "New goal"}
          subtitle="Steps run in order: a visit only reaches step two after step one."
          testId="goal-editor"
          action={
            <div className="flex items-center gap-2">
              <button
                type="button"
                data-goal-cancel
                onClick={() => {
                  setEditorOpen(false);
                  setEditing(null);
                  setSaveError(null);
                }}
                className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-muted transition hover:bg-quiet-soft"
              >
                Cancel
              </button>
              <button
                type="button"
                data-goal-save
                disabled={saving}
                onClick={() => void save()}
                className="rounded-lg border border-accent bg-accent-soft px-2.5 py-1.5 text-[12px] text-accent-strong transition hover:brightness-95 disabled:opacity-50"
              >
                {saving ? "Saving…" : editing ? "Save changes" : "Create goal"}
              </button>
            </div>
          }
        >
          <div className="flex flex-col gap-4 p-4">
            <div className="flex flex-wrap items-end gap-3">
              <label className="flex w-64 flex-col gap-1 text-[11px] tracking-wide text-muted uppercase">
                Name
                <input
                  value={draft.name}
                  data-goal-name
                  onChange={(event) => setDraft({ ...draft, name: event.target.value })}
                  placeholder="Trial signup"
                  className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12.5px] text-ink"
                />
              </label>
              <label className="flex items-center gap-2 pb-1.5 text-[12.5px] text-ink">
                <input
                  type="checkbox"
                  checked={draft.enabled}
                  data-goal-enabled
                  onChange={() => setDraft({ ...draft, enabled: !draft.enabled })}
                  className="size-3.5 accent-accent"
                />
                Record hits
              </label>
            </div>

            <ol className="flex flex-col gap-2" data-goal-steps>
              {draft.steps.map((step, index) => (
                <li
                  key={index}
                  data-goal-step={index + 1}
                  className="flex flex-wrap items-end gap-2 rounded-lg border border-line bg-canvas px-3 py-2"
                >
                  <span className="pb-1.5 font-mono text-[11.5px] text-muted">{index + 1}</span>
                  <label className="flex flex-col gap-1 text-[11px] tracking-wide text-muted uppercase">
                    Kind
                    <select
                      value={step.kind}
                      data-goal-step-kind={index + 1}
                      onChange={(event) => patchStep(index, { kind: event.target.value })}
                      className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12px] text-ink"
                    >
                      {KINDS.map((entry) => (
                        <option key={entry.value} value={entry.value}>
                          {entry.label}
                        </option>
                      ))}
                    </select>
                  </label>

                  {step.kind === "download" ? (
                    <label className="flex flex-col gap-1 text-[11px] tracking-wide text-muted uppercase">
                      File
                      <input
                        value={step.file}
                        data-goal-step-file={index + 1}
                        onChange={(event) => patchStep(index, { file: event.target.value })}
                        placeholder="/files/guide.pdf"
                        className="w-52 rounded-lg border border-line bg-surface px-2 py-1.5 font-mono text-[11.5px] text-ink"
                      />
                    </label>
                  ) : null}

                  {step.kind === "pageview" || step.kind === "download" ? null : (
                    <label className="flex flex-col gap-1 text-[11px] tracking-wide text-muted uppercase">
                      {step.kind === "form_submit" ? "Form" : "Event"}
                      <input
                        value={step.name}
                        data-goal-step-name={index + 1}
                        onChange={(event) => patchStep(index, { name: event.target.value })}
                        placeholder={step.kind === "form_submit" ? "contact" : "signup"}
                        className="w-44 rounded-lg border border-line bg-surface px-2 py-1.5 font-mono text-[11.5px] text-ink"
                      />
                    </label>
                  )}

                  <label className="flex flex-col gap-1 text-[11px] tracking-wide text-muted uppercase">
                    {step.kind === "pageview" ? "Path" : "Path (optional)"}
                    <input
                      value={step.path}
                      data-goal-step-path={index + 1}
                      onChange={(event) => patchStep(index, { path: event.target.value })}
                      placeholder="/pricing"
                      className="w-48 rounded-lg border border-line bg-surface px-2 py-1.5 font-mono text-[11.5px] text-ink"
                    />
                  </label>

                  <div className="ml-auto flex items-center gap-1 pb-1">
                    <button
                      type="button"
                      data-goal-step-up={index + 1}
                      onClick={() => moveStep(index, -1)}
                      disabled={index === 0}
                      aria-label="Move this step up"
                      className="rounded-lg border border-line p-1.5 text-muted transition hover:bg-quiet-soft disabled:opacity-40"
                    >
                      <ArrowUp className="size-3.5" aria-hidden />
                    </button>
                    <button
                      type="button"
                      data-goal-step-down={index + 1}
                      onClick={() => moveStep(index, 1)}
                      disabled={index === draft.steps.length - 1}
                      aria-label="Move this step down"
                      className="rounded-lg border border-line p-1.5 text-muted transition hover:bg-quiet-soft disabled:opacity-40"
                    >
                      <ArrowDown className="size-3.5" aria-hidden />
                    </button>
                    <button
                      type="button"
                      data-goal-step-remove={index + 1}
                      onClick={() =>
                        setDraft((current) => ({
                          ...current,
                          steps: current.steps.filter((_, position) => position !== index),
                        }))
                      }
                      disabled={draft.steps.length === 1}
                      aria-label="Remove this step"
                      className="rounded-lg border border-line p-1.5 text-accent-strong transition hover:bg-quiet-soft disabled:opacity-40"
                    >
                      <Trash2 className="size-3.5" aria-hidden />
                    </button>
                  </div>
                </li>
              ))}
            </ol>

            <div className="flex flex-wrap items-center gap-3">
              <button
                type="button"
                data-goal-step-add
                disabled={draft.steps.length >= 5}
                onClick={() =>
                  setDraft((current) => ({
                    ...current,
                    steps: [...current.steps, { kind: "pageview", path: "", name: "", file: "" }],
                  }))
                }
                className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft disabled:opacity-40"
              >
                <Plus className="size-3.5" aria-hidden />
                Add step
              </button>
              <span className="text-[11.5px] text-muted">
                {draft.steps.length} of 5 steps · a step is only reached after the one before it
              </span>
            </div>

            {saveError ? (
              <p
                data-goal-error
                role="alert"
                className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12px] text-accent-strong"
              >
                {saveError}
              </p>
            ) : null}
          </div>
        </Panel>
      ) : null}

      {goals.length === 0 ? (
        <EmptyPanel
          title="No goals yet"
          dataAttribute="analytics-goals-empty"
          action={
            <button
              type="button"
              data-goal-create
              onClick={startCreate}
              className="flex items-center gap-1.5 rounded-lg border border-accent bg-accent-soft px-3 py-1.5 text-[12.5px] text-accent-strong transition hover:brightness-95"
            >
              <Target className="size-3.5" aria-hidden />
              Create the first goal
            </button>
          }
        >
          <p>
            A goal is what counts as a conversion here: a page someone read, an event the site
            sent, a file someone downloaded or a form someone submitted. Goals with two or more
            steps become funnels.
          </p>
        </EmptyPanel>
      ) : (
        <Panel
          title="Goals"
          subtitle={`${goals.length} goal${goals.length === 1 ? "" : "s"} · conversions are visitors that reached the last step`}
          bodyClassName="p-0"
          testId="goals-table"
          action={
            <button
              type="button"
              data-goal-create
              onClick={startCreate}
              className="flex items-center gap-1.5 rounded-lg border border-accent bg-accent-soft px-2.5 py-1.5 text-[12px] text-accent-strong transition hover:brightness-95"
            >
              <Plus className="size-3.5" aria-hidden />
              New goal
            </button>
          }
        >
          <DataTable
            columns={columns}
            rows={goals}
            rowKey={(row) => row.id}
            onRowClick={(row) => setSelected(row.id)}
          />
        </Panel>
      )}

      {selected ? (
        <Panel
          title={funnel ? `Funnel · ${funnel.name}` : "Funnel"}
          subtitle={
            funnel
              ? `${funnel.from} → ${funnel.to} · ${formatCount(funnel.visitors)} visitors in the range`
              : "Reading the funnel of the selected goal…"
          }
          testId="goal-funnel"
          bodyClassName="p-0"
        >
          {funnelError ? (
            <p data-goal-funnel-error role="alert" className="px-4 py-3 text-[12.5px] text-accent-strong">
              {funnelError}
            </p>
          ) : funnel ? (
            <ol data-goal-funnel className="flex flex-col divide-y divide-line">
              {funnel.steps.map((step) => (
                <li
                  key={step.position}
                  data-goal-funnel-step={step.position}
                  className="flex flex-wrap items-center gap-3 px-4 py-3"
                >
                  <span className="flex size-6 items-center justify-center rounded-full bg-quiet-soft font-mono text-[11.5px] text-ink">
                    {step.position}
                  </span>
                  <span className="min-w-0 flex-1">
                    <span className="block text-[12.5px] text-ink">
                      {KINDS.find((entry) => entry.value === step.kind)?.label ?? step.kind}
                    </span>
                    <span className="block font-mono text-[11.5px] text-muted">
                      {describeMatch(step.kind, step.match)}
                    </span>
                  </span>
                  <span
                    data-goal-funnel-count={step.position}
                    className="font-mono text-[13px] text-ink"
                  >
                    {formatCount(step.visitors)}
                  </span>
                  <span className="w-24 text-right text-[11.5px] text-muted">
                    {step.position === 1
                      ? `${formatRate(step.rate)} of visitors`
                      : step.drop_off > 0
                        ? `−${formatCount(step.drop_off)}`
                        : "no drop-off"}
                  </span>
                </li>
              ))}
              <li className="flex items-center gap-3 px-4 py-3">
                <span className="flex size-6 items-center justify-center rounded-full bg-accent-soft text-accent-strong">
                  <Target className="size-3.5" aria-hidden />
                </span>
                <span className="flex-1 text-[12.5px] text-ink">Conversions</span>
                <span data-goal-funnel-conversions className="font-mono text-[13px] text-ink">
                  {formatCount(funnel.conversions)}
                </span>
                <span className="w-24 text-right text-[11.5px] text-muted">
                  {formatRate(funnel.rate)} of visitors
                </span>
              </li>
            </ol>
          ) : (
            <LoadingRows rows={3} label="Loading the funnel" />
          )}
        </Panel>
      ) : goals.length > 0 ? (
        <p className="text-[12.5px] text-muted" data-goal-funnel-hint>
          Select a goal to read its funnel for this range.
        </p>
      ) : null}

      <p className="flex items-center gap-2 text-[11.5px] text-muted">
        <RefreshCw className="size-3.5" aria-hidden />
        A visitor counts once per step, whatever the beacon repeated — re-sending the same event
        never moves a funnel twice.
      </p>
    </div>
  );
}
