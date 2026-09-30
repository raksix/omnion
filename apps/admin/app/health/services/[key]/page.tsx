import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { HealthServiceDetailScreen } from "@/features/health/health-service-detail";

export const metadata = { title: "Service detail" };

export default async function HealthServicePage({
  params,
}: {
  params: Promise<{ key: string }>;
}) {
  const { key } = await params;
  return (
    <RequireAuth>
      <AppShell
        title="Service detail"
        description="What this dependency answered, and what it has published"
      >
        <HealthServiceDetailScreen />
      </AppShell>
    </RequireAuth>
  );
}
