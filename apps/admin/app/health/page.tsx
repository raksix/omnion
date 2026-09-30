import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { HealthOverviewScreen } from "@/features/health/health-overview";

export const metadata = { title: "System Health" };

export default function HealthPage() {
  return (
    <RequireAuth>
      <AppShell
        title="System Health"
        description="What this deployment can actually reach right now — and what it could not check"
      >
        <HealthOverviewScreen />
      </AppShell>
    </RequireAuth>
  );
}
