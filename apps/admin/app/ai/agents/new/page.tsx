import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AgentForm } from "@/features/ai/agent-form";

export const metadata = { title: "New agent" };

/**
 * Create an agent (REQ-099, slice 1). The form validates every field the API validates and
 * puts the API's own message under the field that caused it.
 */
export default function NewAgentPage() {
  return (
    <RequireAuth>
      <AppShell
        title="New agent"
        description="A model, a goal and the tools it may use"
      >
        <AgentForm />
      </AppShell>
    </RequireAuth>
  );
}
