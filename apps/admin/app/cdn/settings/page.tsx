import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { CdnSettingsView } from "@/features/cdn/cdn-settings-view";

export const metadata = { title: "CDN settings" };

export default function CdnSettingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="CDN settings"
        description="Which provider invalidates this site's cache, and which events queue a purge on their own"
      >
        <CdnSettingsView />
      </AppShell>
    </RequireAuth>
  );
}
