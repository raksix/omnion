import { HrModuleNav } from "@/features/hr/module-nav";
import { LeaveDetailView } from "@/features/hr/leave-detail-view";

/**
 * `/hr/leave/{id}` — the request with its balance, timeline and decision panel.
 *
 * The id arrives as a promise in Next's App Router, so the page is `async` and awaits it rather
 * than reaching for `useParams()`: a page that is a client component cannot do the first without
 * giving up the second.
 */
export default async function Page({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return (
    <div className="space-y-4">
      <HrModuleNav />
      <LeaveDetailView requestId={id} />
    </div>
  );
}
