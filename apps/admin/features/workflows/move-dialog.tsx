"use client";

/**
 * `MoveDialog` — `Move to project…` for one workflow (REQ-133, slice 3).
 *
 * The dialog is where the store's report becomes something a person reads, and the whole screen
 * exists because of a specific asymmetry in the API: a dry run and a real move are **the same
 * request** with a flag, so the report on screen is the report the move was decided on. Anything
 * that recomputes the answer here would reintroduce the gap the flag removes.
 *
 * Four rules the screen keeps:
 *
 * 1. **`unchecked` is rendered, not folded away.** The store reports the dependency kinds it
 *    *cannot* detect on this branch — credential references, sub-workflow calls, webhook
 *    subscriptions, published routes, templates. A dialog that showed only what was found and
 *    said nothing else would read as "this workflow has no dependencies", which is a claim the
 *    server never made. The unchecked list is shown under the found ones, in the same panel, with
 *    a sentence saying why they are there.
 * 2. **A refusal disables the button and keeps the reason on screen.** The store's `refuses` is a
 *    fact about the target, not about the network, so there is nothing to retry — a retry button
 *    on a refusal teaches people that refusals are transient.
 * 3. **An archived target is filtered out of the picker** rather than offered and refused. The API
 *    refuses it by name, so a picker that offered it would be offering an action that always ends
 *    in an error; the refusal path is still reachable by a stale dialog and is still asserted.
 * 4. **The destination is chosen by key and name, never by id alone**, because "moved to
 *    `8f3a…`" is not a sentence anybody can check.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { ArrowRight, Loader2, ShieldAlert, TriangleAlert, X } from "lucide-react";

import {
  ApiError,
  fetchProjects,
  moveWorkflow,
} from "@/lib/api";
import type { MoveDependency, MoveReport, Project } from "@/lib/types";

/** One dependency in a sentence, or `null` for a kind this build does not know. */
function describe(dependency: MoveDependency): string | null {
  switch (dependency.kind) {
    case "run_history":
      return `${dependency.executions} past ${dependency.executions === 1 ? "run" : "runs"} follow the workflow into the new project`;
    case "step_history":
      return `${dependency.steps} recorded ${dependency.steps === 1 ? "step" : "steps"} follow it`;
    case "schedule_cursor":
      return dependency.next_run_at
        ? `its schedule “${dependency.schedule}” carries over, next fire ${new Date(dependency.next_run_at).toLocaleString()}`
        : `its schedule “${dependency.schedule}” carries over`;
    case "source_audit_history":
      return `${dependency.rows} audit ${dependency.rows === 1 ? "row stays" : "rows stay"} in the old project's history — an audit trail does not move`;
    default:
      // A kind the store added and this build has not learned to describe. Rendering it by name
      // is the whole point: a silently dropped row is the failure the report exists to prevent.
      return `a ${(dependency as { kind: string }).kind} dependency this build does not describe yet`;
  }
}

