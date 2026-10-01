import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { UpgradeView } from "@/features/deployment/upgrade-view";

export const metadata = { title: "Upgrade helper" };

/**
 * `/deployment/upgrade` — the ordered path from this build to a target release
 * (REQ-128, slice 4).
 *
 * Two sentences decide what this screen is. The first: **application rollback is always
 * available, database rollback is not** — so the split is shown as two cards with their own
 * commands, and "available" for a database means a verified down script rather than a backup
 * pretending to be one. The second: the destructiveness verdict may be **`unknown`**, because a
 * migration with no down script has not been proven irreversible either, and a release manifest's
 * `migrations_destructive: false` is the publisher's silence rather than a verification.
 *
 * The acknowledgement is a real gate, not a disabled button: until an operator accepts the
 * plan's own verdict, the checklist says what it is waiting for instead of rendering as done.
 */
export default function DeploymentUpgradePage() {
  return (
    <RequireAuth>
      <AppShell
        title="Upgrade helper"
        description="The ordered steps from this build to a target release, what can be rolled back and what cannot, and the point of no return"
      >
        <UpgradeView />
      </AppShell>
    </RequireAuth>
  );
}
