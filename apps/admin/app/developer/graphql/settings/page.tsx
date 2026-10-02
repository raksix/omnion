import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SettingsView } from "@/features/developer/settings-view";

export const metadata = { title: "GraphQL settings" };

/**
 * `/developer/graphql/settings` — the endpoint's limits (REQ-130, slice 2).
 *
 * The screen exists because the endpoint once built its limits from `Settings::default()` and this
 * page reported the change as saved anyway. Every number here is read back by the next request, and
 * the save is refused locally with the endpoint's own field-level message rather than a generic one.
 */
export default function GraphqlSettingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="GraphQL settings"
        description="The depth, cost, alias, fragment, page-size and timeout limits the endpoint enforces on every request"
      >
        <SettingsView />
      </AppShell>
    </RequireAuth>
  );
}