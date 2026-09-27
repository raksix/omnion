import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SecurityView } from "@/features/iam/security-view";

export const metadata = { title: "Security policy" };

/**
 * Password, lockout, address, session and device policy (REQ-006, slice 3): the document every
 * sign-in path reads, with the ranges the server enforces shown on the field.
 */
export default function IamSecurityPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Security"
        description="Password rules, lockout, address lists, session lifetimes and device trust"
      >
        <SecurityView />
      </AppShell>
    </RequireAuth>
  );
}
