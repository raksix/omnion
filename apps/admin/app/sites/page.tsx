import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SitesView } from "@/features/sites/sites-view";

export const metadata = { title: "Sites" };

export default function SitesPage() {
  return (
    <RequireAuth>
      <AppShell title="Sites" description="Sites in this installation">
        <SitesView />
      </AppShell>
    </RequireAuth>
  );
}
