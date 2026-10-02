import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AssignmentSettings } from "@/features/crm-intake/assignment-settings";

export const metadata = { title: "Assignment rules" };

export default function CrmAssignmentSettingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Assignment rules"
        description="Who answers a lead: ordered rules, and a simulator that shows which one wins"
      >
        <AssignmentSettings />
      </AppShell>
    </RequireAuth>
  );
}
