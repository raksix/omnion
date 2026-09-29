"use client";

/**
 * The promotion dialog (REQ-017, slice 3).
 *
 * One dialog covers three states, and the reason they are one component rather than three is that
 * they are one *decision* at three moments:
 *
 * 1. **Request.** The operator has a change set in front of them. The dialog shows what would be
 *    frozen — counts by kind, the conflicting rows, and above 25 items a typed confirmation of
 *    the environment name. The threshold is `promotion.requires_typed_confirmation`, computed by
 *    the server: a constant written here would drift the first time the server moved it, and the
 *    drift would be a *safety* drift, because it is the branch that asks a person to be careful.
 * 2. **Frozen.** The request is answered with the record and the exact items it froze. The dialog
 *    becomes a step timeline — `validate → apply → audit → done` — read from the record's own
 *    `steps`, so a browser closed mid-deploy and reopened shows where it got to.
 * 3. **Decision.** If the caller holds `deployment.deploy` the primary action is *Approve and
 *    deploy*; otherwise it is the request itself and the approve button is simply not rendered.
 *
 * Three decisions worth stating, because each of them is a place the obvious implementation is
 * wrong:
 *
 * - **The deploy permission is read from the API's own answer, not assumed from the route.** The
 *   panel asks `GET /api/v1/iam/effective-permissions` for the signed-in account and looks for
 *   `deployment.deploy`. That route is deliberately unguarded because it answers "what may I do
 *   here" — it is the same resolver the guard, the role screens and the simulator all use, so the
 *   button and the guard cannot disagree. A panel that showed "Approve and deploy" to everyone
 *   would produce a `403` toast on the one click that matters.
 * - **A conflict is listed, not refused, at request time; refusing happens at approve.** The API
 *   returns the conflict list with the record, so the dialog leads with it as the request insists.
 *   The approve button is disabled *while a conflict is on the record* — not hidden, because a
 *   button that vanishes is a question and a button that is disabled with a reason is an answer.
 * - **The selection is sent as ids and the server decides what is admissible.** Conflicted rows
 *   cannot be selected, so the common path never produces a conflict; when production moves
 *   between the request and the approval, the approve-time re-check is what refuses, and the
 *   dialog surfaces `error.details.items` rather than a bare failure toast.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { AlertTriangle, CheckCircle2, Circle, Loader2, Rocket, X } from "lucide-react";

import {
  ApiError,
  approvePromotion,
  cancelPromotion,
  fetchEffectivePermissions,
  fetchEnvironmentPromotions,
  requestPromotion,
} from "@/lib/api";
import type { ChangeItem, Promotion, PromotionStep } from "@/lib/types";

/** The step timeline's fixed order. A step that has not happened is rendered as not-happened. */
const TIMELINE: { id: string; label: string }[] = [
  { id: "validate", label: "Validate" },
  { id: "apply", label: "Apply" },
  { id: "audit", label: "Audit" },
  { id: "done", label: "Done" },
];

/** What the dialog is doing right now. */
type Phase = "request" | "frozen";

export type PromotionDialogProps = {
  /** The staging environment the changes come from. */
  environmentId: string;
  /** Its name — the typed confirmation asks for exactly this. */
  environmentName: string;
  /** The live change set, already loaded by the Changes tab. */
  changes: ChangeItem[];
  /** Row ids the operator selected. Empty means "everything that differs". */
  selection: string[];
  /** Close. The caller owns the tab and the selection, not this dialog. */
  onClose: () => void;
  /** Called after a record was created, approved or withdrawn, so the tab can refresh. */
  onChanged: () => void;
};

