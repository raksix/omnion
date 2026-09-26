import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AnalyticsShell } from "@/features/analytics/analytics-shell";
import { AnalyticsEventsView } from "@/features/analytics/events-view";

export const metadata = { title: "Analytics · Events" };

/**
 * Custom events with their counts, values and property breakdown (docs/requests/REQ-007, slice 2). The toolbar's state lives in the URL, so the
 * screen is wrapped in a Suspense boundary like every other screen that reads search parameters.
 */
export default function AnalyticsEventsPage() {
  return (
    <RequireAuth>
      <AppShell title="Analytics" description="Custom events with their counts, values and property breakdown">
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the report…</p>}>
          <AnalyticsShell active="/analytics/events" report="events">
            <AnalyticsEventsView />
          </AnalyticsShell>
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
