"use client";

/**
 * Task routing and feature overrides (docs/requests/REQ-098, slice 2).
 *
 * This screen answers one question an operator cannot answer anywhere else: **when a request
 * arrives, which model answers it, and why?** Everything here serves that question, and the
 * screen is built around three rules that follow from it.
 *
 * - **Every skip is shown.** A candidate that is switched off, that lost a model, or that cannot
 *   claim a required capability appears in the row with the reason beside it. A fallback chain
 *   whose skipped entries are hidden is a chain that looks configured and silently never fires.
 * - **The dry run is the explanation.** The "Route a request" panel calls the preview endpoint
 *   and renders the walk it returns, so an operator sees the same reasoning the router will use
 *   rather than a second implementation of it in the browser.
 * - **An unresolvable task is a warning, not an error.** The banner is amber and names the tasks
 *   that cannot resolve. It is deliberately *not* red: nothing has failed, and the installation
 *   keeps serving the tasks that do resolve. Colouring it as an error would train operators to
 *   ignore the banner that does mean something.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  Check,
  Loader2,
  Plus,
  RotateCcw,
  Trash2,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiFeatureInfo,
  type AiModel,
  type AiRouteCandidate,
  type AiRouting,
  type AiRoutingPreview,
  type AiRoutingScope,
  type AiTaskRoute,
  fetchAiRouting,
  fetchEffectivePermissions,
  previewAiRouting,
  putAiFeatureOverride,
  putAiTaskMap,
} from "@/lib/api";

/** How a scope reads in a sentence: "installation", "organization" or "site". */
function scopeName(scope: Record<string, string> | AiRoutingScope): string {
  if ("kind" in scope) {
    return scope.kind;
  }
  if ("site" in scope) return "site";
  if ("organization" in scope) return "organization";
  return "installation";
}

/** One candidate chip in a chain, with its own remove and reorder affordances. */
function CandidateChip({
  candidate,
  index,
  total,
  onMove,
  onRemove,
}: {
  candidate: AiRouteCandidate;
  index: number;
  total: number;
  onMove: (from: number, to: number) => void;
  onRemove: () => void;
}) {
  const label = candidate.model_label ?? "removed model";

  return (
    <li
      data-routing-candidate={candidate.model_id ?? "removed"}
      data-routing-needs-attention={candidate.needs_attention ? "true" : "false"}
      className={`flex items-start gap-2 rounded-lg border px-2 py-1.5 text-[12.5px] ${
        candidate.needs_attention
          ? "border-amber-500/40 bg-amber-500/5"
          : "border-line bg-panel"
      }`}
    >
      <span className="mt-px font-mono text-[11px] text-muted">{index + 1}</span>

      <span className="min-w-0 flex-1">
        <span className="block truncate font-medium">{label}</span>
        {candidate.refusal ? (
          <span className="mt-0.5 block text-[11.5px] text-amber-600 dark:text-amber-400">
            {candidate.refusal}
          </span>
        ) : null}
        {candidate.requirements.length > 0 ? (
          <span className="mt-1 flex flex-wrap gap-1">
            {candidate.requirements.map((requirement) => (
              <span
                key={requirement}
                className="rounded border border-line px-1 py-px text-[10.5px] text-muted"
              >
                {requirement}
              </span>
            ))}
          </span>
        ) : null}
      </span>

      {/* Reorder is up/down buttons rather than a drag handle: the screen is read on a laptop
          and a phone, and a control that only works with a pointer is a control half the
          audience cannot reach. */}
      <span className="flex shrink-0 items-center gap-0.5">
        <button
          type="button"
          aria-label={`Move ${label} earlier in the chain`}
          disabled={index === 0}
          onClick={() => onMove(index, index - 1)}
          className="rounded p-1 text-muted hover:bg-raised disabled:opacity-30 disabled:hover:bg-transparent"
        >
          <ArrowUp size={13} />
        </button>
        <button
          type="button"
          aria-label={`Move ${label} later in the chain`}
          disabled={index === total - 1}
          onClick={() => onMove(index, index + 1)}
          className="rounded p-1 text-muted hover:bg-raised disabled:opacity-30 disabled:hover:bg-transparent"
        >
          <ArrowDown size={13} />
        </button>
        <button
          type="button"
          aria-label={`Remove ${label} from the chain`}
          onClick={onRemove}
          className="rounded p-1 text-muted hover:bg-raised"
        >
          <X size={13} />
        </button>
      </span>
    </li>
  );
}

