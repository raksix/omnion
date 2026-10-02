"use client";

/**
 * `/ai/change-sets` — the proposals waiting to be edited (REQ-101, slice 3).
 *
 * The inbox above it answers "may this one tool call run?". This list answers "which proposed
 * *lists* of operations are still open?", and the row leads to the editor that re-plans them.
 * Three decisions shape it, and all three are inherited from the inbox rather than invented
 * here — the two screens are read by the same person minutes apart:
 *
 * 1. **A failed load is not an empty list.** "No proposals yet" sends a reviewer to stop looking,
 *    so a failure keeps its own wording and its own Retry button.
 *
 * 2. **A viewer without `ai.approvals.act` sees a row that explains itself.** The list carries
 *    `viewer_missing` from the server, so a disabled decision names the key it wants instead of
 *    greying out. *Opening* a set is still `read`: filing and editing a proposal is not
 *    authority over it, and a reader who cannot confirm anything can still read exactly what
 *    they would be asked to approve.
 *
 * 3. **The status tab is the server's filter, not a client-side split.** `fetchAiChangeSets`
 *    sends `status` through, so a count on a tab is the number of rows the API would return for
 *    it — a client-side count over one page cannot promise that.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";

import { ChevronRight, Loader2, Search, TriangleAlert } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiChangeSet,
  type AiChangeSetList,
  fetchAiChangeSets,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The decision key a reviewer needs, named in every disabled control's explanation. */
const DECIDE_KEY = "ai.approvals.act";

const STATUS_TABS = [
  { key: "draft", label: "Draft" },
  { key: "pending", label: "Pending" },
  { key: "confirmed", label: "Confirmed" },
  { key: "applied", label: "Applied" },
  { key: "failed", label: "Failed" },
  { key: "discarded", label: "Discarded" },
  { key: "all", label: "All" },
] as const;

const STATUS_TONE: Record<string, string> = {
  draft: "bg-caution-soft text-caution",
  pending: "bg-accent-soft text-accent-strong",
  confirmed: "bg-accent-soft text-accent-strong",
  applied: "bg-positive-soft text-positive",
  failed: "bg-danger-soft text-danger",
  discarded: "bg-quiet-soft text-muted",
};

function statusLabel(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
}

