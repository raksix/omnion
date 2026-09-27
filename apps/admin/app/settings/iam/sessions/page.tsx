import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SessionsView } from "@/features/iam/sessions-view";

export const metadata = { title: "Sessions" };

/**
 * Live sessions with revocation (REQ-006, slice 3): who is signed in, from where, and the
 * policy state each session is in.
 */
export default function IamSessionsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Sessions"
        description="Who is signed in right now, and the state each session is in"
      >
        <SessionsView />
      </AppShell>
    </RequireAuth>
  );
}
