import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { GettingStartedCard } from "@/features/overview/getting-started";
import { OverviewView } from "@/features/overview/overview-view";

export const metadata = { title: "Overview" };

export default function OverviewPage() {
  return (
    <RequireAuth>
      <AppShell title="Overview" description="Your Omnion workspace at a glance">
        <div className="flex flex-col gap-6">
          <GettingStartedCard />
          <OverviewView />
        </div>
      </AppShell>
    </RequireAuth>
  );
}
