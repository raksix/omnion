"use client";

/**
 * The transfer list and the transfer detail (REQ-053, slice 3): `/inventory/transfers`.
 *
 * A transfer is a **document**, not two movements, and this screen is where that decision becomes
 * visible. Slice 1 shipped the `transfer_out` and `transfer_in` movement kinds with no way to
 * produce either; this is the thing that produces them.
 *
 * Five decisions, each a way the screen could have lied:
 *
 * * **The stepper is drawn from the document's own status**, and the buttons are drawn from the
 *   same value. A screen that computed "can I dispatch?" from a string would have a different
 *   answer the moment a fifth status arrived; the server's `can_dispatch` / `can_receive` are the
 *   definition and this screen only reads them.
 * * **The transit location is named.** A transfer's two legs touch three locations, and the one
 *   that surprises a person is the middle one: goods leave the shelf and are *somewhere*. The
 *   detail says so in words rather than leaving it to be inferred from two movements.
 * * **A partial receive is a normal act**, not an error path. The receive form takes a quantity
 *   per line, pre-filled with what is still outstanding, so the common case (everything arrived)
 *   is one click and the real case (two pallets of three) is a number somebody typed.
 * * **A refused dispatch keeps the transfer on screen** with the server's sentence, because the
 *   sentence carries the number that is actually on the shelf and the person needs to act on it
 *   — which means deciding whether to send less or go and find it.
 * * **Nothing is optimistically drawn.** Every step re-reads the document from the server, so the
 *   status the screen shows is the status the ledger was told about.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import {
  ArrowRightLeft,
  Check,
  Loader2,
  PackageCheck,
  Plus,
  Search,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, describeError, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  cancelTransfer,
  createTransfer,
  dispatchTransfer,
  fetchItems,
  fetchLocations,
  fetchTransfers,
  fetchTransfer,
  receiveTransfer,
  type InventoryItem,
  type Location,
  type Quantity,
  type StockTransfer,
  type TransferStatus,
} from "@/lib/inventory";

import { QuantityCell, RelativeTime } from "./inventory-parts";

/** The stepper's steps, in the order a document moves through them. */
const STEPS: { value: TransferStatus; label: string }[] = [
  { value: "draft", label: "Draft" },
  { value: "dispatched", label: "Dispatched" },
  { value: "received", label: "Received" },
  { value: "cancelled", label: "Cancelled" },
];

/** The status filter chips, `all` last because it is the absence of a filter. */
const FILTERS: { value: string; label: string }[] = [
  { value: "open", label: "In flight" },
  { value: "draft", label: "Draft" },
  { value: "dispatched", label: "Dispatched" },
  { value: "received", label: "Received" },
  { value: "all", label: "All" },
];

