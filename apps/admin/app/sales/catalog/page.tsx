import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SalesTabs } from "@/features/sales/sales-parts";
import { CatalogView } from "@/features/sales/catalog-view";

export const metadata = { title: "Sales · Catalog" };

export default function SalesCatalogPage() {
  return (
    <RequireAuth>
      <AppShell title="Sales" description="What this organization sells: the products a quote line can point at">
        <SalesTabs />
        {/* The screens read their filters out of the URL, so they need a boundary for the params. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading…</p>}>
          <CatalogView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
