import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { LeadInbox } from "@/features/crm-intake/lead-inbox";

export const metadata = { title: "Lead inbox" };

export default function CrmLeadsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Lead inbox"
        description="Quote requests and form submissions from your sites, with the deduplication verdicts and the first-response clock"
      >
        <LeadInbox />
      </AppShell>
    </RequireAuth>
  );
}