export function MoveDialog({
  workflowId,
  workflowName,
  organizationId,
  onClose,
  onMoved,
}: {
  workflowId: string;
  workflowName: string;
  organizationId?: string;
  onClose: () => void;
  onMoved: () => void;
}) {
  const [projects, setProjects] = useState<Project[]>([]);
  const [target, setTarget] = useState("");
  const [report, setReport] = useState<MoveReport | null>(null);
  const [loading, setLoading] = useState(false);
  const [moving, setMoving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const panel = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const visible = await fetchProjects(organizationId);
        if (cancelled) return;
        // An archived project is filtered here rather than offered-and-refused: the store refuses
        // a move into one by name, and offering an action that always ends in an error is a
        // dead button wearing a dropdown.
        setProjects(visible.filter((project) => project.status !== "archived"));
      } catch (cause) {
        if (!cancelled) {
          setError(cause instanceof ApiError ? cause.message : "the projects could not be read");
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [organizationId]);

  // Escape closes, and the dialog takes focus on open so the keyboard path works without a
  // click. Nothing here is a "click outside" affordance: the dialog is a form with a Cancel button.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    panel.current?.focus();
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  // Re-report whenever the destination changes, and drop a report that belongs to the old one.
  const reportFor = useCallback(
    async (next: string) => {
      setTarget(next);
      setReport(null);
      setError(null);
      if (!next) return;
      setLoading(true);
      try {
        setReport(await moveWorkflow(workflowId, next, true, organizationId));
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : "the move could not be prepared");
      } finally {
        setLoading(false);
      }
    },
    [workflowId, organizationId],
  );

  const apply = async () => {
    if (!target || !report || report.refuses) return;
    setMoving(true);
    setError(null);
    try {
      await moveWorkflow(workflowId, target, false, organizationId);
      onMoved();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "the workflow could not be moved");
    } finally {
      setMoving(false);
    }
  };

  const described = report ? report.dependencies.map(describe) : [];
  const blocked = report?.refuses ?? false;

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4 pt-[8vh]"
      data-move-dialog
    >
      <div
        ref={panel}
        role="dialog"
        aria-modal="true"
        aria-label={`Move ${workflowName} to another project`}
        tabIndex={-1}
        className="flex w-full max-w-lg flex-col gap-3 rounded-xl border border-line bg-panel p-4 shadow-xl outline-none"
      >
        <div className="flex items-start gap-2">
          <h2 className="flex-1 text-[14px] font-medium">Move {workflowName}</h2>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            data-move-close
            className="rounded-lg border border-line p-1 text-muted hover:text-ink"
          >
            <X className="size-3.5" aria-hidden />
          </button>
        </div>

        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] text-muted">Destination project</span>
          <select
            value={target}
            disabled={projects.length === 0}
            data-move-target
            onChange={(event) => void reportFor(event.target.value)}
            className="rounded-lg border border-line bg-panel px-2.5 py-1.5 text-[13px] outline-none focus:border-accent disabled:bg-quiet-soft"
          >
            <option value="">
              {projects.length === 0 ? "No other project is available" : "Choose a project…"}
            </option>
            {projects.map((project) => (
              <option key={project.id} value={project.id}>
                {project.key} — {project.name}
                {project.is_default ? " (default)" : ""}
              </option>
            ))}
          </select>
        </label>

        {projects.length === 0 ? (
          <p className="text-[12px] text-muted">
            Archived projects are not offered as a destination — a move into one is refused by
            the API, so the picker leaves it out rather than offering an action that always fails.
          </p>
        ) : null}

        {error ? (
          <div
            role="alert"
            data-move-error
            className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2 text-[12px]"
          >
            <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
            <span className="flex-1">{error}</span>
          </div>
        ) : null}

        {loading ? (
          <p className="flex items-center gap-1.5 text-[12px] text-muted" data-move-loading>
            <Loader2 className="size-3.5 animate-spin" aria-hidden />
            Checking what this workflow carries…
          </p>
        ) : null}

        {report ? (
          <section className="flex flex-col gap-2 rounded-lg border border-line bg-quiet-soft p-3">
            <p className="flex items-center gap-1.5 text-[12px]" data-move-route>
              <span className="tag">{report.from_project_key}</span>
              <ArrowRight className="size-3 text-muted" aria-hidden />
              <span className="tag">{report.to_project_key}</span>
            </p>

            {blocked ? (
              <p
                data-move-refusal
                className="flex items-start gap-2 rounded-lg border border-amber-500/40 bg-amber-500/5 px-2.5 py-2 text-[12px]"
              >
                <ShieldAlert className="mt-0.5 size-3.5 shrink-0 text-amber-600" aria-hidden />
                <span>{report.reason ?? "A dependency of this workflow lives outside the target project."}</span>
              </p>
            ) : null}

            {described.length > 0 ? (
              <ul className="flex list-disc flex-col gap-1 pl-4 text-[12px]" data-move-dependencies>
                {described.map((line, index) => (
                  <li key={`${report.dependencies[index]?.kind}-${index}`}>{line}</li>
                ))}
              </ul>
            ) : (
              <p className="text-[12px] text-muted">This workflow carries nothing with it.</p>
            )}

            {report.unchecked.length > 0 ? (
              <div className="flex flex-col gap-1 border-t border-line pt-2" data-move-unchecked>
                <p className="text-[11.5px] text-muted">
                  Not checked on this build — nothing in this platform can hold these yet, so the
                  report makes no claim about them:
                </p>
                <p className="text-[11.5px] text-muted">{report.unchecked.join(", ")}</p>
              </div>
            ) : null}
          </section>
        ) : null}

        <div className="flex items-center gap-2">
          <button
            type="button"
            data-move-confirm
            disabled={!report || blocked || moving || loading}
            onClick={() => void apply()}
            className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent px-2.5 py-1.5 text-[12.5px] text-white disabled:opacity-60"
          >
            {moving ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
            Move
          </button>
          <button
            type="button"
            onClick={onClose}
            className="rounded-lg border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            Cancel
          </button>
          {blocked ? (
            <span className="text-[11.5px] text-muted">
              Resolve the dependency above first — this is not a transient error, so there is
              nothing to retry.
            </span>
          ) : null}
        </div>
      </div>
    </div>
  );
}
