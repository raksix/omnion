import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeploymentOverview } from "@/features/deployment/deployment-overview";

export const metadata = { title: "Deployment" };

export default function DeploymentPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Deployment"
        description="What this instance is running, and what it may upgrade to"
      >
        <DeploymentOverview />
      </AppShell>
    </RequireAuth>
  );
}
