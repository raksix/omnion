import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { PipelinesSettingsView } from "@/features/crm/deals-view";

export const metadata = { title: "CRM · Pipelines" };

export default function CrmPipelinesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="CRM"
        description="The relationship layer: the people, the companies they work for, and the deals in between"
      >
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the stages…</p>}>
          <PipelinesSettingsView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
