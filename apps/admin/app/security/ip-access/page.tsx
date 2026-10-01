import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { IpAccessScreen } from "@/features/security/ip-access";

export const metadata = { title: "IP access" };

export default function SecurityIpAccessPage() {
  return (
    <RequireAuth>
      <AppShell
        title="IP access"
        description="Which networks may reach this API, which may not, and what any address would do"
      >
        <IpAccessScreen />
      </AppShell>
    </RequireAuth>
  );
}