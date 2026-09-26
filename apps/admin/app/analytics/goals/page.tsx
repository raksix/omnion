import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AnalyticsShell } from "@/features/analytics/analytics-shell";
import { AnalyticsGoalsView } from "@/features/analytics/goals-view";

export const metadata = { title: "Analytics · Goals" };

/**
 * Conversions and funnels (docs/requests/REQ-007, slice 3). The toolbar's state lives in the URL,
 * so the screen is wrapped in a Suspense boundary like every other screen that reads search
 * parameters.
 */
export default function AnalyticsGoalsPage() {
  return (
    <RequireAuth>
      <AppShell title="Analytics" description="Goals, ordered funnels and their conversion rates">
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the goals…</p>}>
          <AnalyticsShell active="/analytics/goals" report="overview">
            <AnalyticsGoalsView />
          </AnalyticsShell>
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
