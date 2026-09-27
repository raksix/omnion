import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { UserDetailView } from "@/features/iam/user-detail-view";

export const metadata = { title: "User" };

/**
 * One account in full (REQ-006, slice 2): the profile editor, the role bindings with the scope
 * ladder and an expiry window, and the resolved effective permission set.
 */
export default async function IamUserPage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="User account" description="Profile, role bindings and effective permissions">
        <UserDetailView userId={id} />
      </AppShell>
    </RequireAuth>
  );
}
