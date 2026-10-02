import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiToolsView } from "@/features/ai/ai-tools";

export const metadata = { title: "Tool registry" };

/**
 * The AI tool registry (REQ-100, slice 1).
 *
 * One row per action an agent may take, with the permission it needs, its risk class, whether it
 * is gated behind approval, and what it has cost over thirty days. The screen is where an
 * operator answers "what is this installation's AI allowed to do, and who decided that" —
 * which is why it is a registry with usage columns rather than a settings form.
 */
export default function AiToolsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Tool registry"
        description="Every action an agent may take, the permission it needs, its risk class and its usage"
      >
        <AiToolsView />
      </AppShell>
    </RequireAuth>
  );
}
