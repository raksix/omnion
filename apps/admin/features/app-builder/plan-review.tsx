"use client";

/**
 * The AI app builder's review workspace (docs/requests/REQ-045, slice 2's screens).
 *
 * Generation is the easy half; this screen is the half that decides whether any of it ships.
 * It answers one question at a time — *is this artifact worth applying?* — and everything about
 * the layout follows from that: a tree of artifacts on the left grouped by kind, the focused
 * artifact's whole body on the right, and a footer that says in numbers what stands between
 * this plan and apply.
 *
 * Seven decisions, each of which could have gone the other way:
 *
 * * **The blockers are the server's, not the client's.** The plan detail carries them, and the
 *   footer renders them **by name**. A client that re-derived "is this plan ready" would have to
 *   re-implement the rule and would eventually disagree with apply — and the reviewer would be
 *   told a plan is ready that apply then refuses.
 * * **Accept is offered on every artifact and refused by name on the ones that fail.** The
 *   button is never hidden for an invalid artifact: the `422` carries the first finding, and a
 *   review screen whose only answer to "why can't I accept this?" is a disabled button teaches
 *   the reviewer that the button is broken.
 * * **Reject asks for a reason, and an empty one never leaves the browser.** The store refuses
 *   it, but a refusal that costs a round trip to learn is a screen that looks unresponsive.
 * * **Regenerate keeps the previous version visible.** The tree marks a superseded artifact and
 *   still shows its retired row, because the request asks for kept plan versions so a rejected
 *   attempt stays comparable — a tree that hid the old body would make regeneration a reroll.
 * * **Apply is absent, not disabled-and-promised.** The runner is a later slice, so there is no
 *   Apply button and no confirmation dialog for one: a button that answers "coming soon" is the
 *   exact defect the plan's Definition of Done names. The footer instead names every blocker.
 * * **The spec is rendered per kind, not as raw JSON.** An entity's field table, a UI
 *   artifact's screen list and a permission artifact's keys are each a table a reviewer can read;
 *   the raw body is still one disclosure away for a kind this screen does not know yet.
 * * **Keyboard: `j`/`k` move, `a` accept, `r` reject, `e` edit, `g` regenerate.** Every one of
 *   them has a handler, because a shortcut that silently does nothing is worse than one that is
 *   not advertised.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  Ban,
  Check,
  CheckCircle2,
  CircleDashed,
  History,
  Loader2,
  Pencil,
  RefreshCw,
  RotateCcw,
  X,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import {
  ApiError,
  acceptAppBuilderArtifact,
  editAppBuilderArtifact,
  fetchAppBuilderPlan,
  rejectAppBuilderArtifact,
  rejectAppBuilderPlan,
  streamRegenerateAppBuilderArtifact,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type {
  AppBuilderArtifact,
  AppBuilderBlocker,
  AppBuilderFinding,
  AppBuilderPlanDetail,
} from "@/lib/types";

/** The tree's groups, in the order the request lists them. */
const KIND_ORDER = [
  "entity",
  "field",
  "ui",
  "permission",
  "role",
  "workflow",
  "notification",
  "report",
] as const;

/** What a kind is called in the tree, and what one row of it means. */
const KIND_LABELS: Record<string, { label: string; hint: string }> = {
  entity: { label: "Entities", hint: "The tables the plan proposes" },
  field: { label: "Fields", hint: "The columns of each entity" },
  ui: { label: "UI", hint: "The screens a person uses" },
  permission: { label: "Permissions", hint: "The new permission keys" },
  role: { label: "Roles", hint: "The role that binds them" },
  workflow: { label: "Workflow", hint: "The rule the app runs" },
  notification: { label: "Notifications", hint: "What gets sent, and when" },
  report: { label: "Reports", hint: "The grouped table and its chart" },
};

/** Why each artifact status is drawn the way it is. */
const STATUS_LABELS: Record<string, string> = {
  pending: "Waiting for a decision",
  accepted: "Accepted",
  rejected: "Rejected",
  edited: "Edited by a reviewer",
  invalid: "The validator refused it",
};

/** The status icon, so a status is never a colour alone. */
function StatusIcon({ status }: { status: string }) {
  if (status === "accepted") return <CheckCircle2 className="size-3.5 text-positive" aria-hidden />;
  if (status === "rejected") return <Ban className="size-3.5 text-accent-strong" aria-hidden />;
  if (status === "invalid") return <AlertTriangle className="size-3.5 text-caution" aria-hidden />;
  if (status === "edited") return <Pencil className="size-3.5 text-accent" aria-hidden />;
  return <CircleDashed className="size-3.5 text-muted" aria-hidden />;
}

