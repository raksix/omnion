import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { NotificationSettings } from "@/features/notifications/notification-settings";

export const metadata = { title: "Notification settings" };

export default function NotificationSettingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Notification settings"
        description="Which kinds of notification reach you, and how"
      >
        <NotificationSettings />
      </AppShell>
    </RequireAuth>
  );
}
