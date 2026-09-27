import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { IamOverviewView } from "@/features/iam/overview-view";

export const metadata = { title: "Identity & access" };

/**
 * The IAM overview (REQ-006, slice 2): the counts, the temporary grants that run out soon and
 * the last privileged actions, each linking to the screen that owns it.
 */
export default function IamOverviewPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Identity & access"
        description="Accounts, roles, groups and machine identities — and what they may do"
      >
        <IamOverviewView />
      </AppShell>
    </RequireAuth>
  );
}
