"use client";

/**
 * `/blocks` — the block registry reference (REQ-063, slice 1).
 *
 * Everything on this screen comes from the registry document the API answers with. It is not
 * a hand-written catalogue of "the blocks we have": the moment the platform ships a type, it
 * appears here with its schema, because the panel has no list of its own to fall behind. A
 * reference that can disagree with the editor is worse than no reference.
 */
import { useEffect, useMemo, useState } from "react";

import type { BlockRegistry } from "@omnion/types";
import { Boxes, Search } from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError, fetchBlockRegistry } from "@/lib/api";
import { categoryLabel } from "./block-library";

/** The registry, browsable by category and searchable by name. */
export function BlockRegistryView() {
  const [registry, setRegistry] = useState<BlockRegistry | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [open, setOpen] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetchBlockRegistry()
      .then((document_) => {
        if (!cancelled) {
          setRegistry(document_);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(
            cause instanceof ApiError
              ? cause.message
              : "The block registry could not be loaded.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const grouped = useMemo(() => {
    if (!registry) {
      return [];
    }
    const needle = query.trim().toLowerCase();
    const matches = registry.blocks.filter((entry) => {
      if (needle === "") {
        return true;
      }
      return (
        entry.label.toLowerCase().includes(needle) ||
        entry.key.includes(needle) ||
        entry.description.toLowerCase().includes(needle) ||
        entry.props.some((prop) => prop.key.includes(needle))
      );
    });
    return registry.categories
      .map((category) => ({
        category,
        entries: matches.filter((entry) => entry.category === category),
      }))
      .filter((group) => group.entries.length > 0);
  }, [registry, query]);

  if (error) {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="The block registry could not be loaded"
          hint={`${error} The registry is read with content.blocks.read — an account without that key cannot author against it.`}
        />
      </div>
    );
  }

  if (!registry) {
    return <LoadingTable columns={3} />;
  }

  const total = grouped.reduce((sum, group) => sum + group.entries.length, 0);

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3 rounded-xl border border-line bg-surface px-4 py-3">
        <div className="flex items-baseline gap-2">
          <Boxes className="size-4 text-muted" aria-hidden />
          <h2 className="text-[13.5px] font-medium">Block types</h2>
          <span className="text-[12px] text-muted">
            registry v{registry.version} · {registry.blocks.length} types
          </span>
        </div>
        <label className="flex items-center gap-2">
          <span className="sr-only">Search the block registry</span>
          <Search className="size-3.5 text-muted" aria-hidden />
          <input
            id="registry-search"
            name="registry-search"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Search types and props…"
            className="w-56 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
      </div>

      {total === 0 ? (
        <div className="rounded-xl border border-line bg-surface">
          <EmptyState
            title="Nothing matches that search"
            hint="Search a type name, a prop name or a key such as `heading` or `card_grid`."
          />
        </div>
      ) : (
        grouped.map((group) => (
          <section
            key={group.category}
            className="overflow-hidden rounded-xl border border-line bg-surface"
          >
            <h3 className="border-b border-line px-4 py-2.5 text-[13.5px] font-medium">
              {categoryLabel(group.category)}
              <span className="ml-2 text-[12px] font-normal text-muted">
                {group.entries.length}
              </span>
            </h3>
            <ul className="divide-y divide-line">
              {group.entries.map((entry) => {
                const expanded = open === entry.key;
                return (
                  <li key={entry.key}>
                    <button
                      type="button"
                      data-registry-row={entry.key}
                      onClick={() => setOpen(expanded ? null : entry.key)}
                      aria-expanded={expanded}
                      aria-label={`${expanded ? "Hide" : "Show"} the props of ${entry.label}`}
                      className="flex w-full cursor-pointer items-center gap-3 px-4 py-3 text-left transition hover:bg-canvas/60"
                    >
                      <span className="flex min-w-0 flex-col">
                        <span className="flex items-baseline gap-2">
                          <span className="text-[13px] font-medium">{entry.label}</span>
                          <code className="font-mono text-[11px] text-muted">{entry.key}</code>
                          {entry.container ? (
                            <span className="rounded-full bg-accent-soft px-1.5 py-0.5 text-[10.5px] text-accent-strong">
                              container
                            </span>
                          ) : null}
                          {entry.structure_only ? (
                            // The reference has to explain the one type the insert panel hides,
                            // or a reader who finds `column` in a payload and not in the panel
                            // has no way to know whether it is a bug.
                            <span className="rounded-full bg-quiet-soft px-1.5 py-0.5 text-[10.5px] text-muted">
                              not in the insert panel
                            </span>
                          ) : null}
                        </span>
                        <span className="text-[12px] text-muted">{entry.description}</span>
                      </span>
                      <span className="ml-auto shrink-0 text-[11.5px] text-muted">
                        {entry.props.length} prop{entry.props.length === 1 ? "" : "s"}
                      </span>
                    </button>
                    {expanded ? (
                      <div className="border-t border-line bg-canvas/50 px-4 py-3">
                        <table className="w-full border-collapse text-left text-[12.5px]">
                          <thead>
                            <tr className="text-[11px] tracking-wide text-muted uppercase">
                              <th scope="col" className="py-1.5 pr-3">Prop</th>
                              <th scope="col" className="py-1.5 pr-3">Type</th>
                              <th scope="col" className="py-1.5 pr-3">Required</th>
                              <th scope="col" className="py-1.5">Default / options</th>
                            </tr>
                          </thead>
                          <tbody>
                            {entry.props.map((prop) => (
                              <tr key={prop.key} className="border-t border-line/60">
                                <td className="py-1.5 pr-3">
                                  <span className="font-medium">{prop.label}</span>
                                  <code className="ml-1.5 font-mono text-[11px] text-muted">
                                    {prop.key}
                                  </code>
                                </td>
                                <td className="py-1.5 pr-3 text-muted">{prop.type}</td>
                                <td className="py-1.5 pr-3">
                                  {prop.required ? (
                                    <span className="text-accent-strong">yes</span>
                                  ) : (
                                    <span className="text-muted">no</span>
                                  )}
                                </td>
                                <td className="py-1.5 font-mono text-[11.5px] text-muted">
                                  {prop.enum
                                    ? prop.enum.join(" · ")
                                    : JSON.stringify(prop.default)}
                                  {prop.maxLength ? ` (max ${prop.maxLength})` : ""}
                                </td>
                              </tr>
                            ))}
                          </tbody>
                        </table>
                        <p className="mt-2 text-[11.5px] text-muted">
                          Rendered as <code className="font-mono">&lt;{entry.semantic}&gt;</code>
                          {entry.viewport_aware ? " · per-viewport visibility" : ""}
                        </p>
                      </div>
                    ) : null}
                  </li>
                );
              })}
            </ul>
          </section>
        ))
      )}

      <p className="text-[12px] text-muted">
        Blocks are stored on the page's draft revision, so editing a page writes a revision and
        publishing freezes it. <Link href="/pages" className="text-accent-strong underline">Open the pages list</Link>{" "}
        to start editing one.
      </p>
    </div>
  );
}
