import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { MediaSettingsTabs } from "@/features/media/presets-view";

export const metadata = { title: "Media settings" };

export default function MediaSettingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Media settings"
        description="Where this site's files are stored, and the named image sizes it can serve"
      >
        <MediaSettingsTabs />
      </AppShell>
    </RequireAuth>
  );
}
