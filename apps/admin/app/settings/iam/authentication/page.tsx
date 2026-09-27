import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AuthenticationView } from "@/features/iam/authentication-view";

export const metadata = { title: "Authentication" };

/**
 * Enterprise sign-in (REQ-006, slice 4b-2): the connected OIDC/OAuth2/SAML providers, their
 * discovery test and their sign-in log. Local sign-in is untouched by anything on this screen.
 */
export default function IamAuthenticationPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Authentication"
        description="Sign-in providers — connect a directory, prove it works, then switch it on"
      >
        <AuthenticationView />
      </AppShell>
    </RequireAuth>
  );
}
