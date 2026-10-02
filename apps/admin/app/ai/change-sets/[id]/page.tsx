import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiChangeSetEditorScreen } from "@/features/ai/ai-change-set-editor";

export const metadata = { title: "Edit change set" };

/**
 * `/ai/change-sets/[id]` — the change-set editor (REQ-101, slice 3).
 *
 * The review screen decides one frozen tool call; this one edits a whole proposed list — drop an
 * operation, reorder two, change a value — and confirms it. A set whose operations are all
 * ungated applies on confirm; one that carries a gated operation parks in the same inbox.
 */
export default function AiChangeSetEditorPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Edit change set"
        description="The exact operations this agent proposed, re-planned against the targets as they are now"
      >
        <AiChangeSetEditorScreen />
      </AppShell>
    </RequireAuth>
  );
}
