import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiPermissionsView } from "@/features/ai/ai-permissions";

export const metadata = { title: "AI permissions" };

/**
 * `/ai/permissions` — the tool × (agents, identities) matrix (REQ-100).
 *
 * One screen that puts the registry, the agents' own allow-lists and the identities' grants
 * side by side, because the question an operator actually has — "who can deploy?" — spans all
 * three and answering it from three tabs is how the answer gets guessed.
 */
export default function AiPermissionsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="AI permissions"
        description="Which tools each agent and each AI identity may use, and which are explicitly denied"
      >
        <AiPermissionsView />
      </AppShell>
    </RequireAuth>
  );
}
