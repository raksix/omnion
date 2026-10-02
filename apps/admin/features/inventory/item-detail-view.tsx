"use client";

/**
 * The item detail (REQ-053): `/inventory/items/{id}`.
 *
 * ## Why this screen exists at all
 *
 * Four screens in the module already link here — the stock list (table and cards), the movements
 * ledger and the approvals inbox. **None of them had a route to land on**: `apps/admin/app/inventory/`
 * shipped `alerts`, `approvals`, `movements`, `reports`, `stock`, `stocktake` and `transfers`, and
 * there was no `items` directory, so every one of those links was a dead destination that a Next
 * route 404s. An item's name is the first thing a warehouse worker reads, and on the stock list it
 * was a link that went nowhere. The API was complete the whole time — `GET /items/{id}` answered the
 * position, the per-location rows, the totals and the history — so this screen is the missing half
 * of a feature that already worked, not a new one.
 *
 * ## The three decisions that could have lied
 *
 * * **The numbers are the module's, not the screen's.** `on_hand`, `reserved`, `available` and
 *   `status` come from `StockPosition` — the same struct the stock list's rows are built from, so
 *   the header and a row of that table cannot disagree about the same item. A browser-side
 *   `on_hand - reserved` would be a second implementation of a rule the module owns, and the one
 *   that drifts.
 * * **The editable fields are the module's rules, validated by the module.** The SKU is shown but
 *   *not* editable: `inventory_items_sku_format` is a check constraint and `validate_sku` is the
 *   form's twin, and a rename that silently re-keys a SKU already printed on a label, a purchase
 *   order and a ledger row is not an edit. Everything else — name, category, unit, barcode, the
 *   three thresholds, cost, notes, active — goes through `PATCH /items/{id}`, and the server's
 *   refusal is rendered under the field it names rather than as a banner.
 * * **A refused save keeps what was typed.** A warehouse worker who mistypes `min_threshold` gets
 *   the number back, not an empty form; the alternative teaches them to count twice.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useParams, useRouter } from "next/navigation";
import { ArrowLeft, Loader2, Save } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { ApiError } from "@/lib/api";

import {
  archiveItem,
  fetchItem,
  fetchWarehouses,
  formatQuantity,
  updateItem,
  type ItemDetail,
  type InventoryItem,
  type Location,
  type Movement,
  type Quantity,
  type StockLevel,
  type StockStatus,
} from "@/lib/inventory";

import {
  QuantityCell,
  RelativeTime,
  SignedQuantity,
  StockStatusBadge,
  AdjustDrawer,
  type AdjustResult,
} from "./inventory-parts";

/** The fields the edit form owns. Everything else on the record is read-only here. */
type Editable = {
  name: string;
  category: string;
  unit: string;
  barcode: string;
  min_threshold: string;
  reorder_point: string;
  reorder_qty: string;
  cost: string;
  notes: string;
  active: boolean;
};

function editableOf(item: InventoryItem): Editable {
  return {
    name: item.name,
    category: item.category ?? "",
    unit: item.unit,
    barcode: item.barcode ?? "",
    min_threshold: item.min_threshold,
    reorder_point: item.reorder_point,
    reorder_qty: item.reorder_qty,
    cost: item.cost ?? "",
    notes: item.notes,
    active: item.active,
  };
}

const FIELD_CLASS =
  "mt-1 w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft";

