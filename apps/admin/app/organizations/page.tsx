import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { OrganizationsView } from "@/features/organizations/organizations-view";

export const metadata = { title: "Organizations" };

/**
 * `/organizations` (REQ-005, slice 1): the tenants this installation knows. A platform account
 * picks one here; an organization account is sent straight to its own overview.
 */
export default function OrganizationsPage() {
  return (
    <RequireAuth>
      <AppShell title="Organizations" description="The tenants of this installation">
        <OrganizationsView />
      </AppShell>
    </RequireAuth>
  );
}
