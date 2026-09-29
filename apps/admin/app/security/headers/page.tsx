import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { HeaderPolicyScreen } from "@/features/security/header-policy";

export const metadata = { title: "Security headers" };

export default function SecurityHeadersPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Response headers"
        description="What this deployment sends in every response, and what it would send if you saved the draft below"
      >
        <HeaderPolicyScreen />
      </AppShell>
    </RequireAuth>
  );
}
