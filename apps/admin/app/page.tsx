import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { OverviewView } from "@/features/overview/overview-view";

export const metadata = { title: "Overview" };

export default function OverviewPage() {
  return (
    <RequireAuth>
      <AppShell title="Overview" description="Your Omnion workspace at a glance">
        <OverviewView />
      </AppShell>
    </RequireAuth>
  );
}
