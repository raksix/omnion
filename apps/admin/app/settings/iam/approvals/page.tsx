import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ApprovalsView } from "@/features/iam/approvals-view";

export const metadata = { title: "Approvals" };

/**
 * The permission request inbox (REQ-006, slice 4b): who asked for what, over which window, and
 * the approve/refuse buttons that turn a request into a time-boxed binding.
 */
export default function IamApprovalsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Approvals"
        description="Permission requests waiting for a decision — an approval grants one window, and it ends on its own"
      >
        <ApprovalsView />
      </AppShell>
    </RequireAuth>
  );
}
