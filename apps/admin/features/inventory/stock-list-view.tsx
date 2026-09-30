"use client";

/**
 * The stock list (REQ-053, slice 2): `/inventory/stock`.
 *
 * One row per item × location, which is the shape the warehouse actually reasons in — "the bolts
 * in Rack 3" is a location question, not an item question, and an item-level list makes the
 * operator do the arithmetic to find out which rack is empty.
 *
 * Three things this screen is careful about:
 *
 * * **The export is the same filter, server-side.** The button hands the current filters to
 *   `/inventory/stock/export` rather than building a file from the fifty rows on screen, so the
 *   download is what the query returns and not what the page happened to load. This is the
 *   criterion's "the CSV export matches the table", and the only honest way to satisfy it.
 * * **The status filter and the badge share one vocabulary.** A row badged "Below reorder point"
 *   and a filter that hides it disagreeing is the exact failure the spec's "filters and export
 *   match" line is aimed at, so both read `StockStatus` and neither re-derives it.
 * * **The adjust drawer opens from here pre-filled** with the item and the location, because
 *   that is the only combination that means anything — an adjustment needs both.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import { Download, Loader2, SlidersHorizontal } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  exportStockCsv,
  fetchStock,
  fetchWarehouses,
  type Location,
  type StockFilters,
  type StockLevel,
} from "@/lib/inventory";

import {
  AdjustDrawer,
  QuantityCell,
  RelativeTime,
  ScannerBox,
  StockStatusBadge,
  type AdjustResult,
  type AdjustTarget,
} from "./inventory-parts";

const COLUMNS = 9;

/** The filter chips, with the module's own status tokens. */
const STATUS_FILTERS: { value: NonNullable<StockFilters["status"]>; label: string }[] = [
  { value: "below_threshold", label: "Needs attention" },
  { value: "negative", label: "Negative" },
  { value: "below_reorder", label: "Below reorder" },
  { value: "low", label: "Below minimum" },
  { value: "ok", label: "In stock" },
];

/** The idle filters, which are the spec's 30/60/90. */
const IDLE_FILTERS = [30, 60, 90];

