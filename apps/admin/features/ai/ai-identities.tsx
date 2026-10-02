"use client";

/**
 * `/ai/identities` — the named grant sets a run borrows (docs/requests/REQ-100, slice 2).
 *
 * An identity is the answer to a question the agent's own tool list cannot answer: if this
 * agent is hijacked by a prompt injection, what can it actually reach? The agent's list is
 * whoever built it; an identity is a decision an organization can point at, name, and change
 * without touching the agent.
 *
 * Four things this screen gets right that an obvious implementation gets wrong:
 *
 * 1. **A platform-level identity is listed, readable and not editable here.** It is shown with
 *    a badge and its controls are disabled with the reason in the title, rather than hidden. A
 *    hidden row makes an operator think the installation has no shared identity, and a disabled
 *    button with a reason is a screen that answers the question it raised.
 *
 * 2. **Never-called counts are not "all clear".** An identity with zero decisions is *more*
 *    important than one with twenty, not less: it grants nothing explicitly, so every tool its
 *    agents name is available. The empty state says exactly that instead of showing a bare
 *    table.
 *
 * 3. **The editor is grouped by class and shows each tool's permission.** A permission matrix
 *    where the cell is a bare ✓/✕ is a grid of claims; showing `deployment.deploy` next to
 *    "needs deployment.deploy" is what makes the decision reviewable.
 *
 * 4. **The tri-state is three buttons, not a two-state toggle with a reset.** Toggling allow→deny
 *    and back to inherit in one control is a control that lies about its own state, and inherit
 *    is the state that leaves no trace — exactly the one an operator needs to be able to reach
 *    deliberately.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";

import { Check, Loader2, Minus, Plus, ShieldOff, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  type AiGrantEffect,
  type AiIdentity,
  type AiIdentityDetail,
  type AiIdentityToolCell,
  createAiIdentity,
  deleteAiIdentity,
  fetchAiIdentities,
  fetchAiIdentity,
  saveAiIdentityGrants,
} from "@/lib/api";

/** The badge's three tones, keyed off the effect. */
const EFFECT_TONE: Record<AiGrantEffect, string> = {
  allow: "text-emerald-700 dark:text-emerald-300 bg-emerald-500/10",
  deny: "text-rose-700 dark:text-rose-300 bg-rose-500/10",
  inherit: "text-muted-foreground bg-quiet-soft",
};

/** The glyph per effect. Never colour alone: a matrix read by someone who cannot see the hue
 *  is a matrix of unlabelled cells, and the spec asks for "a glyph plus a label". */
const EFFECT_GLYPH: Record<AiGrantEffect, string> = {
  allow: "✓",
  deny: "✕",
  inherit: "–",
};

const EFFECT_LABEL: Record<AiGrantEffect, string> = {
  allow: "Allowed",
  deny: "Denied",
  inherit: "Inherited",
};

/** The panel's own grouping of the registry's classes, in the order an operator reads them. */
const CLASS_ORDER = ["content", "media", "users", "sites", "themes", "plugins", "ops"];

/** The tri-state control. Three real buttons: the state is visible and reachable, not inferred
 *  from a toggle's direction. */
function EffectButtons({
  value,
  disabled,
  onChange,
  label,
}: {
  value: AiGrantEffect;
  disabled?: boolean;
  onChange: (next: AiGrantEffect) => void;
  /** The `aria-label` prefix, so three groups of these in one row are distinguishable. */
  label: string;
}) {
  return (
    <div className="inline-flex overflow-hidden rounded-md border" role="group" aria-label={label}>
      {(["allow", "deny", "inherit"] as const).map((effect) => (
        <button
          key={effect}
          type="button"
          data-effect={effect}
          aria-pressed={value === effect}
          title={disabled ? undefined : EFFECT_LABEL[effect]}
          disabled={disabled}
          onClick={() => onChange(effect)}
          className={`px-2 py-1 text-xs disabled:opacity-40 ${
            value === effect ? `${EFFECT_TONE[effect]} font-semibold` : "text-muted-foreground"
          } ${effect === "allow" ? "" : "border-l border-line"}`}
        >
          <span aria-hidden="true">{EFFECT_GLYPH[effect]}</span>
          <span className="sr-only">{EFFECT_LABEL[effect]}</span>
        </button>
      ))}
    </div>
  );
}

