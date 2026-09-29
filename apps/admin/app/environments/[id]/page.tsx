import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { EnvironmentDetailView } from "@/features/environments/environment-detail-view";

export const metadata = { title: "Environment" };

export default async function EnvironmentDetailPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="Environment" description="What this copy holds and how it was made">
        <EnvironmentDetailView key={id} />
      </AppShell>
    </RequireAuth>
  );
}
