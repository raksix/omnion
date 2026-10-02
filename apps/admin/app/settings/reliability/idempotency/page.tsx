import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ReliabilityIdempotencyView } from "@/features/reliability/idempotency-view";

export const metadata = { title: "Idempotency keys" };

/**
 * `/settings/reliability/idempotency` — the keyed-write ledger (REQ-127, slice 2).
 *
 * A sibling of `/settings/reliability/limits`, not of `/settings/security/rate-limits`: the two
 * limits screens answer "how much may this caller spend" and this one answers "did this write
 * run twice", and an operator who expects one of them to explain the other will not find it.
 */
export default function ReliabilityIdempotencyPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Idempotency keys"
        description="Keyed writes with their state, replay count and stored-response metadata, and the release action for a key a crashed attempt left behind"
      >
        <ReliabilityIdempotencyView />
      </AppShell>
    </RequireAuth>
  );
}
