import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ReliabilityIntakeView } from "@/features/reliability/intake-view";

export const metadata = { title: "Inbound intake" };

/**
 * `/settings/reliability/intake` — the inbound guard (REQ-127, slice 4).
 *
 * Its own route because it is the screen an integrator opens at 02:00 when a provider stopped
 * being accepted: the declarations, what each one refuses, and a tester that answers "is my
 * signature right?" with the platform's own verdict rather than the browser's.
 */
export default function ReliabilityIntakePage() {
  return (
    <RequireAuth>
      <AppShell
        title="Inbound intake"
        description="Declared inbound paths with their HMAC scheme, tolerance window, size cap and sanitisation profile — plus the rejection log and a sample tester that runs the platform's own guard"
      >
        <ReliabilityIntakeView />
      </AppShell>
    </RequireAuth>
  );
}
