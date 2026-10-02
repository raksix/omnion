import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeveloperKeysView } from "@/features/developer/developer-keys-view";

export const metadata = { title: "API keys" };

export default function DeveloperKeysPage() {
  return (
    <RequireAuth>
      <AppShell
        title="API keys"
        description="Credentials your own integrations present to this platform. Each is created once, shown once, and revocable at any time."
      >
        <DeveloperKeysView />
      </AppShell>
    </RequireAuth>
  );
}
