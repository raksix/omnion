import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AnalyticsShell } from "@/features/analytics/analytics-shell";
import { AnalyticsSourcesView } from "@/features/analytics/sources-view";

export const metadata = { title: "Analytics · Sources" };

/**
 * Where the visits came from: referrers, campaigns and the UTM five (docs/requests/REQ-007, slice 2). The toolbar's state lives in the URL, so the
 * screen is wrapped in a Suspense boundary like every other screen that reads search parameters.
 */
export default function AnalyticsSourcesPage() {
  return (
    <RequireAuth>
      <AppShell title="Analytics" description="Where the visits came from: referrers, campaigns and the UTM five">
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the report…</p>}>
          <AnalyticsShell active="/analytics/sources" report="sources">
            <AnalyticsSourcesView />
          </AnalyticsShell>
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
