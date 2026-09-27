import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ServiceAccountsView } from "@/features/iam/service-accounts-view";

export const metadata = { title: "Service accounts" };

/**
 * Machine identities and their keys (REQ-006, slice 2): a key is shown once, when it is issued,
 * and the identity carries roles exactly the way a person does.
 */
export default function IamServiceAccountsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Service accounts"
        description="Machine identities, their keys and the roles they carry"
      >
        <ServiceAccountsView />
      </AppShell>
    </RequireAuth>
  );
}
