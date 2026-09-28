import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { MediaSettingsView } from "@/features/media/presets-view";

export const metadata = { title: "Media settings" };

export default function MediaSettingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Media settings"
        description="The named image sizes this site can serve, and the URL each one is asked for by"
      >
        <MediaSettingsView />
      </AppShell>
    </RequireAuth>
  );
}
