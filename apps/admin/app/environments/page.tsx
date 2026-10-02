import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { EnvironmentsView } from "@/features/environments/environments-view";

export const metadata = { title: "Environments" };

export default function EnvironmentsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Environments"
        description="Copies of this tenant's content, what each one holds, and when it was last refreshed"
      >
        {/* The type, status and search filters live in the query string so a reload and a shared
            link both keep them, which means the screen is read on the client. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the environments…</p>}>
          <EnvironmentsView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
