"use client";

/**
 * The low-stock alert inbox (REQ-053, slice 3): `/inventory/alerts`.
 *
 * Slice 1 shipped the **edge** — `inventory.stock.low` fires on the downward crossing, once, so
 * a busy warehouse does not flood the automation log. What it did not ship is the *record*, and
 * this screen is that record. Four decisions, each a way the inbox could mislead:
 *
 * * **The threshold is printed on the row, and it is the row's own.** A screen that fetched the
 *   item's live threshold would retroactively justify last month's alerts when somebody lowered
 *   the setting; the row carries what it was crossed against, and lowering the setting says
 *   nothing about the past.
 * * **An alert is an episode, not a crossing.** A restock *clears* it, and the next crossing
 *   opens a new one. The inbox therefore answers "is this still true?" — which is the question
 *   somebody working a list at 08:00 actually has — and the closed episodes are one filter away
 *   rather than gone.
 * * **The inbox opens on what is open.** `open_only` is on by default server-side, and a screen
 *   that overrode it would open on a year of closed rows nobody can act on.
 * * **The sweep is a button, and it says what it did.** A write that reports `raised: 2,
 *   cleared: 1` is a write a person can trust; one that just re-renders is a button that might
 *   be doing anything.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { RefreshCw, Search } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  fetchAlerts,
  sweepAlerts,
  type AlertSweep,
  type Quantity,
  type StockAlert,
} from "@/lib/inventory";

import { QuantityCell, RelativeTime } from "./inventory-parts";

/** The filter chips. `open` is first because it is what the inbox is for. */
const FILTERS: { value: string; label: string }[] = [
  { value: "open", label: "Open" },
  { value: "all", label: "All" },
  { value: "low_stock", label: "Low stock" },
  { value: "negative_stock", label: "Negative" },
];

