import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { OverviewView } from "@/features/observability/overview-view";

export const metadata = { title: "Observability" };

/**
 * `/observability` — the landing screen of the centre (REQ-126).
 *
 * The first screen of this request in the spec, and the one that was missing: six sibling routes
 * shipped without it, so the centre had no front door and an operator arriving from a nav item had
 * to already know which of the six areas held the answer.
 *
 * It is one read rather than six. The panel composing it client-side would need seven round trips
 * before the first number appeared, and each is a separate way for the screen to render
 * partially — which, on the page an operator opens during an incident, is worse than a screen
 * that says which of its sources it could not read.
 */
export default function ObservabilityPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Observability"
        description="What this instance is doing right now, and every door into the detail behind it"
      >
        <OverviewView />
      </AppShell>
    </RequireAuth>
  );
}