export function ItemDetailView() {
  const params = useParams<{ id: string }>();
  const router = useRouter();
  const itemId = params?.id ?? "";

  const [detail, setDetail] = useState<ItemDetail | null>(null);
  // The drawer's location picker needs the **organization's** locations, which is a different list
  // from `detail.position.locations` (the places this item happens to be). Two variables for two
  // things — naming both `locations` shadowed the item's own rows, and the compiler was right to
  // complain: the table and the drawer would have read each other's data.
  const [warehouses, setWarehouses] = useState<Location[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ScreenErrorValue>(null);

  const [form, setForm] = useState<Editable | null>(null);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [adjust, setAdjust] = useState<{
    itemId: string;
    sku: string;
    name: string;
    locationId: string;
  } | null>(null);

  const load = useCallback(async () => {
    if (!itemId) return;
    setLoading(true);
    setError(null);
    try {
      // Two reads, because they answer different questions: the item is this record, and the
      // location list is the warehouse's. The drawer's picker needs the second even when this item
      // is not on a shelf yet — "adjust" from a fresh item is how a receipt is recorded.
      const [next, tree] = await Promise.all([fetchItem(itemId), fetchWarehouses()]);
      setDetail(next);
      setWarehouses(tree.flatMap((warehouse) => warehouse.locations));
      // The form is seeded from what the server answered, never from the URL. A stale form next
      // to a fresh body is how "I edited it and nothing happened" happens.
      setForm(editableOf(next.position.item));
      setFieldError(null);
    } catch (caught) {
      setError(toScreenError(caught, "This item could not be loaded."));
    } finally {
      setLoading(false);
    }
  }, [itemId]);

  useEffect(() => {
    void load();
  }, [load]);

  const dirty = useMemo(() => {
    if (!detail || !form) return false;
    const before = editableOf(detail.position.item);
    return (Object.keys(before) as (keyof Editable)[]).some((key) => before[key] !== form[key]);
  }, [detail, form]);

  const onAdjusted = useCallback(
    (result: AdjustResult) => {
      setAdjust(null);
      if (result.outcome === "recorded") {
        setNotice("Movement recorded.");
        void load();
      } else if (result.outcome === "awaiting_approval") {
        setNotice("The adjustment is waiting on an approval decision.");
      }
    },
    [load],
  );

  const onSave = useCallback(async () => {
    if (!detail || !form || !itemId) return;
    setSaving(true);
    setFieldError(null);
    setNotice(null);
    try {
      // Every field is sent, including the empty ones: `ItemPatch` reads `None` as "not mentioned"
      // and `Some("")` as "cleared", so a partial body would leave a barcode that cannot be
      // removed and a category that cannot be unset.
      await updateItem(itemId, {
        name: form.name,
        category: form.category,
        unit: form.unit,
        barcode: form.barcode,
        min_threshold: form.min_threshold,
        reorder_point: form.reorder_point,
        reorder_qty: form.reorder_qty,
        cost: form.cost,
        notes: form.notes,
        active: form.active,
      });
      setNotice("Saved.");
      void load();
    } catch (caught) {
      // The server's refusal is filed under the field it names, because a form that says only
      // "invalid" sends the reader hunting for which of nine inputs they got wrong. `ApiError`
      // carries it in `details.field`; anything else (a plain `Error` from the browser, a
      // transport failure) has no field and belongs in the screen's own error state.
      const named =
        caught instanceof ApiError && typeof caught.details?.field === "string"
          ? caught.details.field
          : null;
      if (caught instanceof ApiError && named) {
        // The narrowing that matters is `instanceof ApiError`, not `named` being truthy: `named` is
        // a `string | null` and reading `.message` off it is the type error a `&&` chain hides,
        // because TypeScript narrows `caught` inside the `&&` and forgets it at the body.
        setFieldError({ field: named, message: caught.message });
        setNotice(null);
      } else {
        setFieldError(null);
        setError(toScreenError(caught, "The item could not be saved."));
      }
    } finally {
      setSaving(false);
    }
  }, [detail, form, itemId, load]);

  const onArchive = useCallback(async () => {
    if (!detail || !itemId) return;
    if (typeof window !== "undefined" && !window.confirm("Archive this item?")) return;
    try {
      await archiveItem(itemId);
      setNotice("Archived. Ledger rows still name it.");
      void load();
    } catch (caught) {
      setError(toScreenError(caught, "The item could not be archived."));
    }
  }, [detail, itemId, load]);

  if (loading) {
    return (
      <div
        data-qa-inventory-item-loading
        className="flex items-center gap-2 p-6 text-[13px] text-muted"
        aria-busy="true"
      >
        <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
        Loading the item…
      </div>
    );
  }

  // A refusal replaces the body rather than sitting above it: a header with no numbers under it
  // reads as "the item loaded and has no stock", which is a different problem with a different fix.
  if (error) {
    return (
      <div data-qa-inventory-item-error className="p-4">
        <ErrorState
          error={error}
          onRetry={() => void load()}
          qa="inventory-item-error"
          action={
            <Link
              href="/inventory/stock"
              className="rounded border border-line px-3 py-1.5 text-[12.5px] hover:bg-canvas"
            >
              Back to stock
            </Link>
          }
        />
      </div>
    );
  }

  if (!detail || !form) {
    return (
      <EmptyState
        title="This item is not on file"
        hint="It may have been archived, or it belongs to another organization."
        action={
          <Link
            href="/inventory/stock"
            className="rounded bg-stone-900 px-3 py-1.5 text-[13px] font-medium text-white"
          >
            Back to stock
          </Link>
        }
      />
    );
  }

  const { item, locations, on_hand, reserved, available, status, last_movement_at } = detail.position;

  return (
    <div className="space-y-4 p-4" data-qa-inventory-item>
      <div className="flex flex-wrap items-center gap-2">
        <Link
          href="/inventory/stock"
          className="inline-flex items-center gap-1 text-[12.5px] text-muted hover:underline"
        >
          <ArrowLeft className="h-3.5 w-3.5" aria-hidden />
          Stock
        </Link>
        <h1 className="text-[15px] font-medium" data-qa-inventory-item-name>
          {item.name}
        </h1>
        <span className="font-mono text-[12px] text-muted">{item.sku}</span>
        <StockStatusBadge status={status} />
        {item.archived_at ? (
          <span className="rounded-full border border-line px-2 py-0.5 text-[11.5px] text-muted">
            Archived
          </span>
        ) : null}
      </div>

      {notice ? (
        <p
          data-qa-inventory-item-notice
          className="rounded border border-sky-200 bg-sky-50 px-3 py-2 text-[12.5px] text-sky-900"
        >
          {notice}
        </p>
      ) : null}

      {/* The totals are the module's rollup, printed as four numbers rather than one derived
          figure: an operator comparing this screen with the stock list is comparing the same
          `StockPosition` struct, not two implementations of a subtraction. */}
      <div
        className="grid grid-cols-2 gap-3 md:grid-cols-4"
        data-qa-inventory-item-totals
      >
        <Tile label="On hand" value={on_hand} />
        <Tile label="Reserved" value={reserved} />
        <Tile label="Available" value={available} />
        <div className="rounded border border-line p-3">
          <p className="text-[11.5px] uppercase tracking-wide text-muted">Last movement</p>
          <p className="mt-1 text-[13px]">
            <RelativeTime at={last_movement_at} />
          </p>
        </div>
      </div>

      <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_22rem]">
        <div className="min-w-0 space-y-4">
          <section className="rounded border border-line">
            <h2 className="border-b border-line px-3 py-2 text-[13px] font-medium">
              Where it is ({locations.length})
            </h2>
            {locations.length === 0 ? (
              <div className="p-3">
                <EmptyState
                  title="Not on any shelf"
                  hint="Record a receipt or an adjustment to put this item somewhere."
                  action={
                    <Link
                      href="/inventory/movements"
                      className="rounded bg-stone-900 px-3 py-1.5 text-[13px] font-medium text-white"
                    >
                      Record a movement
                    </Link>
                  }
                />
              </div>
            ) : (
              <div className="overflow-x-auto">
                <table className="w-full text-left text-[13px]">
                  <thead className="border-b text-[12px] uppercase tracking-wide text-muted">
                    <tr>
                      <th className="px-2 py-2">Location</th>
                      <th className="px-2 py-2 text-right">On hand</th>
                      <th className="px-2 py-2 text-right">Reserved</th>
                      <th className="px-2 py-2 text-right">Available</th>
                      <th className="px-2 py-2">Status</th>
                      <th className="px-2 py-2" />
                    </tr>
                  </thead>
                  <tbody>
                    {locations.map((row) => (
                      <tr key={row.id} className="border-b last:border-b-0">
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
                        <td className="px-2 py-2 text-right">
                          <button
                            type="button"
                            data-qa-inventory-item-adjust={row.id}
                            onClick={() =>
                              setAdjust({
                                itemId: item.id,
                                sku: item.sku,
                                name: item.name,
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
          </section>

          <section className="rounded border border-line">
            <h2 className="border-b border-line px-3 py-2 text-[13px] font-medium">
              Movement history ({detail.history.length})
            </h2>
            {detail.history.length === 0 ? (
              <div className="p-3">
                <EmptyState
                  title="No movements yet"
                  hint="An item that has never moved has nothing to show here."
                />
              </div>
            ) : (
              <ul className="divide-y" data-qa-inventory-item-history>
                {detail.history.map((movement) => (
                  <li key={movement.id} className="flex flex-wrap items-center gap-2 px-3 py-2">
                    <span className="text-[12px] uppercase tracking-wide text-muted">
                      {movement.kind}
                    </span>
                    <SignedQuantity movement={movement} />
                    <span className="text-[12.5px] text-muted">{movement.location_code}</span>
                    {movement.note ? (
                      <span className="text-[12.5px] text-muted">{movement.note}</span>
                    ) : null}
                    {/* The resulting balance is on the row deliberately: a ledger line without its
                        "and afterwards there were 14" is the line somebody has to compute by hand,
                        and that is the arithmetic the rollup exists to avoid. */}
                    <span className="text-[12.5px] text-muted">
                      → {formatQuantity(movement.on_hand_after)}
                    </span>
                    <span className="ml-auto text-[12px] text-muted">
                      <RelativeTime at={movement.created_at} />
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </section>
        </div>

        <form
          className="h-fit space-y-2.5 rounded-xl border border-line p-3"
          onSubmit={(event) => {
            event.preventDefault();
            void onSave();
          }}
        >
          <h2 className="text-[13px] font-medium">Edit item</h2>

          <Field label="Name" name="name" error={fieldError?.field === "name" ? fieldError.message : null}>
            <input
              value={form.name}
              onChange={(e) => setForm({ ...form, name: e.target.value })}
              data-qa-inventory-item-field="name"
              className={FIELD_CLASS}
            />
          </Field>

          <Field
            label="Category"
            name="category"
            error={fieldError?.field === "category" ? fieldError.message : null}
          >
            <input
              value={form.category}
              onChange={(e) => setForm({ ...form, category: e.target.value })}
              data-qa-inventory-item-field="category"
              className={FIELD_CLASS}
            />
          </Field>

          <Field label="Unit" name="unit" error={fieldError?.field === "unit" ? fieldError.message : null}>
            <input
              value={form.unit}
              onChange={(e) => setForm({ ...form, unit: e.target.value })}
              data-qa-inventory-item-field="unit"
              className={FIELD_CLASS}
            />
          </Field>

          <Field
            label="Barcode"
            name="barcode"
            error={fieldError?.field === "barcode" ? fieldError.message : null}
          >
            <input
              value={form.barcode}
              onChange={(e) => setForm({ ...form, barcode: e.target.value })}
              data-qa-inventory-item-field="barcode"
              className={FIELD_CLASS}
            />
          </Field>

          {/* One column on a phone, three from `md`. Without the prefix these three stay side by
              side at 390px, which is the layout the module's own mobile criterion refuses. */}
          <div className="grid gap-2 sm:grid-cols-3">
            <Field
              label="Min threshold"
              name="min_threshold"
              error={fieldError?.field === "min_threshold" ? fieldError.message : null}
            >
              <input
                value={form.min_threshold}
                onChange={(e) => setForm({ ...form, min_threshold: e.target.value })}
                data-qa-inventory-item-field="min_threshold"
                className={FIELD_CLASS}
              />
            </Field>
            <Field
              label="Reorder point"
              name="reorder_point"
              error={fieldError?.field === "reorder_point" ? fieldError.message : null}
            >
              <input
                value={form.reorder_point}
                onChange={(e) => setForm({ ...form, reorder_point: e.target.value })}
                data-qa-inventory-item-field="reorder_point"
                className={FIELD_CLASS}
              />
            </Field>
            <Field
              label="Reorder qty"
              name="reorder_qty"
              error={fieldError?.field === "reorder_qty" ? fieldError.message : null}
            >
              <input
                value={form.reorder_qty}
                onChange={(e) => setForm({ ...form, reorder_qty: e.target.value })}
                data-qa-inventory-item-field="reorder_qty"
                className={FIELD_CLASS}
              />
            </Field>
          </div>

          <Field label="Unit cost" name="cost" error={fieldError?.field === "cost" ? fieldError.message : null}>
            <input
              value={form.cost}
              onChange={(e) => setForm({ ...form, cost: e.target.value })}
              data-qa-inventory-item-field="cost"
              className={FIELD_CLASS}
            />
          </Field>

          <Field label="Notes" name="notes" error={fieldError?.field === "notes" ? fieldError.message : null}>
            <textarea
              rows={3}
              value={form.notes}
              onChange={(e) => setForm({ ...form, notes: e.target.value })}
              data-qa-inventory-item-field="notes"
              className={FIELD_CLASS}
            />
          </Field>

          <label className="flex items-center gap-2 text-[12.5px]">
            <input
              type="checkbox"
              checked={form.active}
              onChange={(e) => setForm({ ...form, active: e.target.checked })}
              data-qa-inventory-item-field="active"
            />
            Active — new movements may name this item
          </label>

          <div className="flex items-center gap-2 pt-1">
            <button
              type="submit"
              disabled={saving || !dirty}
              data-qa-inventory-item-save
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-50"
            >
              {saving ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <Save className="h-3.5 w-3.5" aria-hidden />}
              {saving ? "Saving…" : "Save"}
            </button>
            <button
              type="button"
              onClick={() => void onArchive()}
              data-qa-inventory-item-archive
              className="rounded border border-line px-3 py-1.5 text-[12.5px] hover:bg-canvas"
            >
              Archive
            </button>
          </div>
          <p className="text-[11.5px] text-muted">
            The SKU is <span className="font-mono">{item.sku}</span> and stays that way: it is a
            check constraint in the schema and a printed label in the aisle.
          </p>
        </form>
      </div>

      {adjust ? (
        <AdjustDrawer
          target={{
            kind: "item",
            itemId: adjust.itemId,
            sku: adjust.sku,
            name: adjust.name,
            locationId: adjust.locationId,
          }}
          locations={warehouses}
          onClose={() => setAdjust(null)}
          onDone={onAdjusted}
        />
      ) : null}
    </div>
  );
}

function Tile({ label, value }: { label: string; value: Quantity }) {
  return (
    <div className="rounded border border-line p-3">
      <p className="text-[11.5px] uppercase tracking-wide text-muted">{label}</p>
      <p className="mt-1">
        <QuantityCell value={value} />
      </p>
    </div>
  );
}

function Field({
  label,
  name,
  error,
  children,
}: {
  label: string;
  name: string;
  error: string | null;
  children: React.ReactNode;
}) {
  return (
    <label htmlFor={`inventory-item-${name}`} className="block text-[12px] text-muted">
      {label}
      <span className="mt-1 block">{children}</span>
      {error ? (
        <span role="alert" data-qa-inventory-item-field-error={name} className="mt-1 block text-[11.5px] text-danger">
          {error}
        </span>
      ) : null}
    </label>
  );
}