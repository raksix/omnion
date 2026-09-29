import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ReliabilityLimitsView } from "@/features/reliability/limits-view";

export const metadata = { title: "Rate limits" };

/**
 * `/settings/reliability/limits` — the platform-wide budgets (REQ-127, slice 1).
 *
 * Deliberately BESIDE `/settings/security/rate-limits`, which is the gateway's per-route document
 * (REQ-040). Both are live in the same request chain, so an operator who cannot tell which
 * document refused a caller widens the wrong one; every refusal this layer writes names itself
 * through `details.limiter`, and the screen shows the same `limiter` name in its header tile.
 */
export default function ReliabilityLimitsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Rate limits"
        description="Platform-wide budgets per user, organization, IP and route, with the refusal rollup and a dry-run that resolves the same policy the request path does"
      >
        <ReliabilityLimitsView />
      </AppShell>
    </RequireAuth>
  );
}
