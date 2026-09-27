import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { PoliciesView } from "@/features/iam/policies-view";

export const metadata = { title: "Policies" };

/**
 * The ABAC policy builder (REQ-006, slice 4a): conditions, the THEN block, the dry run and the
 * version history — the screen that turns the policy engine into something an administrator can
 * actually use.
 */
export default function IamPoliciesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Policies"
        description="Attribute-based rules that run after the roles: when a condition holds, allow or deny"
      >
        <PoliciesView />
      </AppShell>
    </RequireAuth>
  );
}
