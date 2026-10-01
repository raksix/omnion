import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeveloperEventsView } from "@/features/developer/developer-events-view";

export const metadata = { title: "Event catalogue · Developer" };

/**
 * The developer framing of the event registry (REQ-033, slice 3).
 *
 * `/events` (REQ-016) already shows this registry, and this is deliberately not a second copy of
 * it. That page answers "what happened?" for an operator; this one answers "what CAN happen, and
 * what will I receive when it does?" for someone writing a subscriber — so the window, the cursor
 * and the retention panel are all absent, and the webhook deep link and the copyable schema are
 * present instead. `DeveloperEventsView` carries the argument in full.
 *
 * No `Suspense` boundary: unlike the event feed this screen holds no time window, so there is
 * nothing in the query string to read on the client and nothing to suspend on. It is a single
 * registry read that renders on the server and hydrates.
 */
export default function DeveloperEventsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Event catalogue"
        description="Every event name the platform can emit, what it carries, and how to subscribe"
      >
        <DeveloperEventsView />
      </AppShell>
    </RequireAuth>
  );
}
