import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { NotificationList } from "@/features/notifications/notification-list";

export const metadata = { title: "Notifications" };

export default function NotificationsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Notifications"
        description="What the platform needs to tell you, and what you have already seen"
      >
        {/* The filters live in the query string so a filtered view is shareable, which means
            they are read on the client and need a boundary. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading your notifications…</p>}>
          <NotificationList />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
