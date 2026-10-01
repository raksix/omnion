import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { HealthMetricsScreen } from "@/features/health/health-metrics";

export const metadata = { title: "Health metrics" };

export default function HealthMetricsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Health metrics"
        description="What each metric has done over a named window — the same rows the export writes"
      >
        <HealthMetricsScreen />
      </AppShell>
    </RequireAuth>
  );
}