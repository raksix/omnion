import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { PlanReview } from "@/features/app-builder/plan-review";

export const metadata = { title: "Review plan" };

/**
 * `/app-builder/plans/[id]` — the review workspace (docs/requests/REQ-045, slice 2).
 *
 * A server component that awaits its params (Next hands them over as a Promise) and hands the
 * id to a client screen, which fetches the plan itself — so a direct link, a reload and the QA
 * pass all land on the same view without a query string or a Suspense boundary.
 */
export default async function AppBuilderPlanPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell
        title="Review plan"
        description="Every artifact a plan proposes, and what stands between it and apply"
      >
        <PlanReview planId={id} />
      </AppShell>
    </RequireAuth>
  );
}