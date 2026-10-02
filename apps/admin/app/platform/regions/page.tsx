import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { RegionsView } from "@/features/regions/regions-view";

export const metadata = { title: "Regions" };

/**
 * `/platform/regions` — the edge region registry (REQ-035, slice 1).
 *
 * The registry, the per-service health matrix and the region-to-region latency matrix, in one
 * screen and one read. The screen's whole design follows the REQ's own rule that a region with
 * no recent checks shows `Unknown` rather than green, so the doc that explains the screen
 * lives in the view where the decisions are.
 */
export default function RegionsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Regions"
        description="Where this deployment runs, how each region's seven services are answering, and how far apart they are"
      >
        <RegionsView />
      </AppShell>
    </RequireAuth>
  );
}
