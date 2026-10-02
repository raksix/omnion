import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AirgapSettingsView } from "@/features/ai/airgap-settings";

export const metadata = { title: "Air gap" };

/**
 * `/ai/settings/airgap` — the air-gap switch (REQ-106, slice 2).
 *
 * Kept under `/ai` rather than the global `/settings` because the switch is not a policy an
 * operator sets once and forgets: it answers a question they ask *during an incident* — "is
 * anything still leaving this machine?" — and that question belongs beside the endpoint list that
 * classifies every provider, not on a settings page two nav entries away.
 *
 * Reading it needs `ai.local.read`; flipping it needs `ai.airgap.manage`, which is deliberately
 * its own key and granted to the Owner by default.
 */
export default function AirgapSettingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Air gap"
        description="Turn every non-local AI call off at once — refused before the request leaves this host, logged with the provider and the host that would have answered"
      >
        <AirgapSettingsView />
      </AppShell>
    </RequireAuth>
  );
}