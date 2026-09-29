import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { InstalledPackages } from "@/features/nodes/installed-packages";

export const metadata = { title: "Installed packages" };

export default function InstalledPackagesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Installed node packages"
        description="The packages this installation added, what they asked for, and what removing one does to the workflows that use it"
      >
        {/* The ledger is read on the client so the refresh button re-reads it rather than
            re-rendering a server snapshot the person cannot refresh. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Reading the ledger…</p>}>
          <InstalledPackages />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
