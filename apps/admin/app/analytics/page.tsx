import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AnalyticsShell } from "@/features/analytics/analytics-shell";
import { AnalyticsOverviewView } from "@/features/analytics/overview-view";

export const metadata = { title: "Analytics · Overview" };

/**
 * Traffic, conversions and the panels behind them for the selected site (docs/requests/REQ-007, slice 2). The toolbar's state lives in the URL, so the
 * screen is wrapped in a Suspense boundary like every other screen that reads search parameters.
 */
export default function AnalyticsOverviewPage() {
  return (
    <RequireAuth>
      <AppShell title="Analytics" description="Traffic, conversions and the panels behind them for the selected site">
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the report…</p>}>
          <AnalyticsShell active="/analytics" report="overview">
            <AnalyticsOverviewView />
          </AnalyticsShell>
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
