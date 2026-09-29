import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AgentForm } from "@/features/ai/agent-form";

export const metadata = { title: "Agent" };

/**
 * One agent in full (REQ-099): the same config form the create screen uses, so a limit
 * tightened in one is tightened in the other, plus the Skills and Workspace tabs. The Runs tab
 * is the last slice and is not rendered until it is — a tab that lists nothing is a tab that
 * lies.
 */
export default async function AgentPage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="Agent" description="What it may do, what it costs and how it ends">
        <AgentForm agentId={id} />
      </AppShell>
    </RequireAuth>
  );
}
