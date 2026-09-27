import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { CompaniesView } from "@/features/crm/companies-view";

export const metadata = { title: "CRM · Companies" };

export default function CrmCompaniesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="CRM"
        description="The relationship layer: the people, the companies they work for, and the deals in between"
      >
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the companies…</p>}>
          <CompaniesView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
