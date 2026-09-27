import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AutomationsView } from "@/features/automations/automations-view";

export const metadata = { title: "Automations" };

/**
 * `/automations` — the automation rules (REQ-003, slice 1).
 *
 * A rule is trigger → condition → action; this screen lists them, edits one, and fires the
 * two tests the request names: a dry run against a hand-written payload, and a one-shot
 * listener for the next real event.
 */
export default function AutomationsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Automations"
        description="Rules that run when the platform records an event, or when a webhook calls in"
      >
        <AutomationsView />
      </AppShell>
    </RequireAuth>
  );
}
