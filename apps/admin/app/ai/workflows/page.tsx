import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiWorkflowConsole } from "@/features/ai/workflow-console";

export const metadata = { title: "AI workflows" };

/**
 * `/ai/workflows` — the draft console (REQ-046 slice 3).
 *
 * The view reads its filters from the URL (`?status=&q=&by=&offset=`), so it sits inside a
 * Suspense boundary: `useSearchParams` is what the panel's other filtered screens do, and a
 * screen that reads the URL without the boundary fails its own build rather than at runtime.
 */
export default function AiWorkflowsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="AI workflows"
        description="Describe a rule in plain language and review what the model wrote before it runs"
      >
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the console…</p>}>
          <AiWorkflowConsole />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
