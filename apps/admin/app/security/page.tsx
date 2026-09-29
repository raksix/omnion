import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SecurityOverviewScreen } from "@/features/security/security-overview";

export const metadata = { title: "Security" };

export default function SecurityPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Security"
        description="What this deployment can actually prove about itself — and what it cannot"
      >
        <SecurityOverviewScreen />
      </AppShell>
    </RequireAuth>
  );
}
