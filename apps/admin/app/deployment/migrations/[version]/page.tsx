import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { MigrationDetailView } from "@/features/deployment/migration-detail-view";

export const metadata = { title: "Migration" };

/**
 * `/deployment/migrations/{version}` — one migration's SQL, its reversal and the rehearsal
 * (REQ-129, slice 1).
 *
 * The `Rehearse` action asks for a scratch database **name** rather than offering a one-click
 * verify, because the request's rule is absolute: the panel never offers the reversal on a
 * production-marked environment, and the route refuses a name that is the live database.
 */
export default function DeploymentMigrationDetailPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Migration"
        description="The statements, the reversal, and whether anyone has run the reversal against a real schema"
      >
        <MigrationDetailView />
      </AppShell>
    </RequireAuth>
  );
}
