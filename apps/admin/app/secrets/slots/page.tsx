import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SlotsView } from "@/features/secrets/slots-view";

export const metadata = { title: "Credential slots" };

/**
 * `/secrets/slots` — the assignment matrix (REQ-125, slice 2).
 *
 * The screen exists because **no consumer holds a secret id.** A workload asks for `smtp` in the
 * `production` environment and gets whatever this matrix points at, so swapping a credential is
 * a row update here instead of a code change in every module. That is also why the editor asks
 * for a confirmation and names the consumer when it is about to take a slot away from something
 * that is actively resolving it.
 */
export default function SecretsSlotsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Credential slots"
        description="Which credential each named slot resolves — swap one, not the code"
      >
        <SlotsView />
      </AppShell>
    </RequireAuth>
  );
}
