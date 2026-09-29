import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SecurityFindingsScreen } from "@/features/security/security-findings";

export const metadata = { title: "Security findings" };

export default function SecurityFindingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Security findings"
        description="What is wrong, where it came from, and what has been done about it"
      >
        <SecurityFindingsScreen />
      </AppShell>
    </RequireAuth>
  );
}
