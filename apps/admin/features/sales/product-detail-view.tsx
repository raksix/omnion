"use client";

/**
 * One product (REQ-052, slice 1): `/sales/catalog/{id}`.
 *
 * The screen answers the two questions a seller actually has about a product: *what does it cost
 * on each list we sell from*, and *is it still offered*. Everything else on it — the archive state,
 * the fallback price — is there because those two answers are not complete without them:
 *
 * * **A product with no row on a list is not an error, it is a price.** It falls back to the
 *   product's own default, and the screen says so on the row rather than showing a blank. A seller
 *   who cannot tell "no price here" from "priced at zero" will eventually quote at zero.
 * * **An archived product is still readable, on purpose.** A quote line from last month names it,
 *   so the row cannot disappear; what it stops doing is being offered to a new line. The banner
 *   says exactly that, because "archived" otherwise reads as "gone" and the difference is the
 *   whole reason the archive exists.
 */
import { useCallback, useEffect, useState } from "react";
import { useRouter } from "next/navigation";

import { Archive, Loader2 } from "lucide-react";

import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import {
  archiveSalesProduct,
  fetchResolvedPrice,
  fetchSalesPriceLists,
  fetchSalesProduct,
  formatMoney,
  unitLabel,
  validityHint,
  type SalesPriceList,
  type SalesProduct,
} from "@/lib/sales";

import { useSales } from "./sales-parts";

/** One list's answer for this product. */
type ListPrice = {
  list: SalesPriceList;
  price: string | null;
  source: "resolved" | "default" | "unavailable";
};

