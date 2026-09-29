import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { BackupsOverviewScreen } from "@/features/backups/backups-view";

export const metadata = { title: "Backups" };

export default function BackupsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Backups"
        description="What this platform can prove it wrote, and where it wrote it"
      >
        <BackupsOverviewScreen />
      </AppShell>
    </RequireAuth>
  );
}