/** The editor for one task's chain: the primary, its fallbacks and its requirement chips. */
function TaskRow({
  row,
  models,
  requirements,
  canWrite,
  onSave,
  onClear,
}: {
  row: AiTaskRoute;
  models: AiModel[];
  requirements: string[];
  canWrite: boolean;
  onSave: (candidates: { modelId: string; requirements: string[] }[]) => void;
  onClear: () => void;
}) {
  // The draft is separate from the saved row so a half-typed chain is never mistaken for the
  // live one, and so a failed save leaves the operator's input intact to correct.
  const [draft, setDraft] = useState(() =>
    row.candidates.map((candidate) => ({
      modelId: candidate.model_id ?? "",
      requirements: candidate.requirements,
    })),
  );
  const [dirty, setDirty] = useState(false);

  // Re-seed when the server's answer changes underneath the draft (a reload, another operator).
  useEffect(() => {
    setDraft(
      row.candidates.map((candidate) => ({
        modelId: candidate.model_id ?? "",
        requirements: candidate.requirements,
      })),
    );
    setDirty(false);
  }, [row]);

  const update = (next: typeof draft) => {
    setDraft(next);
    setDirty(true);
  };

  const move = (from: number, to: number) => {
    if (to < 0 || to >= draft.length) return;
    const next = [...draft];
    const [moved] = next.splice(from, 1);
    next.splice(to, 0, moved);
    update(next);
  };

  const toggleRequirement = (index: number, requirement: string) => {
    const next = [...draft];
    const current = next[index].requirements;
    next[index] = {
      ...next[index],
      requirements: current.includes(requirement)
        ? current.filter((value) => value !== requirement)
        : [...current, requirement],
    };
    update(next);
  };

  return (
    <section
      data-routing-task={row.task}
      data-routing-inherited={row.inherited ? "true" : "false"}
      className="rounded-xl border border-line bg-panel p-3.5"
    >
      <header className="flex flex-wrap items-baseline justify-between gap-2">
        <div className="min-w-0">
          <h3 className="text-[13.5px] font-medium">{row.task}</h3>
          <p className="mt-0.5 text-[12px] text-muted">{row.description}</p>
        </div>
        {row.inherited ? (
          <span className="shrink-0 rounded border border-line px-1.5 py-0.5 text-[11px] text-muted">
            inherited
          </span>
        ) : null}
      </header>

      {draft.length === 0 ? (
        <p className="mt-3 text-[12.5px] text-muted">
          No primary model. This task falls through to the installation default.
        </p>
      ) : (
        <ul className="mt-3 space-y-1.5">
          {draft.map((entry, index) => {
            // The saved row for this slot, when there is one. A draft row the operator just added
            // has no saved counterpart, and the chip still has to render — otherwise its remove
            // button does not exist and a half-typed chain cannot be undone except by choosing
            // a model first. A synthetic candidate carries the same shape with nothing claimed,
            // which is exactly what "not saved yet" means.
            const candidate =
              row.candidates[index] ??
              ({
                position: index + 1,
                model_id: entry.modelId || null,
                model_label: null,
                needs_attention: false,
                refusal: null,
                requirements: entry.requirements,
                enabled: false,
              } satisfies AiRouteCandidate);

            return (
              // Keyed on the index alone: keying on the model id would remount the select on
              // every choice and drop the focus the operator is typing into.
              <li key={index}>
                <div className="flex flex-wrap items-center gap-2">
                  <select
                    aria-label={`${row.task} candidate ${index + 1} model`}
                    value={entry.modelId}
                    disabled={!canWrite}
                    onChange={(event) => {
                      const next = [...draft];
                      next[index] = { ...next[index], modelId: event.target.value };
                      update(next);
                    }}
                    className="min-w-0 flex-1 rounded border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
                  >
                    <option value="">Choose a model…</option>
                    {models.map((model) => (
                      <option key={model.id} value={model.id}>
                        {model.provider_name}/{model.model_key}
                        {model.enabled ? "" : " (switched off)"}
                      </option>
                    ))}
                  </select>
                  {candidate ? (
                    <CandidateChip
                      candidate={candidate}
                      index={index}
                      total={draft.length}
                      onMove={move}
                      onRemove={() => update(draft.filter((_, at) => at !== index))}
                    />
                  ) : null}
                </div>

                {requirements.length > 0 ? (
                  <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
                    <span className="text-[11px] text-muted">Requires:</span>
                    {requirements.map((requirement) => {
                      const on = entry.requirements.includes(requirement);
                      return (
                        <button
                          key={requirement}
                          type="button"
                          aria-pressed={on}
                          disabled={!canWrite}
                          onClick={() => toggleRequirement(index, requirement)}
                          className={`rounded border px-1.5 py-0.5 text-[11px] disabled:opacity-50 ${
                            on
                              ? "border-accent bg-accent/10 text-accent"
                              : "border-line text-muted hover:bg-raised"
                          }`}
                        >
                          {requirement}
                        </button>
                      );
                    })}
                  </div>
                ) : null}
              </li>
            );
          })}
        </ul>
      )}

      {canWrite ? (
        <div className="mt-3 flex flex-wrap items-center gap-2">
          <button
            type="button"
            onClick={() => update([...draft, { modelId: "", requirements: [] }])}
            className="inline-flex items-center gap-1 rounded border border-line px-2 py-1 text-[12px] hover:bg-raised"
          >
            <Plus size={13} /> Add fallback
          </button>
          <button
            type="button"
            disabled={!dirty}
            onClick={() =>
              onSave(draft.filter((entry) => entry.modelId !== ""))
            }
            className="inline-flex items-center gap-1 rounded bg-accent px-2 py-1 text-[12px] text-accent-fg disabled:opacity-40"
          >
            <Check size={13} /> Save chain
          </button>
          {draft.length > 0 ? (
            <button
              type="button"
              onClick={onClear}
              className="inline-flex items-center gap-1 rounded border border-line px-2 py-1 text-[12px] text-muted hover:bg-raised"
            >
              <RotateCcw size={13} /> Reset to inherited
            </button>
          ) : null}
        </div>
      ) : null}
    </section>
  );
}