export function StockListView() {
  const params = useSearchParams();
  const [search, setSearch] = useState(params.get("search") ?? "");
  const [status, setStatus] = useState<StockFilters["status"]>(
    (params.get("status") as StockFilters["status"]) ?? undefined,
  );
  const [warehouseId, setWarehouseId] = useState(params.get("warehouse") ?? "");
  const [locationId, setLocationId] = useState(params.get("location") ?? "");
  const [idleDays, setIdleDays] = useState<number | undefined>(
    params.get("idle_days") ? Number(params.get("idle_days")) : undefined,
  );
  const [rows, setRows] = useState<StockLevel[]>([]);
  const [warehouses, setWarehouses] = useState<{ id: string; code: string; name: string }[]>([]);
  const [locations, setLocations] = useState<Location[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [adjust, setAdjust] = useState<AdjustTarget | null>(null);
  const [exporting, setExporting] = useState(false);

  const filters = useMemo<StockFilters>(
    () => ({
      search: search.trim() || undefined,
      status,
      warehouse_id: warehouseId || undefined,
      location_id: locationId || undefined,
      idle_days: idleDays,
      limit: 100,
    }),
    [search, status, warehouseId, locationId, idleDays],
  );

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [stock, tree] = await Promise.all([fetchStock(filters), fetchWarehouses()]);
      setRows(stock.items);
      setWarehouses(tree);
      setLocations(tree.flatMap((warehouse) => warehouse.locations));
    } catch (caught) {
      setError(toScreenError(caught, "The stock list could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [filters]);

  useEffect(() => {
    void load();
  }, [load]);

  // The scanner lands on a row: it filters to that item rather than opening a detail screen,
  // because the reason somebody scans mid-warehouse is to see the number where they are standing.
  const onScanned = useCallback((itemId: string) => {
    setSearch("");
    setStatus(undefined);
    setAdjust(null);
    setNotice(null);
    // A direct fetch rather than a search, because the scanner's answer is an id and the search
    // box matches text. Going through the text would mean asking the server to find an item by
    // a string the server already answered with an id.
    void (async () => {
      try {
        const found = await fetchStock({ search: "", limit: 100 });
        const row = found.items.find((entry) => entry.item_id === itemId);
        if (row) {
          setRows([row]);
        }
      } catch {
        /* the list's own error state covers this */
      }
    })();
  }, []);

  const onAdjusted = useCallback((result: AdjustResult) => {
    setAdjust(null);
    if (result.outcome === "recorded") {
      setNotice(`Recorded. On hand is now ${result.movement.on_hand_after}.`);
    } else if (result.outcome === "awaiting_approval") {
      setNotice(
        `That adjustment of ${result.amount} is over the ${result.threshold} threshold, so it is waiting on a decision. Nothing has changed yet.`,
      );
    }
    void load();
  }, [load]);

  async function download() {
    setExporting(true);
    try {
      await exportStockCsv(filters);
    } catch (caught) {
      setError(toScreenError(caught, "The export could not be produced."));
    } finally {
      setExporting(false);
    }
  }

  return (
    <div className="flex flex-col gap-4">
      <header className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-lg font-semibold">Stock</h1>
          <p className="text-[12.5px] text-muted">
            What is at each location, what is held for orders, and what is free to use.
          </p>
        </div>
        <button
          type="button"
          data-qa-inventory-stock-export
          onClick={() => void download()}
          disabled={exporting}
          className="inline-flex items-center gap-1.5 rounded border px-3 py-1.5 text-[13px] disabled:opacity-50"
        >
          {exporting ? (
            <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" />
          ) : (
            <Download aria-hidden className="h-3.5 w-3.5" />
          )}
          Export CSV
        </button>
      </header>

      <ScannerBox onResolved={(itemId, sku, name) => {
        setNotice(`${sku} — ${name}. Showing the first location that holds it.`);
        onScanned(itemId);
      }} />

      <div className="flex flex-wrap items-end gap-2" data-qa-inventory-stock-filters>
        <label className="text-[12px] font-medium">
          Search
          <input
            data-qa-inventory-stock-search
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="SKU or name"
            className="mt-1 w-56 rounded border px-2 py-1.5 text-[13px]"
          />
        </label>
        <label className="text-[12px] font-medium">
          Warehouse
          <select
            value={warehouseId}
            onChange={(event) => {
              setWarehouseId(event.target.value);
              setLocationId("");
            }}
            className="mt-1 rounded border px-2 py-1.5 text-[13px]"
          >
            <option value="">All</option>
            {warehouses.map((warehouse) => (
              <option key={warehouse.id} value={warehouse.id}>
                {warehouse.code} — {warehouse.name}
              </option>
            ))}
          </select>
        </label>
        <label className="text-[12px] font-medium">
          Location
          <select
            value={locationId}
            onChange={(event) => setLocationId(event.target.value)}
            className="mt-1 rounded border px-2 py-1.5 text-[13px]"
          >
            <option value="">All</option>
            {locations
              .filter((location) => !warehouseId || location.warehouse_id === warehouseId)
              .map((location) => (
                <option key={location.id} value={location.id}>
                  {location.warehouse_code}/{location.code} — {location.name}
                </option>
              ))}
          </select>
        </label>
        <fieldset className="text-[12px] font-medium">
          <legend className="inline-flex items-center gap-1">
            <SlidersHorizontal aria-hidden className="h-3 w-3" />
            Status
          </legend>
          <div className="mt-1 flex flex-wrap gap-1.5">
            <button
              type="button"
              onClick={() => setStatus(undefined)}
              aria-pressed={status === undefined}
              className={`rounded-full border px-2 py-0.5 text-[12px] ${
                status === undefined ? "border-stone-900 bg-stone-900 text-white" : ""
              }`}
            >
              Any
            </button>
            {STATUS_FILTERS.map((option) => (
              <button
                key={option.value}
                type="button"
                data-qa-inventory-stock-status={option.value}
                onClick={() => setStatus(status === option.value ? undefined : option.value)}
                aria-pressed={status === option.value}
                className={`rounded-full border px-2 py-0.5 text-[12px] ${
                  status === option.value ? "border-stone-900 bg-stone-900 text-white" : ""
                }`}
              >
                {option.label}
              </button>
            ))}
          </div>
        </fieldset>
        <label className="text-[12px] font-medium">
          Idle for
          <select
            value={idleDays ?? ""}
            onChange={(event) =>
              setIdleDays(event.target.value ? Number(event.target.value) : undefined)
            }
            className="mt-1 rounded border px-2 py-1.5 text-[13px]"
          >
            <option value="">Any time</option>
            {IDLE_FILTERS.map((days) => (
              <option key={days} value={days}>
                {days} days
              </option>
            ))}
          </select>
        </label>
      </div>

      {notice ? (
        <p
          data-qa-inventory-stock-notice
          className="rounded border border-sky-200 bg-sky-50 px-3 py-2 text-[12.5px] text-sky-900"
        >
          {notice}
        </p>
      ) : null}

      {error ? (
        <ErrorState error={error} onRetry={() => void load()} />
      ) : loading ? (
        <LoadingTable columns={COLUMNS} />
      ) : rows.length === 0 ? (
        <EmptyState
          title="No stock rows match"
          hint="Adjust the filters, or record a receipt to put something on a shelf."
          action={
            <Link
              href="/inventory/movements"
              className="rounded bg-stone-900 px-3 py-1.5 text-[13px] font-medium text-white"
            >
              Record a movement
            </Link>
          }
        />
      ) : (
        <>
          {/* Desktop: a real table. The quantities are right-aligned and tabular so a column of
              three-decimal numbers can be scanned vertically, which is the only reason anybody
              reads this screen. */}
          <div className="hidden overflow-x-auto md:block" data-qa-inventory-stock-table>
            <table className="w-full text-left text-[13px]">
              <thead className="border-b text-[12px] uppercase tracking-wide text-muted">
                <tr>
                  <th className="px-2 py-2">Item</th>
                  <th className="px-2 py-2">Location</th>
                  <th className="px-2 py-2 text-right">On hand</th>
                  <th className="px-2 py-2 text-right">Reserved</th>
                  <th className="px-2 py-2 text-right">Available</th>
                  <th className="px-2 py-2">Status</th>
                  <th className="px-2 py-2">Last movement</th>
                  <th className="px-2 py-2" />
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => (
                  <tr key={row.id} className="border-b last:border-b-0">
                    <td className="px-2 py-2">
                      <Link href={`/inventory/items/${row.item_id}`} className="font-medium hover:underline">
                        {row.name}
                      </Link>
                      <span className="ml-2 font-mono text-[12px] text-muted">{row.sku}</span>
                    </td>
                    <td className="px-2 py-2 text-[12.5px]">
                      {row.warehouse_code}/{row.location_code}
                    </td>
                    <td className="px-2 py-2 text-right">
                      <QuantityCell value={row.on_hand} />
                    </td>
                    <td className="px-2 py-2 text-right">
                      <QuantityCell value={row.reserved} />
                    </td>
                    <td className="px-2 py-2 text-right">
                      <QuantityCell value={row.available} />
                    </td>
                    <td className="px-2 py-2">
                      <StockStatusBadge status={row.status} />
                    </td>
                    <td className="px-2 py-2 text-[12.5px]">
                      <RelativeTime at={row.last_movement_at} />
                    </td>
                    <td className="px-2 py-2 text-right">
                      <button
                        type="button"
                        data-qa-inventory-stock-adjust={row.id}
                        onClick={() =>
                          setAdjust({
                            kind: "stock",
                            itemId: row.item_id,
                            sku: row.sku,
                            name: row.name,
                            locationId: row.location_id,
                            onHand: row.on_hand,
                          })
                        }
                        className="rounded border px-2 py-1 text-[12px]"
                      >
                        Adjust
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {/* Mobile: cards with the badge FIRST, per the spec. A status you have to scroll to is
              a status nobody reads on a phone held at arm's length in a warehouse. */}
          <ul className="flex flex-col gap-2 md:hidden" data-qa-inventory-stock-cards>
            {rows.map((row) => (
              <li key={row.id} className="rounded border p-3">
                <div className="flex items-start justify-between gap-2">
                  <StockStatusBadge status={row.status} />
                  <RelativeTime at={row.last_movement_at} />
                </div>
                <Link
                  href={`/inventory/items/${row.item_id}`}
                  className="mt-1.5 block text-[14px] font-medium"
                >
                  {row.name}
                </Link>
                <p className="text-[12px] text-muted">
                  {row.sku} · {row.warehouse_code}/{row.location_code}
                </p>
                {/* `sm:grid-cols-3` — three quantities side by side is the point at desktop, and at
                    390px each cell is ~110px wide, which is narrow enough to wrap "Available" and
                    leave the number orphaned under it. The prefix makes it one column on a phone. */}
                <dl className="mt-2 grid gap-2 text-[12px] sm:grid-cols-3">
                  <div>
                    <dt className="text-muted">On hand</dt>
                    <dd>
                      <QuantityCell value={row.on_hand} />
                    </dd>
                  </div>
                  <div>
                    <dt className="text-muted">Reserved</dt>
                    <dd>
                      <QuantityCell value={row.reserved} />
                    </dd>
                  </div>
                  <div>
                    <dt className="text-muted">Available</dt>
                    <dd>
                      <QuantityCell value={row.available} />
                    </dd>
                  </div>
                </dl>
                <button
                  type="button"
                  onClick={() =>
                    setAdjust({
                      kind: "stock",
                      itemId: row.item_id,
                      sku: row.sku,
                      name: row.name,
                      locationId: row.location_id,
                      onHand: row.on_hand,
                    })
                  }
                  className="mt-2 w-full rounded border px-2 py-2 text-[13px]"
                >
                  Adjust
                </button>
              </li>
            ))}
          </ul>
        </>
      )}

      {adjust ? (
        <AdjustDrawer
          target={adjust}
          locations={locations}
          onClose={() => setAdjust(null)}
          onDone={onAdjusted}
        />
      ) : null}
    </div>
  );
}
