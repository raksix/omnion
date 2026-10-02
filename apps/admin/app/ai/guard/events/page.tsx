import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { GuardEvents } from "@/features/ai/guard-events";

export const metadata = { title: "Guard events" };

/**
 * `/ai/guard/events` — what the guard decided (docs/requests/REQ-105).
 *
 * One row per inspection, carrying the decision, the labels that fired and a short hash of one
 * matched value. **No payload text is shown, because none is stored**: the drawer prints the
 * server's own sentence saying so rather than a local paraphrase of a security claim.
 */
export default function GuardEventsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Guard events"
        description="Every decision the guard made, with the rules that fired — never the text it inspected"
      >
        <GuardEvents />
      </AppShell>
    </RequireAuth>
  );
}
