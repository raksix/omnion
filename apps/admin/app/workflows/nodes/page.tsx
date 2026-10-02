import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { NodeLibrary } from "@/features/nodes/node-library";

export const metadata = { title: "Node library" };

export default function NodeLibraryPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Node library"
        description="Every node the automation canvas can place, and what each one needs"
      >
        {/* The filters live in the query string so a filtered library is shareable, which
            means they are read on the client and need a boundary. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the registry…</p>}>
          <NodeLibrary />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
