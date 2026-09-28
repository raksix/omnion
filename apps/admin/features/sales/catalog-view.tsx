"use client";

/**
 * The sellable catalog (REQ-052, slice 1): `/sales/catalog`.
 *
 * The screen is a list with a create form, because those are the two halves a seller needs before
 * a quote can exist at all, and a product nobody can add is a quote builder with nothing to pick
 * from. Three decisions are worth naming:
 *
 * * **Archiving is not deleting, and the screen says which it did.** A product a quote line named
 *   last month must keep resolving, so `DELETE` here means "stop offering it". The confirmation
 *   uses the word *archive* and the toast says the row is still readable — a seller who thinks
 *   they deleted a product and later finds it in a PDF has been told a lie.
 * * **A price is text, end to end.** The form takes `"12.50"` and hands the string straight back.
 *   Parsing it into a JS number would reintroduce the drift the money module exists to prevent,
 *   which is the whole reason the API sends decimal text.
 * * **The refusal lands under its field.** The API answers a bad SKU with `details.field`, so the
 *   message appears under that input rather than in a banner — a person fixing a form needs to know
 *   *which* field, and a banner is the one place they have to look everywhere for it.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";

import { Archive, Loader2, Pencil, Plus, RotateCcw } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  SALES_UNITS,
  archiveSalesProduct,
  createSalesProduct,
  fetchSalesProducts,
  fieldOf,
  formatMoney,
  updateSalesProduct,
  unitLabel,
  type SalesCatalogQuery,
  type SalesProduct,
} from "@/lib/sales";

import {
  SalesFilterChip,
  SalesFilterSelect,
  SalesShortcutSheet,
  SalesToolbar,
  salesRowCursor,
  useSales,
  useSalesKeyboard,
} from "./sales-parts";

/** The form's state. Every field is a string, because every field arrives as a string. */
type Draft = {
  sku: string;
  name: string;
  description: string;
  category: string;
  unit: string;
  tax_percent: string;
  default_price: string;
  currency: string;
  active: boolean;
};

const BLANK: Draft = {
  sku: "",
  name: "",
  description: "",
  category: "",
  unit: "piece",
  tax_percent: "20",
  default_price: "0.00",
  currency: "TRY",
  active: true,
};

/** The columns, in the order the table draws them and the form asks for them. */
const COLUMNS = ["SKU", "Name", "Category", "Unit", "Tax %", "Default price", "Active", ""];

