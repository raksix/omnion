"use client";

/**
 * The price lists (REQ-052, slice 1): `/sales/pricelists` and one list's editor.
 *
 * A price list is the answer to "what does this customer pay", and the screen has to make two
 * things true at once: the list's own window decides whether a seller may quote from it, and a
 * product with no row on the list falls back to its default price rather than pricing at nothing.
 * Both are visible here, because a list that silently prices everything at the default is a
 * revenue bug nobody would look for.
 *
 * The rows are saved in **one** request, which is why the editor is a grid with a Save rather than
 * a per-row save: a save that deleted the rows and then failed on the third insert would leave a
 * list that prices nothing, and every quote built on it would quietly use the default.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";

import { Archive, Loader2, Plus, RotateCcw, Save, Trash2 } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  archiveSalesPriceList,
  createSalesPriceList,
  fetchSalesPriceList,
  fetchSalesPriceLists,
  fetchSalesProducts,
  fieldOf,
  formatMoney,
  replaceSalesPriceRows,
  updateSalesPriceList,
  validityHint,
  type SalesPriceList,
  type SalesPriceListDetail,
  type SalesProduct,
} from "@/lib/sales";

import {
  SalesFilterChip,
  SalesShortcutSheet,
  SalesToolbar,
  salesRowCursor,
  useSales,
  useSalesKeyboard,
} from "./sales-parts";

/** One row being edited. The price stays a string for the same reason the form's does. */
type RowDraft = { product_id: string; price: string; min_quantity: string };

