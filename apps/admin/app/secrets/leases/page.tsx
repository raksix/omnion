import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { LeasesView } from "@/features/secrets/leases-view";

export const metadata = { title: "Credential leases" };

/**
 * `/secrets/leases` — the outstanding leases (REQ-125, slice 3).
 *
 * A lease is a live copy of a credential, and a deployment makes every one of them false. The
 * screen therefore shows the countdown, the redemption budget and — the part that matters most —
 * *why* a lease is dead, including the automatic revocation a `deployment.started` event causes.
 * There is no reveal here even for an operator who may reveal a secret: a lease is the whole point
 * of a path that keeps values out of browsers.
 */
export default function SecretsLeasesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Credential leases"
        description="Short-lived, use-capped copies of a credential — revoked by a deploy, never read here"
      >
        <LeasesView />
      </AppShell>
    </RequireAuth>
  );
}
