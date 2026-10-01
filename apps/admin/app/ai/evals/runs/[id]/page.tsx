import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { EvalRunDetailView } from "@/features/ai/eval-run-detail";

export const metadata = { title: "Eval run" };

/**
 * `/ai/evals/runs/[id]` — one run (REQ-107, slice 2).
 *
 * The id is read from the route and handed to the view as a prop rather than through
 * `useParams`, for the reason the suite detail page gives: a client component that read the param
 * itself would have no id on the first render, which is exactly when the skeleton has to show.
 */
export default async function EvalRunPage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell
        title="Eval run"
        description="What this run measured, which cases it failed and why, and how it compares with a baseline you choose"
      >
        <EvalRunDetailView runId={decodeURIComponent(id)} />
      </AppShell>
    </RequireAuth>
  );
}
