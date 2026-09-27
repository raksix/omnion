import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { RolesView } from "@/features/iam/roles-view";

export const metadata = { title: "Roles" };

/**
 * The role list (REQ-006, slice 1): platform roles plus the organization's own, with their
 * allow/deny counts, and the whole lifecycle — create, open, duplicate, delete.
 */
export default function RolesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Roles & access"
        description="Custom roles over the platform ladder: permissions, inheritance and history"
      >
        <RolesView />
      </AppShell>
    </RequireAuth>
  );
}
