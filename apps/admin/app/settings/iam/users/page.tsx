import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { UsersView } from "@/features/iam/users-view";

export const metadata = { title: "Users" };

/**
 * The user list (REQ-006, slice 2): search, filters over the whole directory and the create
 * form — invite an address, or set a password directly.
 */
export default function IamUsersPage() {
  return (
    <RequireAuth>
      <AppShell title="Users" description="Every account in the directory, with the roles it holds">
        <UsersView />
      </AppShell>
    </RequireAuth>
  );
}
