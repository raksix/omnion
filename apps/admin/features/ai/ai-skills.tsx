"use client";

/**
 * `/ai/skills` — the skills registry (docs/requests/REQ-099, slice 3).
 *
 * The registry is the installation's list of written guidance. Four rules shape the screen:
 *
 * 1. **Built-in and custom look different, because they behave differently.** A built-in can be
 *    disabled and can never be deleted or edited — it is the installation's documented
 *    behaviour, and an installation that has quietly rewritten it has a definition nobody can
 *    reproduce after an upgrade. The row says which it is *before* the buttons do, so nobody
 *    discovers it by being refused.
 *
 * 2. **Delete is offered only where it works.** Not hidden behind a disabled control: absent.
 *    A greyed-out Delete on a built-in teaches the operator that the screen is stale, and a
 *    403 after the fact teaches them the same thing more slowly.
 *
 * 3. **Validation is a button, and it writes nothing.** "Run validation" answers the same
 *    question the save will answer, from the same function, so a green check is not a hopeful
 *    guess. The problems come back as a list with the offending key named.
 *
 * 4. **The checksum is visible.** It is the thing that decides whether a row is injected, so
 *    an operator debugging "why did my skill stop working" needs to see it without a database
 *    session.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";

import { Loader2, Pencil, Plus, Search, TriangleAlert, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiSkill,
  type AiSkillValidation,
  createAiSkill,
  deleteAiSkill,
  fetchAiSkills,
  updateAiSkill,
  validateAiSkill,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The field limits, quoted from the crate rather than guessed. */
const LIMITS = {
  name: 80,
  description: 200,
  whenToUse: 500,
  instructions: 8000,
} as const;

/** What the form holds while it is open. */
type Draft = {
  key: string;
  name: string;
  description: string;
  whenToUse: string;
  instructions: string;
  tools: string;
  enabled: boolean;
};

/** A blank form. */
const BLANK: Draft = {
  key: "",
  name: "",
  description: "",
  whenToUse: "",
  instructions: "",
  tools: "",
  enabled: true,
};

/** Why a key the form typed would be refused, or `null` when it is fine. */
export function checkSkillKey(key: string): string | null {
  const trimmed = key.trim();
  if (!trimmed) return "A skill needs a key — the stable name runs and prompts refer to it.";
  if (!/^[a-z]/.test(trimmed)) return "A key starts with a lower-case letter.";
  if (!/^[a-z0-9_-]*$/.test(trimmed)) {
    return "A key uses only lower-case letters, digits, _ and -.";
  }
  if (trimmed.length > 64) return `The key is ${trimmed.length} characters; the limit is 64.`;
  return null;
}

