import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { MigrationsView } from "@/features/deployment/migrations-view";

export const metadata = { title: "Migration ledger" };

/**
 * `/deployment/migrations` — the ledger, the pending set and the lock (REQ-129, slice 1).
 *
 * The screen exists for one question an operator asks mid-deploy: **what has this installation
 * applied, what would the next deploy do, and has anybody proved the rollback path.** So the
 * pending set sits above the ledger rather than mixed into it, drift is loud with both hashes,
 * and a reversal nobody has rehearsed says so instead of reading `reversible`.
 */
export default function DeploymentMigrationsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Migration ledger"
        description="Every schema change this installation applied, what the next deploy would run, and which reversals have actually been rehearsed"
      >
        <MigrationsView />
      </AppShell>
    </RequireAuth>
  );
}
