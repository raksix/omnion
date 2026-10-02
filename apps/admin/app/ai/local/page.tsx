import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiLocalView } from "@/features/ai/ai-local";

export const metadata = { title: "Local AI endpoints" };

/**
 * `/ai/local` — the endpoints this installation talks to (REQ-106, slice 1).
 *
 * The screen is deliberately about locality rather than about endpoints: every provider row
 * carries a verified `locality` and the *rule* that produced it, so an operator can answer "what
 * still talks to the internet?" by reading this page. Listing only local endpoints would make
 * that question unanswerable, which is the one thing the air-gap switch (slice 2) will rely on.
 */
export default function AiLocalPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Local AI endpoints"
        description="Where inference can run. A local endpoint is verified against loopback, private ranges and the internal allow-list — never taken on trust"
      >
        <AiLocalView />
      </AppShell>
    </RequireAuth>
  );
}
