"use client";

/**
 * The Promotions tab of `/environments/{id}` (REQ-017, slice 3).
 *
 * The tab answers one question the record table cannot: **what is in flight, and what happened to
 * what was asked for?** So it is a history with one row promoted to the top — the live promotion,
 * if there is one — and the rest of it underneath.
 *
 * Two things here are decisions rather than layout:
 *
 * - **A promotion is shown by what it froze, not by what staging holds now.** The counts on a row
 *   are the frozen set's counts. A promotion that requested four items and was approved after
 *   somebody edited staging further is a record of four items; recomputing from the live change
 *   set would describe a deploy that did not happen.
 * - **A pending promotion leads with its conflicts, not with its status badge.** A row that says
 *   `pending approval` and hides the fact that production moved is a row that invites an approve
 *   which will be refused. The conflict count is on the row, in the attention tone, and the row
 *   expands into the item ids.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { AlertTriangle, ChevronDown, ChevronRight, History } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import { ApiError, fetchEnvironmentPromotions, fetchPromotion } from "@/lib/api";
import type { Promotion } from "@/lib/types";

/** Statuses where a promotion has not finished deciding, newest-first at the top. */
const OPEN = new Set(["pending_approval", "approved", "running"]);

export function PromotionsTab({
  environmentId,
  reloadToken,
}: {
  /** The environment whose history this is. */
  environmentId: string;
  /** Bumped by the dialog after a write, so the tab refreshes without a manual reload. */
  reloadToken: number;
}) {
  const [rows, setRows] = useState<Promotion[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetchEnvironmentPromotions(environmentId)
      .then((next) => {
        if (!cancelled) {
          setRows(next);
          setError(null);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setRows([]);
          setError(
            cause instanceof ApiError ? cause.message : "The promotions could not be read.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [environmentId, reloadToken]);

  // The open promotions first. A history where a pending deploy sits below twenty finished ones
  // is a history that gets scrolled past.
  const ordered = useMemo(() => {
    if (rows === null) {
      return [];
    }
    return [...rows].sort((a, b) => {
      const aOpen = OPEN.has(a.status) ? 0 : 1;
      const bOpen = OPEN.has(b.status) ? 0 : 1;
      if (aOpen !== bOpen) {
        return aOpen - bOpen;
      }
      return new Date(b.created_at).getTime() - new Date(a.created_at).getTime();
    });
  }, [rows]);

  const openCount = useMemo(
    () => (rows ?? []).filter((row) => OPEN.has(row.status)).length,
    [rows],
  );

  const toggle = useCallback((id: string) => {
    setExpanded((current) => (current === id ? null : id));
  }, []);

  if (error) {
    return (
      <p role="alert" data-promotions-error className="rounded-xl border border-accent/30 bg-accent-soft px-4 py-2.5 text-[12.5px] text-accent-strong">
        {error}
      </p>
    );
  }

  if (rows === null) {
    return <LoadingTable columns={5} rows={3} />;
  }

  if (rows.length === 0) {
    return (
      <EmptyState
        title="No promotions yet"
        hint="A promotion freezes the changes this environment holds and asks for them to be written to production. Open the Changes tab, select what should ship, and request one."
      />
    );
  }

  return (
    <section className="flex flex-col gap-2" data-promotions-tab>
      <div className="flex flex-wrap items-center gap-2 text-[12px] text-muted">
        <History className="size-3.5" aria-hidden />
        <span data-promotions-count={rows.length}>
          {rows.length} promotion{rows.length === 1 ? "" : "s"}
        </span>
        {openCount > 0 ? (
          <span
            className="rounded-full bg-caution-soft px-2 py-0.5 text-[11px] text-caution"
            data-promotions-open={openCount}
          >
            {openCount} in flight
          </span>
        ) : null}
      </div>
      <div className="overflow-x-auto rounded-xl border border-line bg-surface">
        <table className="w-full text-left text-[12.5px]">
          <thead className="border-b border-line text-[11px] tracking-wide text-muted uppercase">
            <tr>
              <th scope="col" className="px-4 py-2.5 font-medium">
                Requested
              </th>
              <th scope="col" className="px-4 py-2.5 font-medium">
                Status
              </th>
              <th scope="col" className="px-4 py-2.5 font-medium">
                Frozen set
              </th>
              <th scope="col" className="px-4 py-2.5 font-medium">
                Decision
              </th>
              <th scope="col" className="px-4 py-2.5 text-right font-medium">
                Detail
              </th>
            </tr>
          </thead>
          <tbody>
            {ordered.map((row) => (
              <tr key={row.id} data-promotion-row data-promotion-status={row.status}>
                <td className="px-4 py-3 text-muted">
                  {new Date(row.created_at).toLocaleString()}
                </td>
                <td className="px-4 py-3">
                  <StatusBadge status={row.status} />
                  {row.conflicts.length > 0 ? (
                    <p
                      className="mt-1 flex items-center gap-1 text-[11.5px] text-accent-strong"
                      data-promotion-conflict-count={row.conflicts.length}
                    >
                      <AlertTriangle className="size-3" aria-hidden />
                      {row.conflicts.length} conflict
                      {row.conflicts.length === 1 ? "" : "s"}
                    </p>
                  ) : null}
                </td>
                <td className="px-4 py-3">
                  <span className="font-medium text-ink" data-promotion-item-count={row.item_count}>
                    {row.item_count} item{row.item_count === 1 ? "" : "s"}
                  </span>
                  <p className="text-[11.5px] text-muted">
                    {row.added} added · {row.updated} updated · {row.deleted} deleted
                  </p>
                </td>
                <td className="px-4 py-3 text-muted">
                  {row.approved_at ? (
                    <>Approved {new Date(row.approved_at).toLocaleString()}</>
                  ) : (
                    "Awaiting a decision"
                  )}
                  {row.error ? (
                    <p className="mt-0.5 max-w-xs text-[11.5px] text-accent-strong">{row.error}</p>
                  ) : null}
                </td>
                <td className="px-4 py-3 text-right">
                  <button
                    type="button"
                    onClick={() => toggle(row.id)}
                    data-promotion-expand={row.id}
                    aria-expanded={expanded === row.id}
                    aria-label={expanded === row.id ? "Hide the frozen set" : "Show the frozen set"}
                    className="inline-flex items-center gap-1 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12px] transition hover:bg-canvas"
                  >
                    {expanded === row.id ? (
                      <ChevronDown className="size-3.5" aria-hidden />
                    ) : (
                      <ChevronRight className="size-3.5" aria-hidden />
                    )}
                    {expanded === row.id ? "Hide" : "Show"}
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {expanded ? (
        <ExpandedPromotion id={expanded} />
      ) : null}
    </section>
  );
}

/** The expanded row: the frozen items, the conflicts and the step log. */
function ExpandedPromotion({ id }: { id: string }) {
  const [row, setRow] = useState<Promotion | null>(null);
  const [items, setItems] = useState<
    { page_id: string; slug: string; title: string | null; kind: string }[]
  >([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    // The history row carries the counts; the item list and the step log need the record, and it
    // is read once per expansion rather than kept in sync with the list.
    fetchPromotion(id)
      .then((detail) => {
        if (!cancelled) {
          setRow(detail.promotion);
          setItems(detail.changes.items);
          setError(null);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(cause instanceof ApiError ? cause.message : "The promotion could not be read.");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [id]);

  if (error) {
    return (
      <p role="alert" className="rounded-xl border border-accent/30 bg-accent-soft px-4 py-2.5 text-[12.5px] text-accent-strong">
        {error}
      </p>
    );
  }

  if (row === null) {
    return <LoadingTable columns={2} rows={2} />;
  }

  return (
    <div
      className="flex flex-col gap-3 rounded-xl border border-line bg-canvas px-4 py-3"
      data-promotion-expanded={id}
    >
      <div className="flex flex-col gap-1.5">
        <h3 className="text-[12px] font-medium">The frozen set</h3>
        {items.length === 0 ? (
          <p className="text-[12px] text-muted">
            This promotion froze no items, which the API refuses to create — so a record like this
            came from a row written outside the promotion route.
          </p>
        ) : (
          <ul className="flex flex-col gap-1" data-promotion-expanded-items>
            {items.map((item) => (
              <li
                key={item.page_id}
                className="flex items-center justify-between gap-3 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12px]"
              >
                <span className="min-w-0 truncate">
                  {item.title ?? item.slug}
                  <span className="ml-1.5 font-mono text-[11px] text-muted">
                    /{item.slug}
                  </span>
                </span>
                <span className="shrink-0 font-mono text-[11px] text-muted">{item.kind}</span>
              </li>
            ))}
          </ul>
        )}
      </div>

      {row.steps.length > 0 ? (
        <div className="flex flex-col gap-1.5">
          <h3 className="text-[12px] font-medium">Step log</h3>
          <ol className="flex flex-col gap-1" data-promotion-expanded-steps>
            {row.steps.map((step) => (
              <li
                key={`${step.step}-${step.at}`}
                className="flex flex-wrap items-baseline gap-2 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12px]"
              >
                <span className="font-medium text-ink">{step.step}</span>
                <span className="text-muted">{step.detail}</span>
                <span className="ml-auto text-[11px] text-muted">
                  {new Date(step.at).toLocaleString()}
                </span>
              </li>
            ))}
          </ol>
        </div>
      ) : null}

      {row.conflicts.length > 0 ? (
        <div className="flex flex-col gap-1.5">
          <h3 className="text-[12px] font-medium text-accent-strong">Conflicts</h3>
          <ul className="flex flex-col gap-1 font-mono text-[11.5px] text-accent-strong" data-promotion-expanded-conflicts>
            {row.conflicts.map((conflict) => (
              <li key={conflict}>{conflict}</li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}
