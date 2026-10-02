import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiRunDetailView } from "@/features/ai/ai-run-detail";

export const metadata = { title: "Run" };

/**
 * One run in full (REQ-099, slice 1): the step trace as an accordion, a live tail while the run
 * is going, and Cancel / Resume / Copy transcript.
 *
 * Not in the walkthrough's plain route list for the same reason the media file detail is not:
 * its path carries a run id, and a route walked with a placeholder id only proves the error
 * state renders. The depth pass opens a *real* run's screen instead.
 */
export default async function AiRunPage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="Run" description="Every step, what it cost and why it stopped">
        <AiRunDetailView runId={id} />
      </AppShell>
    </RequireAuth>
  );
}
