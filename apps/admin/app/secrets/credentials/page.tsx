import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { CredentialsView } from "@/features/secrets/credentials-view";

export const metadata = { title: "Credentials" };

/**
 * `/secrets/credentials` — the typed credential list and its create wizard (REQ-125, slice 2).
 *
 * A credential is a stored secret pinned to one of five kinds, with the *non-secret* fields a
 * validator and a consumer need. The screen shows a chip for what the last validation said and
 * never a value: a red chip is a warning, not a lost credential.
 */
export default function SecretsCredentialsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Credentials"
        description="Typed credentials, what each kind needs, and whether it still works"
      >
        <CredentialsView />
      </AppShell>
    </RequireAuth>
  );
}
