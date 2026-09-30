"use client";

/**
 * `TransferOwnershipDialog` — handing a project to somebody else (REQ-133, slice 4).
 *
 * The REQ asks for **two confirmations**, and the reason it asks for two rather than one is worth
 * keeping in the code: ownership transfer and workflow ownership are different things, and the
 * visible consequence is that the previous owner is *demoted to editor*, not removed. A person
 * confirming that they understand they will keep access, and a person confirming they understand
 * the change is written to the audit trail, are two different acknowledgements.
 *
 * **The API refuses a request carrying only one of them** (`ownership_transfer_unconfirmed`), so
 * these checkboxes are not decoration — a panel that let either one be skipped would see the
 * request come back with a 400 and no work done, which is exactly what the dialog is for.
 */
import { useEffect, useRef, useState } from "react";
import { ArrowRightLeft, Loader2, TriangleAlert, X } from "lucide-react";

import { ApiError, transferProjectOwnership } from "@/lib/api";
import type { Project, ProjectMember } from "@/lib/types";

export function TransferOwnershipDialog({
  project,
  members,
  onClose,
  onTransferred,
}: {
  project: Project;
  members: ProjectMember[];
  onClose: () => void;
  onTransferred: (previousOwner: string | null) => void;
}) {
  // A viewer promoted to owner becomes an *editor* (the store's rule), so the picker says so rather
  // than letting somebody discover it after the fact.
  const candidates = members.filter((member) => member.user_id !== project.owner_user_id);
  const [target, setTarget] = useState("");
  const [confirmOwner, setConfirmOwner] = useState(false);
  const [confirmAudit, setConfirmAudit] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const panel = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    panel.current?.focus();
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  const chosen = candidates.find((member) => member.user_id === target);
  const ready = Boolean(target) && confirmOwner && confirmAudit;

  const apply = async () => {
    if (!ready) return;
    setBusy(true);
    setError(null);
    try {
      const answer = await transferProjectOwnership(project.id, target);
      onTransferred(answer.previous_owner_user_id);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? cause.message : "the project could not be handed over",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4 pt-[8vh]"
      data-transfer-dialog
    >
      <div
        ref={panel}
        role="dialog"
        aria-modal="true"
        aria-label={`Hand ${project.key} to a new owner`}
        tabIndex={-1}
        className="flex w-full max-w-lg flex-col gap-3 rounded-xl border border-line bg-panel p-4 shadow-xl outline-none"
      >
        <div className="flex items-start gap-2">
          <h2 className="flex-1 text-[14px] font-medium">Hand over {project.key}</h2>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            data-transfer-close
            className="rounded-lg border border-line p-1 text-muted hover:text-ink"
          >
            <X className="size-3.5" aria-hidden />
          </button>
        </div>

        <p className="text-[12.5px] text-muted">
          <ArrowRightLeft className="mr-1 inline size-3.5" aria-hidden />
          The new owner may manage the project, its members and its limits.{" "}
          <strong>Whoever owns it today is demoted to editor, not removed</strong> — they keep
          access, which is usually the point of a handover.
        </p>

        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] text-muted">New owner</span>
          <select
            value={target}
            disabled={candidates.length === 0}
            data-transfer-target
            onChange={(event) => setTarget(event.target.value)}
            className="rounded-lg border border-line bg-panel px-2.5 py-1.5 text-[13px] outline-none focus:border-accent disabled:bg-quiet-soft"
          >
            <option value="">
              {candidates.length === 0 ? "Nobody else is in this project" : "Choose a member…"}
            </option>
            {candidates.map((member) => (
              <option key={member.user_id} value={member.user_id}>
                {member.display_name} — {member.role}
                {member.role === "viewer" ? " (becomes editor)" : ""}
              </option>
            ))}
          </select>
        </label>

        {chosen?.role === "viewer" ? (
          <p className="rounded-lg border border-line bg-quiet-soft px-2.5 py-2 text-[11.5px] text-muted">
            {chosen.display_name} is a viewer, so they become an <strong>editor</strong> rather than
            an owner. An owner who cannot edit anything is worse than no owner at all, because the
            &quot;at least one owner must remain&quot; rule would not fire for them.
          </p>
        ) : null}

        <label className="flex items-start gap-2 text-[12.5px]" data-transfer-confirm-owner>
          <input
            type="checkbox"
            checked={confirmOwner}
            onChange={(event) => setConfirmOwner(event.target.checked)}
            className="mt-0.5"
          />
          <span>
            I know the current owner is demoted to editor rather than removed, and that they keep
            access to everything in this project.
          </span>
        </label>

        <label className="flex items-start gap-2 text-[12.5px]" data-transfer-confirm-audit>
          <input
            type="checkbox"
            checked={confirmAudit}
            onChange={(event) => setConfirmAudit(event.target.checked)}
            className="mt-0.5"
          />
          <span>
            I know this is written to the audit trail under{" "}
            <code>project_ownership.transferred</code>, naming both accounts.
          </span>
        </label>

        {error ? (
          <div
            role="alert"
            data-transfer-error
            className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2 text-[12px]"
          >
            <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
            <span className="flex-1">{error}</span>
          </div>
        ) : null}

        <div className="flex items-center gap-2">
          <button
            type="button"
            data-transfer-confirm
            disabled={!ready || busy}
            onClick={() => void apply()}
            className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent px-2.5 py-1.5 text-[12.5px] text-white disabled:opacity-60"
          >
            {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
            Hand over
          </button>
          <button type="button" onClick={onClose} className="rounded-lg border border-line px-2.5 py-1.5 text-[12.5px]">
            Cancel
          </button>
          {!ready ? (
            <span className="text-[11.5px] text-muted">Both confirmations are required.</span>
          ) : null}
        </div>
      </div>
    </div>
  );
}
