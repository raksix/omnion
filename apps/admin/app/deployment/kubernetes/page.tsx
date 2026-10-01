import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeploymentCluster } from "@/features/deployment/deployment-cluster";

export const metadata = { title: "Cluster" };

export default function KubernetesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Cluster"
        description="Replicas, resource usage and rollout status for a cluster-backed deployment"
      >
        <DeploymentCluster />
      </AppShell>
    </RequireAuth>
  );
}
