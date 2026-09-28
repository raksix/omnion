import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { CdnRulesView } from "@/features/cdn/cdn-rules-view";

export const metadata = { title: "CDN rules" };

export default function CdnRulesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Cache rules"
        description="Which public responses a visitor's browser and the edge may keep, in the order they are matched"
      >
        <CdnRulesView />
      </AppShell>
    </RequireAuth>
  );
}
