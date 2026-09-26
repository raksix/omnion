import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AnalyticsShell } from "@/features/analytics/analytics-shell";
import { AnalyticsFormsView } from "@/features/analytics/forms-view";

export const metadata = { title: "Analytics · Forms" };

/**
 * Form submissions, completion and abandonment per form (docs/requests/REQ-007, slice 2). The toolbar's state lives in the URL, so the
 * screen is wrapped in a Suspense boundary like every other screen that reads search parameters.
 */
export default function AnalyticsFormsPage() {
  return (
    <RequireAuth>
      <AppShell title="Analytics" description="Form submissions, completion and abandonment per form">
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the report…</p>}>
          <AnalyticsShell active="/analytics/forms" report="forms">
            <AnalyticsFormsView />
          </AnalyticsShell>
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
