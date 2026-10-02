import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { CredentialDetail } from "@/features/nodes/credential-detail";

export const metadata = { title: "Credential" };

export default async function CredentialDetailPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  // The id is a route parameter, not a client read: the screen fetches the row itself so a
  // detail that changes underneath a stale page shows the new value rather than the old one.
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="Credential" description="Connection, health, usage and the way out">
        <CredentialDetail id={id} />
      </AppShell>
    </RequireAuth>
  );
}
