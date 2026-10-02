import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeploymentHistory } from "@/features/deployment/deployment-history";

export const metadata = { title: "Deployment history" };

export default function DeploymentHistoryPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Deployment history"
        description="Every deploy, rollback and restart, with its steps"
      >
        <DeploymentHistory />
      </AppShell>
    </RequireAuth>
  );
}
