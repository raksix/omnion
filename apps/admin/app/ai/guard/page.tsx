import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { GuardPolicyPanel } from "@/features/ai/guard-policy-panel";

export const metadata = { title: "Data guard" };

/**
 * `/ai/guard` — the policy panel (docs/requests/REQ-105).
 *
 * What happens to the text a model is about to see: a default action per label, the masking
 * style, whether a user may weaken a label for their own calls, and the exemptions that narrow
 * any of it. The rules that make those defaults mean anything are one click away, and the log of
 * what actually fired is the other.
 */
export default function GuardPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Data guard"
        description="What is inspected before a provider sees it — one default action per label, and the exemptions that narrow them"
      >
        <GuardPolicyPanel />
      </AppShell>
    </RequireAuth>
  );
}
