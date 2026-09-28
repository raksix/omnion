import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { CdnOverviewView } from "@/features/cdn/cdn-overview-view";

export const metadata = { title: "CDN" };

export default function CdnPage() {
  return (
    <RequireAuth>
      <AppShell
        title="CDN"
        description="What this site's cache is doing, and where to change it"
      >
        <CdnOverviewView />
      </AppShell>
    </RequireAuth>
  );
}
