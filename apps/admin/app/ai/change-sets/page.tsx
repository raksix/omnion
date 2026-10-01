import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiChangeSetsListScreen } from "@/features/ai/ai-change-sets";

export const metadata = { title: "Change sets" };

/**
 * `/ai/change-sets` — the proposed operation lists waiting to be edited (REQ-101, slice 3).
 *
 * The approval inbox holds one frozen tool call per row; a change set is a *list* an agent
 * proposed, and this is where a reviewer picks one up. The editor that follows is the screen
 * that changes anything — this one only routes.
 */
export default function AiChangeSetsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Change sets"
        description="Operations an agent proposed, still editable until somebody confirms them"
      >
        <AiChangeSetsListScreen />
      </AppShell>
    </RequireAuth>
  );
}
