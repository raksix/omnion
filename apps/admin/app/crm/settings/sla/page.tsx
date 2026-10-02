import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SlaSettings } from "@/features/crm-intake/sla-settings";

export const metadata = { title: "First-response targets" };

export default function CrmSlaSettingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="First-response targets"
        description="How long a lead may wait before somebody answers it, and who hears when it does"
      >
        <SlaSettings />
      </AppShell>
    </RequireAuth>
  );
}
