"use client";

/**
 * `/ai/change-sets/[id]` — the change-set editor (REQ-101, slice 3f).
 *
 * The approval review screen answers "may this one tool call run?" and shows a **frozen**
 * preview, because that is the right thing to review. This screen answers a different question
 * — "a conversation proposed four operations, what exactly are they?" — and here the diff is
 * **live**, because the whole point of the editor is to change it.
 *
 * Three decisions shape it, and each of them closes a specific hole:
 *
 * 1. **The diff is re-planned by the server, never derived in the browser.** Until this screen
 *    the sheet recomputed the OLD column from the operation's own `args`, which is the
 *    *proposal's* idea of "before" — a value nobody read from the database. A page somebody
 *    renamed after the set was filed would show the reviewer one "before" while the apply
 *    wrote through the server's own plan read from the row. `POST /ai/change-sets/{id}/preview`
 *    is the same [`target::preview`] the apply calls, so the sheet and the write cannot
 *    disagree. A browser cannot compute a cascade count at all, which is why the delete card's
 *    "1 page, 3 revisions" line could only ever have been a client-side guess.
 *
 * 2. **Re-planning writes nothing.** The response carries the stored `base_revisions` and the
 *    server reports `drifted` rather than quietly re-pinning them: a preview that refreshed the
 *    pins would retire the staleness the confirm route enforces, and the reviewer would stop
 *    being asked to look at a target that had moved. The drift banner is the visible half of
 *    that, and Confirm is disabled while it is up.
 *
 * 3. **The hash travels with every edit.** `content_hash` is what the server compares the
 *    editor against (`409 content_moved` naming both hashes), and it is why an edit is
 *    optimistic rather than last-write-wins. After a save the screen re-reads the answer
 *    rather than guessing the new hash — the client cannot compute a digest it does not own.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useParams, useRouter } from "next/navigation";

import {
  ArrowDown,
  ArrowUp,
  Check,
  Loader2,
  Pencil,
  RefreshCw,
  Trash2,
  TriangleAlert,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiChangeOp,
  type AiChangeSetList,
  type AiPlannedOp,
  type AiRePreviewed,
  confirmAiChangeSet,
  discardAiChangeSet,
  fetchAiChangeSets,
  replanAiChangeSet,
  updateAiChangeSet,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The key a person needs to confirm or discard. Named in every disabled control. */
const DECIDE_KEY = "ai.approvals.act";

/** Risk is never carried by colour alone — each level has its own word. */
const STATUS_TONE: Record<string, string> = {
  draft: "bg-accent-soft text-accent-strong",
  pending: "bg-caution-soft text-caution",
  confirmed: "bg-positive-soft text-positive",
  applied: "bg-positive-soft text-positive",
  discarded: "bg-quiet-soft text-muted",
  failed: "bg-danger-soft text-danger",
};

const ACTION_TONE: Record<string, string> = {
  create: "bg-positive-soft text-positive",
  update: "bg-accent-soft text-accent-strong",
  delete: "bg-danger-soft text-danger",
};

/** A value rendered as text. Long values wrap; nothing is silently truncated. */
function valueText(value: unknown): string {
  if (value === null || value === undefined) return "—";
  if (typeof value === "string") return value.length ? value : "(empty)";
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return JSON.stringify(value);
}

/** The operation's own key, used to match a stored op to its resolved plan. */
function planFor(planned: AiPlannedOp[], key: string): AiPlannedOp | undefined {
  return planned.find((op) => op.key === key);
}

/**
 * One operation card: the resolved diff, and the editor when the set is editable.
 *
 * The `diffs` come from the server's plan, so OLD is the value on the target *now*. The inputs
 * edit the `args` the set stores — not the diff — because the diff is the server's answer about
 * a target the browser cannot read, and typing into it would produce a value the apply never
 * sees. The re-plan that follows a save is what turns those arguments into the next diff.
 */
