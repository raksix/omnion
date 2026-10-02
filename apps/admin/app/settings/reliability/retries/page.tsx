import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ReliabilityRetriesView } from "@/features/reliability/retries-view";

export const metadata = { title: "Retries" };

/**
 * `/settings/reliability/retries` — per-subsystem retry policies, the attempt ledger and the
 * dead-letter list (REQ-127, slice 3).
 *
 * Beside `/settings/reliability/limits` and `/settings/reliability/idempotency` rather than
 * inside them: the three answer different questions at 03:00 — who is being refused, who is
 * being protected from a double write, and what happens when a delivery fails — and one screen
 * that tried to answer all three would bury the one being asked about.
 */
export default function ReliabilityRetriesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Retries"
        description="Per-subsystem retry policies with a delay preview, the attempt ledger a restarted worker resumes from, and dead letters with a retry now action"
      >
        <ReliabilityRetriesView />
      </AppShell>
    </RequireAuth>
  );
}
