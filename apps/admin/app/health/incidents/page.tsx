import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { HealthIncidentsScreen } from "@/features/health/health-incidents";

export const metadata = { title: "Health incidents" };

export default function HealthIncidentsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Health incidents"
        description="What broke, when it started, how long it lasted and whether anybody claimed it"
      >
        <HealthIncidentsScreen />
      </AppShell>
    </RequireAuth>
  );
}