function OperationCard({
  op,
  plan,
  index,
  total,
  editable,
  editing,
  draft,
  onDraftChange,
  showUnchanged,
  onToggleUnchanged,
  onMove,
  onDrop,
}: {
  op: AiChangeOp;
  plan: AiPlannedOp | undefined;
  index: number;
  total: number;
  editable: boolean;
  editing: boolean;
  draft: Record<string, string>;
  showUnchanged: boolean;
  onDraftChange: (key: string, field: string, value: string) => void;
  onToggleUnchanged: () => void;
  onMove: (from: number, to: number) => void;
  onDrop: () => void;
}) {
  const diffs = plan?.diffs ?? [];
  const changed = diffs.filter((field) => valueText(field.before) !== valueText(field.after));
  const unchanged = diffs.length - changed.length;
  const rows = showUnchanged ? diffs : changed;

  return (
    <article
      data-set-operation={op.key}
      data-set-action={op.kind}
      data-set-noop={plan?.no_op ? "true" : "false"}
      className="flex flex-col gap-2 rounded-lg border border-line bg-surface p-3"
    >
      <header className="flex flex-wrap items-center gap-2">
        <span
          data-set-action-badge
          className={`inline-flex items-center rounded-full px-2 py-0.5 text-[10.5px] font-medium ${
            ACTION_TONE[op.kind] ?? "bg-quiet-soft text-muted"
          }`}
        >
          {op.kind}
        </span>
        <span className="font-mono text-[12px] break-all">
          {op.resource_type}
          {op.resource_id ? ` · ${op.resource_id}` : ""}
        </span>
        {plan?.label ? (
          <span data-set-label className="text-[12px] text-muted">
            {plan.label}
          </span>
        ) : null}
        {plan?.gated_class ? (
          <span
            data-set-gated={plan.gated_class}
            className="ml-auto inline-flex items-center gap-1 rounded-full bg-caution-soft px-2 py-0.5 text-[10.5px] font-medium text-caution"
          >
            needs approval
          </span>
        ) : null}
      </header>

      {plan?.cascades?.length ? (
        <p data-set-cascades className="rounded-lg bg-danger-soft px-2.5 py-1.5 text-[12px] text-danger">
          This also removes: {plan.cascades.join(", ")}. This cannot be undone from the panel.
        </p>
      ) : null}

      {plan?.no_op ? (
        <p data-set-noop-note className="rounded-lg bg-caution-soft px-2.5 py-1.5 text-[12px] text-caution">
          This operation would write nothing — the target already carries these values. Drop it
          if you did not mean to keep it.
        </p>
      ) : null}

      {!plan ? (
        <p data-set-unresolved className="text-[12px] text-danger">
          This operation could not be resolved against its target, so its diff is not shown. The
          server refused it when the preview ran.
        </p>
      ) : rows.length === 0 ? (
        <p data-set-no-changes className="text-[12px] text-muted">
          {diffs.length === 0
            ? op.kind === "delete"
              ? "Nothing is written — the diff is the target going away."
              : "The previewed operation names no field."
            : "Every field is unchanged — nothing would be written."}
        </p>
      ) : (
        <ul className="flex flex-col gap-1.5">
          {rows.map((field) => {
            const isChanged = valueText(field.before) !== valueText(field.after);
            return (
              <li
                key={field.field}
                data-set-field={field.field}
                data-set-changed={isChanged ? "true" : "false"}
                className={`rounded-lg border px-2.5 py-1.5 ${
                  isChanged ? "border-accent/40 bg-accent-soft/40" : "border-line bg-canvas"
                }`}
              >
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-mono text-[11.5px]">{field.field}</span>
                  {isChanged ? (
                    <span data-set-changed-marker className="text-[10.5px] font-medium text-accent-strong">
                      changed
                    </span>
                  ) : null}
                </div>
                <div className="mt-1 grid gap-1.5 text-[12px] sm:grid-cols-2">
                  <div data-set-old className="rounded border border-line bg-canvas px-2 py-1">
                    <span className="block text-[10.5px] uppercase tracking-wide text-muted">Old</span>
                    <span className="break-words whitespace-pre-wrap">{valueText(field.before)}</span>
                  </div>
                  <div data-set-new className="rounded border border-line bg-canvas px-2 py-1">
                    <span className="block text-[10.5px] uppercase tracking-wide text-muted">New</span>
                    <span className="break-words whitespace-pre-wrap">{valueText(field.after)}</span>
                  </div>
                </div>
              </li>
            );
          })}
        </ul>
      )}

      {unchanged > 0 ? (
        <button
          type="button"
          onClick={onToggleUnchanged}
          data-set-toggle-unchanged
          className="self-start rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas"
        >
          {showUnchanged ? "Hide unchanged" : `Show unchanged (${unchanged})`}
        </button>
      ) : null}

      {editable && editing ? (
        <div data-set-edit-fields className="flex flex-col gap-1.5 border-t border-line pt-2">
          {Object.keys(op.args ?? {}).map((field) => (
            <label key={field} className="flex flex-col gap-1 text-[11.5px]">
              <span className="font-mono text-muted">{field}</span>
              <input
                data-set-edit-input={field}
                value={draft[field] ?? ""}
                onChange={(event) => onDraftChange(op.key, field, event.target.value)}
                className="rounded-lg border border-line bg-canvas px-2 py-1 text-[12px]"
              />
            </label>
          ))}
        </div>
      ) : null}

      {editable ? (
        <div data-set-operation-actions className="flex flex-wrap items-center gap-1.5">
          <button
            type="button"
            onClick={() => onMove(index, index - 1)}
            disabled={index === 0}
            data-set-move-up={op.key}
            aria-label="Move this operation up"
            className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-40"
          >
            <ArrowUp className="h-3 w-3" /> Up
          </button>
          <button
            type="button"
            onClick={() => onMove(index, index + 1)}
            disabled={index === total - 1}
            data-set-move-down={op.key}
            aria-label="Move this operation down"
            className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-40"
          >
            <ArrowDown className="h-3 w-3" /> Down
          </button>
          <button
            type="button"
            onClick={onDrop}
            data-set-drop={op.key}
            aria-label="Drop this operation"
            className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-danger transition hover:bg-danger-soft"
          >
            <Trash2 className="h-3 w-3" /> Drop
          </button>
        </div>
      ) : null}
    </article>
  );
}

