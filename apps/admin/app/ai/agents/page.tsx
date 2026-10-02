import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiAgentsList } from "@/features/ai/ai-agents-list";

export const metadata = { title: "Agents" };

/**
 * The agents table (REQ-099, slice 1). The search and the filters live in the query string, so
 * the screen is read on the client.
 */
export default function AiAgentsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Agents"
        description="A model, a goal and the tools it may use — and the runs it has produced"
      >
        <AiAgentsList />
      </AppShell>
    </RequireAuth>
  );
}
