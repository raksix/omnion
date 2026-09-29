"use client";

/**
 * The movement ledger (REQ-053, slice 2): `/inventory/movements`.
 *
 * ## The one thing this screen must not have
 *
 * **An edit control.** Not a disabled one, not a hidden one — no pencil, no delete, no "void".
 * The ledger is append-only and the only way to fix a mistake is another movement, which is the
 * same rule the API enforces with a `405` for every method it does not implement. A row that
 * offered a pencil would promise something the server will refuse, and a promise the server
 * breaks is worse than an absence: the operator finds out by losing an afternoon.
 *
 * ## What the columns are for
 *
 * `on_hand_after` is on every row, not only in the detail panel, because the question a ledger
 * exists to answer is "what was on this shelf after that happened" and hiding it behind a
 * disclosure makes everybody open every row to find out. The quantity is signed **and** coloured,
 * and the sign is in the text: colour alone fails the people who cannot see it, and the text
 * alone is a column of dashes and positives that nobody scans.
 *
 * ## Why the export button is above the table
 *
 * It hands the current filters to the server. Building a file from the fifty rows already on
 * screen would export the page, not the query, and the criterion asks for the two to match.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { Download, Loader2, Lock } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  exportMovementsCsv,
  fetchMovements,
  fetchWarehouses,
  type Location,
  type Movement,
  type MovementFilters,
} from "@/lib/inventory";

import {
  AdjustDrawer,
  QuantityCell,
  RelativeTime,
  SignedQuantity,
  type AdjustResult,
  type AdjustTarget,
} from "./inventory-parts";

const COLUMNS = 8;

/** The kinds the drawer can produce, in the module's own vocabulary. */
const KINDS: { value: string; label: string }[] = [
  { value: "receipt", label: "Receipt" },
  { value: "issue", label: "Issue" },
  { value: "adjustment", label: "Adjustment" },
  { value: "transfer_out", label: "Transfer out" },
  { value: "transfer_in", label: "Transfer in" },
  { value: "reserve", label: "Reserved" },
  { value: "release", label: "Released" },
];

/** The kind's badge. A transfer's two halves are labelled apart on purpose: "Transfer" on both
 *  rows of a pair tells the reader nothing about which direction the stock went. */
function KindBadge({ kind }: { kind: Movement["kind"] }) {
  const tone =
    kind === "receipt" || kind === "transfer_in" || kind === "release"
      ? "border-emerald-200 bg-emerald-50 text-emerald-900"
      : kind === "issue" || kind === "transfer_out" || kind === "reserve"
        ? "border-amber-200 bg-amber-50 text-amber-900"
        : "border-stone-300 bg-stone-50 text-stone-800";
  return (
    <span className={`inline-block rounded border px-1.5 py-0.5 text-[11.5px] ${tone}`}>
      {KINDS.find((entry) => entry.value === kind)?.label ?? kind}
    </span>
  );
}

