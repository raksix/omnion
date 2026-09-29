import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { MembersView } from "@/features/members/members-view";

export const metadata = { title: "Members" };

/**
 * `/members` — visitor accounts and the site's membership policy (REQ-064, slice 4c).
 *
 * Carries `memberships.read` for the tables and `memberships.manage` for every write, so the page
 * checks no permission itself: the route guard answers `403` and the panel renders its own state.
 * A second, client-side check would be a second answer to the same question, and the two would
 * eventually disagree.
 */
export default function MembersPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Members"
        description="Who signed up on the site itself — and none of it is a panel account"
      >
        <MembersView />
      </AppShell>
    </RequireAuth>
  );
}
