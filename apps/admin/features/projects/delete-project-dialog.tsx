"use client";

/**
 * `DeleteProjectDialog` — removing a project for good (REQ-133, slice 19).
 *
 * The REQ's settings screen has read "Name, key, description, colour, archive, export, delete"
 * since the module shipped, and `DELETE /api/v1/projects/{id}` has sat in its API table with
 * "typed confirmation, dependency check". Neither existed. This is the panel half of both.
 *
 * **Three things this dialog has to get right, and each is a place a plausible implementation
 * goes wrong:**
 *
 * 1. **The typed confirmation is the project's KEY, and the button stays disabled until the
 *    field matches it exactly.** Not the name, not the id. The key is the short form people write
 *    in a ticket (`OPS`), it is unique per organization, and it is the one identifier that cannot
 *    be produced by muscle memory from a list — typing `OP` when the project is `OPS` fails,
 *    which is the entire purpose of asking somebody to type it. A dialog that compared
 *    case-insensitively, or trimmed whitespace, would train people to paste the key from the
 *    header instead of reading it, and the confirmation would stop being a confirmation.
 * 2. **The dependency count is shown BEFORE the confirmation is accepted, not after the refusal.**
 *    The server refuses a project holding workflows with the count in its message; a dialog that
 *    let somebody type the key, press the button and only then said "it holds 12 workflows" has
 *    made the typed confirmation a formality. The detail screen already knows the workflow
 *    count, so it is passed in and stated up front.
 * 3. **The default project cannot be deleted, and the dialog says so instead of offering a
 *    button that will be refused.** `automation_projects.is_default` is on the row the screen
 *    already has; the server refuses by `project_is_default`, and a button that is present and
 *    permanently refused is a dead button by another name.
 *
 * The trail is the one thing that survives, and the dialog says that plainly: `audit_log.project_id`
 * is `on delete set null`, so the history outlives the container. That is worth telling somebody
 * before they confirm rather than after, because it is the difference between "everything is
 * gone" and "the record stays".
 */
import { useEffect, useRef, useState } from "react";
import { Loader2, Trash2, TriangleAlert, X } from "lucide-react";

import { ApiError, deleteProject } from "@/lib/api";
import type { Project } from "@/lib/types";

export function DeleteProjectDialog({
  project,
  workflowCount,
  memberCount,
  onClose,
  onDeleted,
}: {
  project: Project;
  /** How many workflows the project holds, stated before the confirmation rather than after. */
  workflowCount: number;
  /** How many members would be dismissed with it. */
  memberCount: number;
  onClose: () => void;
  onDeleted: (projectKey: string) => void;
}) {
  const [typed, setTyped] = useState("");
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

  // Exact equality, the same rule the server states. Trimming here would let a key pasted with a
  // trailing space through a check the store then refuses, which is a dialog that disagrees with
  // the product about whether you did the right thing.
  const matches = typed === project.key;
  const isDefault = project.is_default;
  const blocked = workflowCount > 0;
  const ready = matches && !isDefault && !blocked && !busy;

  const apply = async () => {
    if (!ready) return;
    setBusy(true);
    setError(null);
    try {
      await deleteProject(project.id, project.key);
      onDeleted(project.key);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? cause.message : "the project could not be deleted",
      );
      setBusy(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4 pt-[8vh]"
      data-delete-dialog
    >
      <div
        ref={panel}
        role="dialog"
        aria-modal="true"
        aria-label={`Delete ${project.key}`}
        tabIndex={-1}
        className="flex w-full max-w-lg flex-col gap-3 rounded-xl border border-line bg-panel p-4 shadow-xl outline-none"
      >
        <div className="flex items-start gap-2">
          <h2 className="flex-1 text-[14px] font-medium">Delete {project.key}</h2>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            data-delete-close
            className="rounded-lg border border-line p-1 text-muted hover:text-ink"
          >
            <X className="size-3.5" aria-hidden />
          </button>
        </div>

        {isDefault ? (
          <div
            role="status"
            data-delete-default
            className="flex items-start gap-2 rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12px]"
          >
            <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-muted" aria-hidden />
            <span className="flex-1">
              This is the organization&apos;s <strong>default project</strong>, so it cannot be
              deleted — every automation created without a project of its own lands here. Leave it
              alone, or archive it if you want it out of the way (it may not be archived either).
            </span>
          </div>
        ) : null}

        {!isDefault && blocked ? (
          <div
            role="status"
            data-delete-blocked
            className="flex items-start gap-2 rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12px]"
          >
            <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-muted" aria-hidden />
            <span className="flex-1">
              <strong>
                {workflowCount} workflow{workflowCount === 1 ? "" : "s"} still live in this
                project.
              </strong>{" "}
              Move them to another project first — <code>Move to project…</code> on the project
              screen does it and reports what would break. Nothing is deleted until they are gone.
            </span>
          </div>
        ) : null}

        {!isDefault && !blocked ? (
          <>
            <p className="text-[12.5px] text-muted" data-delete-consequences>
              <Trash2 className="mr-1 inline size-3.5" aria-hidden />
              Deleting <strong>{project.name}</strong> removes the project and{" "}
              <strong>
                {memberCount} member{memberCount === 1 ? "" : "s"}
              </strong>{" "}
              with it. The workflows are already gone, so no run history goes with them.{" "}
              <strong>The audit trail stays</strong> — the entries keep their text and are
              detached from the project rather than deleted.
            </p>

            <label className="flex flex-col gap-1">
              <span className="text-[11.5px] text-muted">
                Type <code className="rounded bg-quiet-soft px-1">{project.key}</code> to confirm
              </span>
              <input
                value={typed}
                onChange={(event) => setTyped(event.target.value)}
                autoComplete="off"
                spellCheck={false}
                data-delete-key
                aria-label={`Type ${project.key} to confirm the deletion`}
                className="rounded-lg border border-line bg-panel px-2.5 py-1.5 font-mono text-[13px] uppercase outline-none focus:border-accent"
              />
            </label>
          </>
        ) : null}

        {error ? (
          <div
            role="alert"
            data-delete-error
            className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2 text-[12px]"
          >
            <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
            <span className="flex-1">{error}</span>
          </div>
        ) : null}

        <div className="flex items-center gap-2">
          <button
            type="button"
            data-delete-confirm
            disabled={!ready}
            onClick={() => void apply()}
            className="inline-flex items-center gap-1.5 rounded-lg border border-red-600 bg-red-600 px-2.5 py-1.5 text-[12.5px] text-white disabled:opacity-60"
          >
            {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <Trash2 className="size-3.5" aria-hidden />}
            Delete for good
          </button>
          <button
            type="button"
            onClick={onClose}
            className="rounded-lg border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            {isDefault || blocked ? "Close" : "Cancel"}
          </button>
          {!isDefault && !blocked && !matches ? (
            <span className="text-[11.5px] text-muted">The key has to match exactly.</span>
          ) : null}
        </div>
      </div>
    </div>
  );
}
