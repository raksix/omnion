import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { HealthSettingsScreen } from "@/features/health/health-settings";

export const metadata = { title: "Health settings" };

export default function HealthSettingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Health settings"
        description="Thresholds, check intervals and the windows where a planned change is allowed to be quiet"
      >
        <HealthSettingsScreen />
      </AppShell>
    </RequireAuth>
  );
}