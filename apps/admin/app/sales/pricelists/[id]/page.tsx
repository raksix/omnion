import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SalesTabs } from "@/features/sales/sales-parts";
import { PriceListDetailView } from "@/features/sales/pricelists-view";

export const metadata = { title: "Sales · Price list" };

/** `/sales/pricelists/{id}`: one list and the grid of prices it carries. */
export default async function SalesPriceListPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="Sales" description="What a customer group pays, and when that price applies">
        <SalesTabs />
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the price list…</p>}>
          <PriceListDetailView listId={id} />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