/** The dry-run panel: ask what a request would do today, and read the walk it returns. */
function PreviewPanel({
  routing,
  scope,
}: {
  routing: AiRouting;
  scope: AiRoutingScope;
}) {
  const [task, setTask] = useState(routing.tasks[0]?.task ?? "cheap");
  const [feature, setFeature] = useState("");
  const [requires, setRequires] = useState<string[]>([]);
  const [answer, setAnswer] = useState<AiRoutingPreview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const run = async () => {
    setBusy(true);
    setError(null);
    setAnswer(null);
    try {
      setAnswer(
        await previewAiRouting({
          scope,
          task: task || undefined,
          feature: feature || undefined,
          requires,
        }),
      );
    } catch (cause: unknown) {
      setError(
        cause instanceof ApiError
          ? cause.message
          : "The dry run could not be resolved.",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <section data-routing-preview className="rounded-xl border border-line bg-panel p-3.5">
      <h3 className="text-[13.5px] font-medium">Route a request</h3>
      <p className="mt-0.5 text-[12px] text-muted">
        Resolves against the stored map and calls no provider.
      </p>

      <div className="mt-3 grid gap-2 sm:grid-cols-2">
        <label className="text-[12px]">
          <span className="mb-1 block text-muted">Task</span>
          <select
            value={task}
            onChange={(event) => setTask(event.target.value)}
            className="w-full rounded border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
          >
            {routing.tasks.map((row) => (
              <option key={row.task} value={row.task}>
                {row.task}
              </option>
            ))}
          </select>
        </label>

        <label className="text-[12px]">
          <span className="mb-1 block text-muted">Feature (optional)</span>
          <select
            value={feature}
            onChange={(event) => setFeature(event.target.value)}
            className="w-full rounded border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
          >
            <option value="">No feature pin</option>
            {routing.features.map((entry: AiFeatureInfo) => (
              <option key={entry.key} value={entry.key}>
                {entry.key}
              </option>
            ))}
          </select>
        </label>
      </div>

      {routing.requirements.length > 0 ? (
        <div className="mt-2.5 flex flex-wrap items-center gap-1.5">
          <span className="text-[11px] text-muted">Requires:</span>
          {routing.requirements.map((requirement) => {
            const on = requires.includes(requirement);
            return (
              <button
                key={requirement}
                type="button"
                aria-pressed={on}
                onClick={() =>
                  setRequires(
                    on
                      ? requires.filter((value) => value !== requirement)
                      : [...requires, requirement],
                  )
                }
                className={`rounded border px-1.5 py-0.5 text-[11px] ${
                  on
                    ? "border-accent bg-accent/10 text-accent"
                    : "border-line text-muted hover:bg-raised"
                }`}
              >
                {requirement}
              </button>
            );
          })}
        </div>
      ) : null}

      <button
        type="button"
        onClick={run}
        disabled={busy || (!task && !feature)}
        className="mt-3 inline-flex items-center gap-1.5 rounded bg-accent px-2.5 py-1.5 text-[12.5px] text-accent-fg disabled:opacity-40"
      >
        {busy ? <Loader2 size={13} className="animate-spin" /> : null}
        {busy ? "Resolving…" : "Resolve"}
      </button>

      {error ? (
        <p data-routing-preview-error className="mt-2.5 text-[12.5px] text-danger">
          {error}
        </p>
      ) : null}

      {answer ? (
        <div data-routing-walk className="mt-3 rounded-lg border border-line bg-canvas p-2.5">
          <p className="text-[12.5px]">
            {answer.unresolved ? (
              <span className="text-danger">Nothing can answer this request.</span>
            ) : (
              <>
                <span className="text-muted">Rule </span>
                <span className="font-mono">{answer.rule}</span>
                <span className="text-muted"> → </span>
                <span className="font-mono">{answer.model?.model_id ?? "—"}</span>
                {answer.model?.position ? (
                  <span className="text-muted"> (position {answer.model.position})</span>
                ) : null}
              </>
            )}
          </p>

          {answer.walk.length === 0 ? (
            <p className="mt-1.5 text-[12px] text-muted">
              No candidate was considered for this request.
            </p>
          ) : (
            <ol className="mt-2 space-y-1">
              {answer.walk.map((entry, index) => (
                <li
                  key={`${entry.model_id ?? "none"}-${index}`}
                  data-routing-walk-outcome={entry.outcome}
                  className="flex gap-2 text-[12px]"
                >
                  <span
                    className={
                      entry.outcome === "chosen" ? "text-success" : "text-muted"
                    }
                  >
                    {entry.outcome === "chosen" ? "→" : "·"}
                  </span>
                  <span className="min-w-0 flex-1">
                    <span className="font-mono">{entry.model_id ?? "removed model"}</span>
                    <span className="block text-[11.5px] text-muted">{entry.reason}</span>
                  </span>
                </li>
              ))}
            </ol>
          )}
        </div>
      ) : null}

      <p className="mt-2.5 text-[11.5px] text-muted">
        Resolution order: {routing.rules.join(" → ")}
      </p>
    </section>
  );
}

/** The feature override table: one row per pin, with a real add and a real remove. */
function OverridesTable({
  routing,
  models,
  scope,
  canWrite,
  onChanged,
}: {
  routing: AiRouting;
  models: AiModel[];
  scope: AiRoutingScope;
  canWrite: boolean;
  onChanged: () => void;
}) {
  const [feature, setFeature] = useState("");
  const [modelId, setModelId] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const pinned = useMemo(
    () => new Set(routing.overrides.map((override) => override.feature)),
    [routing.overrides],
  );
  const available = routing.features.filter((entry) => !pinned.has(entry.key));

  const save = async () => {
    if (!feature || !modelId) return;
    setBusy(true);
    setError(null);
    try {
      // The scope on screen, not the installation: a pin written while the site scope is
      // selected must land at that site, or the scope selector would be a decoration over a
      // form that always edits the platform-wide map.
      await putAiFeatureOverride(scope, feature, modelId);
      setFeature("");
      setModelId("");
      onChanged();
    } catch (cause: unknown) {
      setError(
        cause instanceof ApiError ? cause.message : "The override could not be saved.",
      );
    } finally {
      setBusy(false);
    }
  };

  const remove = async (featureKey: string) => {
    setBusy(true);
    setError(null);
    try {
      await putAiFeatureOverride(scope, featureKey, null);
      onChanged();
    } catch (cause: unknown) {
      setError(
        cause instanceof ApiError ? cause.message : "The override could not be removed.",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <section data-routing-overrides className="rounded-xl border border-line bg-panel p-3.5">
      <h3 className="text-[13.5px] font-medium">Feature overrides</h3>
      <p className="mt-0.5 text-[12px] text-muted">
        A pinned feature answers before the task map, without changing it.
      </p>

      {routing.overrides.length === 0 ? (
        <p className="mt-3 text-[12.5px] text-muted">
          No feature is pinned at this scope.
        </p>
      ) : (
        <table className="mt-3 w-full text-[12.5px]">
          <thead>
            <tr className="border-b border-line text-left text-[11.5px] text-muted">
              <th className="py-1.5 pr-2 font-normal">Feature</th>
              <th className="py-1.5 pr-2 font-normal">Model</th>
              <th className="py-1.5 pr-2 font-normal">Scope</th>
              <th className="py-1.5 font-normal" />
            </tr>
          </thead>
          <tbody>
            {routing.overrides.map((override) => (
              <tr key={override.feature} data-routing-override={override.feature}>
                <td className="py-1.5 pr-2 font-medium">{override.feature}</td>
                <td className="py-1.5 pr-2 font-mono">{override.model_label}</td>
                <td className="py-1.5 pr-2 text-muted">{scopeName(override.scope)}</td>
                <td className="py-1.5 text-right">
                  {canWrite ? (
                    <button
                      type="button"
                      aria-label={`Remove the ${override.feature} override`}
                      disabled={busy}
                      onClick={() => remove(override.feature)}
                      className="rounded p-1 text-muted hover:bg-raised disabled:opacity-40"
                    >
                      <Trash2 size={13} />
                    </button>
                  ) : null}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {canWrite && available.length > 0 ? (
        <div className="mt-3 flex flex-wrap items-end gap-2">
          <label className="text-[12px]">
            <span className="mb-1 block text-muted">Feature</span>
            <select
              value={feature}
              onChange={(event) => setFeature(event.target.value)}
              className="rounded border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
            >
              <option value="">Choose…</option>
              {available.map((entry) => (
                <option key={entry.key} value={entry.key}>
                  {entry.key}
                </option>
              ))}
            </select>
          </label>
          <label className="text-[12px]">
            <span className="mb-1 block text-muted">Model</span>
            <select
              value={modelId}
              onChange={(event) => setModelId(event.target.value)}
              className="rounded border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
            >
              <option value="">Choose…</option>
              {models
                .filter((model) => model.enabled)
                .map((model) => (
                  <option key={model.id} value={model.id}>
                    {model.provider_name}/{model.model_key}
                  </option>
                ))}
            </select>
          </label>
          <button
            type="button"
            onClick={save}
            disabled={busy || !feature || !modelId}
            className="inline-flex items-center gap-1 rounded bg-accent px-2.5 py-1.5 text-[12.5px] text-accent-fg disabled:opacity-40"
          >
            {busy ? <Loader2 size={13} className="animate-spin" /> : <Plus size={13} />}
            Add override
          </button>
        </div>
      ) : null}

      {error ? (
        <p data-routing-override-error className="mt-2 text-[12.5px] text-danger">
          {error}
        </p>
      ) : null}
    </section>
  );
}

/**
 * The `/ai/routing` screen.
 *
 * The write power is asked for rather than assumed: `ai.settings.manage` is a separate key from
 * the `ai.providers.read` the map itself needs, and a reader must see the same map with the
 * controls **absent** rather than disabled. A disabled "Save" is a promise the API will refuse,
 * and an operator who presses it learns the permission model by hitting it.
 */
export function AiRoutingScreen({ models }: { models: AiModel[] }) {
  const [scope, setScope] = useState<AiRoutingScope>({ kind: "installation" });
  const [routing, setRouting] = useState<AiRouting | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [canWrite, setCanWrite] = useState(false);
  const [token, setToken] = useState(0);

  // The permission is read once per screen mount, and a failure to read it is not an error
  // banner: it means "assume no write power", which renders the same map read-only rather than
  // hiding it. A panel that disappears because a permissions call failed is worse than one that
  // is briefly read-only.
  useEffect(() => {
    let cancelled = false;
    fetchEffectivePermissions({})
      .then((effective) => {
        if (!cancelled) {
          setCanWrite(
            effective.granted.some((entry) => entry.key === "ai.settings.manage"),
          );
        }
      })
      .catch(() => {
        if (!cancelled) setCanWrite(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    setError(null);
    setRouting(null);

    fetchAiRouting(scope)
      .then((found) => {
        if (!cancelled) setRouting(found);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setError(
          cause instanceof ApiError
            ? cause.message
            : "The routing map could not be read.",
        );
      });

    return () => {
      cancelled = true;
    };
  }, [scope, token]);

  const reload = useCallback(() => setToken((current) => current + 1), []);

  const saveChain = async (
    row: AiTaskRoute,
    candidates: { modelId: string; requirements: string[] }[],
  ) => {
    setError(null);
    setNotice(null);
    try {
      await putAiTaskMap(scope, row.task, candidates);
      setNotice(
        `The ${row.task} chain now starts with ${candidates.length} candidate${
          candidates.length === 1 ? "" : "s"
        }.`,
      );
      reload();
    } catch (cause: unknown) {
      // The refusal is the feature: the endpoint names the task, the candidate and the
      // requirement, and showing it verbatim is what tells the operator which chip to remove.
      setError(
        cause instanceof ApiError ? cause.message : `The ${row.task} chain was refused.`,
      );
    }
  };

  const clearChain = async (row: AiTaskRoute) => {
    setError(null);
    setNotice(null);
    try {
      await putAiTaskMap(scope, row.task, []);
      setNotice(`The ${row.task} chain was reset; it now inherits.`);
      reload();
    } catch (cause: unknown) {
      setError(
        cause instanceof ApiError ? cause.message : `The ${row.task} chain was not reset.`,
      );
    }
  };

  return (
    <div data-routing-screen className="space-y-4">
      <header className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h2 className="text-[15px] font-medium">Routing</h2>
          <p className="mt-0.5 text-[12.5px] text-muted">
            Which model answers each task, at the scope you are editing.
          </p>
        </div>

        <label className="text-[12px]">
          <span className="mb-1 block text-muted">Scope</span>
          <select
            value={
              scope.kind === "installation"
                ? "installation"
                : scope.kind === "organization"
                  ? `organization:${scope.organizationId}`
                  : `site:${scope.siteId}`
            }
            onChange={(event) => {
              const value = event.target.value;
              if (value === "installation") {
                setScope({ kind: "installation" });
              } else if (value.startsWith("organization:")) {
                setScope({ kind: "organization", organizationId: value.slice(13) });
              } else {
                setScope({ kind: "site", siteId: value.slice(5) });
              }
            }}
            className="rounded border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
          >
            <option value="installation">Installation</option>
          </select>
        </label>
      </header>

      {error ? (
        <div
          data-routing-error
          role="alert"
          className="flex items-start justify-between gap-3 rounded-lg border border-danger/40 bg-danger/5 px-3 py-2"
        >
          <p className="text-[12.5px] text-danger">{error}</p>
          <button
            type="button"
            aria-label="Retry reading the routing map"
            onClick={reload}
            className="shrink-0 rounded p-1 text-danger hover:bg-raised"
          >
            <RotateCcw size={13} />
          </button>
        </div>
      ) : null}

      {notice ? (
        <p data-routing-notice className="text-[12.5px] text-success">
          {notice}
        </p>
      ) : null}

      {!routing ? (
        !error && <LoadingTable columns={4} rows={3} />
      ) : routing.unresolved.length > 0 ? (
        <div
          data-routing-warning
          className="flex items-start gap-2 rounded-lg border border-amber-500/40 bg-amber-500/5 px-3 py-2"
        >
          <AlertTriangle size={14} className="mt-px shrink-0 text-amber-600 dark:text-amber-400" />
          <p className="text-[12.5px]">
            <span className="font-medium">Needs a primary model:</span>{" "}
            {routing.unresolved.join(", ")}. These tasks fall through to the installation
            default; nothing has failed.
          </p>
        </div>
      ) : null}

      {routing ? (
        <>
          {routing.overrides.length === 0 && routing.tasks.every((row) => row.empty) ? (
            <EmptyState
              title="No route configured yet"
              hint="Set a primary model for a task and the router will use it before the installation default."
              action={
                <p className="text-[12px] text-muted">
                  Start with the primary select on the first task below.
                </p>
              }
            />
          ) : null}

          <div className="grid gap-3 lg:grid-cols-2">
            {routing.tasks.map((row) => (
              <TaskRow
                key={row.task}
                row={row}
                models={models}
                requirements={routing.requirements}
                canWrite={canWrite}
                onSave={(candidates) => saveChain(row, candidates)}
                onClear={() => clearChain(row)}
              />
            ))}
          </div>

          <PreviewPanel routing={routing} scope={scope} />

          <OverridesTable
            routing={routing}
            models={models}
            scope={scope}
            canWrite={canWrite}
            onChanged={reload}
          />
        </>
      ) : null}
    </div>
  );
}
