import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { RegionDetailView } from "@/features/regions/region-detail-view";

export const metadata = { title: "Region" };

/**
 * `/platform/regions/{code}` — one region (REQ-035, slice 1).
 *
 * The code is a path SEGMENT and not a query parameter, and the reason is the one this
 * repository keeps relearning: `searchParams` is the second argument of the page function and
 * is also passed to `generateMetadata`, so a `?site=` read through a header reaches neither —
 * the page renders the right thing under the wrong title. A segment cannot be read that way.
 *
 * No `Suspense` boundary is declared here even though the view is dynamic: the view does not
 * call `useSearchParams`, so opting into it would buy a build-time requirement for nothing.
 * The one that DOES need the boundary is documented at `/developer/sdks`, where a `?tab=`
 * deep link is read in a client component.
 */
export default async function RegionDetailPage({
  params,
}: {
  params: Promise<{ code: string }>;
}) {
  const { code } = await params;
  return (
    <RequireAuth>
      <AppShell
        title="Region"
        description="One region's services, endpoints, recent checks and its row of the latency matrix"
      >
        <RegionDetailView code={code} />
      </AppShell>
    </RequireAuth>
  );
}