/** `/sales/catalog/{id}`. */
export function ProductDetailView({ productId }: { productId: string }) {
  const router = useRouter();
  const { organizationId } = useSales();

  const [product, setProduct] = useState<SalesProduct | null>(null);
  const [lists, setLists] = useState<SalesPriceList[]>([]);
  const [prices, setPrices] = useState<ListPrice[]>([]);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [reloadToken, setReloadToken] = useState(0);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(() => {
    setError(null);
    fetchSalesProduct(productId, organizationId)
      .then(setProduct)
      .catch((problem) => setError(toScreenError(problem, "The product could not be loaded.")));

    fetchSalesPriceLists({ organization_id: organizationId ?? undefined, limit: 100 })
      .then((page) => {
        setLists(page.items.filter((list) => list.active && !list.archived_at));
        return page.items;
      })
      .catch(() => setLists([]));
  }, [productId, organizationId]);

  useEffect(() => {
    load();
  }, [load, reloadToken]);

  // The price on each list is asked of the same endpoint the quote builder uses, so what this
  // screen shows and what a quote will charge cannot be two different answers. Asking per list is a
  // request each; the alternative — reading the rows and reimplementing the fallback rule here — is
  // a second copy of the one rule that decides what a customer pays.
  useEffect(() => {
    if (lists.length === 0) return;
    let cancelled = false;
    void Promise.all(
      lists.map((list) =>
        fetchResolvedPrice(productId, "1", list.id, organizationId)
          .then((resolved) => ({ list, price: resolved.unit_price, source: resolved.source }))
          .catch((): ListPrice => ({ list, price: null, source: "unavailable" })),
      ),
    ).then((answers) => {
      if (!cancelled) setPrices(answers);
    });
    return () => {
      cancelled = true;
    };
  }, [lists, productId, organizationId]);

  const archive = async () => {
    if (!product) return;
    setBusy(true);
    setNotice(null);
    try {
      const archived = await archiveSalesProduct(product.id, organizationId);
      setProduct(archived);
      setNotice(
        "Archived. It is out of the catalog, and the quote lines that already named it still resolve.",
      );
    } catch (problem) {
      setError(toScreenError(problem, "The product could not be archived."));
    } finally {
      setBusy(false);
    }
  };

  if (error && !product) {
    return <ErrorState error={error} onRetry={() => setReloadToken((token) => token + 1)} />;
  }
  if (!product) {
    return <LoadingTable columns={2} rows={4} />;
  }

  const defaultPrice = product.default_price;

  return (
    <div>
      <p className="mb-3 text-[12.5px] text-muted">
        <button type="button" onClick={() => router.push("/sales/catalog")} className="hover:underline">
          Catalog
        </button>{" "}
        / {product.sku}
      </p>

      {product.archived_at ? (
        <p
          data-qa-sales-archived-banner
          className="mb-3 rounded-md bg-quiet-soft px-3 py-2 text-[12.5px] text-muted"
        >
          This product is archived. It is no longer offered to new quote lines, and the quotes that
          already named it keep resolving against this record.
        </p>
      ) : null}
      {notice ? (
        <p data-qa-sales-notice className="mb-3 rounded-md bg-positive-soft px-3 py-2 text-[12.5px] text-positive">
          {notice}
        </p>
      ) : null}

      <div className="mb-4 rounded-lg border border-line bg-panel p-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <h2 className="text-[15px] font-medium">{product.name}</h2>
            <p className="font-mono text-[12px] text-muted">{product.sku}</p>
          </div>
          <span className="flex items-center gap-2">
            <button
              type="button"
              onClick={() => router.push(`/sales/catalog?search=${encodeURIComponent(product.sku)}`)}
              className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
            >
              Edit in the catalog
            </button>
            {product.archived_at ? null : (
              <button
                type="button"
                onClick={archive}
                disabled={busy}
                data-qa-sales-archive-here
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-60"
              >
                {busy ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                ) : (
                  <Archive className="h-3.5 w-3.5" aria-hidden />
                )}
                Archive
              </button>
            )}
          </span>
        </div>

        <dl className="mt-4 grid gap-3 sm:grid-cols-3">
          <Fact label="Default price" value={formatMoney(defaultPrice, product.currency)} />
          <Fact label="Unit" value={unitLabel(product.unit)} />
          <Fact label="Tax" value={`${product.tax_percent}%`} />
          <Fact label="Category" value={product.category ?? "—"} />
          <Fact
            label="State"
            value={product.archived_at ? "Archived" : product.active ? "Offered" : "Hidden"}
          />
          <Fact
            label="Prices on"
            value={String(prices.filter((entry) => entry.source === "resolved").length)}
          />
        </dl>

        {product.description ? (
          <p className="mt-4 whitespace-pre-line text-[13px] text-muted">{product.description}</p>
        ) : null}
      </div>

      <h3 className="mb-2 text-[13px] font-medium">What it costs on each list</h3>
      {lists.length === 0 ? (
        <p className="rounded-lg border border-line px-3 py-4 text-center text-[12.5px] text-muted">
          There are no active price lists yet, so every quote uses the default price of{" "}
          {formatMoney(defaultPrice, product.currency)}.
        </p>
      ) : prices.length === 0 ? (
        <LoadingTable columns={3} rows={2} />
      ) : (
        <div className="overflow-x-auto rounded-lg border border-line">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                {["Price list", "Price for 1", "Where it comes from", "Valid until"].map((column) => (
                  <th key={column} scope="col" className="px-3 py-2 font-medium">
                    {column}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {prices.map((entry) => {
                const hint = validityHint(entry.list.valid_until);
                return (
                  <tr key={entry.list.id} data-qa-sales-list-price={entry.list.name} className="border-t border-line">
                    <td className="px-3 py-2">
                      <button
                        type="button"
                        onClick={() => router.push(`/sales/pricelists/${entry.list.id}`)}
                        className="text-left hover:underline"
                      >
                        {entry.list.name}
                      </button>
                    </td>
                    <td className="px-3 py-2 font-medium">
                      {entry.price === null
                        ? "—"
                        : formatMoney(entry.price, entry.list.currency)}
                    </td>
                    <td className="px-3 py-2">
                      {entry.source === "resolved" ? (
                        <span className="text-[12px] text-positive">This list&apos;s price</span>
                      ) : entry.source === "default" ? (
                        <span className="text-[12px] text-muted">
                          No row — the default, {formatMoney(defaultPrice, product.currency)}
                        </span>
                      ) : (
                        <span className="text-[12px] text-muted">Could not be read</span>
                      )}
                    </td>
                    <td
                      className={`px-3 py-2 text-[12px] ${
                        hint.tone === "expired"
                          ? "text-negative"
                          : hint.tone === "warn"
                            ? "text-caution"
                            : "text-muted"
                      }`}
                    >
                      {hint.text}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

/** One labelled fact in the header card. */
function Fact({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <dt className="text-[11.5px] uppercase tracking-wide text-muted">{label}</dt>
      <dd className="text-[13px]">{value}</dd>
    </div>
  );
}
