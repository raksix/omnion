import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SimulatorView } from "@/features/iam/simulator-view";

export const metadata = { title: "Simulator" };

/**
 * The permission simulator (REQ-006, slice 2): ask whether a subject holds a permission, in a
 * context that can name a site, a path, a department or a module, and read the chain.
 */
export default function IamSimulatorPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Simulator"
        description="Ask the decision path: subject, action, resource — with the explanation"
      >
        <SimulatorView />
      </AppShell>
    </RequireAuth>
  );
}
