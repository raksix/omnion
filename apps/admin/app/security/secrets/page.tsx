import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SecuritySecretsScreen } from "@/features/security/security-secrets";

export const metadata = { title: "Secret inventory" };

export default function SecuritySecretsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Secret inventory"
        description="What this platform holds, by reference — names and counts, never a value"
      >
        <SecuritySecretsScreen />
      </AppShell>
    </RequireAuth>
  );
}
