"use client";

/**
 * The Changes tab of `/environments/{id}` (REQ-017, slice 2, with slice 3's selection).
 *
 * The tab answers the question the whole environment exists to answer: **what does staging hold
 * that production does not?** and, since slice 3, the follow-up: *which of those do I want to
 * ship?* So the tab is a diff table with checkboxes, and the primary action is
 * `Promote selection`.
 *
 * Three decisions here are load-bearing:
 *
 * - **A checkbox is offered only where promotion can accept the row.** The API freezes a row
 *   whatever its kind, and the conflict check runs at approve, so every row is selectable — but the
 *   *select-all* header checkbox selects only the rows currently visible after the filters, which
 *   is the one place a bulk action silently operating on more than the operator can see would be
 *   a real defect. The count of what is selected is always on the button, so the operator knows
 *   the exact scope before the dialog opens.
 * - **"Promote all" is the same dialog as "promote selection", with an empty selection.** The API
 *   treats an empty list as "everything that differs", so the dialog's own counts tell the truth
 *   either way and the primary button's label is the only difference. That is deliberate: one
 *   dialog, one place the frozen summary is read, and no second code path that can disagree.
 * - **The change set is re-read after a write, never patched locally.** After a request or an
 *   approval staging is (or will be) out of step with what the tab holds, and a table that shows
 *   stale rows next to a fresh count is the difference between "here is what will ship" and "here
 *   is what once was true".
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { CheckSquare, RefreshCw, Square, UploadCloud } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError, fetchEnvironmentChanges } from "@/lib/api";
import type { ChangeItem, ChangeSetResponse } from "@/lib/types";
import { PromotionDialog } from "./promotion-dialog";

/** Filters the table can narrow by. All of them are read from the URL by the caller. */
export type ChangeFilters = {
  /** `added`, `updated`, `deleted` or empty for all. */
  kind?: string;
  /** Free text over the title and the slug. */
  search?: string;
};

