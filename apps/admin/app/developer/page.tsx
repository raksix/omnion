import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeveloperOverviewScreen } from "@/features/developer/developer-overview";

export const metadata = { title: "Developer" };

export default function DeveloperPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Developer"
        description="The credentials your integrations use, and the traffic they are making"
      >
        <DeveloperOverviewScreen />
      </AppShell>
    </RequireAuth>
  );
}
