import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { LocalGuideView } from "@/features/ai/local-guide";

export const metadata = { title: "Run AI locally" };

/**
 * `/ai/local/guide` — the operator manual for local inference (REQ-106, slice 4).
 *
 * A screen rather than a markdown file, because the request asks the page to name "what stops
 * working while the gap is on" and that list is a property of the *installation*, not of the build.
 * The prose lives beside the measurement so the two cannot drift.
 */
export default function AiLocalGuidePage() {
  return (
    <RequireAuth>
      <AppShell
        title="Run AI locally"
        description="Supported servers, the six steps in the order they work, and what the air gap stops — read from this installation's own state"
      >
        <LocalGuideView />
      </AppShell>
    </RequireAuth>
  );
}