import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { GuardAboutView } from "@/features/ai/guard-about";

export const metadata = { title: "Guard coverage" };

/**
 * `/ai/guard/about` — the residual-risk statement (docs/requests/REQ-105).
 *
 * The other four guard screens describe what is in force. This one states what is not covered,
 * because a pattern filter that only ever reports its hits is indistinguishable from a complete
 * one — and the person who has to make that distinction in a review is usually not the person who
 * configures the guard.
 */
export default function GuardAboutPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Guard coverage"
        description="What this data guard does not catch — the residual risk behind the clean event log"
      >
        <GuardAboutView />
      </AppShell>
    </RequireAuth>
  );
}
