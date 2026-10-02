import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiWorkflowDraftReview } from "@/features/ai/workflow-draft-review";

export const metadata = { title: "Workflow draft" };

/**
 * `/ai/workflows/[id]` — the draft review screen (REQ-046 slice 3).
 *
 * A server component: the id comes from the route and the screen fetches the draft itself, so
 * a direct link works without a query string. The review screen reads no filters from the URL,
 * which is why it does not need a Suspense boundary — unlike the console beside it.
 */
export default async function AiWorkflowDraftPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell
        title="Workflow draft"
        description="The definition the model wrote, and what it will do when you approve it"
      >
        <AiWorkflowDraftReview draftId={id} />
      </AppShell>
    </RequireAuth>
  );
}
