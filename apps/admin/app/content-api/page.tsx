import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ContentApiShell } from "@/features/content-api/content-api-shell";
import { ContentApiTokensView } from "@/features/content-api/tokens-view";

export const metadata = { title: "Content API" };

/**
 * `/content-api` — the Tokens tab (REQ-019, slice 1).
 *
 * Carries `content.api.read` for the list and `content.api.manage` for every write, so the page
 * does not check a permission itself: the route guard answers `403` and the panel renders its own
 * state. A second, client-side permission check would be a second answer to the same question, and
 * the two would eventually disagree — which is how a tab ends up visible to somebody who cannot
 * use it.
 *
 * The Explorer, Docs and Usage tabs (REQ-019 slices 2 and 3) are separate routes under the same
 * path prefix, because they answer different questions and will need different permissions: a
 * reader who may not mint a token can still send an Explorer request with one somebody else made.
 */
export default function ContentApiPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Content API"
        description="Read-only tokens for headless frontends"
      >
        <ContentApiShell>
          <ContentApiTokensView />
        </ContentApiShell>
      </AppShell>
    </RequireAuth>
  );
}