/** Read a string out of an untyped spec without pretending it is there. */
function text(spec: Record<string, unknown>, key: string): string {
  const value = spec[key];
  return typeof value === "string" ? value : "";
}

/** Read a list of strings out of an untyped spec; anything else is an empty list. */
function strings(spec: Record<string, unknown>, key: string): string[] {
  const value = spec[key];
  return Array.isArray(value) ? value.filter((entry): entry is string => typeof entry === "string") : [];
}

/** Read a list of objects out of an untyped spec. */
function records(spec: Record<string, unknown>, key: string): Record<string, unknown>[] {
  const value = spec[key];
  if (!Array.isArray(value)) return [];
  return value.filter(
    (entry): entry is Record<string, unknown> =>
      typeof entry === "object" && entry !== null && !Array.isArray(entry),
  );
}

/** The findings the validator stored, normalised: the API sends an array, older rows send `null`. */
function findingsOf(artifact: AppBuilderArtifact): AppBuilderFinding[] {
  return Array.isArray(artifact.validation) ? artifact.validation : [];
}

/** The entity field table the request asks for, rendered from the artifact's own spec. */
function EntityFields({ spec }: { spec: Record<string, unknown> }) {
  const fields = records(spec, "fields");
  if (fields.length === 0) {
    return <p className="text-[12.5px] text-muted">This entity proposes no field of its own.</p>;
  }
  return (
    <div className="overflow-x-auto">
      <table className="w-full border-collapse text-left text-[12.5px]">
        <thead>
          <tr className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
            <th scope="col" className="py-2 pr-3 font-medium">
              Key
            </th>
            <th scope="col" className="py-2 pr-3 font-medium">
              Label
            </th>
            <th scope="col" className="py-2 pr-3 font-medium">
              Type
            </th>
            <th scope="col" className="py-2 pr-3 font-medium">
              Required
            </th>
            <th scope="col" className="py-2 pr-3 font-medium">
              Unique
            </th>
            <th scope="col" className="py-2 font-medium">
              Default
            </th>
          </tr>
        </thead>
        <tbody>
          {fields.map((field, index) => (
            <tr key={`${text(field, "key")}-${index}`} className="border-b border-line last:border-b-0">
              <td className="py-2 pr-3 font-medium text-ink">{text(field, "key") || "—"}</td>
              <td className="py-2 pr-3">{text(field, "label") || "—"}</td>
              <td className="py-2 pr-3 text-muted">{text(field, "type") || "—"}</td>
              <td className="py-2 pr-3 text-muted">{field.required === true ? "yes" : "no"}</td>
              <td className="py-2 pr-3 text-muted">{field.unique === true ? "yes" : "no"}</td>
              <td className="py-2 text-muted">{text(field, "default") || "—"}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** The screen list a UI artifact proposes, with its columns and its form. */
function UiScreens({ spec }: { spec: Record<string, unknown> }) {
  const screens = records(spec, "screens");
  if (screens.length === 0) {
    return <p className="text-[12.5px] text-muted">This artifact names no screen.</p>;
  }
  return (
    <ul className="flex flex-col gap-2">
      {screens.map((screen, index) => {
        const columns = strings(screen, "columns");
        const fields = strings(screen, "fields");
        return (
          <li key={`${text(screen, "key")}-${index}`} className="rounded-lg border border-line p-3">
            <div className="flex flex-wrap items-baseline justify-between gap-2">
              <span className="text-[13px] font-medium text-ink">
                {text(screen, "key") || text(screen, "label") || `Screen ${index + 1}`}
              </span>
              <span className="text-[11.5px] text-muted">
                {text(screen, "label") || "list, detail and form"}
              </span>
            </div>
            {columns.length > 0 ? (
              <p className="mt-1 text-[12px] text-muted">Columns: {columns.join(", ")}</p>
            ) : null}
            {fields.length > 0 ? (
              <p className="text-[12px] text-muted">Form fields: {fields.join(", ")}</p>
            ) : null}
          </li>
        );
      })}
    </ul>
  );
}

/** The new permission keys, as the catalogue will read them once applied. */
function PermissionKeys({ spec }: { spec: Record<string, unknown> }) {
  const keys = records(spec, "keys");
  if (keys.length === 0) {
    return <p className="text-[12.5px] text-muted">This artifact proposes no key.</p>;
  }
  return (
    <ul className="flex flex-col gap-1.5">
      {keys.map((entry, index) => (
        <li key={`${text(entry, "key")}-${index}`} className="rounded-lg border border-line px-3 py-2">
          <span className="font-mono text-[12px] text-ink">{text(entry, "key") || "—"}</span>
          {text(entry, "description") ? (
            <p className="mt-0.5 text-[12px] text-muted">{text(entry, "description")}</p>
          ) : null}
        </li>
      ))}
    </ul>
  );
}

/** The step chain a workflow artifact proposes, in its own order. */
function WorkflowSteps({ spec }: { spec: Record<string, unknown> }) {
  const steps = records(spec, "steps");
  if (steps.length === 0) {
    return <p className="text-[12.5px] text-muted">This artifact proposes no step.</p>;
  }
  return (
    <ol className="flex flex-col gap-1.5">
      {steps.map((step, index) => (
        <li key={index} className="flex items-start gap-2 rounded-lg border border-line px-3 py-2">
          <span className="mt-0.5 rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
            {index + 1}
          </span>
          <span className="flex-1">
            <span className="text-[12.5px] font-medium text-ink">
              {text(step, "kind") || text(step, "label") || "step"}
            </span>
            {text(step, "label") ? (
              <span className="ml-1.5 text-[12px] text-muted">{text(step, "label")}</span>
            ) : null}
          </span>
        </li>
      ))}
    </ol>
  );
}

/**
 * The artifact body, rendered per kind.
 *
 * A JSON blob is a truth an operator cannot check, and the four renderers above are the four
 * kinds a reviewer actually reads. Anything else falls through to the raw body, which is still
 * one disclosure away — so an unknown kind is legible rather than blank.
 */
function SpecBody({ artifact }: { artifact: AppBuilderArtifact }) {
  const spec = artifact.spec ?? {};
  const detailsId = `raw-spec-${artifact.id}`;

  if (artifact.kind === "entity") return <EntityFields spec={spec} />;
  if (artifact.kind === "ui") return <UiScreens spec={spec} />;
  if (artifact.kind === "permission") return <PermissionKeys spec={spec} />;
  if (artifact.kind === "workflow") return <WorkflowSteps spec={spec} />;

  return (
    <details className="text-[12.5px]" data-raw-spec>
      <summary className="cursor-pointer text-muted">
        The raw body as generated ({Object.keys(spec).length} keys)
      </summary>
      <pre
        id={detailsId}
        className="mt-2 max-h-72 overflow-auto rounded-lg border border-line bg-canvas p-3 text-[11.5px] text-ink"
      >
        {JSON.stringify(spec, null, 2)}
      </pre>
    </details>
  );
}

/** The review workspace: a tree on the left, the focused artifact on the right, a footer below. */
export function PlanReview({ planId }: { planId: string }) {
  const [detail, setDetail] = useState<AppBuilderPlanDetail | null>(null);
  const [state, setState] = useState<"loading" | "ready" | "error">("loading");
  const [error, setError] = useState<string | null>(null);

  const [focusedId, setFocusedId] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  // The two dialogs are inline panels rather than a modal: a rejection reason and an edit body
  // are short, and a modal over a tree the reviewer is comparing against is a worse place to
  // type them than a panel that scrolls with the artifact.
  const [rejectingId, setRejectingId] = useState<string | null>(null);
  const [rejectReason, setRejectReason] = useState("");
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editBody, setEditBody] = useState("");
  const [regeneratingId, setRegeneratingId] = useState<string | null>(null);
  const [feedback, setFeedback] = useState("");

  const [rejectPlanOpen, setRejectPlanOpen] = useState(false);
  const [planRejectReason, setPlanRejectReason] = useState("");

  const treeRef = useRef<HTMLDivElement | null>(null);
  const abortRef = useRef<AbortController | null>(null);

  const load = useCallback(async () => {
    setState("loading");
    setError(null);
    try {
      const answer = await fetchAppBuilderPlan(planId);
      setDetail(answer);
      setState("ready");
      setFocusedId((current) => current ?? answer.artifacts[0]?.id ?? null);
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "That plan could not be loaded.");
      setState("error");
    }
  }, [planId]);

  useEffect(() => {
    void load();
  }, [load]);

  const artifacts = detail?.artifacts ?? [];
  const focused = useMemo(
    () => artifacts.find((artifact) => artifact.id === focusedId) ?? artifacts[0] ?? null,
    [artifacts, focusedId],
  );

  const grouped = useMemo(
    () =>
      KIND_ORDER.map((kind) => ({
        kind,
        label: KIND_LABELS[kind]?.label ?? kind,
        hint: KIND_LABELS[kind]?.hint ?? "",
        rows: artifacts.filter((artifact) => artifact.kind === kind),
      })).filter((group) => group.rows.length > 0),
    [artifacts],
  );

  /** Fold a decision answer into the screen: the artifact, the counters and the blockers. */
  const absorb = useCallback(
    (artifact: AppBuilderArtifact, counts: AppBuilderPlanDetail["counts"], blockers: AppBuilderBlocker[]) => {
      setDetail((current) =>
        current
          ? {
              ...current,
              artifacts: current.artifacts.map((row) => (row.id === artifact.id ? artifact : row)),
              counts,
              blockers,
              applicable: blockers.length === 0,
            }
          : current,
      );
      setFocusedId(artifact.id);
    },
    [],
  );

  const accept = useCallback(
    async (artifact: AppBuilderArtifact) => {
      setBusy(artifact.id);
      setActionError(null);
      try {
        const answer = await acceptAppBuilderArtifact(planId, artifact.id);
        absorb(answer.artifact, answer.counts, answer.blockers);
        setNotice(`Accepted ${artifact.kind} · ${artifact.key}`);
      } catch (cause: unknown) {
        setActionError(
          cause instanceof ApiError ? cause.message : "That artifact could not be accepted.",
        );
      } finally {
        setBusy(null);
      }
    },
    [planId, absorb],
  );

  const reject = useCallback(
    async (artifact: AppBuilderArtifact) => {
      const reason = rejectReason.trim();
      if (!reason) {
        setActionError("Say what should change — a rejection with no reason teaches nobody anything.");
        return;
      }
      setBusy(artifact.id);
      setActionError(null);
      try {
        const answer = await rejectAppBuilderArtifact(planId, artifact.id, reason);
        absorb(answer.artifact, answer.counts, answer.blockers);
        setRejectingId(null);
        setRejectReason("");
        setNotice(`Rejected ${artifact.kind} · ${artifact.key}`);
      } catch (cause: unknown) {
        setActionError(
          cause instanceof ApiError ? cause.message : "That artifact could not be rejected.",
        );
      } finally {
        setBusy(null);
      }
    },
    [planId, rejectReason, absorb],
  );

  const saveEdit = useCallback(
    async (artifact: AppBuilderArtifact) => {
      let parsed: Record<string, unknown>;
      try {
        parsed = JSON.parse(editBody) as Record<string, unknown>;
      } catch {
        setActionError("That body is not valid JSON, so it cannot be stored.");
        return;
      }
      setBusy(artifact.id);
      setActionError(null);
      try {
        const answer = await editAppBuilderArtifact(planId, artifact.id, parsed);
        absorb(answer.artifact, answer.counts, answer.blockers);
        setEditingId(null);
        setNotice(`Saved ${artifact.key} — re-validated against the same rules the generator faced`);
      } catch (cause: unknown) {
        setActionError(cause instanceof ApiError ? cause.message : "That artifact could not be saved.");
      } finally {
        setBusy(null);
      }
    },
    [planId, editBody, absorb],
  );

  const regenerate = useCallback(
    async (artifact: AppBuilderArtifact) => {
      const note = feedback.trim();
      if (!note) {
        setActionError("Say what should change — a regeneration with no note is a second guess.");
        return;
      }
      setBusy(artifact.id);
      setActionError(null);
      const controller = new AbortController();
      abortRef.current = controller;
      try {
        await streamRegenerateAppBuilderArtifact(
          planId,
          artifact.id,
          note,
          {
            onArtifact: (answer) => {
              absorb(answer.artifact, answer.counts, answer.blockers);
              setNotice(`Regenerated ${artifact.key} — the previous version is kept in the tree`);
            },
          },
          undefined,
          controller.signal,
        );
        setRegeneratingId(null);
        setFeedback("");
      } catch (cause: unknown) {
        if (!(cause instanceof DOMException && cause.name === "AbortError")) {
          setActionError(
            cause instanceof ApiError ? cause.message : "That artifact could not be regenerated.",
          );
        }
      } finally {
        setBusy(null);
        abortRef.current = null;
      }
    },
    [planId, feedback, absorb],
  );

  const rejectWholePlan = useCallback(async () => {
    const reason = planRejectReason.trim();
    if (!reason) {
      setActionError("A plan rejection needs a reason, the same way an artifact's does.");
      return;
    }
    setBusy("plan");
    setActionError(null);
    try {
      await rejectAppBuilderPlan(planId, reason);
      setRejectPlanOpen(false);
      setPlanRejectReason("");
      setNotice("The plan was rejected. The artifacts stay readable.");
      await load();
    } catch (cause: unknown) {
      setActionError(cause instanceof ApiError ? cause.message : "That plan could not be rejected.");
    } finally {
      setBusy(null);
    }
  }, [planId, planRejectReason, load]);

  // Keyboard: j/k move the selection, a/r/e/g act on it, Escape closes whatever is open.
  const onKeyDown = useCallback(
    (event: React.KeyboardEvent<HTMLDivElement>) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement;
      if (event.key === "Escape") {
        setRejectingId(null);
        setEditingId(null);
        setRegeneratingId(null);
        setRejectPlanOpen(false);
        if (!typing) return;
        return;
      }
      if (typing || event.metaKey || event.ctrlKey || event.altKey || artifacts.length === 0) return;

      const index = focused ? artifacts.findIndex((artifact) => artifact.id === focused.id) : -1;
      if (event.key === "j" || event.key === "ArrowDown") {
        event.preventDefault();
        const next = artifacts[Math.min(index + 1, artifacts.length - 1)];
        if (next) setFocusedId(next.id);
      } else if (event.key === "k" || event.key === "ArrowUp") {
        event.preventDefault();
        const next = artifacts[Math.max(index - 1, 0)];
        if (next) setFocusedId(next.id);
      } else if (!focused || busy) {
        return;
      } else if (event.key === "a") {
        event.preventDefault();
        void accept(focused);
      } else if (event.key === "r") {
        event.preventDefault();
        setRejectingId(focused.id);
      } else if (event.key === "e") {
        event.preventDefault();
        setEditBody(JSON.stringify(focused.spec, null, 2));
        setEditingId(focused.id);
      } else if (event.key === "g") {
        event.preventDefault();
        setRegeneratingId(focused.id);
      }
    },
    [artifacts, focused, busy, accept],
  );

  const plan = detail?.plan;
  const counts = detail?.counts;
  const blockers = detail?.blockers ?? [];

  if (state === "loading") {
    return (
      <div className="flex flex-col gap-4">
        <LoadingTable columns={3} rows={5} />
      </div>
    );
  }

  if (state === "error" || !detail || !plan) {
    return (
      <EmptyState
        testId="app-builder-plan-error"
        title="That plan could not be loaded"
        hint={error ?? undefined}
        action={
          <button
            type="button"
            onClick={() => void load()}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-ink transition hover:border-accent"
          >
            Retry
          </button>
        }
      />
    );
  }

  return (
    <div className="flex flex-col gap-4" onKeyDown={onKeyDown} data-plan-review={planId}>
      {actionError ? (
        <div
          role="alert"
          data-action-error
          className="flex items-start justify-between gap-3 rounded-xl border border-accent/30 bg-accent-soft px-4 py-3 text-[12.5px] text-accent-strong"
        >
          <span className="flex items-start gap-2">
            <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
            {actionError}
          </span>
          <button type="button" onClick={() => setActionError(null)} aria-label="Dismiss">
            <X className="size-3.5" aria-hidden />
          </button>
        </div>
      ) : null}
      {notice ? (
        <p data-action-notice className="text-[12.5px] text-positive">
          {notice}
        </p>
      ) : null}

      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="flex flex-col gap-1">
          <div className="flex items-center gap-2">
            <h2 className="text-[15px] font-medium text-ink">{plan.title || "Untitled plan"}</h2>
            <StatusBadge status={plan.status} />
          </div>
          <p className="text-[12px] text-muted">
            {plan.model_label ? `${plan.model_label} · ` : ""}
            created {formatTimestamp(plan.created_at)} · {plan.artifact_count} artifacts
            {plan.cost_cents > 0 ? ` · ${(plan.cost_cents / 100).toFixed(2)}` : ""}
          </p>
          {detail.decision_reason ? (
            <p data-plan-decision-reason className="text-[12px] text-caution">
              {detail.decision_reason}
            </p>
          ) : null}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Link
            href="/app-builder"
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-ink transition hover:border-accent"
          >
            Back to plans
          </Link>
          <button
            type="button"
            onClick={() => setRejectPlanOpen((open) => !open)}
            data-toggle-reject-plan
            disabled={plan.status === "applied" || busy === "plan"}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted transition hover:text-accent-strong disabled:cursor-not-allowed disabled:opacity-40"
          >
            Discard plan
          </button>
        </div>
      </div>

      {rejectPlanOpen ? (
        <div data-reject-plan-panel className="rounded-xl border border-line bg-surface p-3">
          <label htmlFor="plan-reject-reason" className="text-[12.5px] font-medium text-ink">
            Why is this plan refused?
          </label>
          <textarea
            id="plan-reject-reason"
            data-plan-reject-reason
            value={planRejectReason}
            onChange={(event) => setPlanRejectReason(event.target.value)}
            rows={2}
            className="mt-2 w-full rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink outline-none focus:border-accent"
          />
          <div className="mt-2 flex items-center gap-2">
            <button
              type="button"
              onClick={() => void rejectWholePlan()}
              disabled={busy === "plan"}
              data-confirm-reject-plan
              className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
            >
              {busy === "plan" ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
              Refuse the plan
            </button>
            <button
              type="button"
              onClick={() => setRejectPlanOpen(false)}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted"
            >
              Cancel
            </button>
          </div>
        </div>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-[minmax(0,320px)_minmax(0,1fr)]">
        {/* The tree. Below `lg` it becomes a plain list above the detail, because two columns
            at 390px is a detail pane nobody can read. */}
        <div
          ref={treeRef}
          data-artifact-tree
          className="overflow-hidden rounded-xl border border-line bg-surface"
          aria-label="Artifacts"
        >
          <div className="border-b border-line px-4 py-3">
            <h3 className="text-[13.5px] font-medium">Artifacts</h3>
            <p className="text-[11.5px] text-muted">
              j/k to move · a accept · r reject · e edit · g regenerate
            </p>
          </div>
          <div className="max-h-[28rem] overflow-y-auto">
            {grouped.map((group) => (
              <section key={group.kind} data-tree-group={group.kind} className="border-b border-line last:border-b-0">
                <div className="bg-quiet-soft/40 px-4 py-2">
                  <p className="text-[11.5px] font-medium uppercase tracking-wide text-muted">
                    {group.label}
                  </p>
                  <p className="text-[11px] text-muted">{group.hint}</p>
                </div>
                <ul>
                  {group.rows.map((artifact) => {
                    const isFocused = focused?.id === artifact.id;
                    return (
                      <li key={artifact.id}>
                        <button
                          type="button"
                          data-artifact-row={artifact.id}
                          data-artifact-status={artifact.status}
                          aria-current={isFocused}
                          onClick={() => setFocusedId(artifact.id)}
                          className={`flex w-full items-start gap-2 px-4 py-2.5 text-left transition ${
                            isFocused ? "bg-accent-soft" : "hover:bg-quiet-soft/50"
                          }`}
                        >
                          <span className="mt-0.5">
                            <StatusIcon status={artifact.status} />
                          </span>
                          <span className="flex-1">
                            <span className="block truncate text-[12.5px] text-ink">{artifact.key}</span>
                            <span className="block text-[11px] text-muted">
                              {STATUS_LABELS[artifact.status] ?? artifact.status}
                            </span>
                          </span>
                          {artifact.supersedes_id ? (
                            <History className="mt-0.5 size-3 text-muted" aria-label="Superseded a previous version" />
                          ) : null}
                        </button>
                      </li>
                    );
                  })}
                </ul>
              </section>
            ))}
          </div>
        </div>

        {/* The detail pane. */}
        <div data-artifact-detail className="rounded-xl border border-line bg-surface p-4">
          {!focused ? (
            <EmptyState
              testId="app-builder-no-artifacts"
              title="This plan has no artifacts"
              hint="Nothing was proposed, so there is nothing to review. The plan's own error, if it had one, is above."
            />
          ) : (
            <div className="flex flex-col gap-4">
              <div className="flex flex-wrap items-start justify-between gap-3">
                <div className="flex flex-col gap-1">
                  <div className="flex items-center gap-2">
                    <h3 className="text-[14px] font-medium text-ink" data-focused-key>
                      {focused.key}
                    </h3>
                    <span data-focused-status className="text-[11.5px] text-muted">
                      {STATUS_LABELS[focused.status] ?? focused.status}
                    </span>
                  </div>
                  <p className="text-[11.5px] text-muted">
                    {KIND_LABELS[focused.kind]?.label ?? focused.kind} · updated{" "}
                    {formatTimestamp(focused.updated_at)}
                    {focused.supersedes_id ? " · replaced a previous version" : null}
                  </p>
                </div>
                <div className="flex flex-wrap items-center gap-2">
                  <button
                    type="button"
                    data-accept-artifact
                    onClick={() => void accept(focused)}
                    disabled={busy === focused.id || focused.status === "accepted"}
                    className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-2.5 py-1.5 text-[12px] font-medium text-white transition hover:bg-accent-strong disabled:cursor-not-allowed disabled:opacity-40"
                  >
                    {busy === focused.id ? (
                      <Loader2 className="size-3.5 animate-spin" aria-hidden />
                    ) : (
                      <Check className="size-3.5" aria-hidden />
                    )}
                    Accept
                  </button>
                  <button
                    type="button"
                    data-reject-artifact
                    onClick={() => {
                      setRejectingId(rejectingId === focused.id ? null : focused.id);
                      setRejectReason("");
                    }}
                    className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-muted transition hover:text-accent-strong"
                  >
                    <Ban className="size-3.5" aria-hidden />
                    Reject
                  </button>
                  <button
                    type="button"
                    data-edit-artifact
                    onClick={() => {
                      setEditBody(JSON.stringify(focused.spec, null, 2));
                      setEditingId(editingId === focused.id ? null : focused.id);
                    }}
                    className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-muted transition hover:text-ink"
                  >
                    <Pencil className="size-3.5" aria-hidden />
                    Edit
                  </button>
                  <button
                    type="button"
                    data-regenerate-artifact
                    onClick={() => {
                      setRegeneratingId(regeneratingId === focused.id ? null : focused.id);
                      setFeedback("");
                    }}
                    className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-muted transition hover:text-ink"
                  >
                    <RotateCcw className="size-3.5" aria-hidden />
                    Regenerate
                  </button>
                </div>
              </div>

              {focused.rejected_reason ? (
                <p data-rejected-reason className="rounded-lg bg-quiet-soft px-3 py-2 text-[12px] text-muted">
                  Refused: {focused.rejected_reason}
                </p>
              ) : null}

              {findingsOf(focused).length > 0 ? (
                <ul data-findings className="flex flex-col gap-1">
                  {findingsOf(focused).map((finding, index) => (
                    <li
                      key={index}
                      data-finding
                      className="flex items-start gap-2 rounded-lg bg-caution-soft px-3 py-2 text-[12px] text-caution"
                    >
                      <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
                      <span>
                        {finding.path ? <code className="font-mono">{finding.path}</code> : null}
                        {finding.path ? " — " : null}
                        {finding.message}
                      </span>
                    </li>
                  ))}
                </ul>
              ) : (
                <p data-valid className="text-[12px] text-positive">
                  The validator accepted this artifact as it stands.
                </p>
              )}

              <div>
                <h4 className="text-[12px] font-medium uppercase tracking-wide text-muted">
                  {focused.kind === "entity"
                    ? "Fields"
                    : focused.kind === "ui"
                      ? "Screens"
                      : focused.kind === "permission"
                        ? "Keys"
                        : focused.kind === "workflow"
                          ? "Steps"
                          : "Body"}
                </h4>
                <div className="mt-2">
                  <SpecBody artifact={focused} />
                </div>
              </div>

              {focused.rationale ? (
                <div>
                  <h4 className="text-[12px] font-medium uppercase tracking-wide text-muted">
                    Why the model proposed this
                  </h4>
                  <p data-rationale className="mt-2 text-[12.5px] text-ink">
                    {focused.rationale}
                  </p>
                </div>
              ) : null}

              {rejectingId === focused.id ? (
                <div data-reject-panel className="rounded-lg border border-line p-3">
                  <label htmlFor="artifact-reject-reason" className="text-[12.5px] font-medium text-ink">
                    Why is this artifact refused?
                  </label>
                  <textarea
                    id="artifact-reject-reason"
                    data-reject-reason
                    value={rejectReason}
                    onChange={(event) => setRejectReason(event.target.value)}
                    rows={2}
                    placeholder="The field needs a default, or the screen has no form"
                    className="mt-2 w-full rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink outline-none focus:border-accent"
                  />
                  <div className="mt-2 flex items-center gap-2">
                    <button
                      type="button"
                      onClick={() => void reject(focused)}
                      disabled={busy === focused.id}
                      data-confirm-reject
                      className="rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
                    >
                      Refuse this artifact
                    </button>
                    <button
                      type="button"
                      onClick={() => setRejectingId(null)}
                      className="rounded-lg border border-line px-3 py-1.5 text-[12px] text-muted"
                    >
                      Cancel
                    </button>
                  </div>
                </div>
              ) : null}

              {editingId === focused.id ? (
                <div data-edit-panel className="rounded-lg border border-line p-3">
                  <label htmlFor="artifact-edit-body" className="text-[12.5px] font-medium text-ink">
                    The artifact body, as JSON
                  </label>
                  <p className="mt-1 text-[11.5px] text-muted">
                    It goes back through the same validator the generator faced, so a body that
                    breaks a key comes back `422` with the path rather than being stored.
                  </p>
                  <textarea
                    id="artifact-edit-body"
                    data-edit-body
                    value={editBody}
                    onChange={(event) => setEditBody(event.target.value)}
                    rows={10}
                    spellCheck={false}
                    className="mt-2 w-full rounded-lg border border-line bg-canvas px-3 py-2 font-mono text-[12px] text-ink outline-none focus:border-accent"
                  />
                  <div className="mt-2 flex items-center gap-2">
                    <button
                      type="button"
                      onClick={() => void saveEdit(focused)}
                      disabled={busy === focused.id}
                      data-confirm-edit
                      className="rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
                    >
                      Save and re-validate
                    </button>
                    <button
                      type="button"
                      onClick={() => setEditingId(null)}
                      className="rounded-lg border border-line px-3 py-1.5 text-[12px] text-muted"
                    >
                      Cancel
                    </button>
                  </div>
                </div>
              ) : null}

              {regeneratingId === focused.id ? (
                <div data-regenerate-panel className="rounded-lg border border-line p-3">
                  <label htmlFor="artifact-feedback" className="text-[12.5px] font-medium text-ink">
                    What should change?
                  </label>
                  <p className="mt-1 text-[11.5px] text-muted">
                    One note, one model call. The version this replaces stays in the tree, marked
                    as superseded.
                  </p>
                  <textarea
                    id="artifact-feedback"
                    data-regenerate-feedback
                    value={feedback}
                    onChange={(event) => setFeedback(event.target.value)}
                    rows={2}
                    className="mt-2 w-full rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink outline-none focus:border-accent"
                  />
                  <div className="mt-2 flex items-center gap-2">
                    <button
                      type="button"
                      onClick={() => void regenerate(focused)}
                      disabled={busy === focused.id}
                      data-confirm-regenerate
                      className="rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
                    >
                      {busy === focused.id ? (
                        <Loader2 className="size-3.5 animate-spin" aria-hidden />
                      ) : (
                        <RefreshCw className="size-3.5" aria-hidden />
                      )}
                      Ask for it again
                    </button>
                    <button
                      type="button"
                      onClick={() => setRegeneratingId(null)}
                      className="rounded-lg border border-line px-3 py-1.5 text-[12px] text-muted"
                    >
                      Cancel
                    </button>
                  </div>
                </div>
              ) : null}
            </div>
          )}
        </div>
      </div>

      {/* The footer: what stands between this plan and apply, named. */}
      <div
        data-review-footer
        className="flex flex-col gap-3 rounded-xl border border-line bg-surface px-4 py-3 lg:flex-row lg:items-center lg:justify-between"
      >
        <div className="flex flex-wrap items-center gap-4 text-[12.5px] text-muted">
          <span data-counter-accepted className="text-positive">
            {counts?.accepted ?? 0} accepted
          </span>
          <span data-counter-rejected>{counts?.rejected ?? 0} rejected</span>
          <span data-counter-pending>{counts?.pending ?? 0} pending</span>
          <span data-counter-invalid className={counts?.invalid ? "text-caution" : undefined}>
            {counts?.invalid ?? 0} invalid
          </span>
        </div>
        {blockers.length === 0 ? (
          <p data-no-blockers className="text-[12.5px] text-positive">
            Every required artifact is resolved. Apply arrives with the pipeline slice — nothing
            here applies a plan yet, so nothing pretends otherwise.
          </p>
        ) : (
          <ul data-blockers className="flex flex-wrap gap-2">
            {blockers.map((blocker, index) => (
              <li
                key={`${blocker.kind}-${blocker.key}-${index}`}
                data-blocker={blocker.status}
                title={blocker.reason}
                className="rounded-full bg-caution-soft px-2.5 py-1 text-[11.5px] text-caution"
              >
                {KIND_LABELS[blocker.kind]?.label ?? blocker.kind}
                {blocker.key ? ` · ${blocker.key}` : ""} — {blocker.status}
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}