"use client";

/**
 * The Skills tab (docs/requests/REQ-099, slice 3).
 *
 * A skill is text that ends up in a prompt. This tab's job is to make that visible and
 * reversible, and four decisions follow from that:
 *
 * 1. **Attached is not the same as injected.** Every row states which of the three it is —
 *    injected, disabled, missing from the registry, or checksum-mismatched. A tab that
 *    rendered "3 skills" over a prompt carrying one would be lying in a way nobody could
 *    debug from a run transcript.
 *
 * 2. **The assembled prompt is shown, not reconstructed.** The API returns the exact block the
 *    runtime adds, in the exact order. A client that re-derived it from the rows could be
 *    right about every row and still display an order the runtime never used — and when two
 *    skills disagree, order is the whole answer.
 *
 * 3. **Reorder works without a pointer.** Up/down buttons sit beside the drag handle, because
 *    "drag to reorder" is not a keyboard path and not a touch path, and the spec asks for both
 *    a keyboard and a mobile story for the same control.
 *
 * 4. **A skill naming a tool the agent lacks is allowed, and said out loud.** The attach
 *    response reports the mismatch and the row keeps a warning. Refusing would be wrong (the
 *    operator may be about to grant the tool); saying nothing would produce a run that ignores
 *    half its instructions with no visible cause.
 */
import { useCallback, useEffect, useState } from "react";

import { ArrowDown, ArrowUp, Plus, TriangleAlert, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiAgentSkill,
  type AiAgentSkills,
  type AiSkill,
  attachAiAgentSkill,
  detachAiAgentSkill,
  fetchAiAgentSkills,
  fetchAiSkills,
  setAiAgentSkills,
} from "@/lib/api";