/** The change-set editor. */
export function AiChangeSetEditorScreen() {
  const params = useParams<{ id: string }>();
  const router = useRouter();
  const id = params.id;

  const [data, setData] = useState<AiRePreviewed | null>(null);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<Record<string, Record<string, string>>>({});
  const [showUnchanged, setShowUnchanged] = useState<Record<string, boolean>>({});
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [missing, setMissing] = useState<string[]>([]);
  const [confirmation, setConfirmation] = useState("");
  const [discardReason, setDiscardReason] = useState("");
  const [showDiscard, setShowDiscard] = useState(false);

  const load = useCallback(() => {
    setError(null);
    replanAiChangeSet(id)
      .then((result) => {
        setData(result);
        setDraft({});
      })
      .catch((cause: unknown) => {
        setData(null);
        setError(cause instanceof ApiError ? cause.message : "This change set could not be loaded.");
      });
  }, [id]);

  useEffect(load, [load]);

  // The set list carries the viewer's decision keys, and this screen must agree with it: a
  // Confirm the list called disabled that is live here would be the API refusing in the one
  // place the reviewer expects to act.
  useEffect(() => {
    let cancelled = false;
    fetchAiChangeSets({ status: "all", limit: 1 })
      .then((list: AiChangeSetList) => {
        if (!cancelled) setMissing(list.viewer_missing);
      })
      .catch(() => {
        // A failure here must not take the decision away from somebody who has it: the API is
        // the authority, and treating "unknown" as "forbidden" would be the wrong direction.
        if (!cancelled) setMissing([]);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const canDecide = !missing.includes(DECIDE_KEY);
  const editable = data?.editable ?? false;
  const irreversible = data?.irreversible ?? false;
  const drifted = data?.drifted ?? [];

  /** The operations the editor currently holds: the saved list with the draft's edits. */
  const currentOps = useMemo(() => {
    if (!data) return [];
    return data.operations.map((op) => {
      const patch = draft[op.key];
      if (!patch) return op;
      const args = { ...(op.args ?? {}) };
      for (const [field, value] of Object.entries(patch)) args[field] = value;
      return { ...op, args };
    });
  }, [data, draft]);

  const move = (from: number, to: number) => {
    if (!data) return;
    const next = [...currentOps];
    const [taken] = next.splice(from, 1);
    next.splice(to, 0, taken);
    save(next);
  };

  const drop = (key: string) => {
    const next = currentOps.filter((op) => op.key !== key);
    // The API refuses an empty list, and the button that would produce one is a dead control.
    if (next.length === 0) {
      setError("A change set needs at least one operation. Discard the set instead of emptying it.");
      return;
    }
    save(next);
  };

  /**
   * Save the whole list, with the hash the screen was looking at.
   *
   * The hash is what makes this optimistic: a `409 content_moved` means somebody else saved
   * while this editor was open, and the answer names both so the reviewer can tell which
   * version they are looking at instead of silently overwriting a stranger's edit.
   */
  const save = async (operations: AiChangeOp[]) => {
    if (!data) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const saved = await updateAiChangeSet(data.id, {
        title: data.title,
        operations,
        baseRevisions: data.base_revisions,
        baseContentHash: data.content_hash,
      });
      setNotice(
        `Saved. The set is now hashed ${saved.content_hash.slice(0, 12)}, and the diff below is re-planned against the targets as they are now.`,
      );
      setEditing(false);
      load();
    } catch (cause: unknown) {
      setError(
        cause instanceof ApiError
          ? cause.message
          : "The change set could not be saved.",
      );
    } finally {
      setBusy(false);
    }
  };

  const confirm = async () => {
    if (!data) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const result = await confirmAiChangeSet(data.id, confirmation);
      if (result.needs_approval) {
        setNotice(
          `Parked. ${result.approvals.length} operation(s) need a second person — the set is in the approval inbox now.`,
        );
        router.push("/ai/approvals");
      } else {
        setNotice("Confirmed and applied. Nothing in this set needed a second person.");
        load();
      }
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The set could not be confirmed.");
    } finally {
      setBusy(false);
    }
  };

  const discard = async () => {
    if (!data) return;
    setBusy(true);
    setError(null);
    try {
      await discardAiChangeSet(data.id, discardReason.trim());
      router.push("/ai/change-sets");
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The set could not be discarded.");
    } finally {
      setBusy(false);
    }
  };

  if (error && !data) {
    return (
      <div data-set-error className="flex flex-col gap-3">
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

  if (!data) return <LoadingTable columns={4} rows={3} />;

  const savedOps = data.operations;

  return (
    <div data-set-editor className="flex flex-col gap-4">
      <header className="flex flex-wrap items-start gap-3">
        <div className="min-w-0 flex-1">
          <h1 className="text-[17px] font-semibold break-words">{data.title}</h1>
          <p className="mt-1 flex flex-wrap items-center gap-2 text-[12px] text-muted">
            <span
              data-set-status={data.status}
              className={`inline-flex items-center rounded-full px-2 py-0.5 text-[10.5px] font-medium ${
                STATUS_TONE[data.status] ?? "bg-quiet-soft text-muted"
              }`}
            >
              {data.status}
            </span>
            <span data-set-hash className="font-mono" title="the set's content hash">
              {data.content_hash.slice(0, 12)}
            </span>
            <span>filed {formatTimestamp(data.created_at)}</span>
            {data.updated_by ? <span>edited by {data.updated_by.slice(0, 8)}</span> : null}
            {data.created_by_run ? (
              <Link
                href={`/ai/runs/${data.created_by_run}`}
                data-set-run-link
                className="underline underline-offset-2"
              >
                the run that proposed it
              </Link>
            ) : null}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            onClick={load}
            data-set-replan
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] transition hover:bg-canvas"
          >
            <RefreshCw className="h-3.5 w-3.5" /> Re-plan
          </button>
          {editable ? (
            <button
              type="button"
              onClick={() => setEditing((value) => !value)}
              data-set-toggle-edit
              aria-pressed={editing}
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] transition hover:bg-canvas"
            >
              {editing ? <X className="h-3.5 w-3.5" /> : <Pencil className="h-3.5 w-3.5" />}
              {editing ? "Stop editing" : "Edit operations"}
            </button>
          ) : null}
        </div>
      </header>

      {error ? (
        <p data-set-inline-error className="rounded-lg bg-danger-soft px-3 py-2 text-[13px] text-danger">
          {error}
        </p>
      ) : null}
      {notice ? (
        <p data-set-notice className="rounded-lg bg-positive-soft px-3 py-2 text-[13px] text-positive">
          {notice}
        </p>
      ) : null}

      {drifted.length > 0 ? (
        <div
          data-set-drift-banner
          className="flex flex-col gap-2 rounded-lg border border-caution/40 bg-caution-soft px-3 py-2"
        >
          <p className="flex items-center gap-2 text-[13px] text-caution">
            <TriangleAlert className="h-4 w-4 shrink-0" />
            <span>
              {drifted.length} target(s) changed since this set was proposed: {drifted.join(", ")}.
              The diff below is re-planned against the current values, but confirming is
              refused until the set is re-filed.
            </span>
          </p>
        </div>
      ) : null}

      {data.needs_approval ? (
        <p
          data-set-gate-note
          className="rounded-lg border border-caution/40 bg-canvas px-3 py-2 text-[12.5px]"
        >
          Confirming this set files an approval for each gated operation, and a second person
          decides it in the inbox.
        </p>
      ) : null}

      {currentOps.length === 0 ? (
        <EmptyState
          title="No operations left"
          hint="Discard the set instead of leaving an empty proposal."
        />
      ) : (
        <div className="flex flex-col gap-3">
          {currentOps.map((op, index) => (
            <OperationCard
              key={op.key}
              op={op}
              plan={planFor(data.planned, op.key)}
              index={index}
              total={currentOps.length}
              editable={editable}
              editing={editing}
              draft={draft[op.key] ?? {}}
              showUnchanged={showUnchanged[op.key] ?? false}
              onDraftChange={(key, field, value) =>
                setDraft((state) => ({
                  ...state,
                  [key]: { ...(state[key] ?? {}), [field]: value },
                }))
              }
              onToggleUnchanged={() =>
                setShowUnchanged((state) => ({ ...state, [op.key]: !state[op.key] }))
              }
              onMove={move}
              onDrop={() => drop(op.key)}
            />
          ))}
        </div>
      )}

      {editing && currentOps.length > 0 ? (
        <div data-set-save-bar className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            disabled={busy}
            onClick={() => save(currentOps)}
            data-set-save
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition disabled:opacity-50"
          >
            {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Check className="h-3.5 w-3.5" />}
            Save operations
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() => {
              setDraft({});
              setEditing(false);
              setError(null);
            }}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
          >
            Discard edits
          </button>
          <span className="text-[12px] text-muted">
            Saving records you as the editor, re-hashes the set and re-plans every diff.
          </span>
        </div>
      ) : null}

      {irreversible ? (
        <div data-set-danger className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2.5">
          <p className="flex items-center gap-2 text-[13px] font-medium text-danger">
            <TriangleAlert className="h-4 w-4 shrink-0" />
            <span>
              This set deletes content. Deleting a page takes its revisions with it, and the
              panel cannot undo that.
            </span>
          </p>
          {data.confirmation_phrase ? (
            <label className="mt-2 flex flex-col gap-1 text-[12px]">
              <span>
                Type <span className="font-mono">{data.confirmation_phrase}</span> to confirm
              </span>
              <input
                data-set-confirmation
                value={confirmation}
                onChange={(event) => setConfirmation(event.target.value)}
                className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
              />
            </label>
          ) : null}
        </div>
      ) : null}

      {editable ? (
        <div
          data-set-decision-bar
          className="sticky bottom-0 flex flex-wrap items-center gap-2 border-t border-line bg-surface py-2"
        >
          <button
            type="button"
            disabled={busy || !canDecide || drifted.length > 0 || currentOps.length === 0}
            onClick={confirm}
            data-set-confirm
            title={
              !canDecide
                ? `You need ${DECIDE_KEY}`
                : drifted.length > 0
                  ? "Re-file the set against the current targets first"
                  : undefined
            }
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition disabled:opacity-50"
          >
            {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Check className="h-3.5 w-3.5" />}
            Confirm
          </button>
          <button
            type="button"
            disabled={busy || !canDecide}
            onClick={() => setShowDiscard((value) => !value)}
            data-set-discard-toggle
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-danger transition hover:bg-danger-soft disabled:opacity-50"
          >
            Discard
          </button>
          {!canDecide ? (
            <span data-set-missing className="text-[12px] text-muted">
              You need {DECIDE_KEY} to confirm or discard this set.
            </span>
          ) : null}
          {savedOps.length !== currentOps.length ? (
            <span data-set-unsaved className="text-[12px] text-caution">
              {currentOps.length} operation(s) on screen, {savedOps.length} saved — save first.
            </span>
          ) : null}
        </div>
      ) : (
        <p data-set-readonly className="rounded-lg bg-canvas px-3 py-2 text-[12.5px] text-muted">
          This set is <span className="font-medium">{data.status}</span>, so it is a record now:
          the operations cannot be edited and it cannot be confirmed again.
        </p>
      )}

      {showDiscard ? (
        <div data-set-discard-form className="flex flex-col gap-2 rounded-lg border border-line p-3">
          <label className="flex flex-col gap-1 text-[12px]">
            <span>Why is this proposal being dropped? The reason is stored on the set.</span>
            <textarea
              data-set-discard-reason
              value={discardReason}
              onChange={(event) => setDiscardReason(event.target.value)}
              rows={2}
              className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
            />
          </label>
          <button
            type="button"
            disabled={busy || discardReason.trim().length === 0}
            onClick={discard}
            data-set-discard-confirm
            className="self-start rounded-lg bg-danger px-3 py-1.5 text-[12.5px] font-medium text-white transition disabled:opacity-50"
          >
            Discard this set
          </button>
        </div>
      ) : null}
    </div>
  );
}
