import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { LeadDuplicates } from "@/features/crm-intake/lead-duplicates";

export const metadata = { title: "Duplicate leads" };

export default function CrmLeadDuplicatesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Duplicate leads"
        description="Submissions your dedupe rules filed against an existing contact — each one reversible, each one with the key that matched"
      >
        <LeadDuplicates />
      </AppShell>
    </RequireAuth>
  );
}
