import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { EvalSuiteDetailView } from "@/features/ai/eval-suite-detail";

export const metadata = { title: "Eval suite" };

/**
 * `/ai/evals/[key]` — one suite (REQ-107, slice 1).
 *
 * The key is read from the route and handed to the view as a prop rather than through
 * `useParams`, for the reason the tool detail page gives: a client component that read the param
 * itself would have no key on the first render, which is exactly when the skeleton has to show.
 */
export default async function EvalSuitePage({ params }: { params: Promise<{ key: string }> }) {
  const { key } = await params;
  return (
    <RequireAuth>
      <AppShell
        title="Eval suite"
        description="The cases this suite measures and the configuration it measures them with — a case that asserts nothing is the one thing it will not let you save"
      >
        <EvalSuiteDetailView suiteKey={decodeURIComponent(key)} />
      </AppShell>
    </RequireAuth>
  );
}
