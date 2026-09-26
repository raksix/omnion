import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AnalyticsShell } from "@/features/analytics/analytics-shell";
import { AnalyticsAudienceView } from "@/features/analytics/audience-view";

export const metadata = { title: "Analytics · Audience" };

/**
 * Devices, browsers, systems, screens, languages and countries (docs/requests/REQ-007, slice 2). The toolbar's state lives in the URL, so the
 * screen is wrapped in a Suspense boundary like every other screen that reads search parameters.
 */
export default function AnalyticsAudiencePage() {
  return (
    <RequireAuth>
      <AppShell title="Analytics" description="Devices, browsers, systems, screens, languages and countries">
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the report…</p>}>
          <AnalyticsShell active="/analytics/audience" report="audience">
            <AnalyticsAudienceView />
          </AnalyticsShell>
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
