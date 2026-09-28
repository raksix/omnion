"use client";

/**
 * Retention: how long this library keeps what, and the run log that says what was removed
 * (REQ-010, slice 4).
 *
 * Five things on this tab are not decoration, and each exists because the shortcut is wrong:
 *
 * - **each policy states its consequence in a sentence.** Three number inputs describe the
 *   *settings*; an operator wants to know what happens to *their* file, and that is a
 *   different sentence. `behaviour` is the one they need at 09:00 when a file they uploaded
 *   last month is gone.
 * - **the scope is named, never a uuid.** A policy scoped to a folder prints the folder's
 *   path, and the "whole site" policy is labelled as such — a list of policies where you have
 *   to look up a uuid is a list you cannot tell apart.
 * - **`Run now` reports three numbers, not one.** How many files were removed, how many were
 *   held back and how many were refused are three different outcomes, and a screen that shows
 *   only the first reads "0 files" as a broken worker whether it was a hold, a reference, or
 *   genuinely nothing to do.
 * - **the refusals are listed by name.** "Cannot purge: still referenced" is not something an
 *   operator can act on; `page 0d3f (hero_image_id)` is — it names the record to repoint.
 * - **`Repair stale references` is its own button with its own sentence.** It is the action
 *   that unblocks a library whose pages were deleted during a migration, and it is
 *   destructive in the only way that matters: it removes statements about records that no
 *   longer exist. An operator who cannot see what it will remove will not press it.
 */
import { useCallback, useEffect, useState } from "react";

import {
  Clock,
  FolderTree,
  Loader2,
  Play,
  Plus,
  Save,
  ShieldCheck,
  Trash2,
  TriangleAlert,
  Wrench,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  createMediaRetentionPolicy,
  deleteMediaRetentionPolicy,
  fetchMediaRetention,
  fetchMediaRetentionRuns,
  repairMediaReferences,
  runMediaRetention,
  saveMediaRetentionPolicy,
} from "@/lib/api";
import { useSites } from "@/lib/sites";
import type {
  MediaPurgeRefusal,
  MediaRetentionList,
  MediaRetentionPolicy,
  MediaRetentionRun,
} from "@/lib/types";

/** The bounds the API enforces, restated here so the form can refuse before it posts. */
const WINDOW_RANGE = { min: 1, max: 3650 } as const;

/** One policy as the editor holds it while it is being changed. */
type Draft = {
  name: string;
  keep_versions_days: string;
  trash_days: string;
  purge_after_days: string;
  legal_hold: boolean;
  enabled: boolean;
};

/** The form's draft as a fresh copy of a stored policy. */
function toDraft(policy: MediaRetentionPolicy): Draft {
  return {
    name: policy.name,
    keep_versions_days: String(policy.keep_versions_days),
    trash_days: String(policy.trash_days),
    purge_after_days: String(policy.purge_after_days),
    legal_hold: policy.legal_hold,
    enabled: policy.enabled,
  };
}

/** The draft as the numbers a save sends, or null when one of them is not a whole number. */
function toInput(draft: Draft) {
  const keep = Number(draft.keep_versions_days);
  const trash = Number(draft.trash_days);
  const purge = Number(draft.purge_after_days);
  if (![keep, trash, purge].every((value) => Number.isInteger(value))) {
    return null;
  }
  return {
    name: draft.name,
    keep_versions_days: keep,
    trash_days: trash,
    purge_after_days: purge,
    legal_hold: draft.legal_hold,
    enabled: draft.enabled,
  };
}

const EMPTY_DRAFT: Draft = {
  name: "",
  keep_versions_days: "365",
  trash_days: "30",
  purge_after_days: "90",
  legal_hold: false,
  enabled: true,
};