/** `content_publish` → `Content publish`. The class key stays visible in the editor. */
function resourceLabel(value: string): string {
  const words = value.replace(/_/g, " ").trim();
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/**
 * What a row promises, in one column.
 *
 * Both flags are server-owned and arrive on the row: `editable` is the same list the PATCH's
 * `where` clause runs as, and `needs_approval` is whether confirming parks for a second person.
 * A row that re-derived either from the status string would be a row that lies the moment a
 * status is added.
 */
function summary(set: AiChangeSet): string {
  const kinds = new Set(set.operations.map((op) => op.kind));
  const parts: string[] = [`${set.operations.length} operation${set.operations.length === 1 ? "" : "s"}`];
  if (kinds.size > 0) parts.push([...kinds].sort().join(" · "));
  if (set.needs_approval) parts.push("needs a second person");
  return parts.join(" · ");
}

export function AiChangeSetsListScreen() {
  const [status, setStatus] = useState<string>("draft");
  const [q, setQ] = useState("");
  const [search, setSearch] = useState("");
  const [data, setData] = useState<AiChangeSetList | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(() => {
    setBusy(true);
    setError(null);
    fetchAiChangeSets({ status, q: search, limit: 100 })
      .then(setData)
      .catch((cause: unknown) => {
        setData(null);
        setError(
          cause instanceof ApiError
            ? cause.message
            : "The change sets could not be loaded.",
        );
      })
      .finally(() => setBusy(false));
  }, [status, search]);

  useEffect(load, [load]);

  // Debounced so a paste of a long title is one request, not one per keystroke.
  useEffect(() => {
    const timer = setTimeout(() => setSearch(q.trim()), 300);
    return () => clearTimeout(timer);
  }, [q]);

  const sets = useMemo(() => data?.sets ?? [], [data]);
  const canDecide = useMemo(
    () => !(data?.viewer_missing ?? []).includes(DECIDE_KEY),
    [data],
  );
  const openCount = useMemo(
    () => sets.filter((set) => set.editable).length,
    [sets],
  );

  if (error) {
    return (
      <div data-sets-error className="flex flex-col gap-3">
        <p className="rounded-lg bg-danger-soft px-3 py-2 text-[13px] text-danger">{error}</p>
        <button
          type="button"
          onClick={load}
          className="self-start rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
        >
          Retry
        </button>
      </div>
    );
  }

  if (!data) return <LoadingTable columns={5} rows={4} />;

  return (
    <div data-sets-list className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <nav aria-label="Change set status" className="flex flex-wrap gap-1">
          {STATUS_TABS.map((tab) => (
            <button
              key={tab.key}
              type="button"
              aria-pressed={status === tab.key}
              onClick={() => setStatus(tab.key)}
              className={`rounded-full px-3 py-1 text-[12px] transition ${
                status === tab.key
                  ? "bg-accent-soft text-accent-strong"
                  : "text-muted hover:bg-canvas"
              }`}
            >
              {tab.label}
            </button>
          ))}
        </nav>
        <div className="relative w-full sm:w-64">
          <Search
            aria-hidden
            className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-muted"
            size={15}
          />
          <input
            type="search"
            value={q}
            onChange={(event) => setQ(event.target.value)}
            placeholder="Search by title or resource id"
            aria-label="Search change sets"
            className="w-full rounded-lg border border-line bg-surface py-2 pl-9 pr-3 text-[13px]"
          />
        </div>
      </div>

      {error ? (
        <p className="rounded-lg bg-danger-soft px-3 py-2 text-[13px] text-danger">{error}</p>
      ) : null}

      {busy ? (
        <p className="flex items-center gap-2 text-[12px] text-muted">
          <Loader2 aria-hidden size={14} className="animate-spin" />
          Loading change sets
        </p>
      ) : null}

      {sets.length === 0 ? (
        <EmptyState
          title="No change sets here"
          hint={
            search
              ? "Nothing matches that search. Clear it to see every proposal in this state."
              : "When an agent proposes a set of operations, it lands here for a person to edit and confirm."
          }
          action={
            search ? (
              <button
                type="button"
                onClick={() => setQ("")}
                className="rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
              >
                Clear search
              </button>
            ) : null
          }
        />
      ) : (
        <>
          <p className="text-[12px] text-muted">
            {sets.length} change set{sets.length === 1 ? "" : "s"}
            {openCount > 0 ? ` · ${openCount} still editable` : ""}
            {!canDecide ? ` · you do not hold ${DECIDE_KEY}, so you can read but not confirm` : ""}
          </p>
          {/* One card per set rather than a table: a set carries a variable-length operation
              list, and a row that truncates it to a count cannot show what is about to run. */}
          <ul className="flex flex-col gap-2">
            {sets.map((set) => (
              <li key={set.id}>
                <Link
                  href={`/ai/change-sets/${set.id}`}
                  data-set-row
                  className="group flex items-start gap-3 rounded-xl border border-line bg-surface p-4 transition hover:border-accent"
                >
                  <div className="flex min-w-0 flex-1 flex-col gap-1.5">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="truncate text-[14px] font-medium text-ink">{set.title}</span>
                      <span
                        className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${
                          STATUS_TONE[set.status] ?? "bg-quiet-soft text-muted"
                        }`}
                      >
                        {statusLabel(set.status)}
                      </span>
                      {set.needs_approval ? (
                        <span className="inline-flex items-center gap-1 rounded-full bg-caution-soft px-2 py-0.5 text-[11px] font-medium text-caution">
                          <TriangleAlert aria-hidden size={11} />
                          Gated
                        </span>
                      ) : null}
                      {set.irreversible ? (
                        <span className="inline-flex items-center rounded-full bg-danger-soft px-2 py-0.5 text-[11px] font-medium text-danger">
                          Irreversible
                        </span>
                      ) : null}
                    </div>
                    <p className="text-[12px] text-muted">{summary(set)}</p>
                    <p className="truncate text-[12px] text-muted">
                      {set.operations
                        .slice(0, 4)
                        .map((op) => `${resourceLabel(op.resource_type)} ${op.kind}`)
                        .join(" · ")}
                      {set.operations.length > 4 ? ` · +${set.operations.length - 4} more` : ""}
                    </p>
                    <p className="text-[11px] text-muted">
                      Filed {formatTimestamp(set.created_at)}
                      {set.confirmed_at ? ` · confirmed ${formatTimestamp(set.confirmed_at)}` : ""}
                      {set.applied_at ? ` · applied ${formatTimestamp(set.applied_at)}` : ""}
                    </p>
                    {set.discarded_reason ? (
                      <p className="text-[11px] text-muted">Discarded: {set.discarded_reason}</p>
                    ) : null}
                  </div>
                  <ChevronRight
                    aria-hidden
                    size={16}
                    className="mt-1 shrink-0 text-muted transition group-hover:text-ink"
                  />
                </Link>
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}
