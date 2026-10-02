import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeploymentWizard } from "@/features/deployment/deploy-wizard";

export const metadata = { title: "Deploy" };

export default function DeployPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Deploy"
        description="Pre-flight, confirm, and watch the run"
      >
        <DeploymentWizard />
      </AppShell>
    </RequireAuth>
  );
}
