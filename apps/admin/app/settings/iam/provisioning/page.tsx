import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ProvisioningView } from "@/features/iam/provisioning-view";

export const metadata = { title: "Provisioning" };

/**
 * SCIM provisioning (REQ-006, slice 4b): the tokens a directory authenticates with, and the sync
 * log of everything it did.
 */
export default function IamProvisioningPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Provisioning"
        description="SCIM 2.0 tokens and the sync log — what your directory did to the accounts of this organization"
      >
        <ProvisioningView />
      </AppShell>
    </RequireAuth>
  );
}