/** Bytes as a readable size. A byte count in a run log is a number nobody can act on. */
function readableBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[index]}`;
}

export function MediaRetentionView() {
  const { selectedSite, status: sitesStatus } = useSites();
  const [list, setList] = useState<MediaRetentionList | null>(null);
  const [runs, setRuns] = useState<MediaRetentionRun[] | null>(null);
  const [refused, setRefused] = useState<MediaPurgeRefusal[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [editing, setEditing] = useState<string | "new" | null>(null);
  const [draft, setDraft] = useState<Draft>(EMPTY_DRAFT);
  const [reloadToken, setReloadToken] = useState(0);

  const siteId = selectedSite?.id ?? null;
  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    if (!siteId) {
      setList(null);
      return;
    }
    let cancelled = false;
    setList(null);
    setError(null);
    fetchMediaRetention(siteId)
      .then((answer) => {
        if (!cancelled) setList(answer);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setError(cause instanceof ApiError ? cause.message : "The retention policies could not be loaded.");
      });
    fetchMediaRetentionRuns(siteId)
      .then((answer) => {
        if (!cancelled) setRuns(answer.runs);
      })
      .catch(() => {
        // The log is the second thing on this tab; the policies are the first. A failed log
        // must not blank the screen, so it degrades to "no log" rather than to an error.
        if (!cancelled) setRuns([]);
      });
    return () => {
      cancelled = true;
    };
  }, [siteId, reloadToken]);

  async function save(policy: MediaRetentionPolicy | null) {
    if (!siteId) return;
    const input = toInput(draft);
    if (!input) {
      setFieldError({ field: "windows", message: "Every window is a whole number of days." });
      return;
    }
    // The cross-field rule, checked here so the operator is not told to fix a form by the
    // server. The API refuses it too — the check here is a courtesy, the one there is the
    // guarantee.
    if (input.purge_after_days < input.trash_days) {
      setFieldError({
        field: "purge_after_days",
        message: "The bytes cannot go before the restore window closes.",
      });
      return;
    }
    setBusy(true);
    setFieldError(null);
    setError(null);
    setNotice(null);
    try {
      if (policy) {
        await saveMediaRetentionPolicy(siteId, policy.id, input);
        setNotice(`"${input.name}" saved.`);
      } else {
        await createMediaRetentionPolicy(siteId, input);
        setNotice(`"${input.name}" created. It covers the whole site unless you scope it to a folder.`);
      }
      setEditing(null);
      setDraft(EMPTY_DRAFT);
      reload();
    } catch (cause) {
      if (cause instanceof ApiError) {
        // The API names the wire field it refused in `details.field`; a refusal that arrives
        // without one is still a refusal, and the form puts it under the whole form rather
        // than guessing which of six inputs to blame.
        const named = cause.details?.field;
        const field = typeof named === "string" ? named : "form";
        setFieldError({ field, message: cause.message });
      } else {
        setError("The policy could not be saved.");
      }
    } finally {
      setBusy(false);
    }
  }

  async function remove(policy: MediaRetentionPolicy) {
    if (!siteId) return;
    setBusy(true);
    setError(null);
    try {
      await deleteMediaRetentionPolicy(siteId, policy.id);
      setNotice(`"${policy.name}" removed. Files in its scope fall back to the site-wide rule.`);
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The policy could not be removed.");
    } finally {
      setBusy(false);
    }
  }

  async function runNow() {
    if (!siteId) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    setRefused(null);
    try {
      const result = await runMediaRetention(siteId);
      // All three numbers, always. "0 files" alone is indistinguishable from a broken worker.
      const parts = [result.run.summary];
      parts.push(
        result.remaining_files === 0
          ? "The trash is empty."
          : `${result.remaining_files} file(s) (${readableBytes(result.remaining_bytes)}) are still in the trash.`,
      );
      setNotice(parts.join(" "));
      setRefused(result.refused);
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The retention run could not be started.");
    } finally {
      setBusy(false);
    }
  }

  async function repair() {
    if (!siteId) return;
    setBusy(true);
    setError(null);
    try {
      const result = await repairMediaReferences(siteId);
      setNotice(result.summary);
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The repair scan could not be run.");
    } finally {
      setBusy(false);
    }
  }

  if (!siteId) {
    return (
      <p className="px-1 py-6 text-[12.5px] text-muted">
        {sitesStatus === "loading" ? "Loading sites…" : "Choose a site to see its retention rules."}
      </p>
    );
  }

  return (
    <div className="space-y-4" data-testid="media-retention">
      <p className="text-[12.5px] text-muted">
        Retention is what forgets. A replaced file keeps its old versions for a window, a deleted
        file can be restored for another, and the bytes go after that. A legal hold outranks every
        window below until somebody clears it by hand.
      </p>

      {error ? (
        <p role="alert" className="rounded-lg border border-danger/30 bg-danger/5 px-3 py-2 text-[12.5px] text-danger">
          {error}
        </p>
      ) : null}
      {notice ? (
        <p role="status" className="rounded-lg border border-ok/30 bg-ok/5 px-3 py-2 text-[12.5px] text-ok">
          {notice}
        </p>
      ) : null}

      {list === null ? (
        <LoadingTable columns={3} rows={3} />
      ) : (
        <>
          <header className="flex flex-wrap items-center justify-between gap-2">
            <p className="text-[12.5px] text-muted">{list.summary}</p>
            <div className="flex flex-wrap items-center gap-2">
              <button
                type="button"
                onClick={repair}
                disabled={busy}
                data-testid="media-retention-repair"
                className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-canvas disabled:opacity-50"
              >
                <Wrench size={13} aria-hidden />
                Repair stale references
              </button>
              <button
                type="button"
                onClick={runNow}
                disabled={busy}
                data-testid="media-retention-run"
                className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-2.5 py-1.5 text-[12.5px] text-white disabled:opacity-50"
              >
                {busy ? <Loader2 size={13} className="animate-spin" aria-hidden /> : <Play size={13} aria-hidden />}
                Run now
              </button>
            </div>
          </header>

          <p className="text-[12px] text-muted" data-testid="media-retention-past-restore">
            {list.past_restore === 0
              ? "Nothing in the trash is past its restore window."
              : `${list.past_restore} file(s) (${readableBytes(list.past_restore_bytes)}) are past their restore window under the site-wide rule. They are removed on the next eligible run.`}
          </p>

          <div className="rounded-xl border border-line">
            {list.policies.length === 0 ? (
              <EmptyState
                title="This site has no retention policy"
                hint="Without one, nothing prunes: old versions and trashed files stay for ever. The worker seeds a default for every new site, so this only happens on a database restored from an old backup."
              />
            ) : (
              <ul className="divide-y divide-line">
                {list.policies.map((policy) => (
                  <PolicyRow
                    key={policy.id}
                    policy={policy}
                    editing={editing === policy.id}
                    busy={busy}
                    draft={draft}
                    fieldError={fieldError}
                    onStartEdit={() => {
                      setEditing(policy.id);
                      setDraft(toDraft(policy));
                      setFieldError(null);
                    }}
                    onCancel={() => {
                      setEditing(null);
                      setFieldError(null);
                    }}
                    onDraft={setDraft}
                    onSave={() => save(policy)}
                    onRemove={() => remove(policy)}
                  />
                ))}
              </ul>
            )}
          </div>

          {editing === "new" ? (
            <div className="rounded-xl border border-accent/40 bg-canvas/60 p-3">
              <h3 className="text-[13px] font-medium">New policy</h3>
              <p className="mt-1 text-[12px] text-muted">
                A new policy covers the whole site. Scope it to a folder afterwards if this rule
                is only for one campaign — the narrowest rule is the one that applies.
              </p>
              <DraftForm
                draft={draft}
                fieldError={fieldError}
                onDraft={setDraft}
                onSave={() => save(null)}
                onCancel={() => {
                  setEditing(null);
                  setFieldError(null);
                }}
                busy={busy}
              />
            </div>
          ) : (
            <button
              type="button"
              onClick={() => {
                setEditing("new");
                setDraft(EMPTY_DRAFT);
                setFieldError(null);
              }}
              data-testid="media-retention-new"
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-canvas"
            >
              <Plus size={13} aria-hidden />
              Add a policy
            </button>
          )}

          {/*
            The refusals are their own block rather than a line in the notice: a refusal is a
            to-do for somebody (repoint the page) and a notice that disappears on the next
            action is the wrong place to keep one.
          */}
          {refused && refused.length > 0 ? (
            <div
              className="rounded-xl border border-warn/40 bg-warn/5 p-3"
              data-testid="media-retention-refusals"
            >
              <h3 className="flex items-center gap-1.5 text-[13px] font-medium text-warn">
                <TriangleAlert size={14} aria-hidden />
                {refused.length} file(s) were not removed because something still uses them
              </h3>
              <p className="mt-1 text-[12px] text-muted">
                Repoint the record below, then run again. If the record no longer exists at all,
                use <em>Repair stale references</em>.
              </p>
              <ul className="mt-2 space-y-1">
                {refused.map((row) => (
                  <li key={`${row.media_id}-${row.resource_kind}-${row.resource_id}-${row.field}`} className="text-[12px]">
                    <span className="font-medium">{row.filename}</span>{" "}
                    <span className="text-muted">— used by {row.describe}</span>
                  </li>
                ))}
              </ul>
            </div>
          ) : null}

          <div className="rounded-xl border border-line">
            <h3 className="flex items-center gap-1.5 border-b border-line px-3 py-2 text-[13px] font-medium">
              <Clock size={14} aria-hidden />
              Run log
            </h3>
            {runs === null ? (
              <LoadingTable columns={2} rows={2} />
            ) : runs.length === 0 ? (
              <EmptyState
                title="No retention run has finished yet"
                hint="A run is written even when it finds nothing, so the next line here will be an answer rather than a silence."
              />
            ) : (
              <ul className="divide-y divide-line">
                {runs.map((run) => (
                  <li key={run.id} className="px-3 py-2" data-testid="media-retention-run-row">
                    <p className="text-[12.5px]">{run.summary}</p>
                    <p className="mt-0.5 text-[11.5px] text-muted">
                      {new Date(run.started_at).toLocaleString()} · {run.kind}
                      {run.bytes_reclaimed > 0 ? ` · ${readableBytes(run.bytes_reclaimed)} reclaimed` : ""}
                      {run.held_back > 0 ? ` · ${run.held_back} held` : ""}
                      {run.refused > 0 ? ` · ${run.refused} refused` : ""}
                    </p>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </>
      )}
    </div>
  );
}

/** One policy row: its scope, its windows, and what they do to a file. */
function PolicyRow({
  policy,
  editing,
  busy,
  draft,
  fieldError,
  onStartEdit,
  onCancel,
  onDraft,
  onSave,
  onRemove,
}: {
  policy: MediaRetentionPolicy;
  editing: boolean;
  busy: boolean;
  draft: Draft;
  fieldError: { field: string; message: string } | null;
  onStartEdit: () => void;
  onCancel: () => void;
  onDraft: (draft: Draft) => void;
  onSave: () => void;
  onRemove: () => void;
}) {
  if (editing) {
    return (
      <li className="px-3 py-3">
        <DraftForm
          draft={draft}
          fieldError={fieldError}
          onDraft={onDraft}
          onSave={onSave}
          onCancel={onCancel}
          busy={busy}
        />
      </li>
    );
  }

  return (
    <li className="px-3 py-2.5" data-testid="media-retention-policy">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div className="min-w-0">
          <p className="flex items-center gap-1.5 text-[13px] font-medium">
            {policy.name}
            {policy.folder_id ? (
              <span className="inline-flex items-center gap-1 rounded-md bg-canvas px-1.5 py-0.5 text-[11px] text-muted">
                <FolderTree size={11} aria-hidden />
                {policy.folder_path || "folder"}
              </span>
            ) : (
              <span className="rounded-md bg-canvas px-1.5 py-0.5 text-[11px] text-muted">whole site</span>
            )}
            {policy.legal_hold ? (
              <span className="inline-flex items-center gap-1 rounded-md bg-warn/10 px-1.5 py-0.5 text-[11px] text-warn">
                <ShieldCheck size={11} aria-hidden />
                legal hold
              </span>
            ) : null}
            {!policy.enabled ? (
              <span className="rounded-md bg-canvas px-1.5 py-0.5 text-[11px] text-muted">disabled</span>
            ) : null}
          </p>
          <p className="mt-0.5 text-[12px] text-muted">{policy.behaviour}</p>
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          <button
            type="button"
            onClick={onStartEdit}
            className="rounded-lg border border-line px-2 py-1 text-[12px] hover:bg-canvas"
          >
            Edit
          </button>
          <button
            type="button"
            onClick={onRemove}
            disabled={busy}
            aria-label={`Delete ${policy.name}`}
            data-testid={`media-retention-delete-${policy.id}`}
            className="rounded-lg border border-line px-2 py-1 text-[12px] text-danger hover:bg-danger/5 disabled:opacity-50"
          >
            <Trash2 size={12} aria-hidden />
          </button>
        </div>
      </div>
    </li>
  );
}

/** The three windows, the hold, the switch, and the save. Shared by the create and edit forms. */
function DraftForm({
  draft,
  fieldError,
  onDraft,
  onSave,
  onCancel,
  busy,
}: {
  draft: Draft;
  fieldError: { field: string; message: string } | null;
  onDraft: (draft: Draft) => void;
  onSave: () => void;
  onCancel: () => void;
  busy: boolean;
}) {
  const field = (name: string) => (fieldError?.field === name ? fieldError.message : null);

  return (
    <div className="mt-2 space-y-2.5">
      <label className="block text-[12px]">
        <span className="text-ink">Name</span>
        <input
          value={draft.name}
          onChange={(event) => onDraft({ ...draft, name: event.target.value })}
          placeholder="Campaign assets"
          maxLength={120}
          data-testid="media-retention-name"
          className="mt-0.5 w-full rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
        />
        {field("name") ? <span className="mt-0.5 block text-[11.5px] text-danger">{field("name")}</span> : null}
      </label>

      <div className="grid gap-2.5 sm:grid-cols-3">
        {(
          [
            ["keep_versions_days", "Keep old versions (days)"],
            ["trash_days", "Restore window (days)"],
            ["purge_after_days", "Remove bytes after (days)"],
          ] as const
        ).map(([name, label]) => (
          <label key={name} className="block text-[12px]">
            <span className="text-ink">{label}</span>
            <input
              type="number"
              inputMode="numeric"
              min={WINDOW_RANGE.min}
              max={WINDOW_RANGE.max}
              value={draft[name]}
              onChange={(event) => onDraft({ ...draft, [name]: event.target.value })}
              data-testid={`media-retention-${name}`}
              className="mt-0.5 w-full rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
            />
            {field(name) ? <span className="mt-0.5 block text-[11.5px] text-danger">{field(name)}</span> : null}
          </label>
        ))}
      </div>

      {/*
        The warning is live rather than on save, because the pair is the only combination the
        API refuses and a person who cannot see it until the save is a person who reads an
        error instead of their own form.
      */}
      {Number(draft.purge_after_days) < Number(draft.trash_days) ? (
        <p className="text-[11.5px] text-warn">
          The bytes would go before the restore window closes. The platform refuses this: a file
          you were promised you could restore would already be gone.
        </p>
      ) : null}
      {fieldError && fieldError.field === "windows" ? (
        <p className="text-[11.5px] text-danger">{fieldError.message}</p>
      ) : null}

      <div className="flex flex-wrap items-center gap-3">
        <label className="inline-flex items-center gap-1.5 text-[12px]">
          <input
            type="checkbox"
            checked={draft.legal_hold}
            onChange={(event) => onDraft({ ...draft, legal_hold: event.target.checked })}
            data-testid="media-retention-hold"
          />
          Legal hold
        </label>
        <label className="inline-flex items-center gap-1.5 text-[12px]">
          <input
            type="checkbox"
            checked={draft.enabled}
            onChange={(event) => onDraft({ ...draft, enabled: event.target.checked })}
          />
          Enabled
        </label>
        <div className="ml-auto flex items-center gap-1.5">
          <button
            type="button"
            onClick={onCancel}
            className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] hover:bg-canvas"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={onSave}
            disabled={busy}
            data-testid="media-retention-save"
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-2.5 py-1.5 text-[12px] text-white disabled:opacity-50"
          >
            {busy ? <Loader2 size={13} className="animate-spin" aria-hidden /> : <Save size={13} aria-hidden />}
            Save
          </button>
        </div>
      </div>
    </div>
  );
}
