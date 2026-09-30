import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiApprovalsView } from "@/features/ai/ai-approvals";

export const metadata = { title: "AI approvals" };

/**
 * `/ai/approvals` — the review inbox (REQ-101).
 *
 * Nothing on this screen has happened yet: every row is an action an agent asked for and a person
 * has not yet allowed. The class policy sits below the inbox rather than on its own page because
 * the two are read together — "nothing is waiting" is only reassuring next to "and here is what
 * is waiting, if you ever un-gate a class".
 */
export default function AiApprovalsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="AI approvals"
        description="What an agent wants to do, held until a person decides — plus the policy that decides what gets held"
      >
        <AiApprovalsView />
      </AppShell>
    </RequireAuth>
  );
}