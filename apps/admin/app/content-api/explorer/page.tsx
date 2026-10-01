import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ContentApiShell } from "@/features/content-api/content-api-shell";
import { ContentApiExplorerView } from "@/features/content-api/explorer-view";

export const metadata = { title: "Content API · Explorer" };

/**
 * `/content-api/explorer` — the Explorer tab (REQ-019, slice 3).
 *
 * `RequireAuth` carries `content.api.read`, the same power the token list and the usage tab take.
 * There is deliberately **no** `content.api.manage` requirement on the dispatch itself: making a
 * call spends the chosen token's own budget and reads what that token may read, and a reader who
 * may already mint tokens can legitimately try one out. What stops a call from doing damage is
 * not a permission — it is the scope list on the token and the limiter on its tier, both of which
 * the server enforces on the real request.
 *
 * A route rather than a tab state of `/content-api`, for the same reason the other two are: this
 * screen is linkable, and its state (`?endpoint=…&token=…&limit=5`) is the thing somebody pastes
 * into a bug report. A link that only exists as a state of another page cannot be put in a runbook.
 */
export default function ContentApiExplorerPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Content API"
        description="Make a real call against the headless surface"
      >
        <ContentApiShell>
          <ContentApiExplorerView />
        </ContentApiShell>
      </AppShell>
    </RequireAuth>
  );
}