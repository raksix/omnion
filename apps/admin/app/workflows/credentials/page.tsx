import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { CredentialList } from "@/features/nodes/credential-list";

export const metadata = { title: "Credentials" };

export default function CredentialsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Credentials"
        description="The keys your workflows use, and what state each one is in"
      >
        {/* The filters live in the query string so a filtered list is shareable, which means
            they are read on the client and need a boundary. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading credentials…</p>}>
          <CredentialList />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
