import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeveloperOAuthAppsView } from "@/features/developer/developer-oauth-apps-view";

export const metadata = { title: "OAuth applications" };

export default function DeveloperOAuthAppsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="OAuth applications"
        description="Third-party clients that sign a person into this platform instead of asking for their password. Each one has a client id you publish, a client secret you keep, and a redirect URI list you control."
      >
        <DeveloperOAuthAppsView />
      </AppShell>
    </RequireAuth>
  );
}