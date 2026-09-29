import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { EventConsole } from "@/features/events/event-console";

export const metadata = { title: "Events" };

export default function EventsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Events"
        description="What the platform recorded, and every event name it is able to record"
      >
        {/* The tab, the window and the name filter all live in the query string, so the screen
            is read on the client and needs a boundary. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the event feed…</p>}>
          <EventConsole />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
