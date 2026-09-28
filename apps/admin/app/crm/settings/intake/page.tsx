import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { IntakeSources } from "@/features/crm-intake/intake-sources";

export const metadata = { title: "Intake sources" };

export default function CrmIntakeSettingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Intake sources"
        description="Where a lead comes from: a bound form, or a keyed endpoint your own pages post to"
      >
        <IntakeSources />
      </AppShell>
    </RequireAuth>
  );
}
