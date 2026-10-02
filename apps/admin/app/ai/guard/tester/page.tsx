import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { GuardTester } from "@/features/ai/guard-tester";

export const metadata = { title: "Guard tester" };

/**
 * `/ai/guard/tester` — a dry run (docs/requests/REQ-105).
 *
 * Paste a payload, see the matches with their spans and hashes, and read the exact text a
 * provider would receive. **No provider is contacted**: the endpoint inspects the payload in the
 * API process and the text never leaves it, which is why this is safe to use with a customer's
 * real data while a policy is still being argued about.
 */
export default function GuardTesterPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Guard tester"
        description="Dry-run a payload against the current policy — no provider is contacted and the text never leaves this process"
      >
        <GuardTester />
      </AppShell>
    </RequireAuth>
  );
}
