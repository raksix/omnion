import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SecurityEventsScreen } from "@/features/security/security-events";

export const metadata = { title: "Security events" };

export default function SecurityEventsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Security events"
        description="Sign-in attempts and privileged actions on one timeline"
      >
        <SecurityEventsScreen />
      </AppShell>
    </RequireAuth>
  );
}