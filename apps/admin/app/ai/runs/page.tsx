import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiRunsList } from "@/features/ai/ai-runs-list";

export const metadata = { title: "Agent runs" };

/**
 * The run history (REQ-099, slice 1). Filters live in the query string, so a filtered log can
 * be pasted to a colleague.
 */
export default function AiRunsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Agent runs"
        description="Every run, its stop reason, its tokens and what it cost"
      >
        <AiRunsList />
      </AppShell>
    </RequireAuth>
  );
}
