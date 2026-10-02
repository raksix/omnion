import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { PlaygroundView } from "@/features/developer/playground-view";
import { readSessionUser } from "@/lib/session-server";

export const metadata = { title: "GraphQL playground" };

/**
 * `/developer/graphql` — the playground (REQ-130, slice 2).
 *
 * The playground keeps unsent text **per user** locally, so the page reads the session's user id on
 * the server and passes it down. Reading it in the browser instead would make every operator on a
 * shared machine share one draft — which is the opposite of what the request asks for, and is
 * invisible to a single-account walk.
 *
 * The fallback is `"anonymous"` rather than a fresh uuid: a signed-out visitor reaching this page
 * must still get a stable key or every render would invent a new draft namespace and the stored text
 * would never come back.
 */
export default async function GraphqlPlaygroundPage() {
  const user = await readSessionUser();
  return (
    <RequireAuth>
      <AppShell
        title="GraphQL playground"
        description="Run a document, read the depth and cost it was charged, and see the refusal when it is over budget"
      >
        <PlaygroundView userId={user?.id ?? "anonymous"} />
      </AppShell>
    </RequireAuth>
  );
}