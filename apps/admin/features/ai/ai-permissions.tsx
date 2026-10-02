"use client";

/**
 * `/ai/permissions` — the tool × (agents, identities) matrix (docs/requests/REQ-100, slice 3
 * screen, shipped with slice 2's API).
 *
 * This is the one screen in the AI hub that is a *grid*, and every hard decision on it comes
 * from that fact:
 *
 * 1. **Two column kinds, one vocabulary.** An agent's column is two-state (the agent's
 *    allow-list either names the tool or does not) and an identity's column is three-state. They
 *    are drawn with the same glyph set and the same legend, because an operator scanning the
 *    grid has to learn one notation — and a grid where allow means two different things is a
 *    grid nobody trusts.
 *
 * 2. **A cell the viewer cannot change is disabled *and named*.** The API sends
 *    `viewer_missing[tool_key]`, so the cell says "`content.publish` is missing" rather than
 *    being greyed out with no explanation. The same change is refused with a `403` by the write
 *    path, so the disabled state is a promise the API keeps.
 *
 * 3. **Only the agent's own list is editable from here, and only when the agent's author
 *    decides it.** The identity columns are read-only on this screen — the identity editor at
 *    `/ai/identities` is where a grant is a deliberate act with a save, and a grid where every
 *    click writes immediately is a grid where a mis-click is a permission change. Each column
 *    links to where it *is* edited rather than duplicating a second write path, which would be
 *    exactly the "second door" the request forbids.
 *
 * 4. **Mobile is an accordion, not a scrolling grid.** The spec asks for a per-agent list of
 *    tools with a three-state control each, and the reason is legible: a 20-column grid on a
 *    phone is a horizontal scroll that hides the row it is meant to be triaging.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";

import { ChevronDown, ChevronRight, Loader2, Pause, Search, TriangleAlert } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { type AiGrantEffect, type AiPermissionMatrix, fetchAiPermissionMatrix } from "@/lib/api";

import { EFFECT_GLYPH, EFFECT_LABEL, EFFECT_TONE } from "./ai-identities";

/** The class grouping order, shared with the identity editor so the two screens read alike. */
const CLASS_ORDER = ["content", "media", "users", "sites", "themes", "plugins", "ops"];

/** A read-only cell. It renders the same glyph vocabulary as the editor, which is the point:
 *  the legend on this screen is the legend the operator learned there. */
function Cell({
  effect,
  missing,
}: {
  effect: AiGrantEffect;
  missing?: string[];
}) {
  const title = missing?.length
    ? `You are missing ${missing.join(", ")} — this cell cannot be changed here`
    : EFFECT_LABEL[effect];
  return (
    <span
      title={title}
      data-cell={effect}
      className={`inline-flex min-w-[2.25rem] items-center justify-center rounded px-1.5 py-0.5 text-xs ${
        missing?.length ? "opacity-40" : EFFECT_TONE[effect]
      }`}
    >
      <span aria-hidden="true">{EFFECT_GLYPH[effect]}</span>
      <span className="sr-only">{missing?.length ? title : EFFECT_LABEL[effect]}</span>
    </span>
  );
}