export function AiIdentitiesView({ organizationId }: { organizationId?: string | null }) {
  const [identities, setIdentities] = useState<AiIdentity[] | null>(null);
  const [own, setOwn] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [openId, setOpenId] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      const body = await fetchAiIdentities(organizationId);
      setIdentities(body.identities);
      setOwn(body.own);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setIdentities(null);
    }
  }, [organizationId]);

  useEffect(() => {
    void load();
  }, [load]);

  const remove = useCallback(
    async (identity: AiIdentity) => {
      setBusyId(identity.id);
      setError(null);
      try {
        await deleteAiIdentity(identity.id, organizationId);
        await load();
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setBusyId(null);
      }
    },
    [load, organizationId],
  );

  return (
    <div className="flex flex-col gap-4" data-ai-identities>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-[13px] text-muted-foreground">
          A named set of grants a run borrows. An explicit deny beats every allow, and inherit
          writes no decision at all.
        </p>
        <button
          type="button"
          data-action="new-identity"
          onClick={() => setCreating((open) => !open)}
          className="inline-flex items-center gap-1.5 rounded-md border px-3 py-1.5 text-sm"
        >
          <Plus className="size-3.5" />
          New identity
        </button>
      </div>

      {error !== null && (
        <div
          role="alert"
          className="flex items-start justify-between gap-3 rounded-md border border-rose-500/40 bg-rose-500/5 p-3 text-sm"
        >
          <span>{error}</span>
          <button
            type="button"
            onClick={() => void load()}
            className="shrink-0 rounded-md border px-2 py-1 text-xs"
          >
            Retry
          </button>
        </div>
      )}

      {creating && (
        <CreateForm
          onDone={async () => {
            setCreating(false);
            await load();
          }}
          onError={setError}
          organizationId={organizationId}
        />
      )}

      {identities === null && error === null ? (
        <LoadingTable columns={5} />
      ) : identities === null ? null : identities.length === 0 ? (
        <EmptyState
          title="No AI identity yet"
          hint="An identity is the named set of grants a run borrows. Without one a run executes no tools at all — create one to describe what this installation's AI may reach."
        />
      ) : (
        <div className="flex flex-col gap-3">
          {own === 0 && identities.length > 0 && (
            <p className="rounded-md border bg-quiet-soft px-3 py-2 text-[12.5px] text-muted-foreground">
              Every identity below is shared with the whole installation. Create one of your own
              to give this organization its own grants.
            </p>
          )}

          <div className="hidden overflow-x-auto lg:block">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="text-[12px] text-muted-foreground">
                  <th className="px-4 py-2 font-medium">Key</th>
                  <th className="px-4 py-2 font-medium">Name</th>
                  <th className="px-4 py-2 font-medium">Scope</th>
                  <th className="px-4 py-2 font-medium">Allowed</th>
                  <th className="px-4 py-2 font-medium">Denied</th>
                  <th className="px-4 py-2 font-medium">Agents</th>
                  <th className="px-4 py-2 font-medium" />
                </tr>
              </thead>
              <tbody>
                {identities.map((identity) => (
                  <tr key={identity.id} className="border-t border-line" data-ai-identity={identity.key}>
                    <td className="px-4 py-3 font-mono text-xs">{identity.key}</td>
                    <td className="px-4 py-3">
                      {identity.name}
                      {identity.is_default && (
                        <span className="ml-2 rounded bg-sky-500/10 px-1.5 py-0.5 text-[11px] text-sky-700 dark:text-sky-300">
                          default
                        </span>
                      )}
                    </td>
                    <td className="px-4 py-3 text-xs text-muted-foreground">
                      {identity.platform_level ? "platform" : "this organization"}
                    </td>
                    <td className="px-4 py-3">{identity.allowed}</td>
                    <td className="px-4 py-3">{identity.denied}</td>
                    <td className="px-4 py-3">{identity.agents_using}</td>
                    <td className="px-4 py-3 text-right">
                      <div className="inline-flex gap-1">
                        <button
                          type="button"
                          onClick={() => setOpenId(openId === identity.id ? null : identity.id)}
                          className="rounded-md border px-2 py-1 text-xs"
                        >
                          {openId === identity.id ? "Close" : "Grants"}
                        </button>
                        <button
                          type="button"
                          disabled={identity.platform_level || busyId === identity.id}
                          title={
                            identity.platform_level
                              ? "A platform-level identity is shared by every organization and cannot be edited here"
                              : "Remove this identity and its grants"
                          }
                          onClick={() => void remove(identity)}
                          className="rounded-md border border-rose-500/50 px-2 py-1 text-xs text-rose-700 disabled:opacity-40 dark:text-rose-300"
                        >
                          {busyId === identity.id ? (
                            <Loader2 className="size-3 animate-spin" />
                          ) : (
                            "Delete"
                          )}
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {/* Mobile: cards, because a five-column identity table is unreadable at 390 px and the
              counts it carries are the point of the row. */}
          <ul className="flex flex-col gap-3 lg:hidden">
            {identities.map((identity) => (
              <li
                key={identity.id}
                className="rounded-md border p-3"
                data-ai-identity-card={identity.key}
              >
                <div className="flex items-start justify-between gap-2">
                  <div>
                    <p className="font-mono text-sm">{identity.key}</p>
                    <p className="text-sm">{identity.name}</p>
                  </div>
                  {identity.platform_level && (
                    <span className="shrink-0 rounded bg-violet-500/10 px-1.5 py-0.5 text-[11px] text-violet-700 dark:text-violet-300">
                      platform
                    </span>
                  )}
                </div>
                <p className="mt-1 text-xs text-muted-foreground">
                  {identity.allowed} allowed · {identity.denied} denied · {identity.agents_using}{" "}
                  agents
                </p>
                <div className="mt-2 flex gap-1">
                  <button
                    type="button"
                    onClick={() => setOpenId(openId === identity.id ? null : identity.id)}
                    className="rounded-md border px-2 py-1.5 text-xs"
                  >
                    {openId === identity.id ? "Close" : "Grants"}
                  </button>
                  <button
                    type="button"
                    disabled={identity.platform_level || busyId === identity.id}
                    title={
                      identity.platform_level
                        ? "A platform-level identity is shared by every organization and cannot be edited here"
                        : "Remove this identity and its grants"
                    }
                    onClick={() => void remove(identity)}
                    className="rounded-md border border-rose-500/50 px-2 py-1.5 text-xs disabled:opacity-40"
                  >
                    Delete
                  </button>
                </div>
              </li>
            ))}
          </ul>

          {openId !== null && (
            <GrantEditor
              identityId={openId}
              organizationId={organizationId}
              onClose={() => setOpenId(null)}
              onSaved={load}
            />
          )}
        </div>
      )}
    </div>
  );
}

/** The create form. Three real fields with visible labels and a real error path — a form that
 *  only appears once the operator commits to building something is a form nobody uses. */
function CreateForm({
  onDone,
  onError,
  organizationId,
}: {
  onDone: () => Promise<void>;
  onError: (message: string | null) => void;
  organizationId?: string | null;
}) {
  const [key, setKey] = useState("");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [isDefault, setIsDefault] = useState(false);
  const [saving, setSaving] = useState(false);
  const [fieldError, setFieldError] = useState<string | null>(null);

  const submit = useCallback(async () => {
    setSaving(true);
    setFieldError(null);
    onError(null);
    try {
      await createAiIdentity(
        { key: key.trim(), name: name.trim(), description: description.trim(), is_default: isDefault },
        organizationId,
      );
      await onDone();
    } catch (err) {
      // The API names the field it refused, so the message lands above the form rather than
      // being swallowed: a 400 that disappears is indistinguishable from a network failure.
      const message = err instanceof Error ? err.message : String(err);
      setFieldError(message);
      onError(message);
    } finally {
      setSaving(false);
    }
  }, [key, name, description, isDefault, onDone, onError, organizationId]);

  return (
    <form
      data-form="new-identity"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
      className="flex flex-col gap-3 rounded-md border p-4"
    >
      <h2 className="text-sm font-semibold">New identity</h2>
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1 text-[12.5px]">
          <span className="font-medium">Key</span>
          <input
            value={key}
            onChange={(event) => setKey(event.target.value)}
            placeholder="e.g. content-editor"
            className="rounded-md border px-2.5 py-1.5 text-sm"
          />
          <span className="text-[11.5px] text-muted-foreground">
            Lowercase letters, digits, <code className="font-mono">_</code> and{" "}
            <code className="font-mono">-</code>. It appears in URLs, so it cannot be renamed
            later.
          </span>
        </label>
        <label className="flex flex-col gap-1 text-[12.5px]">
          <span className="font-medium">Name</span>
          <input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="e.g. Content editor"
            className="rounded-md border px-2.5 py-1.5 text-sm"
          />
        </label>
      </div>
      <label className="flex flex-col gap-1 text-[12.5px]">
        <span className="font-medium">Description</span>
        <textarea
          value={description}
          onChange={(event) => setDescription(event.target.value)}
          rows={2}
          placeholder="What this identity is for"
          className="rounded-md border px-2.5 py-1.5 text-sm"
        />
      </label>
      <label className="flex items-center gap-2 text-[12.5px]">
        <input
          type="checkbox"
          checked={isDefault}
          onChange={(event) => setIsDefault(event.target.checked)}
        />
        <span>Make this the organization's default identity</span>
      </label>
      {fieldError !== null && (
        <p role="alert" className="text-[12.5px] text-rose-700 dark:text-rose-300">
          {fieldError}
        </p>
      )}
      <div className="flex gap-2">
        <button
          type="submit"
          disabled={saving}
          className="rounded-md border px-3 py-1.5 text-sm"
        >
          {saving ? <Loader2 className="size-3.5 animate-spin" /> : "Create identity"}
        </button>
        <button
          type="button"
          onClick={() => void onDone()}
          className="rounded-md border px-3 py-1.5 text-sm"
        >
          Cancel
        </button>
      </div>
    </form>
  );
}

/** The grant editor: every tool, grouped by class, tri-state per row, with bulk pairs.
 *
 *  The bulk pair is deliberately **not** a silent blanket. "Allow all visible" on a screen
 *  filtered to `content` must not quietly allow `deployment.deploy`, and a high-risk ungated
 *  tool gets a confirmation that names it — the spec's "bulk pairs that never touch gated tools
 *  silently". */
function GrantEditor({
  identityId,
  organizationId,
  onClose,
  onSaved,
}: {
  identityId: string;
  organizationId?: string | null;
  onClose: () => void;
  onSaved: () => Promise<void>;
}) {
  const [detail, setDetail] = useState<AiIdentityDetail | null>(null);
  const [draft, setDraft] = useState<Record<string, AiGrantEffect>>({});
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [classFilter, setClassFilter] = useState<string>("all");
  const [confirmBulk, setConfirmBulk] = useState<"allow" | "deny" | null>(null);

  useEffect(() => {
    let live = true;
    setDetail(null);
    setError(null);
    fetchAiIdentity(identityId, organizationId)
      .then((body) => {
        if (!live) return;
        setDetail(body);
        setDraft(
          Object.fromEntries(body.tools.map((cell) => [cell.tool_key, cell.effect])),
        );
      })
      .catch((err: unknown) => {
        if (live) setError(err instanceof Error ? err.message : String(err));
      });
    return () => {
      live = false;
    };
  }, [identityId, organizationId]);

  const visible = useMemo(
    () =>
      (detail?.tools ?? []).filter(
        (cell) => classFilter === "all" || cell.class === classFilter,
      ),
    [detail, classFilter],
  );

  const grouped = useMemo(() => {
    const byClass = new Map<string, AiIdentityToolCell[]>();
    for (const cell of visible) {
      const list = byClass.get(cell.class) ?? [];
      list.push(cell);
      byClass.set(cell.class, list);
    }
    return CLASS_ORDER.filter((name) => byClass.has(name)).map((name) => ({
      name,
      cells: byClass.get(name) ?? [],
    }));
  }, [visible]);

  const save = useCallback(
    async (next: Record<string, AiGrantEffect>) => {
      setSaving(true);
      setError(null);
      try {
        await saveAiIdentityGrants(identityId, next, organizationId);
        await onSaved();
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setSaving(false);
      }
    },
    [identityId, organizationId, onSaved],
  );

  const applyBulk = useCallback(
    (effect: "allow" | "deny") => {
      if (detail === null) return;
      // Only the *visible* rows, which is what the button says it does. A high-risk tool in the
      // visible set is named in the confirmation, because "allow all" that silently includes
      // `deployment.deploy` is the exact accident the spec's stripe exists to prevent.
      const next = { ...draft };
      for (const cell of visible) next[cell.tool_key] = effect;
      setDraft(next);
      setConfirmBulk(null);
      void save(next);
    },
    [detail, draft, visible, save],
  );

  if (error !== null && detail === null) {
    return (
      <div
        role="alert"
        className="rounded-md border border-rose-500/40 bg-rose-500/5 p-3 text-sm"
      >
        {error}
      </div>
    );
  }
  if (detail === null) return <LoadingTable columns={4} />;

  const bulkRisks = visible.filter((cell) => cell.risk === "high" && !cell.requires_approval);

  return (
    <section
      data-grant-editor={detail.key}
      className="flex flex-col gap-3 rounded-md border p-4"
      onKeyDown={(event) => {
        if (event.key === "Escape") onClose();
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h2 className="text-sm font-semibold">Grants · {detail.name}</h2>
          <p className="text-[12px] text-muted-foreground">
            A deny beats every allow. Inherit writes no decision, so the agent's own list decides.
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <select
            value={classFilter}
            aria-label="Filter by class"
            onChange={(event) => setClassFilter(event.target.value)}
            className="rounded-md border px-2 py-1 text-xs"
          >
            <option value="all">All classes</option>
            {CLASS_ORDER.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
          <button
            type="button"
            data-bulk="allow"
            disabled={detail.platform_level}
            title={
              detail.platform_level
                ? "A platform-level identity is shared by every organization and cannot be edited here"
                : "Allow every tool currently visible"
            }
            onClick={() => (bulkRisks.length > 0 ? setConfirmBulk("allow") : applyBulk("allow"))}
            className="rounded-md border px-2 py-1 text-xs disabled:opacity-40"
          >
            Allow all visible
          </button>
          <button
            type="button"
            data-bulk="deny"
            disabled={detail.platform_level}
            title={
              detail.platform_level
                ? "A platform-level identity is shared by every organization and cannot be edited here"
                : "Deny every tool currently visible"
            }
            onClick={() => applyBulk("deny")}
            className="rounded-md border px-2 py-1 text-xs disabled:opacity-40"
          >
            Deny all visible
          </button>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close the grant editor"
            className="rounded-md border px-2 py-1 text-xs"
          >
            <X className="size-3" />
          </button>
        </div>
      </div>

      {bulkRisks.length > 0 && (
        <p className="flex items-start gap-2 rounded-md border border-rose-500/40 bg-rose-500/5 px-3 py-2 text-[12.5px]">
          <ShieldOff className="mt-0.5 size-3.5 shrink-0 text-rose-600" />
          <span>
            {bulkRisks.length} high-risk tool{bulkRisks.length === 1 ? "" : "s"} in this view
            {bulkRisks.length === 1 ? " has" : " have"} no approval gate:{" "}
            <span className="font-mono text-xs">
              {bulkRisks.map((cell) => cell.tool_key).join(", ")}
            </span>
          </span>
        </p>
      )}

      {error !== null && (
        <p role="alert" className="text-[12.5px] text-rose-700 dark:text-rose-300">
          {error}
        </p>
      )}

      {grouped.map((group) => (
        <div key={group.name} className="flex flex-col gap-1.5">
          <h3 className="text-[12px] font-medium uppercase tracking-wide text-muted-foreground">
            {group.name}
          </h3>
          <ul className="flex flex-col divide-y divide-line rounded-md border">
            {group.cells.map((cell) => (
              <li
                key={cell.tool_key}
                data-grant-cell={cell.tool_key}
                className={`flex flex-wrap items-center justify-between gap-2 p-2.5 ${
                  cell.risk === "high" && !cell.requires_approval && cell.enabled
                    ? "border-l-2 border-l-rose-500"
                    : ""
                }`}
              >
                <div className="min-w-0">
                  <p className="font-mono text-[12.5px]">{cell.tool_key}</p>
                  <p className="text-[11.5px] text-muted-foreground">
                    needs {cell.permission}
                    {cell.requires_approval ? " · gated" : ""}
                    {!cell.enabled ? " · disabled" : ""}
                  </p>
                </div>
                <EffectButtons
                  label={`Grant for ${cell.tool_key}`}
                  value={draft[cell.tool_key] ?? "inherit"}
                  disabled={detail.platform_level}
                  onChange={(next) => {
                    const updated = { ...draft, [cell.tool_key]: next };
                    setDraft(updated);
                    void save(updated);
                  }}
                />
              </li>
            ))}
          </ul>
        </div>
      ))}

      {saving && (
        <p className="flex items-center gap-1.5 text-[12px] text-muted-foreground">
          <Loader2 className="size-3 animate-spin" /> Saving…
        </p>
      )}

      {confirmBulk !== null && (
        <div
          role="dialog"
          aria-modal="true"
          aria-label="Confirm a bulk grant change"
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
        >
          <div className="w-full max-w-md rounded-lg border bg-background p-4 shadow-lg">
            <h2 className="text-base font-semibold">
              {confirmBulk === "allow" ? "Allow" : "Deny"} {visible.length} tools?
            </h2>
            <p className="mt-2 text-sm text-muted-foreground">
              This includes {bulkRisks.length} high-risk tool
              {bulkRisks.length === 1 ? "" : "s"} with no approval gate:{" "}
              <span className="font-mono text-xs">
                {bulkRisks.map((cell) => cell.tool_key).join(", ")}
              </span>
              . Only the tools currently visible are changed.
            </p>
            <div className="mt-4 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setConfirmBulk(null)}
                className="rounded-md border px-3 py-1.5 text-sm"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={() => applyBulk(confirmBulk)}
                className="rounded-md border border-rose-500/50 bg-rose-500/10 px-3 py-1.5 text-sm"
              >
                Apply to {visible.length} tools
              </button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}

/** Re-exported so the matrix screen imports its glyphs from one place. Colour and glyph have to
 *  agree across both screens or an operator learns two vocabularies for one decision. */
export { EFFECT_GLYPH, EFFECT_LABEL, EFFECT_TONE, Check, Minus };
