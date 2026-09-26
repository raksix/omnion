import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AnalyticsShell } from "@/features/analytics/analytics-shell";
import { AnalyticsRealtimeView } from "@/features/analytics/realtime-view";

export const metadata = { title: "Analytics · Realtime" };

/**
 * What is happening right now (docs/requests/REQ-007, slice 3): counters for the last five and
 * thirty minutes, the pages being read and a live event feed over the server-sent stream.
 */
export default function AnalyticsRealtimePage() {
  return (
    <RequireAuth>
      <AppShell title="Analytics" description="The last thirty minutes, kept fresh over a stream">
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the realtime view…</p>}>
          <AnalyticsShell active="/analytics/realtime" report="overview">
            <AnalyticsRealtimeView />
          </AnalyticsShell>
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
