import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeploymentChecks } from "@/features/deployment/deployment-history";

export const metadata = { title: "Update checks" };

export default function DeploymentChecksPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Update checks"
        description="When the release manifest was last read, and what it announced"
      >
        <DeploymentChecks />
      </AppShell>
    </RequireAuth>
  );
}
