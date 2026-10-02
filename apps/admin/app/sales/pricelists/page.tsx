import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SalesTabs } from "@/features/sales/sales-parts";
import { PriceListsView } from "@/features/sales/pricelists-view";

export const metadata = { title: "Sales · Price lists" };

export default function SalesPriceListsPage() {
  return (
    <RequireAuth>
      <AppShell title="Sales" description="What a customer group pays, and when that price applies">
        <SalesTabs />
        {/* The screens read their filters out of the URL, so they need a boundary for the params. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading…</p>}>
          <PriceListsView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
