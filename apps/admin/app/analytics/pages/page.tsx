import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AnalyticsShell } from "@/features/analytics/analytics-shell";
import { AnalyticsPagesView } from "@/features/analytics/pages-view";

export const metadata = { title: "Analytics · Pages" };

/**
 * Every page the site served in the range, with its own series (docs/requests/REQ-007, slice 2). The toolbar's state lives in the URL, so the
 * screen is wrapped in a Suspense boundary like every other screen that reads search parameters.
 */
export default function AnalyticsPagesPage() {
  return (
    <RequireAuth>
      <AppShell title="Analytics" description="Every page the site served in the range, with its own series">
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the report…</p>}>
          <AnalyticsShell active="/analytics/pages" report="pages">
            <AnalyticsPagesView />
          </AnalyticsShell>
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