export function AiPermissionsView({ organizationId }: { organizationId?: string | null }) {
  const [matrix, setMatrix] = useState<AiPermissionMatrix | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [q, setQ] = useState("");
  const [classFilter, setClassFilter] = useState("all");
  const [differencesOnly, setDifferencesOnly] = useState(false);
  const [openColumn, setOpenColumn] = useState<string | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      setMatrix(await fetchAiPermissionMatrix(organizationId));
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setMatrix(null);
    }
  }, [organizationId]);

  useEffect(() => {
    void load();
  }, [load]);

  const needle = q.trim().toLowerCase();

  const tools = useMemo(() => {
    if (matrix === null) return [];
    return matrix.tools.filter((tool) => {
      if (classFilter !== "all" && tool.class !== classFilter) return false;
      if (needle && !tool.key.toLowerCase().includes(needle) && !tool.description.toLowerCase().includes(needle)) {
        return false;
      }
      if (!differencesOnly) return true;
      // "Only differences from the default" means: at least one column carries a real decision
      // that is not inherit, or at least one agent names the tool. A row where nothing is
      // decided anywhere is exactly the row this filter is meant to remove.
      const decided =
        matrix.identities.some(
          (column) => (column.grants[tool.key] ?? "inherit") !== "inherit",
        ) || matrix.agents.some((column) => column.tools.includes(tool.key));
      return decided;
    });
  }, [matrix, classFilter, needle, differencesOnly]);

  const grouped = useMemo(() => {
    const byClass = new Map<string, typeof tools>();
    for (const tool of tools) {
      const list = byClass.get(tool.class) ?? [];
      list.push(tool);
      byClass.set(tool.class, list);
    }
    return CLASS_ORDER.filter((name) => byClass.has(name)).map((name) => ({
      name,
      tools: byClass.get(name) ?? [],
    }));
  }, [tools]);

  /** The columns, in one list: agents first (they are per-agent decisions), then identities. */
  const columns = useMemo(() => {
    if (matrix === null) return [];
    return [
      ...matrix.agents.map((agent) => ({
        id: agent.id,
        name: agent.name,
        subtitle: agent.key,
        kind: "agent" as const,
        platform: false,
        readOnly: true,
        cell: (toolKey: string) => (agent.tools.includes(toolKey) ? "allow" : "inherit"),
        gated: (toolKey: string) => agent.approvals.includes(toolKey),
      })),
      ...matrix.identities.map((identity) => ({
        id: identity.id,
        name: identity.name,
        subtitle: identity.key,
        kind: "identity" as const,
        platform: identity.platform_level,
        readOnly: true,
        cell: (toolKey: string) => identity.grants[toolKey] ?? "inherit",
        gated: () => false,
      })),
    ];
  }, [matrix]);

  if (matrix === null && error === null) return <LoadingTable columns={5} />;
  if (matrix === null) {
    return (
      <div
        role="alert"
        className="flex flex-wrap items-center justify-between gap-3 rounded-md border border-rose-500/40 bg-rose-500/5 p-3 text-sm"
      >
        <span>{error}</span>
        {/* The retry the other two screens in this wave carry. This one rendered the message and
            stopped, which is the dead end the acceptance criterion names: a failed matrix fetch
            left the operator staring at a red sentence with no way back except a full page
            reload. The error string is kept rather than replaced by a generic "try again" because
            a 403 has to stay legible — it means the viewer lacks a permission, and retrying will
            not change that. */}
        <button
          type="button"
          onClick={() => void load()}
          className="shrink-0 rounded-md border px-2 py-1 text-xs"
        >
          Retry
        </button>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4" data-ai-permissions>
      <div className="flex flex-wrap items-center gap-2">
        <label className="flex items-center gap-1.5 text-[12.5px]">
          <Search className="size-3.5 text-muted-foreground" />
          <span className="sr-only">Filter tools</span>
          <input
            value={q}
            onChange={(event) => setQ(event.target.value)}
            placeholder="Filter tools"
            className="rounded-md border px-2.5 py-1.5 text-sm"
          />
        </label>
        <select
          value={classFilter}
          aria-label="Filter by class"
          onChange={(event) => setClassFilter(event.target.value)}
          className="rounded-md border px-2 py-1.5 text-xs"
        >
          <option value="all">All classes</option>
          {CLASS_ORDER.map((name) => (
            <option key={name} value={name}>
              {name}
            </option>
          ))}
        </select>
        <label className="flex items-center gap-1.5 text-[12.5px]">
          <input
            type="checkbox"
            checked={differencesOnly}
            onChange={(event) => setDifferencesOnly(event.target.checked)}
          />
          <span>Only rows with a decision</span>
        </label>
        <button
          type="button"
          onClick={() => void load()}
          className="ml-auto rounded-md border px-2 py-1.5 text-xs"
        >
          Reload
        </button>
      </div>

      {/* The legend. The spec asks for one, and it is load-bearing: a glyph-only matrix is
          unreadable to anyone who cannot distinguish the colours, and this screen is about
          making a permission decision responsibly. */}
      <div className="flex flex-wrap items-center gap-3 text-[12px] text-muted-foreground">
        <span className="font-medium text-foreground">Legend</span>
        {(["allow", "deny", "inherit"] as const).map((effect) => (
          <span key={effect} className="inline-flex items-center gap-1">
            <Cell effect={effect} />
            {EFFECT_LABEL[effect]}
          </span>
        ))}
        <span>Agents show only their own allow-list; identities carry the tri-state.</span>
      </div>

      {columns.length === 0 ? (
        <EmptyState
          title="No agent or identity to place in the matrix"
          hint="Create an agent or an AI identity and its column appears here automatically."
          action={
            <Link
              href="/ai/identities"
              className="rounded-md border px-3 py-1.5 text-sm"
            >
              Go to identities
            </Link>
          }
        />
      ) : tools.length === 0 ? (
        <EmptyState
          title="No tool matches these filters"
          hint="Clear the search or the class filter to see the whole registry."
        />
      ) : (
        <>
          {/* Desktop: the grid. The header is a real table head so a screen reader announces the
              column, and the required permission rides on the row label rather than in a
              tooltip nobody on a touch screen can reach. */}
          <div className="hidden overflow-x-auto lg:block">
            <table className="w-full border-collapse text-left text-[12.5px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] text-muted-foreground">
                  <th className="sticky left-0 bg-background px-3 py-2 font-medium">Tool</th>
                  {columns.map((column) => (
                    <th key={column.id} className="px-2 py-2 font-medium" title={column.subtitle}>
                      <span className="block max-w-[9rem] truncate">{column.name}</span>
                      <span className="block text-[10.5px] font-normal opacity-70">
                        {column.kind}
                        {column.platform ? " · shared" : ""}
                      </span>
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {grouped.map((group) => (
                  <>
                    <tr key={`group-${group.name}`} className="bg-quiet-soft">
                      <td
                        className="sticky left-0 bg-quiet-soft px-3 py-1.5 text-[11.5px] font-medium uppercase tracking-wide"
                        colSpan={columns.length + 1}
                      >
                        {group.name}
                      </td>
                    </tr>
                    {group.tools.map((tool) => (
                      <tr
                        key={tool.key}
                        data-matrix-tool={tool.key}
                        className={`border-t border-line ${
                          tool.ungated_high_risk ? "border-l-2 border-l-rose-500" : ""
                        }`}
                      >
                        <td className="sticky left-0 bg-background px-3 py-2">
                          <span className="font-mono text-[12px]">{tool.key}</span>
                          <span className="block text-[11px] text-muted-foreground">
                            needs {tool.permission}
                            {tool.requires_approval ? " · gated" : ""}
                            {!tool.enabled ? " · disabled" : ""}
                          </span>
                        </td>
                        {columns.map((column) => (
                          <td key={column.id} className="px-2 py-2">
                            <Cell
                              effect={column.cell(tool.key)}
                              missing={matrix.viewer_missing[tool.key]}
                            />
                            {column.gated(tool.key) && (
                              <span
                                title="This agent parks the run for a person instead of running it"
                                className="ml-1 inline-flex align-middle text-violet-600 dark:text-violet-400"
                              >
                                {/*
                                  A Lucide icon rather than a ⏸ glyph: the emoji-shaped character
                                  renders at a different weight and baseline on every platform and
                                  cannot inherit `currentColor` reliably, so the marker that means
                                  "a person must approve this" ended up a different size from the
                                  tri-state cell it annotates — including as the only symbol on the
                                  screen that no screen reader announced as an image.
                                */}
                                <Pause className="size-3" aria-hidden="true" />
                                <span className="sr-only">gated behind approval</span>
                              </span>
                            )}
                          </td>
                        ))}
                      </tr>
                    ))}
                  </>
                ))}
              </tbody>
            </table>
          </div>

          {tools.filter((tool) => tool.ungated_high_risk).length > 0 && (
            <p className="flex items-start gap-2 rounded-md border border-rose-500/40 bg-rose-500/5 px-3 py-2 text-[12.5px]">
              <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-rose-600" />
              <span>
                {tools.filter((tool) => tool.ungated_high_risk).length} enabled high-risk tool
                {tools.filter((tool) => tool.ungated_high_risk).length === 1 ? "" : "s"} run
                without an approval gate:{" "}
                <span className="font-mono text-xs">
                  {tools
                    .filter((tool) => tool.ungated_high_risk)
                    .map((tool) => tool.key)
                    .join(", ")}
                </span>
              </span>
            </p>
          )}

          {/* Mobile: one accordion per column. The spec asks for exactly this rather than a
              scrollable grid, and the reason survives review: a grid a phone has to scroll
              sideways hides the very cells the operator came to check. */}
          <ul className="flex flex-col gap-2 lg:hidden">
            {columns.map((column) => {
              const open = openColumn === column.id;
              return (
                <li key={column.id} className="rounded-md border" data-matrix-column={column.subtitle}>
                  <button
                    type="button"
                    onClick={() => setOpenColumn(open ? null : column.id)}
                    aria-expanded={open}
                    className="flex w-full items-center justify-between gap-2 p-3 text-left text-sm"
                  >
                    <span>
                      {column.name}
                      <span className="block text-[11.5px] text-muted-foreground">
                        {column.kind}
                        {column.platform ? " · shared" : ""}
                      </span>
                    </span>
                    {open ? (
                      <ChevronDown className="size-4" />
                    ) : (
                      <ChevronRight className="size-4" />
                    )}
                  </button>
                  {open && (
                    <ul className="flex flex-col divide-y divide-line border-t border-line">
                      {grouped.map((group) => (
                        <li key={`${column.id}-${group.name}`}>
                          <p className="bg-quiet-soft px-3 py-1 text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
                            {group.name}
                          </p>
                          <ul>
                            {group.tools.map((tool) => (
                              <li
                                key={tool.key}
                                className="flex items-center justify-between gap-2 px-3 py-2"
                              >
                                <span className="font-mono text-[12px]">{tool.key}</span>
                                <span className="flex items-center gap-1">
                                  {column.gated(tool.key) && (
                                    <span className="text-[10px] text-violet-600 dark:text-violet-400">
                                      gated
                                    </span>
                                  )}
                                  <Cell
                                    effect={column.cell(tool.key)}
                                    missing={matrix.viewer_missing[tool.key]}
                                  />
                                </span>
                              </li>
                            ))}
                          </ul>
                        </li>
                      ))}
                    </ul>
                  )}
                </li>
              );
            })}
          </ul>

          {/* Where each column is actually edited. A read-only grid with a link is the honest
              shape: the second door a write-enabled grid would open is what the request
              forbids, and saying so beats shipping it. */}
          <p className="text-[12px] text-muted-foreground">
            This grid is a read. Change a grant in{" "}
            <Link href="/ai/identities" className="underline underline-offset-2">
              AI identities
            </Link>{" "}
            and an agent&apos;s own allow-list in{" "}
            <Link href="/ai/agents" className="underline underline-offset-2">
              Agents
            </Link>
            .
          </p>
        </>
      )}
    </div>
  );
}

/** Kept so the loading state of the reload path is visible rather than implied. */
export function MatrixReloadHint({ busy }: { busy: boolean }) {
  if (!busy) return null;
  return (
    <p className="text-[12px] text-muted-foreground">
      <Loader2 className="mr-1 inline size-3 animate-spin" />
      Reloading…
    </p>
  );
}
