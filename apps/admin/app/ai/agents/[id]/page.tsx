import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AgentForm } from "@/features/ai/agent-form";

export const metadata = { title: "Agent" };

/**
 * One agent in full (REQ-099, slice 1): the same config form the create screen uses, so a
 * limit tightened in one is tightened in the other. The Skills, Runs and Workspace tabs land
 * with slice 2 — the workspace needs the file table the migration has not shipped yet, and a
 * tab that lists nothing is a tab that lies.
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
