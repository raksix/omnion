import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SearchSettingsView } from "@/features/search/search-settings-view";

export const metadata = { title: "Search settings" };

/**
 * The index's own screen (REQ-002, slice 3): what every provider holds, when it was last
 * written, whether a pass is running or failed, and the ranking weights search orders with.
 */
export default function SearchSettingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Search"
        description="The index behind the platform's one search box: providers, passes and ranking"
      >
        <SearchSettingsView />
      </AppShell>
    </RequireAuth>
  );
}
