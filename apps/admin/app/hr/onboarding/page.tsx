import { Suspense } from "react";

import { OnboardingView } from "@/features/hr/onboarding-view";

/**
 * `/hr/onboarding`
 *
 * The last screen slice 4 was missing: the API, the module, the migration and nine walks had all
 * existed for a tick with no route a person could open. The screen renders its own module shelf and
 * heading, so this route is only the Suspense boundary its `useSearchParams` needs.
 */
export default function Page() {
  return (
    <Suspense fallback={<p className="text-[13px] text-muted" aria-busy="true">Loading…</p>}>
      <OnboardingView />
    </Suspense>
  );
}
