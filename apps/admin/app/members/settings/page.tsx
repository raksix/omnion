import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { MemberSettingsScreen } from "@/features/members/members-view";

export const metadata = { title: "Membership settings" };

/**
 * `/members/settings` — the site's membership policy (REQ-064, slice 4c).
 *
 * The REQ names this as its own route, and an owner reaches for it from a settings menu rather
 * than from the members table. It renders the SAME component `/members` embeds rather than a
 * copy: two copies of a policy form are two forms that answer the same question differently the
 * first time a field is added, and the one somebody finds later is the wrong one.
 */
export default function MemberSettingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Membership settings"
        description="Who may sign up on the site, what they get, and what a gated page tells somebody who cannot read it"
      >
        <MemberSettingsScreen />
      </AppShell>
    </RequireAuth>
  );
}
