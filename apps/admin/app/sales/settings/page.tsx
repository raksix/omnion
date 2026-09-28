import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SalesTabs } from "@/features/sales/sales-parts";
import { SalesSettingsView } from "@/features/sales/settings-view";

export const metadata = { title: "Sales · Settings" };

export default function SalesSettingsPage() {
  return (
    <RequireAuth>
      <AppShell title="Sales" description="The defaults every new quote is built from">
        <SalesTabs />
        {/* The screens read their filters out of the URL, so they need a boundary for the params. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading…</p>}>
          <SalesSettingsView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
