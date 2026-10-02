import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeployKeysView } from "@/features/secrets/deploy-keys-view";

export const metadata = { title: "Deployment keys" };

/**
 * `/secrets/deploy-keys` — scoped machine credentials for CI (REQ-125, slice 3).
 *
 * A deployment key is the one identity here that exists to be pasted into a CI secret store and
 * forgotten, so the screen is built around the fact that it will eventually leak: the value is
 * shown exactly once, an expiry is required rather than optional, scopes are chosen explicitly
 * with least privilege first, and the use log — including the denials — is one click away, because
 * "I thought that key was dead" has to have an answer.
 */
export default function SecretsDeployKeysPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Deployment keys"
        description="Scoped, expiring machine identities for CI — they lease inside one environment and can never reveal"
      >
        <DeployKeysView />
      </AppShell>
    </RequireAuth>
  );
}
