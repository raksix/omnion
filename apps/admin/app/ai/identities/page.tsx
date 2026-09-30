import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiIdentitiesView } from "@/features/ai/ai-identities";

export const metadata = { title: "AI identities" };

/**
 * `/ai/identities` — the named grant sets a run borrows (REQ-100, slice 2).
 *
 * The registry screen says what the platform's AI *may* do; this one says what a particular
 * organization has *decided* about it. An agent's own tool list is whoever built it, so an
 * identity is the row an organization can name, review and change without touching an agent —
 * and the row the answer to "if this agent is compromised, what can it reach" is read from.
 */
export default function AiIdentitiesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="AI identities"
        description="Named grant sets a run borrows. An explicit deny beats every allow, and inherit writes no decision"
      >
        <AiIdentitiesView />
      </AppShell>
    </RequireAuth>
  );
}
