import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { MetricsView } from "@/features/observability/metrics-view";

export const metadata = { title: "Metric catalogue" };

/**
 * `/observability/metrics` — the documented metric families and the chart behind one selector
 * (REQ-126, slice 2).
 *
 * The screen is fed from the registry rather than from a list written here: a family that exists in
 * a call site but not in the catalogue is impossible, and a family with no samples says so instead
 * of drawing an empty graph. The per-family series cap is shown against the live series count,
 * because a family approaching its cap is a resolution problem an operator should see before the
 * folding starts rather than after.
 */
export default function ObservabilityMetricsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Metric catalogue"
        description="Every family this instance can record, its labels, its cardinality against its cap, and a chart for one selection"
      >
        <MetricsView />
      </AppShell>
    </RequireAuth>
  );
}