/** `/sales/pricelists`: the lists, and the form that creates one. */
export function PriceListsView() {
  const router = useRouter();
  const params = useSearchParams();
  const { organizationId } = useSales();

  const [page, setPage] = useState<{ items: SalesPriceList[]; total_estimate: number } | null>(null);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [reloadToken, setReloadToken] = useState(0);
  const [selected, setSelected] = useState(0);
  const [search, setSearch] = useState(params.get("search") ?? "");
  const searchRef = useRef<HTMLInputElement | null>(null);

  const [name, setName] = useState("");
  const [currency, setCurrency] = useState("");
  const [validFrom, setValidFrom] = useState("");
  const [validUntil, setValidUntil] = useState("");
  const [creating, setCreating] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [formField, setFormField] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const search_ = params.get("search") ?? "";

  useEffect(() => {
    setError(null);
    fetchSalesPriceLists({
      search: search_ || undefined,
      organization_id: organizationId ?? undefined,
      limit: 100,
    })
      .then((loaded) => {
        setPage(loaded);
        setSelected((current) => (current < loaded.items.length ? current : 0));
      })
      .catch((problem) => setError(toScreenError(problem, "The price lists could not be loaded.")));
  }, [search_, organizationId, reloadToken]);

  const items = page?.items ?? [];
  const filtered = search_ !== "" || params.get("archived") === "1";

  const { shortcutsOpen, setShortcutsOpen } = useSalesKeyboard(
    {
      count: items.length,
      selected,
      onSelect: setSelected,
      onOpen: (index) => {
        const list = items[index];
        if (list) router.push(`/sales/pricelists/${list.id}`);
      },
      onNew: () => {
        setName("");
        setCurrency("");
        setValidFrom("");
        setValidUntil("");
        setFormError(null);
        setFormField(null);
        (document.querySelector('[data-qa-sales-field="name"]') as HTMLInputElement | null)?.focus();
      },
    },
    searchRef,
  );

  const create = async (event: React.FormEvent) => {
    event.preventDefault();
    setCreating(true);
    setFormError(null);
    setFormField(null);
    try {
      const created = await createSalesPriceList(
        {
          name: name.trim(),
          // An empty date field is `null`, not the epoch: the list is open-ended until somebody
          // gives it an end, and `1970-01-01` would be a list that expired the day it was made.
          valid_from: validFrom.trim() === "" ? null : validFrom.trim(),
          valid_until: validUntil.trim() === "" ? null : validUntil.trim(),
          ...(currency.trim() === "" ? {} : { currency: currency.trim().toUpperCase() }),
        },
        organizationId,
      );
      setNotice(`Created ${created.name}. Add its prices next.`);
      setName("");
      setCurrency("");
      setValidFrom("");
      setValidUntil("");
      setReloadToken((token) => token + 1);
    } catch (problem) {
      setFormError(problem instanceof Error ? problem.message : "The list could not be created.");
      setFormField(fieldOf(problem));
    } finally {
      setCreating(false);
    }
  };

  const archive = async (list: SalesPriceList) => {
    setNotice(null);
    try {
      await archiveSalesPriceList(list.id, organizationId);
      setNotice(`${list.name} is archived, and its name is free for a new list.`);
      setReloadToken((token) => token + 1);
    } catch (problem) {
      setError(toScreenError(problem, `${list.name} could not be archived.`));
    }
  };

  const fieldError = (field: string) => (formField === field ? formError : null);

  return (
    <div>
      <SalesToolbar
        search={search}
        onSearchChange={setSearch}
        searchRef={searchRef}
        count={items.length}
        filtered={filtered}
      >
        <SalesFilterChip name="archived" value="1" active={params.get("archived") === "1"}>
          Show archived
        </SalesFilterChip>
      </SalesToolbar>

      {notice ? (
        <p data-qa-sales-notice className="mb-3 rounded-md bg-positive-soft px-3 py-2 text-[12.5px] text-positive">
          {notice}
        </p>
      ) : null}

      <form onSubmit={create} data-qa-sales-list-form className="mb-4 rounded-lg border border-line bg-panel p-3">
        <p className="mb-3 text-[13px] font-medium">New price list</p>
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          <ListField label="Name" error={fieldError("name")}>
            <input
              value={name}
              onChange={(event) => setName(event.target.value)}
              data-qa-sales-field="name"
              aria-invalid={fieldError("name") ? true : undefined}
              className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
            />
          </ListField>
          <ListField label="Currency" error={fieldError("currency")}>
            <input
              value={currency}
              onChange={(event) => setCurrency(event.target.value)}
              data-qa-sales-field="currency"
              maxLength={3}
              placeholder="From the settings"
              className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] uppercase outline-none focus:border-ink-soft"
            />
          </ListField>
          <ListField label="Valid from" error={fieldError("valid_from")}>
            <input
              type="date"
              value={validFrom}
              onChange={(event) => setValidFrom(event.target.value)}
              data-qa-sales-field="valid_from"
              className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
            />
          </ListField>
          <ListField label="Valid until" error={fieldError("valid_until")}>
            <input
              type="date"
              value={validUntil}
              onChange={(event) => setValidUntil(event.target.value)}
              data-qa-sales-field="valid_until"
              className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
            />
          </ListField>
        </div>
        {formError && !formField ? (
          <p data-qa-sales-form-error className="mt-3 rounded-md bg-negative-soft px-3 py-2 text-[12.5px] text-negative">
            {formError}
          </p>
        ) : null}
        <div className="mt-3">
          <button
            type="submit"
            disabled={creating}
            data-qa-sales-create-list
            className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12.5px] font-medium text-panel disabled:opacity-60"
          >
            {creating ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <Plus className="h-3.5 w-3.5" aria-hidden />}
            Create the list
          </button>
        </div>
      </form>

      {error ? (
        <ErrorState error={error} onRetry={() => setReloadToken((token) => token + 1)} />
      ) : !page ? (
        <LoadingTable columns={5} />
      ) : items.length === 0 ? (
        <EmptyState
          title={filtered ? "Nothing matches that filter" : "No price lists yet"}
          hint={
            filtered
              ? "There are price lists, just not the one this filter names."
              : "A price list is what one customer group pays. Until there is one, every quote uses the product's own default price."
          }
          action={
            filtered ? (
              <button
                type="button"
                onClick={() => router.replace("/sales/pricelists")}
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
              >
                <RotateCcw className="h-3.5 w-3.5" aria-hidden />
                Clear the filters
              </button>
            ) : null
          }
        />
      ) : (
        <div className="overflow-x-auto rounded-lg border border-line">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                {["Name", "Currency", "Prices", "Valid until", "State", ""].map((column) => (
                  <th key={column} scope="col" className="px-3 py-2 font-medium">
                    {column}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {items.map((list, index) => {
                const cursor = salesRowCursor(index === selected);
                const hint = validityHint(list.valid_until);
                return (
                  <tr
                    key={list.id}
                    data-qa-sales-pricelist={list.name}
                    aria-selected={cursor["aria-selected"]}
                    data-qa-sales-cursor={cursor["data-qa-sales-cursor"]}
                    className={`border-t border-line ${cursor.className} ${
                      list.archived_at ? "opacity-60" : ""
                    }`}
                  >
                    <td className="px-3 py-2">
                      <button
                        type="button"
                        onClick={() => router.push(`/sales/pricelists/${list.id}`)}
                        data-qa-sales-open-list={list.name}
                        className="text-left hover:underline"
                      >
                        {list.name}
                      </button>
                    </td>
                    <td className="px-3 py-2 text-muted">{list.currency}</td>
                    <td className="px-3 py-2 text-muted">{list.item_count}</td>
                    <td className="px-3 py-2">
                      <span
                        data-qa-sales-validity={list.name}
                        className={
                          hint.tone === "expired"
                            ? "text-[12px] text-negative"
                            : hint.tone === "warn"
                              ? "text-[12px] text-caution"
                              : "text-[12px] text-muted"
                        }
                      >
                        {hint.text}
                      </span>
                    </td>
                    <td className="px-3 py-2">
                      {list.archived_at ? (
                        <span className="text-[11.5px] text-muted">Archived</span>
                      ) : list.active ? (
                        <span className="text-[11.5px] text-positive">In use</span>
                      ) : (
                        <span className="text-[11.5px] text-muted">Off</span>
                      )}
                    </td>
                    <td className="px-3 py-2">
                      <span className="flex items-center justify-end">
                        {list.archived_at ? null : (
                          <button
                            type="button"
                            onClick={() => archive(list)}
                            data-qa-sales-archive-list={list.name}
                            aria-label={`Archive ${list.name}`}
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

/** `/sales/pricelists/{id}`: one list, its window, and the grid of prices. */
export function PriceListDetailView({ listId }: { listId: string }) {
  const router = useRouter();
  const { organizationId } = useSales();

  const [detail, setDetail] = useState<SalesPriceListDetail | null>(null);
  const [products, setProducts] = useState<SalesProduct[]>([]);
  const [rows, setRows] = useState<RowDraft[]>([]);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [reloadToken, setReloadToken] = useState(0);
  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [formField, setFormField] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    setError(null);
    fetchSalesPriceList(listId, organizationId)
      .then((loaded) => {
        setDetail(loaded);
        setRows(
          loaded.items.map((row) => ({
            product_id: row.product_id,
            price: row.price,
            min_quantity: row.min_quantity,
          })),
        );
      })
      .catch((problem) => setError(toScreenError(problem, "The price list could not be loaded.")));

    // The catalog is what a row may name, so a row cannot be typed for a product that does not
    // exist — the form's own source of truth is the same list the module validates against.
    fetchSalesProducts({ organization_id: organizationId ?? undefined, limit: 200 })
      .then((page) => setProducts(page.items))
      .catch(() => setProducts([]));
  }, [listId, organizationId, reloadToken]);

  const addRow = (product: SalesProduct) => {
    if (rows.some((row) => row.product_id === product.id)) return;
    setRows([...rows, { product_id: product.id, price: product.default_price, min_quantity: "1" }]);
  };

  const removeRow = (productId: string) => {
    setRows(rows.filter((row) => row.product_id !== productId));
  };

  const save = async (event: React.FormEvent) => {
    event.preventDefault();
    setSaving(true);
    setFormError(null);
    setFormField(null);
    try {
      const saved = await replaceSalesPriceRows(
        listId,
        rows.map((row) => ({
          product_id: row.product_id,
          price: row.price.trim() || "0.00",
          min_quantity: row.min_quantity.trim() || "1",
        })),
        organizationId,
      );
      setDetail(saved);
      setNotice(`Saved ${saved.items.length} price${saved.items.length === 1 ? "" : "s"}.`);
    } catch (problem) {
      setFormError(problem instanceof Error ? problem.message : "The prices could not be saved.");
      setFormField(fieldOf(problem));
    } finally {
      setSaving(false);
    }
  };

  const rename = async (value: string) => {
    if (!detail) return;
    try {
      const updated = await updateSalesPriceList(listId, { name: value.trim() }, organizationId);
      setDetail({ ...detail, list: updated });
      setNotice("Renamed.");
    } catch (problem) {
      setFormError(problem instanceof Error ? problem.message : "The list could not be renamed.");
    }
  };

  if (error) {
    return <ErrorState error={error} onRetry={() => setReloadToken((token) => token + 1)} />;
  }
  if (!detail) {
    return <LoadingTable columns={4} />;
  }

  const list = detail.list;
  const hint = validityHint(list.valid_until);
  const available = products.filter((product) => !rows.some((row) => row.product_id === product.id));

  return (
    <div>
      <p className="mb-3 text-[12.5px] text-muted">
        <button type="button" onClick={() => router.push("/sales/pricelists")} className="hover:underline">
          Price lists
        </button>{" "}
        / {list.name}
      </p>

      {notice ? (
        <p data-qa-sales-notice className="mb-3 rounded-md bg-positive-soft px-3 py-2 text-[12.5px] text-positive">
          {notice}
        </p>
      ) : null}

      <div className="mb-4 flex flex-wrap items-end gap-3 rounded-lg border border-line bg-panel p-3">
        <label className="block text-[12.5px]">
          <span className="mb-1 block text-muted">Name</span>
          <input
            key={list.id}
            defaultValue={list.name}
            onBlur={(event) => {
              if (event.target.value.trim() !== list.name && event.target.value.trim() !== "") {
                void rename(event.target.value);
              }
            }}
            data-qa-sales-list-name
            className="rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
          />
        </label>
        <p className="text-[12.5px] text-muted">
          {list.currency} ·{" "}
          <span className={hint.tone === "expired" ? "text-negative" : hint.tone === "warn" ? "text-caution" : ""}>
            {hint.text}
          </span>
        </p>
        {list.archived_at ? (
          <p className="text-[12.5px] text-muted">This list is archived.</p>
        ) : null}
      </div>

      <form onSubmit={save} data-qa-sales-price-form>
        {rows.length === 0 ? (
          <EmptyState
            title="No prices on this list yet"
            hint="Everything a quote uses from this list falls back to the product's own default price until a row says otherwise."
          />
        ) : (
          <div className="mb-3 overflow-x-auto rounded-lg border border-line">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                  {["Product", "From qty", "Price", ""].map((column) => (
                    <th key={column} scope="col" className="px-3 py-2 font-medium">
                      {column}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => {
                  const product = products.find((entry) => entry.id === row.product_id);
                  return (
                    <tr key={row.product_id} data-qa-sales-price-row={product?.sku ?? row.product_id} className="border-t border-line">
                      <td className="px-3 py-2">
                        <span className="font-mono text-[12px] text-muted">{product?.sku ?? "—"}</span>{" "}
                        {product?.name ?? "A product outside this catalog"}
                      </td>
                      <td className="px-3 py-2">
                        <input
                          value={row.min_quantity}
                          onChange={(event) =>
                            setRows(
                              rows.map((entry) =>
                                entry.product_id === row.product_id
                                  ? { ...entry, min_quantity: event.target.value }
                                  : entry,
                              ),
                            )
                          }
                          data-qa-sales-price-min={row.product_id}
                          inputMode="decimal"
                          className="w-24 rounded-md border border-line bg-canvas px-2 py-1 text-[13px] outline-none focus:border-ink-soft"
                        />
                      </td>
                      <td className="px-3 py-2">
                        <input
                          value={row.price}
                          onChange={(event) =>
                            setRows(
                              rows.map((entry) =>
                                entry.product_id === row.product_id
                                  ? { ...entry, price: event.target.value }
                                  : entry,
                              ),
                            )
                          }
                          data-qa-sales-price={row.product_id}
                          inputMode="decimal"
                          aria-invalid={formField === `price:${row.product_id}` ? true : undefined}
                          className="w-32 rounded-md border border-line bg-canvas px-2 py-1 text-[13px] outline-none focus:border-ink-soft"
                        />
                        {formField === `price:${row.product_id}` ? (
                          <span className="mt-1 block text-[11.5px] text-negative">{formError}</span>
                        ) : null}
                      </td>
                      <td className="px-3 py-2 text-right">
                        <button
                          type="button"
                          onClick={() => removeRow(row.product_id)}
                          data-qa-sales-remove-price={row.product_id}
                          aria-label={`Remove the price for ${product?.name ?? row.product_id}`}
                          className="rounded p-1 text-muted hover:text-ink"
                        >
                          <Trash2 className="h-3.5 w-3.5" aria-hidden />
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}

        {formError && !formField ? (
          <p data-qa-sales-form-error className="mb-3 rounded-md bg-negative-soft px-3 py-2 text-[12.5px] text-negative">
            {formError}
          </p>
        ) : null}

        <div className="mb-3 flex flex-wrap items-center gap-2">
          <label className="text-[12.5px] text-muted">
            Add a product
            <select
              value=""
              onChange={(event) => {
                const product = products.find((entry) => entry.id === event.target.value);
                if (product) addRow(product);
              }}
              data-qa-sales-add-row
              className="ml-2 rounded-md border border-line bg-panel px-2 py-1.5 text-[12.5px] outline-none"
            >
              <option value="">Choose a product…</option>
              {available.map((product) => (
                <option key={product.id} value={product.id}>
                  {product.sku} — {product.name} ({formatMoney(product.default_price, product.currency)})
                </option>
              ))}
            </select>
          </label>
          <button
            type="submit"
            disabled={saving || list.archived_at !== null}
            data-qa-sales-save-prices
            className="ml-auto inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12.5px] font-medium text-panel disabled:opacity-60"
          >
            {saving ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <Save className="h-3.5 w-3.5" aria-hidden />}
            Save the prices
          </button>
        </div>
      </form>
    </div>
  );
}

/** A labelled input for the create form, with its own refusal under it. */
function ListField({
  label,
  error,
  children,
}: {
  label: string;
  error: string | null;
  children: React.ReactNode;
}) {
  return (
    <label className="block text-[12.5px]">
      <span className="mb-1 block text-muted">{label}</span>
      {children}
      {error ? <span className="mt-1 block text-[11.5px] text-negative">{error}</span> : null}
    </label>
  );
}