/** The registry screen. */
export function AiSkills({ organizationId }: { organizationId?: string | null }) {
  const [rows, setRows] = useState<AiSkill[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [editing, setEditing] = useState<AiSkill | "new" | null>(null);
  const [open, setOpen] = useState<AiSkill | null>(null);
  const [confirming, setConfirming] = useState<AiSkill | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const list = await fetchAiSkills({ organizationId });
      setRows(list.skills);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, [organizationId]);

  useEffect(() => {
    void load();
  }, [load]);

  // Filtering client-side on purpose: the registry is a small, bounded list (one tenant's
  // skills plus the shared built-ins), and a request per keystroke is a request per keystroke.
  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return rows;
    return rows.filter(
      (row) =>
        row.key.toLowerCase().includes(needle) ||
        row.name.toLowerCase().includes(needle) ||
        row.description.toLowerCase().includes(needle),
    );
  }, [rows, query]);

  const toggle = useCallback(
    async (skill: AiSkill) => {
      setBusy(true);
      setError(null);
      try {
        await updateAiSkill(skill.key, { enabled: !skill.enabled }, organizationId);
        await load();
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : String(cause));
      } finally {
        setBusy(false);
      }
    },
    [organizationId, load],
  );

  const remove = useCallback(
    async (skill: AiSkill) => {
      setBusy(true);
      setError(null);
      try {
        await deleteAiSkill(skill.key, organizationId);
        setConfirming(null);
        await load();
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : String(cause));
        setConfirming(null);
      } finally {
        setBusy(false);
      }
    },
    [organizationId, load],
  );

  if (loading) {
    return <LoadingTable columns={6} rows={5} />;
  }

  return (
    <div data-ai-skills className="space-y-4">
      {error ? (
        <p
          data-ai-skills-error
          role="alert"
          className="flex items-start gap-2 rounded-xl border border-danger/40 bg-danger/5 px-3.5 py-3 text-[12.5px] text-danger"
        >
          <TriangleAlert className="mt-px size-3.5 shrink-0" aria-hidden />
          <span className="flex-1">{error}</span>
          <button
            type="button"
            onClick={() => void load()}
            className="shrink-0 underline underline-offset-2"
          >
            Retry
          </button>
        </p>
      ) : null}

      <div className="flex flex-wrap items-center gap-2">
        <div className="relative flex-1 min-w-[200px]">
          <Search
            className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted"
            aria-hidden
          />
          <input
            type="search"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Search skills"
            aria-label="Search skills"
            data-ai-skills-search
            className="w-full rounded-lg border border-line bg-surface py-1.5 pl-8 pr-2.5 text-[12.5px]"
          />
        </div>
        <button
          type="button"
          data-ai-skills-new
          onClick={() => setEditing("new")}
          className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
        >
          <Plus className="size-3.5" aria-hidden />
          New skill
        </button>
      </div>

      {rows.length === 0 ? (
        <EmptyState
          title="No skill in the registry"
          hint="A skill is a short piece of written guidance the model gets with every run — when to use it and what a good answer looks like. It is data, never code, and it grants no tool the agent does not already hold."
          action={
            <button
              type="button"
              onClick={() => setEditing("new")}
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
            >
              <Plus className="size-3.5" aria-hidden />
              New skill
            </button>
          }
        />
      ) : visible.length === 0 ? (
        // Different fact from "nothing exists": the registry has rows, the filter hid them.
        <p data-ai-skills-no-match className="px-1 py-6 text-center text-[12.5px] text-muted">
          No skill matches “{query}”.
        </p>
      ) : (
        <div className="overflow-x-auto rounded-xl border border-line">
          <table className="w-full min-w-[760px] text-left text-[12.5px]">
            <thead className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
              <tr>
                <th scope="col" className="px-3.5 py-2.5">Key</th>
                <th scope="col" className="px-3.5 py-2.5">Name</th>
                <th scope="col" className="px-3.5 py-2.5">Version</th>
                <th scope="col" className="px-3.5 py-2.5">Tools</th>
                <th scope="col" className="px-3.5 py-2.5">Used by</th>
                <th scope="col" className="px-3.5 py-2.5">Enabled</th>
                <th scope="col" className="px-3.5 py-2.5">Source</th>
                <th scope="col" className="px-3.5 py-2.5">Updated</th>
                <th scope="col" className="px-3.5 py-2.5"><span className="sr-only">Actions</span></th>
              </tr>
            </thead>
            <tbody>
              {visible.map((skill) => (
                <tr key={skill.key} data-ai-skill={skill.key} className="border-b border-line last:border-0">
                  <td className="px-3.5 py-2.5 align-top">
                    <button
                      type="button"
                      onClick={() => setOpen(skill)}
                      data-ai-skill-open={skill.key}
                      className="font-mono text-[11.5px] underline underline-offset-2"
                    >
                      {skill.key}
                    </button>
                  </td>
                  <td className="px-3.5 py-2.5 align-top">{skill.name}</td>
                  <td className="px-3.5 py-2.5 align-top text-muted">v{skill.version}</td>
                  <td className="px-3.5 py-2.5 align-top text-muted">
                    {skill.tools.length === 0 ? "—" : skill.tools.length}
                  </td>
                  <td className="px-3.5 py-2.5 align-top text-muted">{skill.used_by}</td>
                  <td className="px-3.5 py-2.5 align-top">
                    <button
                      type="button"
                      data-ai-skill-toggle={skill.key}
                      disabled={busy}
                      onClick={() => void toggle(skill)}
                      className={skill.enabled ? "text-positive" : "text-muted"}
                    >
                      {skill.enabled ? "Enabled" : "Disabled"}
                    </button>
                  </td>
                  <td className="px-3.5 py-2.5 align-top text-muted">
                    {skill.built_in ? "built-in" : "custom"}
                  </td>
                  <td className="px-3.5 py-2.5 align-top text-muted">
                    {formatTimestamp(skill.updated_at)}
                  </td>
                  <td className="px-3.5 py-2.5 align-top text-right">
                    {/* A built-in can be enabled, disabled and read — never rewritten or
                        removed. Offering the buttons and refusing on click would teach the
                        operator that the screen is stale. */}
                    {skill.built_in ? null : (
                      <span className="inline-flex items-center gap-2">
                        <button
                          type="button"
                          data-ai-skill-edit={skill.key}
                          aria-label={`Edit ${skill.name}`}
                          onClick={() => setEditing(skill)}
                          className="text-muted hover:text-ink"
                        >
                          <Pencil className="size-3.5" aria-hidden />
                        </button>
                        <button
                          type="button"
                          data-ai-skill-delete={skill.key}
                          onClick={() => setConfirming(skill)}
                          className="text-muted underline underline-offset-2 hover:text-danger"
                        >
                          Delete
                        </button>
                      </span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {open ? (
        <SkillDrawer
          skill={open}
          onClose={() => setOpen(null)}
          onEdit={() => {
            setEditing(open);
            setOpen(null);
          }}
        />
      ) : null}

      {editing ? (
        <SkillForm
          skill={editing === "new" ? null : editing}
          organizationId={organizationId}
          busy={busy}
          onCancel={() => setEditing(null)}
          onSaved={async () => {
            setEditing(null);
            await load();
          }}
        />
      ) : null}

      {confirming ? (
        <div
          data-ai-skill-confirm
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
        >
          <div className="w-full max-w-sm rounded-xl border border-line bg-surface p-4 shadow-lg">
            <h2 className="text-[13.5px] font-medium">Delete “{confirming.name}”?</h2>
            <p className="mt-1 text-[12.5px] text-muted">
              {confirming.used_by > 0
                ? `${confirming.used_by} agent(s) have it attached. Their next run stops receiving it.`
                : "No agent has it attached."}
            </p>
            <div className="mt-3.5 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setConfirming(null)}
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium"
              >
                Cancel
              </button>
              <button
                type="button"
                data-ai-skill-delete-confirm
                disabled={busy}
                onClick={() => void remove(confirming)}
                className="inline-flex items-center gap-1.5 rounded-lg bg-danger px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-50"
              >
                {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
                Delete
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}

/** The detail drawer: the whole definition, the checksum, and what it can reach. */
function SkillDrawer({
  skill,
  onClose,
  onEdit,
}: {
  skill: AiSkill;
  onClose: () => void;
  onEdit: () => void;
}) {
  return (
    <div
      data-ai-skill-drawer
      className="fixed inset-0 z-50 flex justify-end bg-black/40"
      onClick={onClose}
    >
      <div
        className="w-full max-w-lg overflow-y-auto border-l border-line bg-surface p-5"
        onClick={(event) => event.stopPropagation()}
      >
        <div className="flex items-start justify-between gap-3">
          <div>
            <h2 className="text-[15px] font-medium">{skill.name}</h2>
            <p className="text-[12.5px] text-muted">
              <code className="text-[11.5px]">{skill.key}</code> · v{skill.version} ·{" "}
              {skill.built_in ? "built-in" : "custom"}
            </p>
          </div>
          <button type="button" onClick={onClose} aria-label="Close" className="text-muted hover:text-ink">
            <X className="size-4" aria-hidden />
          </button>
        </div>

        <dl className="mt-4 space-y-3 text-[12.5px]">
          <div>
            <dt className="font-medium">What it is for</dt>
            <dd className="text-muted">{skill.description || "—"}</dd>
          </div>
          <div>
            <dt className="font-medium">When to use</dt>
            <dd className="text-muted">{skill.when_to_use || "—"}</dd>
          </div>
          <div>
            <dt className="font-medium">Tools it names</dt>
            <dd className="text-muted">
              {skill.tools.length === 0
                ? "None — this skill names no tool, so it cannot cause one."
                : skill.tools.join(", ")}
            </dd>
          </div>
          <div>
            <dt className="font-medium">Used by</dt>
            <dd className="text-muted">
              {skill.used_by} agent{skill.used_by === 1 ? "" : "s"}
            </dd>
          </div>
          <div>
            <dt className="font-medium">Checksum</dt>
            <dd className="break-all font-mono text-[11px] text-muted">{skill.checksum}</dd>
            <dd className="text-[11.5px] text-muted">
              Recomputed at every run. A row edited outside the panel no longer matches and is
              not injected.
            </dd>
          </div>
        </dl>

        <div className="mt-4">
          <h3 className="text-[12.5px] font-medium">Instructions</h3>
          <pre className="mt-1.5 max-h-80 overflow-auto whitespace-pre-wrap rounded-xl border border-line bg-subtle p-3 text-[12px] leading-relaxed">
            {skill.instructions}
          </pre>
        </div>

        {skill.built_in ? (
          <p className="mt-4 rounded-xl border border-line px-3 py-2.5 text-[12px] text-muted">
            A built-in skill can be enabled and disabled, but its definition is read-only: it
            is the installation&apos;s documented behaviour, and an installation that quietly
            rewrote it would have a definition nobody can reproduce after an upgrade.
          </p>
        ) : (
          <div className="mt-4 flex justify-end">
            <button
              type="button"
              data-ai-skill-drawer-edit
              onClick={onEdit}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium"
            >
              Edit
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

/** The create/edit form, with the Run validation button the spec asks for. */
function SkillForm({
  skill,
  organizationId,
  busy,
  onCancel,
  onSaved,
}: {
  skill: AiSkill | null;
  organizationId?: string | null;
  busy: boolean;
  onCancel: () => void;
  onSaved: () => Promise<void>;
}) {
  const [draft, setDraft] = useState<Draft>(() =>
    skill
      ? {
          key: skill.key,
          name: skill.name,
          description: skill.description,
          whenToUse: skill.when_to_use,
          instructions: skill.instructions,
          tools: skill.tools.join(", "),
          enabled: skill.enabled,
        }
      : BLANK,
  );
  const [error, setError] = useState<string | null>(null);
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});
  const [verdict, setVerdict] = useState<AiSkillValidation | null>(null);

  const keyError = checkSkillKey(draft.key);

  const validate = useCallback(async () => {
    setError(null);
    try {
      setVerdict(
        await validateAiSkill(
          draft.key.trim() || "draft",
          {
            name: draft.name,
            description: draft.description,
            when_to_use: draft.whenToUse,
            instructions: draft.instructions,
            tools: draft.tools.split(",").map((t) => t.trim()).filter(Boolean),
            expected_checksum: skill?.checksum ?? null,
          },
          organizationId,
        ),
      );
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    }
  }, [draft, organizationId, skill]);

  const save = useCallback(async () => {
    setError(null);
    setFieldErrors({});
    if (keyError) {
      setFieldErrors({ key: keyError });
      return;
    }
    setBusyOn(async () => {
      if (skill) {
        await updateAiSkill(
          skill.key,
          {
            name: draft.name,
            description: draft.description,
            when_to_use: draft.whenToUse,
            instructions: draft.instructions,
            tools: draft.tools.split(",").map((t) => t.trim()).filter(Boolean),
            enabled: draft.enabled,
          },
          organizationId,
        );
      } else {
        await createAiSkill({
          key: draft.key.trim(),
          name: draft.name,
          description: draft.description,
          when_to_use: draft.whenToUse,
          instructions: draft.instructions,
          tools: draft.tools.split(",").map((t) => t.trim()).filter(Boolean),
          enabled: draft.enabled,
          organizationId,
        });
      }
    });
    await onSaved();
  }, [draft, keyError, onSaved, organizationId, skill]);

  // The busy flag is owned by the screen; this is the bridge that turns a rejection into the
  // screen's error line rather than an unhandled promise.
  async function setBusyOn(action: () => Promise<void>) {
    try {
      await action();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    }
  }

  return (
    <div
      data-ai-skill-form
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4"
      onClick={onCancel}
    >
      <div
        className="w-full max-w-2xl space-y-3.5 rounded-xl border border-line bg-surface p-5"
        onClick={(event) => event.stopPropagation()}
      >
        <h2 className="text-[15px] font-medium">{skill ? `Edit ${skill.name}` : "New skill"}</h2>

        {error ? (
          <p role="alert" className="rounded-xl border border-danger/40 bg-danger/5 px-3 py-2.5 text-[12.5px] text-danger">
            {error}
          </p>
        ) : null}
        {verdict ? (
          <div
            data-ai-skill-verdict
            className={
              verdict.valid
                ? "rounded-xl border border-positive/40 bg-positive/5 px-3 py-2.5 text-[12.5px] text-positive"
                : "rounded-xl border border-warning/40 bg-warning/5 px-3 py-2.5 text-[12.5px] text-warning"
            }
          >
            {verdict.valid ? (
              <p>Valid. Checksum {verdict.checksum.slice(0, 12)}…</p>
            ) : (
              <ul className="list-disc pl-4">
                {verdict.problems.map((problem) => (
                  <li key={problem}>{problem}</li>
                ))}
              </ul>
            )}
            {verdict.checksum_matched ? null : skill ? (
              <p className="mt-1">
                The stored checksum differs from what you are editing — somebody changed this
                definition since you loaded it.
              </p>
            ) : null}
          </div>
        ) : null}

        <label className="block text-[12.5px]">
          <span className="font-medium">Key</span>
          <input
            type="text"
            value={draft.key}
            // Immutable after create, like an agent's: a key is what runs and prompts refer to.
            disabled={Boolean(skill)}
            onChange={(event) => setDraft({ ...draft, key: event.target.value })}
            data-ai-skill-key
            className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5 font-mono text-[12.5px] disabled:opacity-60"
          />
          {skill ? (
            <span className="mt-0.5 block text-[11.5px] text-muted">
              A key is immutable once created — runs and prompts refer to it.
            </span>
          ) : fieldErrors.key ? (
            <span className="mt-0.5 block text-[11.5px] text-danger">{fieldErrors.key}</span>
          ) : null}
        </label>

        <label className="block text-[12.5px]">
          <span className="font-medium">Name</span>
          <input
            type="text"
            value={draft.name}
            onChange={(event) => setDraft({ ...draft, name: event.target.value })}
            data-ai-skill-name
            className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
          <span className="mt-0.5 block text-[11.5px] text-muted">
            {draft.name.length} / {LIMITS.name}
          </span>
        </label>

        <label className="block text-[12.5px]">
          <span className="font-medium">Description</span>
          <input
            type="text"
            value={draft.description}
            onChange={(event) => setDraft({ ...draft, description: event.target.value })}
            data-ai-skill-description
            className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
          <span className="mt-0.5 block text-[11.5px] text-muted">
            {draft.description.length} / {LIMITS.description}
          </span>
        </label>

        <label className="block text-[12.5px]">
          <span className="font-medium">When to use</span>
          <textarea
            value={draft.whenToUse}
            onChange={(event) => setDraft({ ...draft, whenToUse: event.target.value })}
            data-ai-skill-when
            rows={2}
            className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
          <span className="mt-0.5 block text-[11.5px] text-muted">
            {draft.whenToUse.length} / {LIMITS.whenToUse}
          </span>
        </label>

        <label className="block text-[12.5px]">
          <span className="font-medium">Instructions</span>
          <textarea
            value={draft.instructions}
            onChange={(event) => setDraft({ ...draft, instructions: event.target.value })}
            data-ai-skill-instructions
            rows={8}
            className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] leading-relaxed"
          />
          <span className="mt-0.5 block text-[11.5px] text-muted">
            {draft.instructions.length} / {LIMITS.instructions} — this is data the model reads,
            never code it runs.
          </span>
        </label>

        <label className="block text-[12.5px]">
          <span className="font-medium">Tools it names</span>
          <input
            type="text"
            value={draft.tools}
            onChange={(event) => setDraft({ ...draft, tools: event.target.value })}
            data-ai-skill-tools
            placeholder="comma separated, e.g. page.search"
            className="mt-1 w-full rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
          <span className="mt-0.5 block text-[11.5px] text-muted">
            A relevance list, not a grant: naming a tool here does not give the agent the
            tool.
          </span>
        </label>

        <label className="flex items-center gap-2 text-[12.5px]">
          <input
            type="checkbox"
            checked={draft.enabled}
            onChange={(event) => setDraft({ ...draft, enabled: event.target.checked })}
            data-ai-skill-enabled
          />
          Enabled
        </label>

        <div className="flex justify-end gap-2 pt-1">
          <button
            type="button"
            data-ai-skill-validate
            onClick={() => void validate()}
            className="mr-auto rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium"
          >
            Run validation
          </button>
          <button
            type="button"
            onClick={onCancel}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium"
          >
            Cancel
          </button>
          <button
            type="button"
            data-ai-skill-save
            disabled={busy}
            onClick={() => void save()}
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-50"
          >
            {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
            {skill ? "Save" : "Create skill"}
          </button>
        </div>
      </div>
    </div>
  );
}
