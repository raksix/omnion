import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ExportsView } from "@/features/deployment/exports-view";

export const metadata = { title: "Anonymised exports" };

/**
 * `/deployment/exports` — anonymised support exports (REQ-129, slice 4).
 *
 * The screen an operator opens when support asks for a copy of the data. Everything here is a
 * decision with a consequence outside the platform: what leaves, once, before it expires. So the
 * classification map sits above the exports themselves — the unclassified count decides whether
 * this feature works at all — and each export names which of the four reasons a download is
 * refused before anyone presses.
 */
export default function DeploymentExportsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Anonymised exports"
        description="Classify the columns, then produce a single-use support dump that expires on its own"
      >
        <ExportsView />
      </AppShell>
    </RequireAuth>
  );
}