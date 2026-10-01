import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeploymentReleaseDetailView } from "@/features/deployment/deployment-releases";

export const metadata = { title: "Release" };

export default function DeploymentReleasePage() {
  return (
    <RequireAuth>
      <AppShell title="Release" description="What this version changed, and what it needs">
        <DeploymentReleaseDetailView />
      </AppShell>
    </RequireAuth>
  );
}