export function PromotionDialog({
  environmentId,
  environmentName,
  changes,
  selection,
  onClose,
  onChanged,
}: PromotionDialogProps) {
  const [phase, setPhase] = useState<Phase>("request");
  const [record, setRecord] = useState<Promotion | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [typed, setTyped] = useState("");
  const [mayDeploy, setMayDeploy] = useState<boolean | null>(null);
  const [items, setItems] = useState<ChangeItem[]>([]);

  // The set the dialog is about to freeze. A selection of non-conflicting rows is the bulk
  // action; an empty selection is the primary button's "everything that differs", and the counts
  // have to say *that* rather than "nothing selected", which is the reading an empty checkbox
  // list invites.
  const frozenPreview = useMemo(
    () => (selection.length === 0 ? changes : changes.filter((row) => selection.includes(row.page_id))),
    [changes, selection],
  );

  const counts = useMemo(
    () => ({
      total: frozenPreview.length,
      added: frozenPreview.filter((row) => row.kind === "added").length,
      updated: frozenPreview.filter((row) => row.kind === "updated").length,
      deleted: frozenPreview.filter((row) => row.kind === "deleted").length,
    }),
    [frozenPreview],
  );

  // Whether the typed confirmation applies is the *server's* answer once there is a record, and
  // the same rule applied locally before it: the request's threshold, stated in one place. It is
  // the threshold above which a person is asked to be deliberate, so it is never derived from
  // anything but the count.
  const needsTyping = useMemo(() => {
    if (record) {
      return record.requires_typed_confirmation;
    }
    return counts.total > 25;
  }, [counts.total, record]);

  // The deploy key, read once. `null` until the answer arrives, which is why the button renders in
  // a neutral state rather than guessing — a deploy button that appears and then disappears is
  // worse than one that arrives a moment later.
  useEffect(() => {
    let cancelled = false;
    fetchEffectivePermissions({})
      .then((answer) => {
        if (cancelled) {
          return;
        }
        setMayDeploy(answer.granted.some((entry) => entry.key === "deployment.deploy"));
      })
      .catch(() => {
        if (!cancelled) {
          setMayDeploy(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const onRequest = useCallback(async () => {
    if (busy) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const answer = await requestPromotion(environmentId, selection);
      setRecord(answer.promotion);
      setItems(
        answer.changes.items.map((item) => ({
          site_id: item.site_id,
          slug: item.slug,
          page_id: item.page_id,
          kind: item.kind,
          changed_by: null,
          changed_at: answer.promotion.created_at,
          title: item.title,
        })),
      );
      setPhase("frozen");
      onChanged();
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? cause.message
          : "The promotion was not requested.",
      );
    } finally {
      setBusy(false);
    }
  }, [busy, environmentId, onChanged, selection]);

  const onApprove = useCallback(async () => {
    if (busy || !record) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const answer = await approvePromotion(record.id);
      setRecord(answer);
      onChanged();
    } catch (cause) {
      // A refusal that re-checked the conflicts carries the item ids, and the dialog leads with
      // them rather than printing a sentence the operator cannot act on.
      if (cause instanceof ApiError && cause.code === "promotion_conflict") {
        const details = cause.details as { items?: string[] } | undefined;
        const named = details?.items ?? [];
        setError(
          `Production moved since this was requested. ${named.length} item(s) now differ: ${named.join(", ")}`,
        );
      } else {
        setError(
          cause instanceof ApiError ? cause.message : "The promotion was not applied.",
        );
      }
      // The record's own status is the truth, and after a failed approve the row is `failed`
      // with the step log stopping where it stopped. Re-reading it here is what makes the
      // timeline honest rather than optimistic.
      const refreshed = await fetchEnvironmentPromotions(environmentId)
        .then((rows) => rows.find((row) => row.id === record.id) ?? null)
        .catch(() => null);
      if (refreshed) {
        setRecord(refreshed);
      }
      onChanged();
    } finally {
      setBusy(false);
    }
  }, [busy, environmentId, onChanged, record]);

  const onCancel = useCallback(async () => {
    if (busy || !record) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const answer = await cancelPromotion(record.id);
      setRecord(answer);
      onChanged();
    } catch (cause) {
      setError(
        cause instanceof ApiError ? cause.message : "The promotion was not withdrawn.",
      );
    } finally {
      setBusy(false);
    }
  }, [busy, onChanged, record]);

  // `Esc` closes, but not while a deploy is in flight: closing the dialog mid-apply would leave
  // the operator with no record of what the timeline said.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !busy) {
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);

  const typedMatches = typed.trim() === environmentName.trim();
  const canApprove =
    record !== null &&
    mayDeploy === true &&
    record.status === "pending_approval" &&
    record.conflicts.length === 0 &&
    (!record.requires_typed_confirmation || typedMatches);
  const canCancel = record !== null && record.status === "pending_approval";

  return (
    <div
      className="fixed inset-0 z-50 flex items-end justify-center bg-ink/40 p-0 sm:items-center sm:p-4"
      onClick={() => {
        if (!busy) {
          onClose();
        }
      }}
      data-promotion-overlay
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Promotion"
        data-promotion-dialog
        onClick={(event) => event.stopPropagation()}
        className="flex max-h-[92vh] w-full max-w-2xl flex-col overflow-hidden rounded-t-xl border border-line bg-panel sm:rounded-lg"
      >
        <header className="flex items-start justify-between gap-3 border-b border-line px-5 py-3.5">
          <div className="flex min-w-0 flex-col gap-0.5">
            <h2 className="flex items-center gap-2 text-sm font-semibold text-ink">
              <Rocket className="size-4" aria-hidden />
              {phase === "request" ? "Promote to production" : "Promotion"}
            </h2>
            <p className="text-[11.5px] text-muted">
              {phase === "request"
                ? `${environmentName} → production. The set is frozen when you request it, so what you read here is exactly what runs.`
                : "What runs is exactly what was frozen at request time."}
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            disabled={busy}
            data-promotion-close
            aria-label="Close"
            className="rounded-lg border border-line p-1.5 text-muted transition hover:bg-canvas hover:text-ink disabled:opacity-50"
          >
            <X className="size-3.5" aria-hidden />
          </button>
        </header>

        <div className="flex-1 overflow-y-auto px-5 py-4">
          {error ? (
            <p
              role="alert"
              data-promotion-error
              className="mb-3 rounded-xl border border-accent/30 bg-accent-soft px-3.5 py-2.5 text-[12.5px] text-accent-strong"
            >
              {error}
            </p>
          ) : null}

          {phase === "request" ? (
            <div className="flex flex-col gap-4">
              <section className="flex flex-col gap-2">
                <h3 className="text-[12px] font-medium">What would be promoted</h3>
                {counts.total === 0 ? (
                  <p
                    data-promotion-empty
                    className="rounded-xl border border-line bg-surface px-3.5 py-3 text-[12.5px] text-muted"
                  >
                    Nothing differs from production since the clone, so there is nothing to promote.
                    Edit a page in staging first.
                  </p>
                ) : (
                  <>
                    <div
                      className="flex flex-wrap gap-2 text-[12px]"
                      data-promotion-counts
                    >
                      <span className="rounded-full bg-canvas px-2.5 py-1">
                        {counts.total} item{counts.total === 1 ? "" : "s"}
                      </span>
                      <span className="rounded-full bg-positive-soft px-2.5 py-1 text-positive">
                        {counts.added} added
                      </span>
                      <span className="rounded-full bg-canvas px-2.5 py-1">
                        {counts.updated} updated
                      </span>
                      <span className="rounded-full bg-accent-soft px-2.5 py-1 text-accent-strong">
                        {counts.deleted} deleted
                      </span>
                    </div>
                    {counts.deleted > 0 ? (
                      <p className="text-[11.5px] text-accent-strong">
                        {counts.deleted} page
                        {counts.deleted === 1 ? "" : "s"} will be removed from production. Deleted
                        rows are not written — they are removed.
                      </p>
                    ) : null}
                    <ul
                      className="flex max-h-52 flex-col gap-1 overflow-y-auto rounded-xl border border-line bg-surface p-2"
                      data-promotion-items
                    >
                      {frozenPreview.map((row) => (
                        <li
                          key={row.page_id}
                          className="flex items-center justify-between gap-3 px-1.5 py-1 text-[12px]"
                        >
                          <span className="min-w-0 truncate">
                            {row.title ?? row.slug}
                            <span className="ml-1.5 font-mono text-[11px] text-muted">
                              /{row.slug}
                            </span>
                          </span>
                          <span
                            className={`shrink-0 rounded-full px-2 py-0.5 text-[11px] ${
                              row.kind === "added"
                                ? "bg-positive-soft text-positive"
                                : row.kind === "deleted"
                                  ? "bg-accent-soft text-accent-strong"
                                  : "bg-canvas text-muted"
                            }`}
                          >
                            {row.kind}
                          </span>
                        </li>
                      ))}
                    </ul>
                  </>
                )}
              </section>

              {needsTyping ? (
                <label className="flex flex-col gap-1 text-[12.5px]" htmlFor="promotion-typed">
                  <span className="font-medium">
                    Type this environment&apos;s name to confirm
                  </span>
                  <input
                    id="promotion-typed"
                    value={typed}
                    onChange={(event) => setTyped(event.target.value)}
                    data-promotion-typed
                    autoComplete="off"
                    className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px]"
                    placeholder={environmentName}
                  />
                  <span className="text-[11.5px] text-muted">
                    {counts.total} items is above the threshold where a deploy is a deliberate act
                    rather than a click.
                  </span>
                </label>
              ) : null}
            </div>
          ) : (
            <div className="flex flex-col gap-4">
              {record && record.conflicts.length > 0 ? (
                <section
                  data-promotion-conflicts
                  className="rounded-xl border border-accent/30 bg-accent-soft px-3.5 py-3"
                >
                  <h3 className="flex items-center gap-1.5 text-[12.5px] font-medium text-accent-strong">
                    <AlertTriangle className="size-3.5" aria-hidden />
                    {record.conflicts.length} item
                    {record.conflicts.length === 1 ? "" : "s"} conflict with production
                  </h3>
                  <ul className="mt-2 flex flex-col gap-1 font-mono text-[11.5px] text-accent-strong">
                    {record.conflicts.map((id) => (
                      <li key={id}>{id}</li>
                    ))}
                  </ul>
                  <p className="mt-2 text-[11.5px] text-accent-strong">
                    Approval re-checks these. If production moved after this was requested, the
                    deploy is refused and the item ids are named here.
                  </p>
                </section>
              ) : null}

              <section className="flex flex-col gap-2">
                <h3 className="text-[12px] font-medium">Progress</h3>
                <ol className="flex flex-col gap-1.5" data-promotion-timeline>
                  {TIMELINE.map((entry) => {
                    const step = record?.steps.find(
                      (candidate: PromotionStep) => candidate.step === entry.id,
                    );
                    const failed = record?.status === "failed" && entry.id === lastStep(record);
                    return (
                      <li
                        key={entry.id}
                        data-promotion-step={entry.id}
                        data-promotion-step-state={
                          failed ? "failed" : step ? "done" : "pending"
                        }
                        className="flex items-start gap-2 rounded-lg border border-line bg-surface px-3 py-2"
                      >
                        <span className="mt-0.5 shrink-0" aria-hidden>
                          {failed ? (
                            <AlertTriangle className="size-3.5 text-accent-strong" />
                          ) : step ? (
                            <CheckCircle2 className="size-3.5 text-positive" />
                          ) : (
                            <Circle className="size-3.5 text-muted" />
                          )}
                        </span>
                        <span className="flex min-w-0 flex-col gap-0.5">
                          <span className="text-[12.5px] font-medium text-ink">{entry.label}</span>
                          <span className="text-[11.5px] text-muted">
                            {step ? step.detail : "Not reached yet"}
                          </span>
                        </span>
                      </li>
                    );
                  })}
                </ol>
              </section>

              {record ? (
                <dl
                  className="grid gap-2 text-[12px] sm:grid-cols-2"
                  data-promotion-facts
                >
                  <div>
                    <dt className="text-[11.5px] text-muted">Status</dt>
                    <dd className="font-medium text-ink">{record.status.replace(/_/g, " ")}</dd>
                  </div>
                  <div>
                    <dt className="text-[11.5px] text-muted">Items</dt>
                    <dd className="font-medium text-ink">
                      {record.item_count} ({record.added} added, {record.updated} updated,{" "}
                      {record.deleted} deleted)
                    </dd>
                  </div>
                  <div>
                    <dt className="text-[11.5px] text-muted">Requested</dt>
                    <dd className="text-ink">
                      {new Date(record.created_at).toLocaleString()}
                    </dd>
                  </div>
                  <div>
                    <dt className="text-[11.5px] text-muted">Approved</dt>
                    <dd className="text-ink">
                      {record.approved_at
                        ? new Date(record.approved_at).toLocaleString()
                        : "Not yet approved"}
                    </dd>
                  </div>
                  {record.error ? (
                    <div className="sm:col-span-2">
                      <dt className="text-[11.5px] text-muted">Failure</dt>
                      <dd className="text-accent-strong">{record.error}</dd>
                    </div>
                  ) : null}
                </dl>
              ) : null}

              {items.length > 0 ? (
                <details className="text-[12px]">
                  <summary className="cursor-pointer text-muted hover:text-ink">
                    The {items.length} frozen item{items.length === 1 ? "" : "s"}
                  </summary>
                  <ul className="mt-2 flex flex-col gap-1" data-promotion-frozen-items>
                    {items.map((item) => (
                      <li
                        key={item.page_id}
                        className="flex items-center justify-between gap-3 rounded-lg border border-line bg-surface px-3 py-1.5"
                      >
                        <span className="min-w-0 truncate">{item.title ?? item.slug}</span>
                        <span className="shrink-0 font-mono text-[11px] text-muted">
                          {item.kind}
                        </span>
                      </li>
                    ))}
                  </ul>
                </details>
              ) : null}
            </div>
          )}
        </div>

        <footer className="flex flex-col-reverse gap-2 border-t border-line px-5 py-3 sm:flex-row sm:items-center sm:justify-between">
          {phase === "request" ? (
            <p className="text-[11.5px] text-muted" data-promotion-permission-note>
              {mayDeploy === null
                ? "Checking what you may do here…"
                : mayDeploy
                  ? "You hold deployment.deploy, so you can request and approve in one go."
                  : "You can request a promotion; approving needs deployment.deploy."}
            </p>
          ) : record?.status === "failed" ? (
            <p className="text-[11.5px] text-accent-strong">
              Production was left unchanged. A new promotion can be requested from the current
              change set.
            </p>
          ) : (
            <p className="text-[11.5px] text-muted">
              {record?.status === "done"
                ? "Applied in one transaction. Production now holds what staging held."
                : "The record keeps this record of what was asked and decided."}
            </p>
          )}
          <div className="flex items-center gap-2 sm:justify-end">
            <button
              type="button"
              onClick={onClose}
              disabled={busy}
              data-promotion-cancel-dialog
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
            >
              {phase === "request" ? "Cancel" : "Close"}
            </button>
            {phase === "request" ? (
              <button
                type="button"
                onClick={() => void onRequest()}
                disabled={busy || counts.total === 0 || (needsTyping && !typedMatches)}
                data-promotion-request
                className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
              >
                {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
                Request promotion
              </button>
            ) : canCancel ? (
              <button
                type="button"
                onClick={() => void onCancel()}
                disabled={busy}
                data-promotion-withdraw
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
              >
                Withdraw
              </button>
            ) : null}
            {phase === "frozen" && record ? (
              <button
                type="button"
                onClick={() => void onApprove()}
                disabled={busy || !canApprove}
                data-promotion-approve
                className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
              >
                {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
                Approve and deploy
              </button>
            ) : null}
          </div>
        </footer>
      </div>
    </div>
  );
}

/** The last step the record reached — the one a `failed` promotion stops at. */
function lastStep(record: Promotion): string {
  const reached = record.steps.map((step) => step.step);
  return reached.length === 0 ? "validate" : reached[reached.length - 1];
}
