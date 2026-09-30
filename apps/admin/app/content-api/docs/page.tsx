import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ContentApiShell } from "@/features/content-api/content-api-shell";
import { ContentApiDocsView } from "@/features/content-api/docs-view";

export const metadata = { title: "Content API · Docs" };

/**
 * `/content-api/docs` — the Docs tab (REQ-019, slice 2).
 *
 * `RequireAuth` carries `content.api.read` for the session, and the route guard answers `403` for
 * anybody without it — the same power the token list needs, because a reader who may see the
 * tokens may see the contract they are for.
 *
 * Separate from `/content-api` rather than a tab inside it: the Tokens tab is a working screen
 * (mint, rotate, revoke) and this is a document. They also have different jobs in the walkthrough,
 * and a screen that only exists as a state of another screen cannot be linked to directly.
 */
export default function ContentApiDocsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Content API"
        description="The contract a headless frontend integrates against"
      >
        <ContentApiShell>
          <ContentApiDocsView />
        </ContentApiShell>
      </AppShell>
    </RequireAuth>
  );
}