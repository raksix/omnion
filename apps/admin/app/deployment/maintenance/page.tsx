import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeploymentMaintenance } from "@/features/deployment/deployment-maintenance";

export const metadata = { title: "Maintenance windows" };

export default function MaintenancePage() {
  return (
    <RequireAuth>
      <AppShell
        title="Maintenance windows"
        description="Pause writes for an environment, with a message every client sees"
      >
        <DeploymentMaintenance />
      </AppShell>
    </RequireAuth>
  );
}
