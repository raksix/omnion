import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { GroupsView } from "@/features/iam/groups-view";

export const metadata = { title: "Groups" };

/**
 * Groups (teams) and their membership (REQ-006, slice 2): a group carries roles for everyone in
 * it, and both the membership and the roles are edited on this screen.
 */
export default function IamGroupsPage() {
  return (
    <RequireAuth>
      <AppShell title="Groups" description="Teams that carry roles for their members">
        <GroupsView />
      </AppShell>
    </RequireAuth>
  );
}
