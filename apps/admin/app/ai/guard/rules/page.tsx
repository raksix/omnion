import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { GuardRules } from "@/features/ai/guard-rules";

export const metadata = { title: "Guard rules" };

/**
 * `/ai/guard/rules` — the detector's rule set (docs/requests/REQ-105).
 *
 * A label default on the policy panel does nothing unless an enabled rule reports under that
 * label, so this table is where the guard is actually decided. Built-in rows can be disabled but
 * never edited or deleted; custom rows are the tenant's own.
 */
export default function GuardRulesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Guard rules"
        description="The expressions that decide what is inspected — built-in rules are disabled, never rewritten"
      >
        <GuardRules />
      </AppShell>
    </RequireAuth>
  );
}
