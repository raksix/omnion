import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { LeadDetail } from "@/features/crm-intake/lead-detail";

export const metadata = { title: "Lead" };

export default function CrmLeadPage() {
  // The route carries an id, so `params` is read inside the client detail view: a server page
  // that resolved the id itself would have to read the lead twice, and the client already has
  // the answer the screen needs (including a 404 that renders its own state).
  return (
    <RequireAuth>
      <AppShell
        title="Lead"
        description="The submission as it arrived, the verdicts it earned and everything that happened to it since"
      >
        <LeadDetail />
      </AppShell>
    </RequireAuth>
  );
}
