import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ContentApiShell } from "@/features/content-api/content-api-shell";
import { ContentApiUsageView } from "@/features/content-api/usage-view";

export const metadata = { title: "Content API · Usage" };

/**
 * `/content-api/usage` — the Usage tab (REQ-019, slice 3).
 *
 * `RequireAuth` carries `content.api.read`, the same power the token list and the docs take.
 * There is deliberately no separate `content.api.usage` permission: a viewer of this section can
 * already see a token's name, prefix and rate tier, so a permission they need in order to *act* on
 * nothing is a permission that only ever shows up as absent from somebody's role.
 *
 * A route rather than a tab state of `/content-api`, for the same reason the Docs tab is one: this
 * screen is linkable, and a link that only exists as a state of another page is a link you cannot
 * put in a runbook.
 */
export default function ContentApiUsagePage() {
  return (
    <RequireAuth>
      <AppShell
        title="Content API"
        description="What the headless surface has been asked for"
      >
        <ContentApiShell>
          <ContentApiUsageView />
        </ContentApiShell>
      </AppShell>
    </RequireAuth>
  );
}
