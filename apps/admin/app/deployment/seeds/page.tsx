import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SeedsView } from "@/features/deployment/seeds-view";

export const metadata = { title: "Seed datasets" };

/**
 * `/deployment/seeds` — the datasets an operator can load (REQ-129, slice 3).
 *
 * A fresh installation has no content, and a demo has to have some. The three datasets the platform
 * ships (`minimal`, `demo`, `fixture`) are declared by migration 0216 and loaded here behind a typed
 * confirmation, because every one of them writes into tables that already hold data.
 */
export default function DeploymentSeedsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Seed datasets"
        description="Load a starting dataset into this installation, behind a typed confirmation"
      >
        <SeedsView />
      </AppShell>
    </RequireAuth>
  );
}
