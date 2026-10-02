import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SalesTabs } from "@/features/sales/sales-parts";
import { ProductDetailView } from "@/features/sales/product-detail-view";

export const metadata = { title: "Sales · Product" };

/** `/sales/catalog/{id}`: one product, its prices across the lists, and its state. */
export default async function SalesProductPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell
        title="Sales"
        description="What this organization sells: the products a quote line can point at"
      >
        <SalesTabs />
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the product…</p>}>
          <ProductDetailView productId={id} />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
