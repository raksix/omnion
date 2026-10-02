"use client";

/**
 * The stocktake list, the counting sheet and the variance report (REQ-053, slice 4):
 * `/inventory/stocktake`.
 *
 * The screen is one file with three states because they are **three claims about the same
 * document**, and a person moving between them must not be told a different story:
 *
 * * **The list** answers "is a count running?" — the open ones first, because a count nobody has
 *   finished is a shelf that is not being trusted right now.
 * * **The sheet** answers "what did the rollup claim, and what did the counter see?" — the two
 *   numbers side by side, because a count with only the variance on it cannot be argued with.
 * * **The report** answers "what did we post, and does the ledger agree?" — which is a different
 *   question, asked later, by somebody who was not there.
 *
 * ## The one rule the UI has to earn
 *
 * **An uncounted line must not look like a zero.** The input is left visibly empty with a
 * "not counted" word beside it, because a blank box on a stock sheet reads as zero to everybody
 * who has ever counted a shelf — and a screen that invites that reading will get a warehouse to
 * close a sheet that destroys stock. The close button says how many are left, and the server
 * refuses regardless, so the worst this can cost is a person being told a number they already
 * knew.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { ClipboardCheck, ClipboardList, RefreshCw, Search } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  cancelStocktake,
  closeStocktake,
  countStocktake,
  createStocktake,
  fetchLocations,
  fetchStocktake,
  fetchStocktakeReport,
  fetchStocktakes,
  varianceText,
  varianceTextFromWire,
  varianceTone,
  varianceToneFromWire,
  type Location,
  type Stocktake,
  type StocktakeLine,
  type StocktakeReport,
} from "@/lib/inventory";

import { InventoryModuleNav } from "./module-nav";
import { RelativeTime } from "./inventory-parts";

/** The status chip. One definition, and the sheet's buttons read the same one. */
function StatusBadge({ status }: { status: Stocktake["status"] }) {
  const tone =
    status === "open"
      ? "border-amber-300 bg-amber-50 text-amber-900"
      : status === "closed"
        ? "border-emerald-200 bg-emerald-50 text-emerald-900"
        : "border-border bg-muted/40 text-muted-foreground";
  return (
    <span
      data-qa-inventory-stocktake-status={status}
      className={`inline-flex items-center rounded-full border px-2 py-0.5 text-[11px] ${tone}`}
    >
      {status === "open" ? "Counting" : status === "closed" ? "Closed" : "Cancelled"}
    </span>
  );
}

// -------------------------------------------------------------------------------------------
// The switch
// -------------------------------------------------------------------------------------------

/**
 * The list or the sheet, chosen by `?id=`.
 *
 * **One of the two is rendered, never both.** A screen that showed a list behind a sheet would
 * carry two tables' worth of DOM and two focus orders, and a person tabbing through the page
 * would walk into rows that are not part of what they are doing.
 *
 * The parameter is read from the URL rather than pushed through `router.push`, because a link
 * to a specific count has to work when it is pasted into a chat — a state that only exists
 * inside the app is a state nobody can send to anybody.
 */
export function StocktakeScreen() {
  const [selected, setSelected] = useState<string | null>(null);

  useEffect(() => {
    setSelected(new URLSearchParams(window.location.search).get("id"));
  }, []);

  // Nothing is rendered until the parameter has been read, because rendering the list first and
  // swapping to the sheet a tick later would flash the wrong screen at somebody who followed a
  // link to a count they were told about.
  if (selected === null) {
    return (
      <div className="space-y-4">
        <InventoryModuleNav />
        <LoadingTable columns={7} rows={3} />
      </div>
    );
  }

  return selected ? (
    <StocktakeSheet id={selected} />
  ) : (
    <StocktakesView onOpened={setSelected} />
  );
}

// -------------------------------------------------------------------------------------------
// The list
// -------------------------------------------------------------------------------------------

const SCOPES: { value: string; label: string }[] = [
  { value: "open", label: "Counting" },
  { value: "all", label: "All" },
  { value: "closed", label: "Closed" },
];

