import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SignInProtectionScreen } from "@/features/security/sign-in-protection";

export const metadata = { title: "Sign-in protection" };

export default function SecuritySignInProtectionPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Sign-in protection"
        description="When a failed sign-in locks an account, and which accounts that has locked"
      >
        <SignInProtectionScreen />
      </AppShell>
    </RequireAuth>
  );
}
