import { Suspense } from "react";

import { ReportsView } from "@/features/hr/reports-view";

/**
 * `/hr/reports`
 *
 * The screen renders its own module shelf and heading, so this route is only the Suspense
 * boundary the client's `useSearchParams` needs — the two cannot be split without the shelf
 * rendering twice.
 */
export default function Page() {
  return (
    <Suspense fallback={<p className="text-[13px] text-muted" aria-busy="true">Loading…</p>}>
      <ReportsView />
    </Suspense>
  );
}
