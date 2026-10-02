import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ReliabilityBreakersView } from "@/features/reliability/breakers-view";

export const metadata = { title: "Breakers" };

/**
 * `/settings/reliability/breakers` — the outbound circuit breakers (REQ-127, slice 3).
 *
 * Its own route because it is the screen a maintainer opens to **stop** the platform calling
 * something: `Force open` drains a provider until somebody says otherwise, which is the one
 * action on this centre that changes production behaviour immediately.
 */
export default function ReliabilityBreakersPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Breakers"
        description="One breaker per outbound provider, with its state, thresholds, cooldown and transition timeline — including a forced drain that stays refused until it is reset"
      >
        <ReliabilityBreakersView />
      </AppShell>
    </RequireAuth>
  );
}
