import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiApprovalReviewScreen } from "@/features/ai/ai-approval-review";

export const metadata = { title: "Review AI request" };

/**
 * `/ai/approvals/[id]` — the review screen (REQ-101), the heart of the request.
 *
 * One screen, one decision: the header says who asked and what for, the middle is the exact
 * frozen diff the agent would apply, and the bar at the bottom is the decision itself.
 */
export default function AiApprovalReviewPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Review AI request"
        description="The exact change this agent would make, and what deciding it does"
      >
        <AiApprovalReviewScreen />
      </AppShell>
    </RequireAuth>
  );
}