import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DealsView } from "@/features/crm/deals-view";

export const metadata = { title: "CRM · Deals" };

export default function CrmDealsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="CRM"
        description="The relationship layer: the people, the companies they work for, and the deals in between"
      >
        {/* The board reads its pipeline out of the API, so it needs a boundary for search params. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the pipeline…</p>}>
          <DealsView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
