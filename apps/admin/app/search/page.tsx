import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SearchView } from "@/features/search/search-view";

export const metadata = { title: "Search" };

/**
 * The results screen (REQ-002). The query lives in the URL, so the view reads it with
 * `useSearchParams` — which is why it sits inside a Suspense boundary.
 */
export default function SearchPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Search"
        description="Every hit the platform's index has for one query"
      >
        <Suspense
          fallback={<p className="text-[13px] text-muted">Loading the results screen…</p>}
        >
          <SearchView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
