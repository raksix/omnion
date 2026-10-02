import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { CredentialCreate } from "@/features/nodes/credential-create";

export const metadata = { title: "New credential" };

export default function NewCredentialPage() {
  return (
    <RequireAuth>
      <AppShell
        title="New credential"
        description="Pick a type, fill in its fields, and the key your workflows will use"
      >
        {/* The type may arrive as `?type=`, which is how the list's empty-state quick actions
            skip the picker. That makes it a client read, so it needs a boundary. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the catalogue…</p>}>
          <CredentialCreate />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