export function ChangesTab({
  environmentId,
  environmentName,
  filters,
  onPromoted,
}: {
  /** The staging environment. */
  environmentId: string;
  /** Its name — the dialog's typed confirmation asks for it. */
  environmentName: string;
  /** The URL's filters. */
  filters: ChangeFilters;
  /** Called after a promotion changed something, so the header badge can refresh. */
  onPromoted: () => void;
}) {
  const [data, setData] = useState<ChangeSetResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    let cancelled = false;
    fetchEnvironmentChanges(environmentId)
      .then((next) => {
        if (!cancelled) {
          setData(next);
          setError(null);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setData(null);
          setError(
            cause instanceof ApiError ? cause.message : "The changes could not be read.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [environmentId, reloadToken]);

  // A selection is a set of ids; after a re-read the ids that are no longer in the change set are
  // dropped, because a selection that carries a row the table no longer shows is a selection the
  // operator cannot see and cannot uncheck.
  useEffect(() => {
    if (data === null) {
      return;
    }
    const known = new Set(data.items.map((row) => row.page_id));
    setSelected((current) => {
      const next = current.filter((id) => known.has(id));
      return next.length === current.length ? current : next;
    });
  }, [data]);

  const visible = useMemo(() => {
    if (data === null) {
      return [];
    }
    const kind = filters.kind ?? "";
    const search = (filters.search ?? "").trim().toLowerCase();
    return data.items.filter((row) => {
      if (kind !== "" && row.kind !== kind) {
        return false;
      }
      if (search === "") {
        return true;
      }
      return (
        (row.title ?? "").toLowerCase().includes(search) || row.slug.toLowerCase().includes(search)
      );
    });
  }, [data, filters.kind, filters.search]);

  const allVisibleSelected =
    visible.length > 0 && visible.every((row) => selected.includes(row.page_id));

  const toggleAll = useCallback(() => {
    setSelected((current) => {
      if (allVisibleSelected) {
        const visibleIds = new Set(visible.map((row) => row.page_id));
        return current.filter((id) => !visibleIds.has(id));
      }
      const known = new Set(visible.map((row) => row.page_id));
      return [...new Set([...current, ...known])];
    });
  }, [allVisibleSelected, visible]);

  const toggleOne = useCallback((id: string) => {
    setSelected((current) =>
      current.includes(id) ? current.filter((value) => value !== id) : [...current, id],
    );
  }, []);

  if (error) {
    return (
      <p role="alert" data-changes-error className="rounded-xl border border-accent/30 bg-accent-soft px-4 py-2.5 text-[12.5px] text-accent-strong">
        {error}
      </p>
    );
  }

  if (data === null) {
    return <LoadingTable columns={5} rows={4} />;
  }

  if (data.empty) {
    return (
      <EmptyState
        title="No changes since the clone"
        hint="Staging and production hold the same pages right now. Edit a page in staging and it will appear here with its author and timestamp."
      />
    );
  }

  return (
    <section className="flex flex-col gap-3" data-changes-tab>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap gap-2 text-[12px] text-muted" data-changes-counts>
          <span className="rounded-full bg-canvas px-2.5 py-1">
            {data.items.length} item{data.items.length === 1 ? "" : "s"}
          </span>
          <span className="rounded-full bg-positive-soft px-2.5 py-1 text-positive">
            {data.added} added
          </span>
          <span className="rounded-full bg-canvas px-2.5 py-1">
            {data.updated} updated
          </span>
          <span className="rounded-full bg-accent-soft px-2.5 py-1 text-accent-strong">
            {data.deleted} deleted
          </span>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={reload}
            data-changes-reload
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12px] transition hover:bg-canvas"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Re-read
          </button>
          <button
            type="button"
            onClick={() => setDialogOpen(true)}
            disabled={visible.length === 0}
            data-changes-promote
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
          >
            <UploadCloud className="size-3.5" aria-hidden />
            {selected.length === 0
              ? `Promote all ${data.items.length}`
              : `Promote selection (${selected.length})`}
          </button>
        </div>
      </div>

      {selected.length > 0 ? (
        <p className="text-[11.5px] text-muted" data-changes-selection-note>
          {selected.length} of {data.items.length} selected. The promotion freezes exactly these
          rows, so anything you leave unchecked is not published.
        </p>
      ) : null}

      <div className="overflow-x-auto rounded-xl border border-line bg-surface">
        <table className="w-full text-left text-[12.5px]">
          <thead className="border-b border-line text-[11px] tracking-wide text-muted uppercase">
            <tr>
              <th scope="col" className="w-10 px-3 py-2.5">
                <button
                  type="button"
                  onClick={toggleAll}
                  data-changes-select-all
                  aria-label={allVisibleSelected ? "Deselect all" : "Select all"}
                  className="rounded p-1 text-muted transition hover:text-ink"
                >
                  {allVisibleSelected ? (
                    <CheckSquare className="size-3.5" aria-hidden />
                  ) : (
                    <Square className="size-3.5" aria-hidden />
                  )}
                </button>
              </th>
              <th scope="col" className="px-4 py-2.5 font-medium">
                Item
              </th>
              <th scope="col" className="px-4 py-2.5 font-medium">
                Change
              </th>
              <th scope="col" className="px-4 py-2.5 font-medium">
                Last change
              </th>
            </tr>
          </thead>
          <tbody>
            {visible.map((row) => (
              <ChangeRow
                key={row.page_id}
                row={row}
                selected={selected.includes(row.page_id)}
                onToggle={toggleOne}
              />
            ))}
          </tbody>
        </table>
      </div>

      {visible.length === 0 ? (
        <p className="text-[12.5px] text-muted" data-changes-filtered-empty>
          The filters match none of the {data.items.length} changed items. Clear them to see the
          whole set.
        </p>
      ) : null}

      {dialogOpen ? (
        <PromotionDialog
          environmentId={environmentId}
          environmentName={environmentName}
          changes={data.items}
          selection={selected}
          onClose={() => setDialogOpen(false)}
          onChanged={() => {
            setSelected([]);
            reload();
            onPromoted();
          }}
        />
      ) : null}
    </section>
  );
}

/** One row: a checkbox, the page, the kind and who moved it. */
function ChangeRow({
  row,
  selected,
  onToggle,
}: {
  row: ChangeItem;
  selected: boolean;
  onToggle: (id: string) => void;
}) {
  return (
    <tr data-change-row={row.page_id} data-change-kind={row.kind} data-change-selected={selected}>
      <td className="px-3 py-2.5">
        <input
          type="checkbox"
          checked={selected}
          onChange={() => onToggle(row.page_id)}
          data-change-select={row.page_id}
          aria-label={`Select ${row.title ?? row.slug}`}
          className="size-3.5"
        />
      </td>
      <td className="px-4 py-2.5">
        <span className="font-medium text-ink">{row.title ?? row.slug}</span>
        <span className="ml-1.5 font-mono text-[11px] text-muted">/{row.slug}</span>
      </td>
      <td className="px-4 py-2.5">
        <span
          className={`rounded-full px-2 py-0.5 text-[11px] ${
            row.kind === "added"
              ? "bg-positive-soft text-positive"
              : row.kind === "deleted"
                ? "bg-accent-soft text-accent-strong"
                : "bg-canvas text-muted"
          }`}
        >
          {row.kind}
        </span>
      </td>
      <td className="px-4 py-2.5 text-muted">
        {row.changed_by ?? "unknown author"}
        <span className="ml-1.5 text-[11px]">{new Date(row.changed_at).toLocaleString()}</span>
      </td>
    </tr>
  );
}