/** `/sales/catalog`: the list, its filters, and the create/edit form. */
export function CatalogView() {
  const router = useRouter();
  const params = useSearchParams();
  const { organizationId, vocabulary } = useSales();

  const [page, setPage] = useState<{ items: SalesProduct[]; total_estimate: number } | null>(null);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [reloadToken, setReloadToken] = useState(0);
  const [selected, setSelected] = useState(0);
  const [search, setSearch] = useState(params.get("search") ?? "");
  const searchRef = useRef<HTMLInputElement | null>(null);

  // The form's own state: null is closed, a draft is an open form, and `editing` says which row.
  const [draft, setDraft] = useState<Draft | null>(null);
  const [editing, setEditing] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [formField, setFormField] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const query = useCallback((): SalesCatalogQuery => {
    const text = params.get("search");
    const category = params.get("category");
    const archived = params.get("archived");
    return {
      search: text ?? undefined,
      category: category ?? undefined,
      include_archived: archived === "1" ? true : undefined,
      organization_id: organizationId ?? undefined,
      limit: 100,
    };
  }, [params, organizationId]);

  useEffect(() => {
    setError(null);
    fetchSalesProducts(query())
      .then((loaded) => {
        setPage(loaded);
        setSelected((current) => (current < loaded.items.length ? current : 0));
      })
      .catch((problem) => setError(toScreenError(problem, "The catalog could not be loaded.")));
  }, [query, reloadToken]);

  const items = page?.items ?? [];
  const filtered = (params.get("search") ?? "") !== "" || (params.get("category") ?? "") !== "" || (params.get("archived") ?? "") !== "";

  const openNew = useCallback(() => {
    setDraft({ ...BLANK });
    setEditing(null);
    setFormError(null);
    setFormField(null);
  }, []);

  const openEdit = useCallback(
    (index: number) => {
      const product = items[index];
      if (!product) return;
      setDraft({
        sku: product.sku,
        name: product.name,
        description: product.description,
        category: product.category ?? "",
        unit: product.unit,
        tax_percent: String(product.tax_percent),
        default_price: product.default_price,
        currency: product.currency,
        active: product.active,
      });
      setEditing(product.id);
      setFormError(null);
      setFormField(null);
    },
    [items],
  );

  const { shortcutsOpen, setShortcutsOpen } = useSalesKeyboard(
    {
      count: items.length,
      selected,
      onSelect: setSelected,
      onOpen: (index) => {
        const product = items[index];
        if (product) router.push(`/sales/catalog/${product.id}`);
      },
      onEdit: openEdit,
      onNew: openNew,
    },
    searchRef,
  );

  const save = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!draft) return;
    setSaving(true);
    setFormError(null);
    setFormField(null);

    // The numbers go to the API as the strings the form holds. A `Number()` here would turn
    // "12.50" into 12.5 and then into whatever the float holds — which is the exact drift the
    // money module refuses, re-introduced in the last hop.
    const payload = {
      sku: draft.sku.trim(),
      name: draft.name.trim(),
      description: draft.description,
      category: draft.category.trim() === "" ? null : draft.category.trim(),
      unit: draft.unit,
      tax_percent: Number(draft.tax_percent || "0"),
      default_price: draft.default_price.trim() || "0.00",
      currency: draft.currency.trim().toUpperCase(),
      active: draft.active,
    };

    try {
      if (editing) {
        await updateSalesProduct(editing, payload, organizationId);
        setNotice(`Saved ${payload.sku}.`);
      } else {
        await createSalesProduct(payload, organizationId);
        setNotice(`Added ${payload.sku} to the catalog.`);
      }
      setDraft(null);
      setEditing(null);
      setReloadToken((token) => token + 1);
    } catch (problem) {
      setFormError(problem instanceof Error ? problem.message : "The product could not be saved.");
      setFormField(fieldOf(problem));
    } finally {
      setSaving(false);
    }
  };

  const archive = async (product: SalesProduct) => {
    setNotice(null);
    setError(null);
    try {
      await archiveSalesProduct(product.id, organizationId);
      setNotice(
        `${product.sku} is archived. It is out of the catalog, and the quotes that named it still resolve.`,
      );
      setReloadToken((token) => token + 1);
    } catch (problem) {
      setError(toScreenError(problem, `${product.sku} could not be archived.`));
    }
  };

  const fieldError = (name: string) => (formField === name ? formError : null);

  return (
    <div>
      <SalesToolbar
        search={search}
        onSearchChange={setSearch}
        searchRef={searchRef}
        count={items.length}
        filtered={filtered}
      >
        <SalesFilterSelect
          name="category"
          value={params.get("category") ?? ""}
          allLabel="All categories"
          options={(vocabulary?.categories ?? []).map((category) => ({ value: category, label: category }))}
        />
        <SalesFilterChip name="archived" value="1" active={params.get("archived") === "1"}>
          Show archived
        </SalesFilterChip>
        <button
          type="button"
          onClick={openNew}
          data-qa-sales-new
          className="ml-auto inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] font-medium text-panel"
        >
          <Plus className="h-3.5 w-3.5" aria-hidden />
          New product
        </button>
      </SalesToolbar>

      {notice ? (
        <p
          data-qa-sales-notice
          className="mb-3 rounded-md bg-positive-soft px-3 py-2 text-[12.5px] text-positive"
        >
          {notice}
        </p>
      ) : null}

      {draft ? (
        <form
          onSubmit={save}
          data-qa-sales-product-form
          className="mb-4 rounded-lg border border-line bg-panel p-3"
        >
          <div className="mb-3 flex items-center justify-between">
            <p className="text-[13px] font-medium">
              {editing ? `Edit ${draft.sku || "product"}` : "Add a product"}
            </p>
            <button
              type="button"
              onClick={() => {
                setDraft(null);
                setEditing(null);
              }}
              className="text-[12px] text-muted hover:text-ink"
            >
              Cancel
            </button>
          </div>

          {formError && !formField ? (
            <p data-qa-sales-form-error className="mb-3 rounded-md bg-negative-soft px-3 py-2 text-[12.5px] text-negative">
              {formError}
            </p>
          ) : null}

          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            <Labelled label="SKU" name="sku" error={fieldError("sku")}>
              <input
                value={draft.sku}
                onChange={(event) => setDraft({ ...draft, sku: event.target.value })}
                data-qa-sales-field="sku"
                aria-invalid={fieldError("sku") ? true : undefined}
                className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
              />
            </Labelled>
            <Labelled label="Name" name="name" error={fieldError("name")}>
              <input
                value={draft.name}
                onChange={(event) => setDraft({ ...draft, name: event.target.value })}
                data-qa-sales-field="name"
                aria-invalid={fieldError("name") ? true : undefined}
                className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
              />
            </Labelled>
            <Labelled label="Category" name="category" error={fieldError("category")}>
              <input
                list="sales-categories"
                value={draft.category}
                onChange={(event) => setDraft({ ...draft, category: event.target.value })}
                data-qa-sales-field="category"
                className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
              />
              <datalist id="sales-categories">
                {(vocabulary?.categories ?? []).map((category) => (
                  <option key={category} value={category} />
                ))}
              </datalist>
            </Labelled>
            <Labelled label="Unit" name="unit" error={fieldError("unit")}>
              <select
                value={draft.unit}
                onChange={(event) => setDraft({ ...draft, unit: event.target.value })}
                data-qa-sales-field="unit"
                className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none"
              >
                {SALES_UNITS.map((unit) => (
                  <option key={unit.value} value={unit.value}>
                    {unit.label}
                  </option>
                ))}
              </select>
            </Labelled>
            <Labelled label="Tax %" name="tax_percent" error={fieldError("tax_percent")}>
              <input
                type="number"
                min="0"
                max="100"
                step="0.01"
                value={draft.tax_percent}
                onChange={(event) => setDraft({ ...draft, tax_percent: event.target.value })}
                data-qa-sales-field="tax_percent"
                className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
              />
            </Labelled>
            <Labelled label="Default price" name="default_price" error={fieldError("default_price")}>
              <input
                inputMode="decimal"
                value={draft.default_price}
                onChange={(event) => setDraft({ ...draft, default_price: event.target.value })}
                data-qa-sales-field="default_price"
                aria-invalid={fieldError("default_price") ? true : undefined}
                className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
              />
            </Labelled>
            <Labelled label="Currency" name="currency" error={fieldError("currency")}>
              <input
                value={draft.currency}
                onChange={(event) => setDraft({ ...draft, currency: event.target.value })}
                data-qa-sales-field="currency"
                maxLength={3}
                aria-invalid={fieldError("currency") ? true : undefined}
                className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] uppercase outline-none focus:border-ink-soft"
              />
            </Labelled>
            <Labelled label="Description" name="description" error={fieldError("description")}>
              <textarea
                value={draft.description}
                onChange={(event) => setDraft({ ...draft, description: event.target.value })}
                data-qa-sales-field="description"
                rows={2}
                className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
              />
            </Labelled>
            <label className="flex items-center gap-2 text-[13px]">
              <input
                type="checkbox"
                checked={draft.active}
                onChange={(event) => setDraft({ ...draft, active: event.target.checked })}
                data-qa-sales-field="active"
                className="h-3.5 w-3.5"
              />
              Offered to new quote lines
            </label>
          </div>

          <div className="mt-3 flex items-center gap-2">
            <button
              type="submit"
              disabled={saving}
              data-qa-sales-save
              className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12.5px] font-medium text-panel disabled:opacity-60"
            >
              {saving ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
              {editing ? "Save product" : "Add product"}
            </button>
          </div>
        </form>
      ) : null}

      {error ? (
        <ErrorState error={error} onRetry={() => setReloadToken((token) => token + 1)} />
      ) : !page ? (
        <LoadingTable columns={COLUMNS.length} />
      ) : items.length === 0 ? (
        <EmptyState
          title={filtered ? "Nothing matches that filter" : "No products yet"}
          hint={
            filtered
              ? "The catalog has products, just not the ones this filter names."
              : "A quote line needs something to point at. Add the first product and it becomes available to every price list."
          }
          action={
            filtered ? (
              <button
                type="button"
                onClick={() => router.replace("/sales/catalog")}
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
              >
                <RotateCcw className="h-3.5 w-3.5" aria-hidden />
                Clear the filters
              </button>
            ) : (
              <button
                type="button"
                onClick={openNew}
                data-qa-sales-empty-new
                className="inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] font-medium text-panel"
              >
                <Plus className="h-3.5 w-3.5" aria-hidden />
                Add your first product
              </button>
            )
          }
        />
      ) : (
        <div className="overflow-x-auto rounded-lg border border-line">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                {COLUMNS.map((column) => (
                  <th key={column} scope="col" className="px-3 py-2 font-medium">
                    {column}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {items.map((product, index) => {
                const cursor = salesRowCursor(index === selected);
                return (
                  <tr
                    key={product.id}
                    data-qa-sales-product={product.sku}
                    aria-selected={cursor["aria-selected"]}
                    data-qa-sales-cursor={cursor["data-qa-sales-cursor"]}
                    className={`border-t border-line ${cursor.className} ${
                      product.archived_at ? "opacity-60" : ""
                    }`}
                  >
                    <td className="px-3 py-2 font-mono text-[12px]">{product.sku}</td>
                    <td className="px-3 py-2">
                      <button
                        type="button"
                        onClick={() => router.push(`/sales/catalog/${product.id}`)}
                        data-qa-sales-open={product.sku}
                        className="text-left hover:underline"
                      >
                        {product.name}
                      </button>
                    </td>
                    <td className="px-3 py-2 text-muted">{product.category ?? "—"}</td>
                    <td className="px-3 py-2 text-muted">{unitLabel(product.unit)}</td>
                    <td className="px-3 py-2 text-right text-muted">{product.tax_percent}%</td>
                    <td className="px-3 py-2 text-right font-medium">
                      {formatMoney(product.default_price, product.currency)}
                    </td>
                    <td className="px-3 py-2">
                      {product.archived_at ? (
                        <span className="text-[11.5px] text-muted">Archived</span>
                      ) : product.active ? (
                        <span className="text-[11.5px] text-positive">Active</span>
                      ) : (
                        <span className="text-[11.5px] text-muted">Hidden</span>
                      )}
                    </td>
                    <td className="px-3 py-2">
                      <span className="flex items-center justify-end gap-1">
                        <button
                          type="button"
                          onClick={() => openEdit(index)}
                          data-qa-sales-edit={product.sku}
                          aria-label={`Edit ${product.name}`}
                          className="rounded p-1 text-muted hover:text-ink"
                        >
                          <Pencil className="h-3.5 w-3.5" aria-hidden />
                        </button>
                        {product.archived_at ? null : (
                          <button
                            type="button"
                            onClick={() => archive(product)}
                            data-qa-sales-archive={product.sku}
                            aria-label={`Archive ${product.name}`}
                            className="rounded p-1 text-muted hover:text-ink"
                          >
                            <Archive className="h-3.5 w-3.5" aria-hidden />
                          </button>
                        )}
                      </span>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {shortcutsOpen ? <SalesShortcutSheet onClose={() => setShortcutsOpen(false)} /> : null}
    </div>
  );
}

/** A labelled input, with the refusal that belongs to *this* field under it. */
function Labelled({
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
    <label className="block text-[12.5px]">
      <span className="mb-1 block text-muted">{label}</span>
      {children}
      {error ? (
        <span data-qa-sales-field-error={name} className="mt-1 block text-[11.5px] text-negative">
          {error}
        </span>
      ) : null}
    </label>
  );
}
