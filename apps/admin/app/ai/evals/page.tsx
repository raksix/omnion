import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiEvalsView } from "@/features/ai/ai-evals";

export const metadata = { title: "Agent evals" };

/**
 * `/ai/evals` — the eval suites (REQ-107, slice 1).
 *
 * The screen is about measurement, not about names: every row carries what the suite is
 * currently able to prove, because a suite of easy cases that passes everything is the failure
 * mode this request exists to catch.
 */
export default function AiEvalsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Agent evals"
        description="Suites of cases and the properties their output must satisfy. A suite with no cases measures nothing, and the panel says so on the row"
      >
        <AiEvalsView />
      </AppShell>
    </RequireAuth>
  );
}
