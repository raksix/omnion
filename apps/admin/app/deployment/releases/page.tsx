import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeploymentReleases } from "@/features/deployment/deployment-releases";

export const metadata = { title: "Releases" };

export default function DeploymentReleasesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Releases"
        description="What the update check has cached for this channel"
      >
        <DeploymentReleases />
      </AppShell>
    </RequireAuth>
  );
}
