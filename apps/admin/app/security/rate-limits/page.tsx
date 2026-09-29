import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { RateLimitsScreen } from "@/features/security/rate-limits";

export const metadata = { title: "Rate limits" };

export default function SecurityRateLimitsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Rate limits"
        description="What each scope allows, and whether a given request would be refused by it"
      >
        <RateLimitsScreen />
      </AppShell>
    </RequireAuth>
  );
}
