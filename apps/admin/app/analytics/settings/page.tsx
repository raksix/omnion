import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AnalyticsShell } from "@/features/analytics/analytics-shell";
import { AnalyticsSettingsView } from "@/features/analytics/settings-view";

export const metadata = { title: "Analytics · Settings" };

/**
 * Tracking, privacy and the data operations (docs/requests/REQ-007, slices 1 and 4): how a site
 * is counted, what is stored about a visitor, where the snippet goes, and the two irreversible
 * operations — the retention purge and erasing one visitor — with the audit trail they leave.
 */
export default function AnalyticsSettingsPage() {
  return (
    <RequireAuth>
      <AppShell title="Analytics" description="Tracking, privacy and the data operations">
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the settings…</p>}>
          <AnalyticsShell active="/analytics/settings" report="overview" toolbar={false}>
            <AnalyticsSettingsView />
          </AnalyticsShell>
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
