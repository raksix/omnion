import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { NotificationOutbox } from "@/features/notifications/notification-outbox";

export const metadata = { title: "Notification outbox" };

/**
 * `/notifications/outbox` — the organization's delivery log and the routing rules that fill it.
 *
 * Carries `notifications.admin`, which is why the page itself does not check a permission: the
 * route guard answers `403` and the panel renders its own "you do not have this" state. A
 * client-side permission check on top of that would be a second answer to the same question,
 * and the two would eventually disagree.
 */
export default function NotificationOutboxPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Notification outbox"
        description="Every delivery across the organization, and the rules that turn events into notifications"
      >
        <NotificationOutbox />
      </AppShell>
    </RequireAuth>
  );
}