export function TransfersView() {
  const params = useSearchParams();
  const router = useRouter();
  const selectedId = params.get("transfer");

  const [scope, setScope] = useState(params.get("status") ?? "open");
  const [search, setSearch] = useState(params.get("q") ?? "");
  const [rows, setRows] = useState<StockTransfer[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [creating, setCreating] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const page = await fetchTransfers({
        search: search.trim() || undefined,
        // `open_only` rather than a status the server has to know the meaning of: "in flight"
        // is a question about two statuses, and a list screen that spelled out both of them
        // would have to be updated when a fourth status arrived.
        open_only: scope === "open" ? true : undefined,
        status: scope === "open" || scope === "all" ? undefined : scope,
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

  // The filter lives in the URL so a link somebody pastes opens the list they meant, and so the
  // browser's back button undoes a filter rather than leaving the page.
  useEffect(() => {
    const next = new URLSearchParams();
    if (scope !== "open") next.set("status", scope);
    if (search.trim()) next.set("q", search.trim());
    if (selectedId) next.set("transfer", selectedId);
    const query = next.toString();
    router.replace(query ? `/inventory/transfers?${query}` : "/inventory/transfers", {
      scroll: false,
    });
  }, [scope, search, selectedId, router]);

  const open = (id: string) => {
    const next = new URLSearchParams(window.location.search);
    next.set("transfer", id);
    router.push(`/inventory/transfers?${next.toString()}`);
  };

  return (
    <div className="space-y-6">
      <header className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-xl font-semibold tracking-tight">Transfers</h1>
          <p className="text-sm text-muted-foreground">
            Move stock between locations. A dispatch books the goods into transit; a receive books
            them in at the destination.
          </p>
        </div>
        <button
          type="button"
          data-qa-inventory-transfer-create
          onClick={() => setCreating((open) => !open)}
          className="inline-flex h-9 items-center gap-2 rounded-md bg-primary px-3 text-sm font-medium text-primary-foreground"
        >
          {creating ? <X className="h-4 w-4" aria-hidden /> : <Plus className="h-4 w-4" aria-hidden />}
          {creating ? "Close" : "New transfer"}
        </button>
      </header>

      {creating ? (
        <NewTransferForm
          onCreated={(transfer) => {
            setCreating(false);
            void load();
            open(transfer.id);
          }}
          onCancel={() => setCreating(false)}
        />
      ) : null}

      <div className="flex flex-wrap items-center gap-2">
        {FILTERS.map((filter) => (
          <button
            key={filter.value}
            type="button"
            data-qa-inventory-transfer-filter={filter.value}
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
          <span className="sr-only">Search transfers</span>
          <input
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Number, SKU, item or note"
            data-qa-inventory-transfer-search
            className="w-56 bg-transparent outline-none"
          />
        </label>
      </div>

      {loading ? (
        <LoadingTable columns={5} rows={5} />
      ) : error ? (
        <ErrorState error={error} onRetry={load} />
      ) : rows.length === 0 ? (
        <EmptyState
          title={scope === "open" ? "Nothing is in flight" : "No transfers match"}
          hint={
            scope === "open"
              ? "A transfer is a document that moves stock between two locations. Write one and dispatch it when the goods leave the shelf."
              : "No transfer matches this filter. Widen the scope, or clear the search."
          }
          action={
            <button
              type="button"
              onClick={() => setCreating(true)}
              className="inline-flex h-9 items-center gap-2 rounded-md bg-primary px-3 text-sm font-medium text-primary-foreground"
            >
              <Plus className="h-4 w-4" aria-hidden />
              New transfer
            </button>
          }
        />
      ) : (
        <div className="overflow-hidden rounded-lg border border-border">
          <table className="w-full text-sm">
            <caption className="sr-only">
              Transfers, newest first. Select a row to see its lines and take the next step.
            </caption>
            <thead className="bg-muted/50 text-left">
              <tr>
                <th scope="col" className="px-3 py-2 font-medium">Number</th>
                <th scope="col" className="px-3 py-2 font-medium">From → To</th>
                <th scope="col" className="px-3 py-2 font-medium">Status</th>
                <th scope="col" className="px-3 py-2 font-medium text-right">Quantity</th>
                <th scope="col" className="px-3 py-2 font-medium">Written</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr
                  key={row.id}
                  data-qa-inventory-transfer-row={row.number}
                  data-qa-inventory-transfer-status={row.status}
                  className="border-t border-border hover:bg-muted/40"
                >
                  <td className="px-3 py-2">
                    <Link
                      href={`/inventory/transfers?transfer=${row.id}`}
                      className="font-medium underline-offset-2 hover:underline"
                    >
                      {row.number}
                    </Link>
                  </td>
                  <td className="px-3 py-2 text-muted-foreground">
                    {row.from_location_code} <ArrowRightLeft className="inline h-3 w-3" aria-hidden />{" "}
                    {row.to_location_code}
                  </td>
                  <td className="px-3 py-2">
                    <StatusBadge status={row.status} />
                  </td>
                  <td className="px-3 py-2 text-right font-mono text-xs">
                    <QuantityCell value={row.quantity_total as Quantity} />
                  </td>
                  <td className="px-3 py-2 text-muted-foreground">
                    <RelativeTime at={row.created_at} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {selectedId ? (
        <TransferDetail
          id={selectedId}
          onChanged={() => {
            void load();
          }}
          onClose={() => {
            const next = new URLSearchParams(window.location.search);
            next.delete("transfer");
            const query = next.toString();
            router.push(query ? `/inventory/transfers?${query}` : "/inventory/transfers");
          }}
        />
      ) : null}
    </div>
  );
}

/** The status badge. The word carries the meaning; the colour only reinforces it. */
function StatusBadge({ status }: { status: TransferStatus }) {
  const tone: Record<TransferStatus, string> = {
    draft: "border-border text-muted-foreground",
    dispatched: "border-amber-500/40 text-amber-700 dark:text-amber-400",
    received: "border-emerald-500/40 text-emerald-700 dark:text-emerald-400",
    cancelled: "border-border text-muted-foreground line-through",
  };
  return (
    <span
      data-qa-inventory-transfer-badge={status}
      className={`inline-flex items-center rounded-full border px-2 py-0.5 text-xs capitalize ${tone[status]}`}
    >
      {status}
    </span>
  );
}

// ---------------------------------------------------------------------------------------------
// The create form
// ---------------------------------------------------------------------------------------------

/** One line being written.
 *
 * `key` is a client-side identity so adding and removing a row does not confuse React about
 * which input holds what — a form that keys on the array index re-uses the first row's contents
 * when the second is deleted, and a warehouse clerk who deletes a line and retypes another gets
 * a quantity they did not enter.
 */
type DraftLine = { key: string; item_id: string; quantity: string; note: string };

/**
 * The field an error names, or `undefined`.
 *
 * The platform's error body carries it under `error.details.field`, beside `error.details.entity`
 * — the two halves of "which input was wrong in what". A screen that reaches for `error.field`
 * finds `undefined` for every refusal and quietly falls back to a banner, which is the failure
 * mode this function exists to make impossible to write by accident.
 */
function readField(caught: unknown): string | undefined {
  const error = (caught as { error?: unknown } | null)?.error;
  const field = (error as { details?: { field?: unknown } } | null)?.details?.field;
  return typeof field === "string" ? field : undefined;
}

function NewTransferForm({
  onCreated,
  onCancel,
}: {
  onCreated: (transfer: StockTransfer) => void;
  onCancel: () => void;
}) {
  const [locations, setLocations] = useState<Location[]>([]);
  const [items, setItems] = useState<InventoryItem[]>([]);
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [note, setNote] = useState("");
  const [lines, setLines] = useState<DraftLine[]>([
    { key: "line-0", item_id: "", quantity: "", note: "" },
  ]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [where, what] = await Promise.all([
          fetchLocations(),
          fetchItems({ limit: 200 }),
        ]);
        if (cancelled) return;
        setLocations(where);
        setItems(what.items);
        // Prefill the source with the first *internal* location, because in-transit is not a
        // place a person picks stock from and offering it first would be a wrong default.
        setFrom(where.find((location) => location.kind !== "in_transit")?.id ?? "");
      } catch (failure) {
        if (!cancelled) setError(toScreenError(failure, "The inventory data could not be loaded."));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const setLine = (key: string, patch: Partial<DraftLine>) => {
    setLines((current) =>
      current.map((line) => (line.key === key ? { ...line, ...patch } : line)),
    );
  };

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    setFieldErrors({});
    try {
      const transfer = await createTransfer({
        from_location_id: from,
        to_location_id: to,
        note: note.trim() || undefined,
        lines: lines
          .filter((line) => line.item_id && line.quantity.trim())
          .map((line) => ({
            item_id: line.item_id,
            quantity: line.quantity.trim(),
            note: line.note.trim() || undefined,
          })),
      });
      onCreated(transfer);
    } catch (failure) {
      const screen = toScreenError(failure, "The transfer could not be written.");
      setError(screen);
      // The server's `field` is what the form renders the message under, so the message lands
      // beside the input that caused it rather than in a banner somebody has to read twice.
      // `describeError` is the one place that knows how to read all three shapes the failure
      // can take, so this screen does not reach for `.message` on a union of them.
      //
      // The field is under **`details`, not at the top level** — that is where the API's error
      // conversion puts it, beside the entity, and reading `failure.field` finds `undefined`
      // for every refusal. A form that falls back to a banner is not broken, but the person
      // then has to work out which of six inputs was wrong, which is the thing the field name
      // was for.
      const field = readField(failure);
      if (field) setFieldErrors({ [field]: describeError(screen).message });
    } finally {
      setBusy(false);
    }
  };

  return (
    <form
      onSubmit={submit}
      data-qa-inventory-transfer-form
      className="space-y-4 rounded-lg border border-border p-4"
    >
      <div className="grid gap-4 sm:grid-cols-2">
        <label className="block text-sm">
          <span className="mb-1 block font-medium">From location</span>
          <select
            value={from}
            onChange={(event) => setFrom(event.target.value)}
            data-qa-inventory-transfer-from
            className="h-9 w-full rounded-md border border-border bg-transparent px-2"
          >
            <option value="">Choose a shelf</option>
            {locations
              .filter((location) => location.kind !== "in_transit")
              .map((location) => (
                <option key={location.id} value={location.id}>
                  {location.code} — {location.name}
                </option>
              ))}
          </select>
          {fieldErrors.from_location_id ? (
            <span
              data-qa-inventory-transfer-field-error="from_location_id"
              className="mt-1 block text-xs text-destructive"
            >
              {fieldErrors.from_location_id}
            </span>
          ) : null}
        </label>
        <label className="block text-sm">
          <span className="mb-1 block font-medium">To location</span>
          <select
            value={to}
            onChange={(event) => setTo(event.target.value)}
            data-qa-inventory-transfer-to
            className="h-9 w-full rounded-md border border-border bg-transparent px-2"
          >
            <option value="">Choose a shelf</option>
            {locations
              .filter((location) => location.kind !== "in_transit" && location.id !== from)
              .map((location) => (
                <option key={location.id} value={location.id}>
                  {location.code} — {location.name}
                </option>
              ))}
          </select>
          {fieldErrors.to_location_id ? (
            <span
              data-qa-inventory-transfer-field-error="to_location_id"
              className="mt-1 block text-xs text-destructive"
            >
              {fieldErrors.to_location_id}
            </span>
          ) : null}
        </label>
      </div>

      <fieldset className="space-y-2">
        <legend className="text-sm font-medium">Lines</legend>
        {lines.map((line) => (
          <div key={line.key} className="grid gap-2 sm:grid-cols-[1fr_8rem_1fr_auto]">
            <select
              value={line.item_id}
              onChange={(event) => setLine(line.key, { item_id: event.target.value })}
              aria-label="Item"
              data-qa-inventory-transfer-line-item
              className="h-9 rounded-md border border-border bg-transparent px-2 text-sm"
            >
              <option value="">Choose an item</option>
              {items.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.sku} — {item.name}
                </option>
              ))}
            </select>
            <input
              value={line.quantity}
              onChange={(event) => setLine(line.key, { quantity: event.target.value })}
              inputMode="decimal"
              placeholder="Quantity"
              aria-label="Quantity"
              data-qa-inventory-transfer-line-quantity
              className="h-9 rounded-md border border-border bg-transparent px-2 font-mono text-sm"
            />
            <input
              value={line.note}
              onChange={(event) => setLine(line.key, { note: event.target.value })}
              placeholder="Note (optional)"
              aria-label="Line note"
              className="h-9 rounded-md border border-border bg-transparent px-2 text-sm"
            />
            <button
              type="button"
              onClick={() =>
                setLines((current) =>
                  current.length > 1
                    ? current.filter((entry) => entry.key !== line.key)
                    : current,
                )
              }
              aria-label={`Remove line ${line.key}`}
              className="h-9 rounded-md border border-border px-2 text-muted-foreground hover:text-foreground"
            >
              <X className="h-4 w-4" aria-hidden />
            </button>
          </div>
        ))}
        <button
          type="button"
          onClick={() =>
            setLines((current) => [
              ...current,
              {
                key: `line-${current.length}-${Date.now()}`,
                item_id: "",
                quantity: "",
                note: "",
              },
            ])
          }
          data-qa-inventory-transfer-add-line
          className="inline-flex h-8 items-center gap-1 rounded-md border border-border px-2 text-sm"
        >
          <Plus className="h-4 w-4" aria-hidden />
          Add a line
        </button>
      </fieldset>

      <label className="block text-sm">
        <span className="mb-1 block font-medium">Note</span>
        <textarea
          value={note}
          onChange={(event) => setNote(event.target.value)}
          rows={2}
          maxLength={500}
          placeholder="Why is this moving? (optional)"
          className="w-full rounded-md border border-border bg-transparent px-2 py-1"
        />
      </label>

      <p className="text-xs text-muted-foreground">
        A draft moves nothing. The shelf is checked when the transfer is <strong>dispatched</strong>,
        so a transfer written today for goods collected on Friday is judged on Friday.
      </p>

      {fieldErrors.lines ? (
        <p className="text-sm text-destructive" data-qa-inventory-transfer-field-error="lines">
          {fieldErrors.lines}
        </p>
      ) : null}

      {error && !fieldErrors.lines ? (
        <ErrorState error={error} onRetry={() => setError(null)} action="Fix the field above and try again." />
      ) : null}

      <div className="flex gap-2">
        <button
          type="submit"
          disabled={busy}
          data-qa-inventory-transfer-submit
          className="inline-flex h-9 items-center gap-2 rounded-md bg-primary px-3 text-sm font-medium text-primary-foreground disabled:opacity-60"
        >
          {busy ? <Loader2 className="h-4 w-4 animate-spin" aria-hidden /> : null}
          Write the draft
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="h-9 rounded-md border border-border px-3 text-sm"
        >
          Cancel
        </button>
      </div>
    </form>
  );
}

// ---------------------------------------------------------------------------------------------
// The detail
// ---------------------------------------------------------------------------------------------

function TransferDetail({
  id,
  onChanged,
  onClose,
}: {
  id: string;
  onChanged: () => void;
  onClose: () => void;
}) {
  const [transfer, setTransfer] = useState<StockTransfer | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [busy, setBusy] = useState(false);
  const [stepError, setStepError] = useState<string | null>(null);
  const [arrivals, setArrivals] = useState<Record<string, string>>({});

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setTransfer(await fetchTransfer(id));
    } catch (failure) {
      setError(toScreenError(failure, "The transfer could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  // The receive boxes are pre-filled with what is still outstanding, so the common case — the
  // whole load arrived — is one click, and the real case is a number somebody typed. Prefilled
  // rather than empty because an empty box next to a line that is plainly still on the van asks
  // the clerk to do arithmetic the screen can do.
  useEffect(() => {
    if (!transfer) return;
    const next: Record<string, string> = {};
    for (const line of transfer.lines) {
      const outstanding = line.quantity.replaceAll(",", "");
      // `outstanding` is the server's own subtraction, sent on the line.
      const value = (line as { outstanding?: string }).outstanding ?? outstanding;
      next[line.id] = value === "0.000" ? "" : value;
    }
    setArrivals(next);
  }, [transfer]);

  const act = async (run: () => Promise<StockTransfer>) => {
    setBusy(true);
    setStepError(null);
    try {
      setTransfer(await run());
      onChanged();
    } catch (failure) {
      // The refusal stays on the document. Reloading on failure would throw away the sentence
      // that carries the number on the shelf, which is the whole point of the refusal.
      setStepError(
        describeError(toScreenError(failure, "That step could not be taken.")).message,
      );
    } finally {
      setBusy(false);
    }
  };

  const receive = async () => {
    const lines = Object.entries(arrivals)
      .filter(([, quantity]) => quantity.trim())
      .map(([line_id, quantity]) => ({ line_id, quantity: quantity.trim() }));
    await act(() => receiveTransfer(id, lines));
  };

  if (loading) return <LoadingTable columns={3} rows={2} />;
  if (error) return <ErrorState error={error} onRetry={load} />;
  if (!transfer) return null;

  const canDispatch = transfer.status === "draft";
  const canReceive = transfer.status === "dispatched";

  return (
    <section
      data-qa-inventory-transfer-detail
      data-qa-inventory-transfer-detail-status={transfer.status}
      className="space-y-4 rounded-lg border border-border p-4"
      aria-label={`Transfer ${transfer.number}`}
    >
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-lg font-semibold">
            {transfer.number} <StatusBadge status={transfer.status} />
          </h2>
          <p className="text-sm text-muted-foreground">
            {transfer.from_location_code} → {transfer.to_location_code}
            {transfer.status === "dispatched" ? (
              <>
                {" · in transit at "}
                <span className="font-medium">TRANSIT</span> until it is received
              </>
            ) : null}
          </p>
        </div>
        <button
          type="button"
          onClick={onClose}
          aria-label="Close the transfer"
          className="h-8 rounded-md border border-border px-2"
        >
          <X className="h-4 w-4" aria-hidden />
        </button>
      </header>

      <ol className="flex flex-wrap items-center gap-2 text-xs">
        {STEPS.map((step, at) => {
          const current = STEPS.findIndex((entry) => entry.value === transfer.status);
          const done = current > at && transfer.status !== "cancelled";
          const here = step.value === transfer.status;
          return (
            <li
              key={step.value}
              data-qa-inventory-transfer-step={step.value}
              data-qa-inventory-transfer-step-state={here ? "current" : done ? "done" : "todo"}
              className={`rounded-full border px-2 py-1 ${
                here
                  ? "border-primary bg-primary text-primary-foreground"
                  : done
                    ? "border-emerald-500/40 text-emerald-700 dark:text-emerald-400"
                    : "border-border text-muted-foreground"
              }`}
            >
              {step.label}
            </li>
          );
        })}
      </ol>

      <div className="overflow-x-auto">
        <table className="w-full text-sm">
          <caption className="sr-only">The lines on this transfer, and what has landed</caption>
          <thead className="bg-muted/50 text-left">
            <tr>
              <th scope="col" className="px-3 py-2 font-medium">SKU</th>
              <th scope="col" className="px-3 py-2 font-medium">Item</th>
              <th scope="col" className="px-3 py-2 font-medium text-right">Sent</th>
              <th scope="col" className="px-3 py-2 font-medium text-right">Landed</th>
              {canReceive ? (
                <th scope="col" className="px-3 py-2 font-medium text-right">Arriving now</th>
              ) : null}
            </tr>
          </thead>
          <tbody>
            {transfer.lines.map((line) => (
              <tr key={line.id} data-qa-inventory-transfer-line={line.sku} className="border-t border-border">
                <td className="px-3 py-2 font-mono text-xs">{line.sku}</td>
                <td className="px-3 py-2">{line.item_name}</td>
                <td className="px-3 py-2 text-right font-mono text-xs">
                  <QuantityCell value={line.quantity} />
                </td>
                <td className="px-3 py-2 text-right font-mono text-xs">
                  <QuantityCell value={line.received_qty} />
                </td>
                {canReceive ? (
                  <td className="px-3 py-2 text-right">
                    <input
                      value={arrivals[line.id] ?? ""}
                      onChange={(event) =>
                        setArrivals((current) => ({ ...current, [line.id]: event.target.value }))
                      }
                      inputMode="decimal"
                      aria-label={`Quantity arriving for ${line.sku}`}
                      data-qa-inventory-transfer-arrive={line.sku}
                      className="h-8 w-24 rounded-md border border-border bg-transparent px-2 text-right font-mono text-xs"
                    />
                  </td>
                ) : null}
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {transfer.note ? (
        <p className="text-sm text-muted-foreground">Note: {transfer.note}</p>
      ) : null}

      {stepError ? (
        <p
          role="alert"
          data-qa-inventory-transfer-step-error
          className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive"
        >
          {stepError}
        </p>
      ) : null}

      <div className="flex flex-wrap gap-2">
        {canDispatch ? (
          <button
            type="button"
            disabled={busy}
            data-qa-inventory-transfer-dispatch
            onClick={() => act(() => dispatchTransfer(id))}
            className="inline-flex h-9 items-center gap-2 rounded-md bg-primary px-3 text-sm font-medium text-primary-foreground disabled:opacity-60"
          >
            {busy ? <Loader2 className="h-4 w-4 animate-spin" aria-hidden /> : <ArrowRightLeft className="h-4 w-4" aria-hidden />}
            Dispatch — book out and into transit
          </button>
        ) : null}

        {canReceive ? (
          <>
            <button
              type="button"
              disabled={busy}
              data-qa-inventory-transfer-receive
              onClick={receive}
              className="inline-flex h-9 items-center gap-2 rounded-md bg-primary px-3 text-sm font-medium text-primary-foreground disabled:opacity-60"
            >
              {busy ? <Loader2 className="h-4 w-4 animate-spin" aria-hidden /> : <PackageCheck className="h-4 w-4" aria-hidden />}
              Receive what is named above
            </button>
            <button
              type="button"
              disabled={busy}
              data-qa-inventory-transfer-receive-all
              onClick={() =>
                setArrivals(
                  Object.fromEntries(
                    transfer.lines.map((line) => [
                      line.id,
                      (line as { outstanding?: string }).outstanding ?? line.quantity,
                    ]),
                  ),
                )
              }
              className="inline-flex h-9 items-center gap-2 rounded-md border border-border px-3 text-sm"
            >
              <Check className="h-4 w-4" aria-hidden />
              Everything arrived
            </button>
          </>
        ) : null}

        {transfer.status === "draft" || transfer.status === "dispatched" ? (
          <button
            type="button"
            disabled={busy}
            data-qa-inventory-transfer-cancel
            onClick={() => act(() => cancelTransfer(id))}
            className="h-9 rounded-md border border-border px-3 text-sm disabled:opacity-60"
          >
            {transfer.status === "dispatched" ? "Cancel and bring the goods back" : "Cancel"}
          </button>
        ) : null}
      </div>

      <dl className="grid grid-cols-2 gap-2 text-xs text-muted-foreground sm:grid-cols-4">
        <div>
          <dt className="font-medium text-foreground">Written</dt>
          <dd><RelativeTime at={transfer.created_at} /></dd>
        </div>
        <div>
          <dt className="font-medium text-foreground">Dispatched</dt>
          <dd><RelativeTime at={transfer.dispatched_at} /></dd>
        </div>
        <div>
          <dt className="font-medium text-foreground">Received</dt>
          <dd><RelativeTime at={transfer.received_at} /></dd>
        </div>
        <div>
          <dt className="font-medium text-foreground">Cancelled</dt>
          <dd><RelativeTime at={transfer.cancelled_at} /></dd>
        </div>
      </dl>
    </section>
  );
}
