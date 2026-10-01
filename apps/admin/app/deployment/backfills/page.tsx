import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { BackfillsView } from "@/features/deployment/backfills-view";

export const metadata = { title: "Data backfills" };

/**
 * `/deployment/backfills` — the data migrations an operator runs by hand (REQ-129, slice 3).
 *
 * A schema migration adds a column; a BACKFILL fills it for the rows that already exist, and that
 * write cannot happen inside the deploy transaction — a million rows is not a migration. So it
 * becomes a job an operator starts, watches and stops, and this screen is the whole of that surface:
 * the cursor it will resume from, the rows it has written, and the database's own words when its
 * statement is wrong.
 */
export default function DeploymentBackfillsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Data backfills"
        description="Fill a newly added column for the rows that already exist, in batches you can stop between"
      >
        <BackfillsView />
      </AppShell>
    </RequireAuth>
  );
}
