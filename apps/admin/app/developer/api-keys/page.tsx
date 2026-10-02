import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeveloperKeysScreen } from "@/features/developer/developer-keys";

export const metadata = { title: "API keys" };

export default function DeveloperApiKeysPage() {
  return (
    <RequireAuth>
      <AppShell
        title="API keys"
        description="Each key is a delegation with its own scopes — it may only narrow what its issuer already holds"
      >
        {/* No `Suspense` here, unlike the filtered log screen: this one reads its filters from
            component state, not the query string, so it has no reason to be a client boundary. */}
        <DeveloperKeysScreen />
      </AppShell>
    </RequireAuth>
  );
}
