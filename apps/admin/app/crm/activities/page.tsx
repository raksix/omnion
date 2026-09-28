import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ActivitiesView } from "@/features/crm/activities-view";

export const metadata = { title: "CRM · Activities" };

export default function CrmActivitiesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="CRM"
        description="The relationship layer: the people, the companies they work for, and the deals in between"
      >
        <ActivitiesView />
      </AppShell>
    </RequireAuth>
  );
}
