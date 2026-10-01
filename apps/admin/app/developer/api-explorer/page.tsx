import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeveloperApiExplorerView } from "@/features/developer/developer-api-explorer-view";

export const metadata = { title: "API Explorer" };

export default function DeveloperApiExplorerPage() {
  return (
    <RequireAuth>
      <AppShell
        title="API Explorer"
        description="Every operation this platform serves, with a form generated from its own schema. A call is sent as you, with your permissions — never as a service credential."
      >
        <DeveloperApiExplorerView />
      </AppShell>
    </RequireAuth>
  );
}