export function StocktakesView({ onOpened }: { onOpened: (id: string) => void }) {
  const [scope, setScope] = useState("open");
  const [search, setSearch] = useState("");
  const [rows, setRows] = useState<Stocktake[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [locations, setLocations] = useState<Location[]>([]);
  const [selected, setSelected] = useState<string[]>([]);
  const [opening, setOpening] = useState(false);
  const [formError, setFormError] = useState<ScreenErrorValue>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const page = await fetchStocktakes({
        search: search.trim() || undefined,
        open_only: scope === "open" ? true : undefined,
        // `all` is not a status and `open` is not a status either, so neither goes in `status` —
        // a server that had to know the meaning of the word "all" would have the list's
        // vocabulary inside the module.
        status: scope === "closed" ? "closed" : undefined,
        limit: 50,
      });
      setRows(page.items);
    } catch (failure) {
      setError(toScreenError(failure, "The stocktakes could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [scope, search]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    // The pickers need the organization's locations, and **in-transit is filtered out here** —
    // the server refuses a sheet that includes it, and offering a choice that always fails is
    // worse than not offering it.
    fetchLocations()
      .then((all) => setLocations(all.filter((location) => location.kind !== "in_transit")))
      .catch(() => setLocations([]));
  }, []);

  const toggle = (id: string) =>
    setSelected((current) =>
      current.includes(id) ? current.filter((one) => one !== id) : [...current, id],
    );

  const openSheet = async () => {
    if (selected.length === 0) return;
    setOpening(true);
    setFormError(null);
    try {
      const sheet = await createStocktake({ location_ids: selected });
      // Straight to the sheet: a count that has just been opened is the thing to do next, and
      // landing on a list row that says "ST-0001" is one more click than the moment deserves.
      // The parent's callback writes the id into the URL, so the sheet can be linked to.
      onOpened(sheet.id);
    } catch (failure) {
      setFormError(toScreenError(failure, "The stocktake could not be opened."));
    } finally {
      setOpening(false);
    }
  };

  return (
    <div className="space-y-6">
      <InventoryModuleNav />
      <header className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-xl font-semibold tracking-tight">Stocktake</h1>
          <p className="text-sm text-muted-foreground">
            Count a shelf against what the stock list claimed, and post what the count found.
          </p>
        </div>
        <button
          type="button"
          onClick={load}
          data-qa-inventory-stocktake-refresh
          className="inline-flex h-9 items-center gap-2 rounded-md border border-border px-3 text-sm"
        >
          <RefreshCw className="h-4 w-4" aria-hidden />
          Refresh
        </button>
      </header>

      {/* The scope picker doubles as the create form: a stocktake IS a set of shelves, and
          asking for them here rather than behind a second screen is one click saved per count. */}
      <section
        data-qa-inventory-stocktake-new
        className="rounded-lg border border-border p-4"
        aria-labelledby="stocktake-new-heading"
      >
        <h2 id="stocktake-new-heading" className="text-sm font-medium">
          Count a shelf
        </h2>
        <p className="mt-1 text-xs text-muted-foreground">
          The stock list&apos;s number is written on the sheet when you open it, and a later
          movement will not rewrite it — so what you see is what you were counting against.
        </p>
        {locations.length === 0 ? (
          <p data-qa-inventory-stocktake-no-locations className="mt-3 text-sm text-muted-foreground">
            There is nothing to count yet. An item has to be stocked somewhere before a shelf can
            be counted.
          </p>
        ) : (
          <div className="mt-3 flex flex-wrap gap-2">
            {locations.map((location) => {
              const on = selected.includes(location.id);
              return (
                <button
                  key={location.id}
                  type="button"
                  onClick={() => toggle(location.id)}
                  aria-pressed={on}
                  data-qa-inventory-stocktake-location={location.code}
                  className={`inline-flex h-8 items-center rounded-md border px-2.5 text-sm ${
                    on
                      ? "border-foreground bg-muted font-medium"
                      : "border-border text-muted-foreground hover:text-foreground"
                  }`}
                >
                  {location.code}
                </button>
              );
            })}
            <button
              type="button"
              onClick={openSheet}
              disabled={opening || selected.length === 0}
              data-qa-inventory-stocktake-open
              className="inline-flex h-8 items-center gap-2 rounded-md border border-foreground px-3 text-sm font-medium disabled:opacity-50"
            >
              <ClipboardList className="h-4 w-4" aria-hidden />
              {opening ? "Opening…" : "Open a sheet"}
            </button>
          </div>
        )}
        {formError ? <ErrorState error={formError} onRetry={openSheet} /> : null}
      </section>

      <div className="flex flex-wrap items-center gap-2">
        {SCOPES.map((option) => (
          <button
            key={option.value}
            type="button"
            onClick={() => setScope(option.value)}
            aria-pressed={scope === option.value}
            data-qa-inventory-stocktake-scope={option.value}
            className={`h-8 rounded-md border px-2.5 text-sm ${
              scope === option.value
                ? "border-foreground bg-muted font-medium"
                : "border-border text-muted-foreground"
            }`}
          >
            {option.label}
          </button>
        ))}
        <label className="flex h-8 items-center gap-2 rounded-md border border-border px-2.5 text-sm">
          <Search className="h-4 w-4 text-muted-foreground" aria-hidden />
          <span className="sr-only">Search the stocktakes</span>
          <input
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            data-qa-inventory-stocktake-search
            className="w-40 bg-transparent outline-none"
            placeholder="Number or note"
          />
        </label>
      </div>

      {error ? <ErrorState error={error} onRetry={load} /> : null}

      {loading ? (
        <LoadingTable columns={7} rows={3} />
      ) : rows.length === 0 ? (
        <EmptyState
          title="No stocktakes here"
          hint="Pick a shelf above to count it. A sheet records what the stock list claimed and what you found, and posts the difference when you close it."
        />
      ) : (
        <div className="overflow-x-auto rounded-lg border border-border">
          <table className="w-full min-w-[720px] text-sm">
            <thead className="border-b border-border bg-muted/40 text-left text-xs uppercase tracking-wide text-muted-foreground">
              <tr>
                <th className="px-3 py-2 font-medium">Number</th>
                <th className="px-3 py-2 font-medium">Shelves</th>
                <th className="px-3 py-2 font-medium">Lines</th>
                <th className="px-3 py-2 text-right font-medium">Variances</th>
                <th className="px-3 py-2 text-right font-medium">Total</th>
                <th className="px-3 py-2 font-medium">Status</th>
                <th className="px-3 py-2 font-medium">Opened</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr
                  key={row.id}
                  data-qa-inventory-stocktake={row.number}
                  className="border-b border-border last:border-0"
                >
                  <td className="px-3 py-2.5">
                    <Link
                      href={`/inventory/stocktake?id=${encodeURIComponent(row.id)}`}
                      className="font-medium underline-offset-2 hover:underline"
                    >
                      {row.number}
                    </Link>
                  </td>
                  <td className="px-3 py-2.5 text-muted-foreground">
                    {row.location_codes.join(", ") || "—"}
                  </td>
                  <td className="px-3 py-2.5 text-muted-foreground">
                    {row.lines_counted}
                    {row.lines_pending > 0 ? (
                      <span
                        data-qa-inventory-stocktake-pending
                        className="ml-1.5 text-amber-800"
                      >
                        · {row.lines_pending} not counted
                      </span>
                    ) : null}
                  </td>
                  <td className="px-3 py-2.5 text-right">
                    {row.variances_count}
                  </td>
                  <td
                    className={`px-3 py-2.5 text-right font-medium ${varianceToneFromWire(row.variance_total)}`}
                  >
                    {varianceTextFromWire(row.variance_total)}
                  </td>
                  <td className="px-3 py-2.5">
                    <StatusBadge status={row.status} />
                  </td>
                  <td className="px-3 py-2.5 text-muted-foreground">
                    <RelativeTime at={row.created_at} />
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

// -------------------------------------------------------------------------------------------
// The sheet
// -------------------------------------------------------------------------------------------

/** The sheet, its counting inputs and its two buttons. */
export function StocktakeSheet({ id }: { id: string }) {
  const [sheet, setSheet] = useState<Stocktake | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [actionError, setActionError] = useState<ScreenErrorValue>(null);
  const [counts, setCounts] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [closing, setClosing] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const next = await fetchStocktake(id);
      setSheet(next);
      // The box is seeded from the **server's** number, so a half-typed count survives a reload
      // and a re-count overwrites rather than stacking.
      setCounts(
        Object.fromEntries(
          next.lines
            .filter((line) => line.counted_qty !== null)
            .map((line) => [line.id, line.counted_qty as string]),
        ),
      );
    } catch (failure) {
      setError(toScreenError(failure, "The stocktake could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  // The number the sheet would post, computed from the box as it stands. It is a **preview** of
  // the server's arithmetic and nothing more: the count is not saved until "Save the count" is
  // pressed, and the close posts what the server has.
  const pending = useMemo(() => {
    if (!sheet) return { entries: [], variances: 0, total: 0 };
    const entries = sheet.lines
      .filter((line) => counts[line.id] !== undefined && counts[line.id] !== "")
      .map((line) => ({
        line,
        value: Number.parseFloat(counts[line.id]),
        variance: Number.parseFloat(counts[line.id]) - Number.parseFloat(line.expected_qty),
      }));
    const variances = entries.filter((entry) => Math.abs(entry.variance) > 1e-9);
    return {
      entries,
      variances: variances.length,
      total: variances.reduce((acc, entry) => acc + entry.variance, 0),
    };
  }, [counts, sheet]);

  const save = async () => {
    if (!sheet) return;
    setBusy(true);
    setActionError(null);
    try {
      await countStocktake(
        sheet.id,
        pending.entries.map((entry) => ({ line_id: entry.line.id, quantity: counts[entry.line.id] })),
      );
      await load();
    } catch (failure) {
      setActionError(toScreenError(failure, "The count could not be saved."));
    } finally {
      setBusy(false);
    }
  };

  const close = async () => {
    if (!sheet) return;
    setClosing(true);
    setActionError(null);
    try {
      await closeStocktake(sheet.id);
      await load();
    } catch (failure) {
      setActionError(toScreenError(failure, "The stocktake could not be closed."));
    } finally {
      setClosing(false);
    }
  };

  const cancel = async () => {
    if (!sheet) return;
    setBusy(true);
    setActionError(null);
    try {
      await cancelStocktake(sheet.id);
      await load();
    } catch (failure) {
      setActionError(toScreenError(failure, "The stocktake could not be cancelled."));
    } finally {
      setBusy(false);
    }
  };

  if (error) {
    return (
      <div className="space-y-4">
        <InventoryModuleNav />
        <ErrorState error={error} onRetry={load} />
      </div>
    );
  }

  if (loading || !sheet) {
    return (
      <div className="space-y-4">
        <InventoryModuleNav />
        <LoadingTable columns={7} rows={3} />
      </div>
    );
  }

  const open = sheet.status === "open";

  return (
    <div className="space-y-6">
      <InventoryModuleNav />
      <header className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="flex items-center gap-2 text-xl font-semibold tracking-tight">
            <ClipboardCheck className="h-5 w-5" aria-hidden />
            {sheet.number}
          </h1>
          <p className="text-sm text-muted-foreground">
            {sheet.location_codes.join(", ")}
            {sheet.counted_on ? ` · counted for ${sheet.counted_on}` : null}
            {sheet.category ? ` · ${sheet.category}` : null}
          </p>
        </div>
        <div className="flex items-center gap-2">
          <StatusBadge status={sheet.status} />
          <Link href="/inventory/stocktake" className="text-sm underline underline-offset-2">
            All stocktakes
          </Link>
        </div>
      </header>

      <p className="text-sm text-muted-foreground">
        {open
          ? "Write what is on the shelf. Leave a line empty if you have not looked at it — an uncounted shelf is not an empty one, and the sheet cannot be closed until every line has a number."
          : sheet.closed_at
            ? `Closed ${new Date(sheet.closed_at).toLocaleString()}.`
            : "Withdrawn."}
      </p>

      {actionError ? <ErrorState error={actionError} onRetry={load} /> : null}

      {sheet.lines.length === 0 ? (
        <EmptyState
          title="This shelf is empty"
          hint="Nothing has been stocked at the locations on this sheet, so there is nothing to count. Stock something and open a new count."
        />
      ) : (
        <div className="overflow-x-auto rounded-lg border border-border">
          <table className="w-full min-w-[640px] text-sm">
            <thead className="border-b border-border bg-muted/40 text-left text-xs uppercase tracking-wide text-muted-foreground">
              <tr>
                <th className="px-3 py-2 font-medium">Item</th>
                <th className="px-3 py-2 font-medium">Shelf</th>
                <th className="px-3 py-2 text-right font-medium">Expected</th>
                <th className="px-3 py-2 text-right font-medium">Counted</th>
                <th className="px-3 py-2 text-right font-medium">Variance</th>
              </tr>
            </thead>
            <tbody>
              {sheet.lines.map((line) => (
                <StocktakeRow
                  key={line.id}
                  line={line}
                  open={open}
                  value={counts[line.id] ?? ""}
                  onChange={(next) => setCounts((current) => ({ ...current, [line.id]: next }))}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}

      {open ? (
        <div className="flex flex-wrap items-center gap-3">
          <button
            type="button"
            onClick={save}
            disabled={busy || pending.entries.length === 0}
            data-qa-inventory-stocktake-save
            className="inline-flex h-9 items-center gap-2 rounded-md border border-border px-3 text-sm disabled:opacity-50"
          >
            {busy ? "Saving…" : `Save ${pending.entries.length} count${pending.entries.length === 1 ? "" : "s"}`}
          </button>
          <button
            type="button"
            onClick={close}
            disabled={closing}
            data-qa-inventory-stocktake-close
            className="inline-flex h-9 items-center gap-2 rounded-md border border-foreground px-3 text-sm font-medium disabled:opacity-50"
          >
            {closing ? "Closing…" : "Close and post variances"}
          </button>
          <button
            type="button"
            onClick={cancel}
            disabled={busy}
            data-qa-inventory-stocktake-cancel
            className="inline-flex h-9 items-center rounded-md border border-border px-3 text-sm text-muted-foreground disabled:opacity-50"
          >
            Withdraw
          </button>
          {/* The count the close is waiting on, shown next to the button rather than only in the
              refusal: a person who presses "close" and is told one line is uncounted should not
              have to go and find which one. */}
          {sheet.lines_pending > 0 ? (
            <p data-qa-inventory-stocktake-close-blocked className="text-sm text-amber-800">
              {sheet.lines_pending} line{sheet.lines_pending === 1 ? "" : "s"} still not counted
            </p>
          ) : null}
        </div>
      ) : null}

      {sheet.status === "closed" ? (
        <p data-qa-inventory-stocktake-closed-summary className="text-sm text-muted-foreground">
          {sheet.lines_counted} lines counted · {sheet.variances_count} disagreed ·{" "}
          <span className={varianceToneFromWire(sheet.variance_total)}>
            {varianceTextFromWire(sheet.variance_total)}
          </span>{" "}
          posted
        </p>
      ) : null}

      {sheet.status === "closed" ? <StocktakeReportPanel id={sheet.id} /> : null}
    </div>
  );
}

/** One line of the sheet, with its own deviation highlighted. */
function StocktakeRow({
  line,
  open,
  value,
  onChange,
}: {
  line: StocktakeLine;
  open: boolean;
  value: string;
  onChange: (next: string) => void;
}) {
  const counted = value !== "" ? Number.parseFloat(value) : null;
  const variance =
    counted === null || Number.isNaN(counted) ? null : counted - Number.parseFloat(line.expected_qty);
  const short = variance !== null && Math.abs(variance) > 1e-9;

  return (
    <tr
      data-qa-inventory-stocktake-line={line.sku}
      data-variance={short ? "true" : "false"}
      className={`border-b border-border last:border-0 ${short ? "bg-amber-50/60" : ""}`}
    >
      <td className="px-3 py-2">
        <span className="font-medium">{line.sku}</span>
        <span className="block text-xs text-muted-foreground">{line.item_name}</span>
      </td>
      <td className="px-3 py-2 text-muted-foreground">{line.location_code}</td>
      <td className="px-3 py-2 text-right tabular-nums">{line.expected_qty}</td>
      <td className="px-3 py-2 text-right">
        {open ? (
          <input
            value={value}
            onChange={(event) => onChange(event.target.value)}
            inputMode="decimal"
            aria-label={`Counted quantity for ${line.sku} at ${line.location_code}`}
            data-qa-inventory-stocktake-input={line.sku}
            className="w-24 rounded-md border border-border bg-transparent px-2 py-1 text-right tabular-nums outline-none focus:border-foreground"
          />
        ) : (
          <span className="tabular-nums">
            {line.counted_qty ?? <span className="text-amber-800">not counted</span>}
          </span>
        )}
        {open && value === "" ? (
          <span data-qa-inventory-stocktake-uncounted className="block text-[11px] text-amber-800">
            not counted
          </span>
        ) : null}
      </td>
      <td
        className={`px-3 py-2 text-right font-medium tabular-nums ${
          variance === null ? "text-muted" : varianceTone(variance)
        }`}
      >
        {/* The deviation is printed whenever there is one, and a dash when there is nothing to
            compare — a blank cell in a variance column reads as "no variance" and a person
            moves on believing the shelf was checked. */}
        {variance === null ? "—" : varianceText(variance)}
      </td>
    </tr>
  );
}

// -------------------------------------------------------------------------------------------
// The report
// -------------------------------------------------------------------------------------------

/** The variance report: the movements the close posted, read back from the ledger. */
function StocktakeReportPanel({ id }: { id: string }) {
  const [report, setReport] = useState<StocktakeReport | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  // A key rather than a boolean: "try again" has to actually re-run the effect, and a boolean
  // `reload` in a dependency array re-runs on every render instead of on the press.
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let live = true;
    setLoading(true);
    setError(null);
    fetchStocktakeReport(id)
      .then((next) => {
        if (live) setReport(next);
      })
      .catch((failure) => {
        if (live) setError(toScreenError(failure, "The variance report could not be loaded."));
      })
      .finally(() => {
        if (live) setLoading(false);
      });
    return () => {
      live = false;
    };
  }, [id, attempt]);

  if (loading) return <LoadingTable columns={4} rows={2} />;
  if (error)
    return <ErrorState error={error} onRetry={() => setAttempt((n) => n + 1)} />;
  if (!report) return null;

  return (
    <section
      data-qa-inventory-stocktake-report
      className="space-y-3 rounded-lg border border-border p-4"
      aria-labelledby="stocktake-report-heading"
    >
      <h2 id="stocktake-report-heading" className="text-sm font-medium">
        Variance report
      </h2>
      {/* Two totals and whether they agree. One number would be a report that cannot fail. */}
      <p className="text-sm text-muted-foreground">
        Sheet total <strong className={varianceToneFromWire(report.variance_total)}>{varianceTextFromWire(report.variance_total)}</strong>
        {" · "}
        ledger total <strong className={varianceToneFromWire(report.ledger_total)}>{varianceTextFromWire(report.ledger_total)}</strong>
        {" · "}
        <span data-qa-inventory-stocktake-report-agrees={String(report.agrees)}>
          {report.agrees ? "the two agree" : "these do not agree"}
        </span>
      </p>

      {report.movements.length === 0 ? (
        <p data-qa-inventory-stocktake-report-empty className="text-sm text-muted-foreground">
          The count agreed with the stock list everywhere, so nothing was posted. That is a
          successful count, not an empty one.
        </p>
      ) : (
        <div className="overflow-x-auto rounded-md border border-border">
          <table className="w-full min-w-[520px] text-sm">
            <thead className="border-b border-border bg-muted/40 text-left text-xs uppercase tracking-wide text-muted-foreground">
              <tr>
                <th className="px-3 py-2 font-medium">Item</th>
                <th className="px-3 py-2 font-medium">Shelf</th>
                <th className="px-3 py-2 text-right font-medium">Posted</th>
                <th className="px-3 py-2 text-right font-medium">On hand now</th>
              </tr>
            </thead>
            <tbody>
              {report.movements.map((movement) => (
                <tr
                  key={movement.id}
                  data-qa-inventory-stocktake-movement={movement.sku}
                  className="border-b border-border last:border-0"
                >
                  <td className="px-3 py-2">{movement.sku}</td>
                  <td className="px-3 py-2 text-muted-foreground">{movement.location_code}</td>
                  <td
                    className={`px-3 py-2 text-right font-medium tabular-nums ${varianceToneFromWire(movement.quantity)}`}
                  >
                    {varianceTextFromWire(movement.quantity)}
                  </td>
                  <td className="px-3 py-2 text-right tabular-nums">{movement.on_hand_after}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
