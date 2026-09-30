import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiApprovalPolicyScreen } from "@/features/ai/ai-approvals";

export const metadata = { title: "AI approval policy" };

/**
 * `/ai/approvals/policies` — the class policy on its own route (REQ-101).
 *
 * The inbox links here from its empty state, so this is the screen an operator reaches *before*
 * anything has ever been gated. It has to stand alone: the same six rows, the same guardrail,
 * without an inbox above it.
 */
export default function AiApprovalPoliciesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="AI approval policy"
        description="Which dangerous classes an agent must ask about before it acts — and what it costs to let one through"
      >
        <AiApprovalPolicyScreen />
      </AppShell>
    </RequireAuth>
  );
}