export function MovementsView() {
  const [search, setSearch] = useState("");
  const [kinds, setKinds] = useState<string[]>([]);
  const [locationId, setLocationId] = useState("");
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [rows, setRows] = useState<Movement[]>([]);
  const [locations, setLocations] = useState<Location[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [adjust, setAdjust] = useState<AdjustTarget | null>(null);
  const [exporting, setExporting] = useState(false);

  const filters = useMemo<MovementFilters>(
    () => ({
      search: search.trim() || undefined,
      // An empty `kinds` is **no filter**, not "match nothing": a screen that starts empty and
      // shows nothing is indistinguishable from a screen with a bug.
      kinds: kinds.length ? kinds : undefined,
      location_id: locationId || undefined,
      from: from || undefined,
      to: to || undefined,
      limit: 100,
    }),
    [search, kinds, locationId, from, to],
  );

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [ledger, tree] = await Promise.all([fetchMovements(filters), fetchWarehouses()]);
      setRows(ledger.items);
      setLocations(tree.flatMap((warehouse) => warehouse.locations));
    } catch (caught) {
      setError(toScreenError(caught, "The ledger could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [filters]);

  useEffect(() => {
    void load();
  }, [load]);

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
      await exportMovementsCsv(filters);
    } catch (caught) {
      setError(toScreenError(caught, "The export could not be produced."));
    } finally {
      setExporting(false);
    }
  }

  const toggleKind = (value: string) =>
    setKinds((current) =>
      current.includes(value) ? current.filter((entry) => entry !== value) : [...current, value],
    );

  return (
    <div className="flex flex-col gap-4">
      <header className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-lg font-semibold">Movements</h1>
          <p className="inline-flex items-center gap-1.5 text-[12.5px] text-muted">
            <Lock aria-hidden className="h-3 w-3" />
            Every row is permanent. A mistake is fixed by recording another movement.
          </p>
        </div>
        <button
          type="button"
          data-qa-inventory-movements-export
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

      <div className="flex flex-wrap items-end gap-3" data-qa-inventory-movements-filters>
        <label className="text-[12px] font-medium">
          Search
          <input
            data-qa-inventory-movements-search
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="SKU, item or note"
            className="mt-1 w-52 rounded border px-2 py-1.5 text-[13px]"
          />
        </label>
        <label className="text-[12px] font-medium">
          Location
          <select
            value={locationId}
            onChange={(event) => setLocationId(event.target.value)}
            className="mt-1 rounded border px-2 py-1.5 text-[13px]"
          >
            <option value="">All</option>
            {locations.map((location) => (
              <option key={location.id} value={location.id}>
                {location.warehouse_code}/{location.code} — {location.name}
              </option>
            ))}
          </select>
        </label>
        <label className="text-[12px] font-medium">
          From
          <input
            type="date"
            value={from}
            onChange={(event) => setFrom(event.target.value)}
            className="mt-1 rounded border px-2 py-1.5 text-[13px]"
          />
        </label>
        <label className="text-[12px] font-medium">
          To
          <input
            type="date"
            value={to}
            onChange={(event) => setTo(event.target.value)}
            className="mt-1 rounded border px-2 py-1.5 text-[13px]"
          />
        </label>
        <fieldset className="text-[12px] font-medium">
          <legend>Kind</legend>
          <div className="mt-1 flex flex-wrap gap-1.5">
            {KINDS.map((option) => (
              <button
                key={option.value}
                type="button"
                data-qa-inventory-kind={option.value}
                aria-pressed={kinds.includes(option.value)}
                onClick={() => toggleKind(option.value)}
                className={`rounded-full border px-2 py-0.5 text-[12px] ${
                  kinds.includes(option.value) ? "border-stone-900 bg-stone-900 text-white" : ""
                }`}
              >
                {option.label}
              </button>
            ))}
          </div>
        </fieldset>
      </div>

      {notice ? (
        <p
          data-qa-inventory-movements-notice
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
          title="No movements match"
          hint="Record a receipt to see the first row. The ledger is append-only — it is never rewritten."
        />
      ) : (
        <div className="overflow-x-auto" data-qa-inventory-movements-table>
          <table className="w-full text-left text-[13px]">
            <thead className="border-b text-[12px] uppercase tracking-wide text-muted">
              <tr>
                <th className="px-2 py-2">When</th>
                <th className="px-2 py-2">Item</th>
                <th className="px-2 py-2">Kind</th>
                <th className="px-2 py-2 text-right">Quantity</th>
                <th className="px-2 py-2">Location</th>
                <th className="px-2 py-2">Reason</th>
                <th className="px-2 py-2 text-right">On hand after</th>
                <th className="px-2 py-2" />
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr key={row.id} className="border-b last:border-b-0" data-qa-inventory-movement={row.id}>
                  <td className="px-2 py-2 text-[12.5px]">
                    <RelativeTime at={row.created_at} />
                  </td>
                  <td className="px-2 py-2">
                    <Link href={`/inventory/items/${row.item_id}`} className="font-medium hover:underline">
                      {row.item_name}
                    </Link>
                    <span className="ml-2 font-mono text-[12px] text-muted">{row.sku}</span>
                    {row.note ? (
                      <span className="ml-2 text-[12px] text-muted">— {row.note}</span>
                    ) : null}
                  </td>
                  <td className="px-2 py-2">
                    <KindBadge kind={row.kind} />
                  </td>
                  <td className="px-2 py-2 text-right">
                    <SignedQuantity movement={row} />
                  </td>
                  <td className="px-2 py-2 text-[12.5px]">{row.location_code}</td>
                  <td className="px-2 py-2 text-[12.5px]">{row.reason.replace(/_/g, " ")}</td>
                  <td className="px-2 py-2 text-right">
                    <QuantityCell value={row.on_hand_after} />
                  </td>
                  <td className="px-2 py-2 text-right">
                    <button
                      type="button"
                      onClick={() =>
                        setAdjust({
                          kind: "item",
                          itemId: row.item_id,
                          sku: row.sku,
                          name: row.item_name,
                          locationId: row.location_id,
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