export function AlertsView() {
  const [scope, setScope] = useState("open");
  const [search, setSearch] = useState("");
  const [rows, setRows] = useState<StockAlert[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [sweeping, setSweeping] = useState(false);
  const [swept, setSwept] = useState<AlertSweep | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const page = await fetchAlerts({
        search: search.trim() || undefined,
        // A named kind is a kind filter; `open` and `all` are not kinds, so `open_only` carries
        // the first and neither carries the second. A server that had to know the meaning of
        // "all" would be a server with the list's vocabulary in the module.
        kind: scope === "low_stock" || scope === "negative_stock" ? scope : undefined,
        open_only: scope === "open" ? true : undefined,
        limit: 50,
      });
      setRows(page.items);
    } catch (failure) {
      setError(toScreenError(failure, "The inventory data could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [scope, search]);

  useEffect(() => {
    void load();
  }, [load]);

  const runSweep = async () => {
    setSweeping(true);
    setError(null);
    try {
      const result = await sweepAlerts();
      setSwept(result);
      await load();
    } catch (failure) {
      setError(toScreenError(failure, "The inventory data could not be loaded."));
    } finally {
      setSweeping(false);
    }
  };

  return (
    <div className="space-y-6">
      <header className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-xl font-semibold tracking-tight">Low stock</h1>
          <p className="text-sm text-muted-foreground">
            One alert per crossing. A restock clears it, and the next crossing opens a new one.
          </p>
        </div>
        <div className="flex items-center gap-2">
          {swept ? (
            <p data-qa-inventory-alert-swept className="text-xs text-muted-foreground">
              Examined {swept.examined} · raised <strong>{swept.raised}</strong> · cleared{" "}
              <strong>{swept.cleared}</strong> · {swept.open} open
            </p>
          ) : null}
          <button
            type="button"
            onClick={runSweep}
            disabled={sweeping}
            data-qa-inventory-alert-sweep
            className="inline-flex h-9 items-center gap-2 rounded-md border border-border px-3 text-sm disabled:opacity-60"
          >
            <RefreshCw className={`h-4 w-4 ${sweeping ? "animate-spin" : ""}`} aria-hidden />
            Run the sweep
          </button>
        </div>
      </header>

      <div className="flex flex-wrap items-center gap-2">
        {FILTERS.map((filter) => (
          <button
            key={filter.value}
            type="button"
            data-qa-inventory-alert-filter={filter.value}
            aria-pressed={scope === filter.value}
            onClick={() => setScope(filter.value)}
            className={`h-8 rounded-full border px-3 text-sm ${
              scope === filter.value
                ? "border-primary bg-primary text-primary-foreground"
                : "border-border text-muted-foreground hover:text-foreground"
            }`}
          >
            {filter.label}
          </button>
        ))}
        <label className="ml-auto flex h-8 items-center gap-2 rounded-md border border-border px-2 text-sm">
          <Search className="h-4 w-4 text-muted-foreground" aria-hidden />
          <span className="sr-only">Search alerts</span>
          <input
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="SKU, item or location"
            data-qa-inventory-alert-search
            className="w-56 bg-transparent outline-none"
          />
        </label>
      </div>

      {loading ? (
        <LoadingTable columns={5} rows={4} />
      ) : error ? (
        <ErrorState error={error} onRetry={load} />
      ) : rows.length === 0 ? (
        <EmptyState
          title={scope === "open" ? "Nothing is below its threshold" : "No alerts match"}
          hint={
            scope === "open"
              ? "Every item is above the level it should be at. A crossing will appear here the next time a balance falls."
              : "No alert matches this filter. Widen the scope, or clear the search."
          }
          action={
            <button
              type="button"
              onClick={runSweep}
              disabled={sweeping}
              className="inline-flex h-9 items-center gap-2 rounded-md border border-border px-3 text-sm"
            >
              <RefreshCw className={`h-4 w-4 ${sweeping ? "animate-spin" : ""}`} aria-hidden />
              Run the sweep
            </button>
          }
        />
      ) : (
        <div className="overflow-hidden rounded-lg border border-border">
          <table className="w-full text-sm">
            <caption className="sr-only">Low-stock alerts, newest first</caption>
            <thead className="bg-muted/50 text-left">
              <tr>
                <th scope="col" className="px-3 py-2 font-medium">SKU</th>
                <th scope="col" className="px-3 py-2 font-medium">Item</th>
                <th scope="col" className="px-3 py-2 font-medium">Location</th>
                <th scope="col" className="px-3 py-2 font-medium text-right">Was</th>
                <th scope="col" className="px-3 py-2 font-medium text-right">Against</th>
                <th scope="col" className="px-3 py-2 font-medium">Raised</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr
                  key={row.id}
                  data-qa-inventory-alert-row={row.sku}
                  data-qa-inventory-alert-kind={row.kind}
                  data-qa-inventory-alert-open={row.cleared_at === null ? "true" : "false"}
                  className="border-t border-border"
                >
                  <td className="px-3 py-2">
                    <Link
                      href={`/inventory/items/${row.item_id}`}
                      className="font-mono text-xs underline-offset-2 hover:underline"
                    >
                      {row.sku}
                    </Link>
                  </td>
                  <td className="px-3 py-2">
                    {row.item_name}
                    <span className="ml-2">
                      <KindBadge kind={row.kind} />
                    </span>
                  </td>
                  <td className="px-3 py-2 text-muted-foreground">
                    {row.location_code ?? "Every location"}
                  </td>
                  <td className="px-3 py-2 text-right font-mono text-xs">
                    <QuantityCell value={row.observed as Quantity} />
                  </td>
                  <td className="px-3 py-2 text-right font-mono text-xs text-muted-foreground">
                    <QuantityCell value={row.threshold as Quantity} />
                  </td>
                  <td className="px-3 py-2 text-muted-foreground">
                    <RelativeTime at={row.raised_at} />
                    {row.cleared_at ? (
                      <span className="ml-1 text-xs">(cleared)</span>
                    ) : null}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

/** The kind badge. The word carries the meaning; colour only reinforces it. */
function KindBadge({ kind }: { kind: StockAlert["kind"] }) {
  const negative = kind === "negative_stock";
  return (
    <span
      data-qa-inventory-alert-badge={kind}
      className={`inline-flex items-center rounded-full border px-2 py-0.5 text-xs ${
        negative
          ? "border-destructive/40 text-destructive"
          : "border-amber-500/40 text-amber-700 dark:text-amber-400"
      }`}
    >
      {negative ? "Negative" : "Low"}
    </span>
  );
}
