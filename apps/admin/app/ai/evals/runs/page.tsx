import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { EvalRunsScreen } from "@/features/ai/eval-runs-screen";

export const metadata = { title: "Eval runs" };

/**
 * `/ai/evals/runs` — the run history (REQ-107, slice 2).
 *
 * A separate screen from the suites, not a tab inside them, because a run is read across suites:
 * "did anything regress today" is not a per-suite question, and a tab would make the operator
 * pick a suite before they know which one regressed.
 */
export default function EvalRunsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Eval runs"
        description="Every run your suites have produced, with what it measured and whether it held its gate. A queued run is waiting for a runner — that is the state worth looking at"
      >
        <EvalRunsScreen />
      </AppShell>
    </RequireAuth>
  );
}
