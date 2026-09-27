import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { RoleDetailView } from "@/features/iam/role-detail-view";

export const metadata = { title: "Role" };

/**
 * One role in full (REQ-006, slice 1): the tri-state permission matrix with its diff preview,
 * the members the role applies to, the roles that inherit from it and its version history.
 */
export default async function RoleDetailPage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="Role" description="Permissions, members, inheritance and history">
        <RoleDetailView roleId={id} />
      </AppShell>
    </RequireAuth>
  );
}
