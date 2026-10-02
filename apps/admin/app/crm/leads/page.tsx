import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { LeadsView } from "@/features/crm/leads-view";

export const metadata = { title: "CRM · Leads" };

export default function CrmLeadsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="CRM"
        description="The relationship layer: the people, the companies they work for, and the deals in between"
      >
        {/* The outcome filter lives in the URL, so the screen needs a boundary for it. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the inbox…</p>}>
          <LeadsView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
