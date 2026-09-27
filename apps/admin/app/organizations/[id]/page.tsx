import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { OrganizationDetailView } from "@/features/organizations/organization-detail-view";

export const metadata = { title: "Organization" };

/**
 * `/organizations/[id]` (REQ-005, slice 1): one tenant. This slice ships the Members tab with
 * the invite dialog; departments, roles, modules, settings, billing and audit arrive with
 * slices 2 and 3.
 */
export default async function OrganizationDetailPage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="Organization" description="Members, invitations and — from slice 2 — departments and roles">
        <OrganizationDetailView organizationId={id} />
      </AppShell>
    </RequireAuth>
  );
}
