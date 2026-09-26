import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AnalyticsShell } from "@/features/analytics/analytics-shell";
import { AnalyticsDownloadsView } from "@/features/analytics/downloads-view";

export const metadata = { title: "Analytics · Downloads" };

/**
 * Files people take, and the pages they take them from (docs/requests/REQ-007, slice 2). The toolbar's state lives in the URL, so the
 * screen is wrapped in a Suspense boundary like every other screen that reads search parameters.
 */
export default function AnalyticsDownloadsPage() {
  return (
    <RequireAuth>
      <AppShell title="Analytics" description="Files people take, and the pages they take them from">
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the report…</p>}>
          <AnalyticsShell active="/analytics/downloads" report="downloads">
            <AnalyticsDownloadsView />
          </AnalyticsShell>
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