/** The Skills tab. */
export function AgentSkills({
  agentId,
  organizationId,
}: {
  agentId: string;
  organizationId?: string | null;
}) {
  const [tab, setTab] = useState<AiAgentSkills | null>(null);
  const [registry, setRegistry] = useState<AiSkill[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [picking, setPicking] = useState(false);
  const [showPrompt, setShowPrompt] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      // Both in one pass: the attached list decides what the rows say, the registry decides
      // what the picker offers. Fetching them separately lets the picker list a skill the
      // tab has already reported as missing.
      const [attached, available] = await Promise.all([
        fetchAiAgentSkills(agentId, organizationId),
        fetchAiSkills({ organizationId, enabledOnly: true }),
      ]);
      setTab(attached);
      setRegistry(available.skills);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, [agentId, organizationId]);

  useEffect(() => {
    void load();
  }, [load]);

  /** Write the whole order, so a drag and a button produce the same request. */
  const persist = useCallback(
    async (keys: string[]) => {
      setBusy(true);
      setError(null);
      try {
        setTab(await setAiAgentSkills(agentId, keys, organizationId));
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : String(cause));
      } finally {
        setBusy(false);
      }
    },
    [agentId, organizationId],
  );

  const move = useCallback(
    (index: number, delta: number) => {
      if (!tab) return;
      const keys = tab.skills.map((skill) => skill.key);
      const target = index + delta;
      // No wrap-around: a wrap makes "move up" on the first row land somewhere nobody aimed
      // at, which is a worse surprise than a disabled button.
      if (target < 0 || target >= keys.length) return;
      [keys[index], keys[target]] = [keys[target], keys[index]];
      void persist(keys);
    },
    [tab, persist],
  );

  const attach = useCallback(
    async (key: string) => {
      setBusy(true);
      setError(null);
      try {
        const result = await attachAiAgentSkill(agentId, key, organizationId);
        setPicking(false);
        await load();
        if (result.tools_not_in_agent.length > 0) {
          // A warning, not a refusal — but said out loud, because a skill that says "use
          // web.search" on an agent that cannot is a mismatch somebody would otherwise find
          // by reading a transcript.
          setError(
            `Attached, but this agent cannot call ${result.tools_not_in_agent.join(", ")} — the skill will ask for a tool it does not hold.`,
          );
        }
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : String(cause));
      } finally {
        setBusy(false);
      }
    },
    [agentId, organizationId, load],
  );

  const detach = useCallback(
    async (key: string) => {
      setBusy(true);
      setError(null);
      try {
        await detachAiAgentSkill(agentId, key, organizationId);
        await load();
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : String(cause));
      } finally {
        setBusy(false);
      }
    },
    [agentId, organizationId, load],
  );

  if (loading) {
    return <LoadingTable columns={4} rows={3} />;
  }

  const attached = tab?.skills ?? [];
  const attachedKeys = new Set(attached.map((skill) => skill.key));
  const attachable = registry.filter((skill) => !attachedKeys.has(skill.key));

  return (
    <div data-agent-skills className="space-y-4">
      {error ? (
        <p
          data-agent-skills-error
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

      {attached.length === 0 ? (
        <EmptyState
          title="No skill attached"
          hint="A skill is a short piece of written guidance the model gets with every run — when to use it and what a good answer looks like. It is data, never code, and it grants no tool the agent does not already hold."
          action={
            <button
              type="button"
              data-agent-skills-add
              onClick={() => setPicking((open) => !open)}
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
            >
              <Plus className="size-3.5" aria-hidden />
              Add skill
            </button>
          }
        />
      ) : (
        <div className="overflow-x-auto rounded-xl border border-line">
          <table className="w-full min-w-[720px] text-left text-[12.5px]">
            <thead className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
              <tr>
                <th scope="col" className="px-3.5 py-2.5">Order</th>
                <th scope="col" className="px-3.5 py-2.5">Skill</th>
                <th scope="col" className="px-3.5 py-2.5">When to use</th>
                <th scope="col" className="px-3.5 py-2.5">Tools</th>
                <th scope="col" className="px-3.5 py-2.5">State</th>
                <th scope="col" className="px-3.5 py-2.5"><span className="sr-only">Actions</span></th>
              </tr>
            </thead>
            <tbody>
              {attached.map((skill, index) => (
                <SkillRow
                  key={skill.key}
                  skill={skill}
                  first={index === 0}
                  last={index === attached.length - 1}
                  busy={busy}
                  onMove={(delta) => move(index, delta)}
                  onDetach={() => void detach(skill.key)}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}

      {picking ? (
        <div data-agent-skills-picker className="rounded-xl border border-line p-3.5">
          <div className="mb-2.5 flex items-center justify-between">
            <h3 className="text-[12.5px] font-medium">Add a skill</h3>
            <button
              type="button"
              onClick={() => setPicking(false)}
              aria-label="Close the skill picker"
              className="text-muted hover:text-ink"
            >
              <X className="size-3.5" aria-hidden />
            </button>
          </div>
          {attachable.length === 0 ? (
            <p className="text-[12.5px] text-muted">
              Every enabled skill is already attached. Create more in the registry.
            </p>
          ) : (
            <ul className="space-y-1.5">
              {attachable.map((skill) => (
                <li key={skill.key} className="flex items-start justify-between gap-3">
                  <div className="min-w-0">
                    <p className="font-medium">{skill.name}</p>
                    <p className="text-muted">{skill.description}</p>
                  </div>
                  <button
                    type="button"
                    data-agent-skills-attach={skill.key}
                    disabled={busy}
                    onClick={() => void attach(skill.key)}
                    className="shrink-0 rounded-lg border border-line px-2.5 py-1 font-medium disabled:opacity-50"
                  >
                    Attach
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      ) : attached.length > 0 ? (
        <button
          type="button"
          data-agent-skills-add
          onClick={() => setPicking((open) => !open)}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium"
        >
          <Plus className="size-3.5" aria-hidden />
          Add skill
        </button>
      ) : null}

      {tab?.prompt_block ? (
        <div data-agent-skills-prompt>
          <button
            type="button"
            onClick={() => setShowPrompt((open) => !open)}
            aria-expanded={showPrompt}
            className="text-[12.5px] text-muted underline underline-offset-2 hover:text-ink"
          >
            {showPrompt ? "Hide" : "Show"} the assembled prompt
          </button>
          {showPrompt ? (
            <pre className="mt-2 max-h-96 overflow-auto whitespace-pre-wrap rounded-xl border border-line bg-subtle p-3.5 text-[12px] leading-relaxed">
              {tab.prompt_block}
            </pre>
          ) : null}
        </div>
      ) : attached.length > 0 ? (
        <p data-agent-skills-nothing-injected className="text-[12.5px] text-muted">
          Nothing is injected: every attached skill is withheld, so the next run's prompt is
          unchanged.
        </p>
      ) : null}
    </div>
  );
}

/** One attached skill. */
function SkillRow({
  skill,
  first,
  last,
  busy,
  onMove,
  onDetach,
}: {
  skill: AiAgentSkill;
  first: boolean;
  last: boolean;
  busy: boolean;
  onMove: (delta: number) => void;
  onDetach: () => void;
}) {
  return (
    <tr
      data-agent-skill={skill.key}
      data-injected={skill.injected ? "true" : "false"}
      className="border-b border-line last:border-0"
    >
      <td className="px-3.5 py-2.5 align-top">
        {/* Up/down rather than drag alone: a drag is neither a keyboard path nor a touch
            path, and the spec asks for both for this control. */}
        <div className="flex items-center gap-1">
          <button
            type="button"
            data-agent-skill-up={skill.key}
            aria-label={`Move ${skill.name} up`}
            disabled={first || busy}
            onClick={() => onMove(-1)}
            className="rounded p-1 text-muted hover:text-ink disabled:opacity-30 disabled:hover:text-muted"
          >
            <ArrowUp className="size-3.5" aria-hidden />
          </button>
          <button
            type="button"
            data-agent-skill-down={skill.key}
            aria-label={`Move ${skill.name} down`}
            disabled={last || busy}
            onClick={() => onMove(1)}
            className="rounded p-1 text-muted hover:text-ink disabled:opacity-30 disabled:hover:text-muted"
          >
            <ArrowDown className="size-3.5" aria-hidden />
          </button>
        </div>
      </td>
      <td className="px-3.5 py-2.5 align-top">
        <p className="font-medium">{skill.name}</p>
        <p className="text-muted">
          <code className="text-[11.5px]">{skill.key}</code>
          {skill.version > 0 ? <span> · v{skill.version}</span> : null}
          {skill.source ? <span> · {skill.source === "built_in" ? "built-in" : "custom"}</span> : null}
        </p>
      </td>
      <td className="px-3.5 py-2.5 align-top text-muted">{skill.when_to_use}</td>
      <td className="px-3.5 py-2.5 align-top text-muted">
        {skill.tools.length === 0 ? "—" : skill.tools.join(", ")}
      </td>
      <td className="px-3.5 py-2.5 align-top">
        {skill.injected ? (
          <span data-agent-skill-state={skill.key} className="text-positive">
            Injected
          </span>
        ) : (
          <span
            data-agent-skill-state={skill.key}
            data-withheld={skill.withheld_code ?? "unknown"}
            className="flex items-start gap-1 text-warning"
          >
            <TriangleAlert className="mt-px size-3.5 shrink-0" aria-hidden />
            {skill.withheld_reason}
          </span>
        )}
      </td>
      <td className="px-3.5 py-2.5 align-top text-right">
        <button
          type="button"
          data-agent-skill-detach={skill.key}
          disabled={busy}
          onClick={onDetach}
          className="text-muted underline underline-offset-2 hover:text-danger disabled:opacity-50"
        >
          Detach
        </button>
      </td>
    </tr>
  );
}
